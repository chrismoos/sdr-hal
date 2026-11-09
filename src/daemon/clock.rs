use std::sync::Arc;
use std::sync::atomic::{AtomicBool, AtomicU64, Ordering};
use std::thread::{self, JoinHandle};
use std::time::{Duration, Instant};

use crate::error::{Error, ErrorKind};
use crate::network::clock::NetworkClock;
use crate::{RadioClock, RadioTx};

const CLOCK_REFRESH_INTERVAL: Duration = Duration::from_secs(1);
const MAX_QUERY_DURATION: Duration = Duration::from_millis(1);
const STARTUP_SAMPLE_ATTEMPTS: usize = 8;

#[derive(Default)]
struct ClockMetrics {
    max_refresh_us: AtomicU64,
    rejected_queries: AtomicU64,
    uncertainty_us: AtomicU64,
}

pub(super) struct ClockDiagnostics {
    pub max_refresh: Duration,
    pub rejected_queries: u64,
    pub uncertainty_us: u64,
}

struct ClockReading {
    ticks: u64,
    elapsed: Duration,
}

impl ClockReading {
    fn observe(&self, clock: &NetworkClock, metrics: &ClockMetrics) {
        if self.accept(metrics) {
            clock.observe_server_time(self.ticks);
        }
    }

    fn accept(&self, metrics: &ClockMetrics) -> bool {
        metrics
            .max_refresh_us
            .fetch_max(self.elapsed.as_micros() as u64, Ordering::Relaxed);
        if self.elapsed > MAX_QUERY_DURATION {
            metrics.rejected_queries.fetch_add(1, Ordering::Relaxed);
            return false;
        }
        metrics.uncertainty_us.store(
            self.elapsed.as_micros().div_ceil(2) as u64,
            Ordering::Relaxed,
        );
        true
    }
}

pub(super) struct ClockSampler {
    clock: Arc<NetworkClock>,
    stop: Arc<AtomicBool>,
    metrics: Arc<ClockMetrics>,
    thread: Option<JoinHandle<()>>,
}

fn sample(reader: &dyn RadioClock, tick_rate: u64) -> Result<ClockReading, Error> {
    let started = Instant::now();
    let ticks = reader.get_hardware_time()?;
    let elapsed = started.elapsed();
    // Half the query time estimates the delay between the device reading and its return.
    let midpoint_ticks = (elapsed.as_nanos() * u128::from(tick_rate)
        / (2 * Duration::from_secs(1).as_nanos()))
    .min(u128::from(u64::MAX)) as u64;
    Ok(ClockReading {
        ticks: ticks.saturating_add(midpoint_ticks),
        elapsed,
    })
}

fn initial_sample(
    reader: &dyn RadioClock,
    tick_rate: u64,
    metrics: &ClockMetrics,
) -> Result<ClockReading, Error> {
    for _ in 0..STARTUP_SAMPLE_ATTEMPTS {
        let reading = sample(reader, tick_rate)?;
        if reading.accept(metrics) {
            return Ok(reading);
        }
    }
    Err(Error::new(
        ErrorKind::Timeout,
        format!(
            "device clock initialization: no query completed within {} us after {STARTUP_SAMPLE_ATTEMPTS} attempts",
            MAX_QUERY_DURATION.as_micros()
        ),
    ))
}

impl ClockSampler {
    pub(super) fn start(reader: Box<dyn RadioClock>, tick_rate: u64) -> Result<Self, Error> {
        let metrics = Arc::new(ClockMetrics::default());
        let reading = initial_sample(&*reader, tick_rate, &metrics)?;
        let clock = Arc::new(NetworkClock::new(tick_rate, reading.ticks));
        let stop = Arc::new(AtomicBool::new(false));
        let worker_clock = Arc::clone(&clock);
        let worker_stop = Arc::clone(&stop);
        let worker_metrics = Arc::clone(&metrics);
        let thread = thread::Builder::new()
            .name("radiod-clock".into())
            .spawn(move || {
                super::realtime::apply_data();
                loop {
                    thread::park_timeout(CLOCK_REFRESH_INTERVAL);
                    if worker_stop.load(Ordering::Acquire) {
                        return;
                    }
                    match sample(&*reader, tick_rate) {
                        Ok(reading) => {
                            reading.observe(&worker_clock, &worker_metrics);
                        }
                        Err(error) => log::warn!("radiod: device clock refresh failed: {error}"),
                    }
                }
            })?;
        log::info!(
            "radiod: cached hardware clock enabled, refresh_interval_ms={} max_query_us={} query_uncertainty_us={}",
            CLOCK_REFRESH_INTERVAL.as_millis(),
            MAX_QUERY_DURATION.as_micros(),
            metrics.uncertainty_us.load(Ordering::Relaxed)
        );
        Ok(Self {
            clock,
            stop,
            metrics,
            thread: Some(thread),
        })
    }

    pub(super) fn shared_clock(&self) -> Arc<NetworkClock> {
        Arc::clone(&self.clock)
    }

    pub(super) fn now_ticks(&self) -> u64 {
        self.clock.now_ticks()
    }

    pub(super) fn take_diagnostics(&self) -> ClockDiagnostics {
        // These independent counters are diagnostics and do not synchronize the clock estimate.
        ClockDiagnostics {
            max_refresh: Duration::from_micros(
                self.metrics.max_refresh_us.swap(0, Ordering::Relaxed),
            ),
            rejected_queries: self.metrics.rejected_queries.swap(0, Ordering::Relaxed),
            uncertainty_us: self.metrics.uncertainty_us.load(Ordering::Relaxed),
        }
    }
}

