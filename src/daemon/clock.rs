use std::sync::Arc;
use std::sync::atomic::{AtomicBool, AtomicU64, Ordering};
use std::thread::{self, JoinHandle};
use std::time::{Duration, Instant};

use crate::error::Error;
use crate::network::clock::NetworkClock;
use crate::{RadioClock, RadioTx};

const CLOCK_REFRESH_INTERVAL: Duration = Duration::from_secs(1);

pub(super) struct ClockSampler {
    clock: Arc<NetworkClock>,
    stop: Arc<AtomicBool>,
    max_refresh_us: Arc<AtomicU64>,
    thread: Option<JoinHandle<()>>,
}

fn sample(reader: &dyn RadioClock, tick_rate: u64) -> Result<(u64, Duration), Error> {
    let started = Instant::now();
    let ticks = reader.get_hardware_time()?;
    let elapsed = started.elapsed();
    // The device reading occurs within the call, so this bounds current time from above.
    let elapsed_ticks = (elapsed.as_nanos() * u128::from(tick_rate)
        / Duration::from_secs(1).as_nanos())
    .min(u128::from(u64::MAX)) as u64;
    Ok((ticks.saturating_add(elapsed_ticks), elapsed))
}

impl ClockSampler {
    pub(super) fn start(reader: Box<dyn RadioClock>, tick_rate: u64) -> Result<Self, Error> {
        let (ticks, elapsed) = sample(&*reader, tick_rate)?;
        let clock = Arc::new(NetworkClock::new(tick_rate, ticks));
        let stop = Arc::new(AtomicBool::new(false));
        let max_refresh_us = Arc::new(AtomicU64::new(elapsed.as_micros() as u64));
        let worker_clock = Arc::clone(&clock);
        let worker_stop = Arc::clone(&stop);
        let worker_max = Arc::clone(&max_refresh_us);
        let thread = thread::Builder::new()
            .name("radiod-clock".into())
            .spawn(move || {
                loop {
                    thread::park_timeout(CLOCK_REFRESH_INTERVAL);
                    if worker_stop.load(Ordering::Acquire) {
                        return;
                    }
                    match sample(&*reader, tick_rate) {
                        Ok((ticks, elapsed)) => {
                            worker_clock.observe_server_time(ticks);
                            worker_max.fetch_max(elapsed.as_micros() as u64, Ordering::Relaxed);
                        }
                        Err(error) => log::warn!("radiod: device clock refresh failed: {error}"),
                    }
                }
            })?;
        log::info!(
            "radiod: cached hardware clock enabled, refresh_interval_ms={}",
            CLOCK_REFRESH_INTERVAL.as_millis()
        );
        Ok(Self {
            clock,
            stop,
            max_refresh_us,
            thread: Some(thread),
        })
    }

    pub(super) fn now_ticks(&self) -> u64 {
        self.clock.now_ticks()
    }

    pub(super) fn take_max_refresh(&self) -> Duration {
        Duration::from_micros(self.max_refresh_us.swap(0, Ordering::Relaxed))
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