impl Drop for ClockSampler {
    fn drop(&mut self) {
        self.stop.store(true, Ordering::Release);
        if let Some(thread) = self.thread.take() {
            thread.thread().unpark();
            let _ = thread.join();
        }
    }
}

pub(super) fn device_time(tx: &dyn RadioTx, cache: Option<&ClockSampler>) -> Result<u64, Error> {
    match cache {
        Some(cache) => Ok(cache.now_ticks()),
        None => tx.get_hardware_time(),
    }
}

#[cfg(test)]
mod tests {
    use super::*;
    use std::sync::atomic::AtomicUsize;
    use std::sync::{Condvar, Mutex};

    const TEST_TICK_RATE: u64 = 1_000_000_000;

    struct DelayedClock {
        calls: AtomicUsize,
        always_slow: bool,
    }

    impl RadioClock for DelayedClock {
        fn get_hardware_time(&self) -> Result<u64, Error> {
            let call = self.calls.fetch_add(1, Ordering::SeqCst);
            if call == 0 || self.always_slow {
                thread::sleep(MAX_QUERY_DURATION * 2);
            }
            Ok(42 * TEST_TICK_RATE)
        }
    }

    #[test]
    fn initialization_discards_a_slow_query_and_retries_before_anchoring() {
        let reader = DelayedClock {
            calls: AtomicUsize::new(0),
            always_slow: false,
        };
        let metrics = ClockMetrics::default();
        let reading = initial_sample(&reader, TEST_TICK_RATE, &metrics).unwrap();
        assert!(reader.calls.load(Ordering::SeqCst) >= 2);
        assert!(reading.elapsed <= MAX_QUERY_DURATION);
        assert!(reading.ticks - 42 * TEST_TICK_RATE <= MAX_QUERY_DURATION.as_nanos() as u64 / 2);
        assert!(metrics.rejected_queries.load(Ordering::Relaxed) >= 1);
    }

    #[test]
    fn initialization_fails_when_no_clock_query_has_bounded_uncertainty() {
        let reader = DelayedClock {
            calls: AtomicUsize::new(0),
            always_slow: true,
        };
        let error = initial_sample(&reader, TEST_TICK_RATE, &ClockMetrics::default())
            .err()
            .unwrap();
        assert_eq!(error.kind(), ErrorKind::Timeout);
        assert_eq!(reader.calls.load(Ordering::SeqCst), STARTUP_SAMPLE_ATTEMPTS);
    }

    #[test]
    fn slow_refresh_cannot_push_the_running_clock_ahead() {
        let clock = NetworkClock::new(TEST_TICK_RATE, TEST_TICK_RATE);
        let metrics = ClockMetrics::default();
        let before = clock.now_ticks();
        ClockReading {
            ticks: before + TEST_TICK_RATE,
            elapsed: Duration::from_millis(134),
        }
        .observe(&clock, &metrics);
        let after = clock.now_ticks();
        assert!(after >= before);
        assert!(after < before + TEST_TICK_RATE / 2);
        assert_eq!(metrics.rejected_queries.load(Ordering::Relaxed), 1);
        let accepted = clock.now_ticks() + TEST_TICK_RATE;
        ClockReading {
            ticks: accepted,
            elapsed: Duration::from_micros(200),
        }
        .observe(&clock, &metrics);
        assert!(clock.now_ticks() >= accepted);
        assert_eq!(metrics.uncertainty_us.load(Ordering::Relaxed), 100);
    }

    struct BlockingClock {
        epoch: Instant,
        calls: Arc<AtomicUsize>,
        release: Arc<(Mutex<bool>, Condvar)>,
    }

    impl RadioClock for BlockingClock {
        fn get_hardware_time(&self) -> Result<u64, Error> {
            let call = self.calls.fetch_add(1, Ordering::SeqCst);
            if call != 0 {
                let (lock, ready) = &*self.release;
                let mut release = lock.lock().unwrap();
                while !*release {
                    release = ready.wait(release).unwrap();
                }
            }
            Ok(self.epoch.elapsed().as_nanos() as u64)
        }
    }

    #[test]
    fn cached_time_advances_while_the_once_per_second_refresh_is_blocked() {
        let calls = Arc::new(AtomicUsize::new(0));
        let release = Arc::new((Mutex::new(false), Condvar::new()));
        let cache = ClockSampler::start(
            Box::new(BlockingClock {
                epoch: Instant::now(),
                calls: calls.clone(),
                release: release.clone(),
            }),
            TEST_TICK_RATE,
        )
        .unwrap();
        for _ in 0..1000 {
            cache.now_ticks();
        }
        let initial_calls = calls.load(Ordering::SeqCst);
        let deadline = Instant::now() + Duration::from_secs(3);
        while calls.load(Ordering::SeqCst) < 2 && Instant::now() < deadline {
            thread::sleep(Duration::from_millis(1));
        }
        let refresh_started = calls.load(Ordering::SeqCst);
        let before = cache.now_ticks();
        thread::sleep(Duration::from_millis(20));
        let after = cache.now_ticks();
        let (lock, ready) = &*release;
        *lock.lock().unwrap() = true;
        ready.notify_all();
        drop(cache);
        assert_eq!(initial_calls, 1);
        assert_eq!(refresh_started, 2);
        assert!(after - before >= Duration::from_millis(20).as_nanos() as u64);
        assert_eq!(calls.load(Ordering::SeqCst), 2);
    }
}
