//! Remote device time in native ticks, extrapolated from a local monotonic clock.

use std::sync::atomic::{AtomicI64, AtomicU64, Ordering, fence};
use std::time::Instant;

use parking_lot::Mutex;

/// Thread-safe estimate of the server's device clock, in that device's own ticks.
///
/// Each heartbeat replaces the anchor, so the estimate never drifts further than one
/// heartbeat interval. A heartbeat is already one network transit old when it arrives,
/// so the estimate reads slightly behind true device time, which is the safe direction
/// for scheduling ahead. Reads are nondecreasing unless [`Self::reanchor`] moves them.
pub struct NetworkClock {
    tick_rate: u64,
    epoch: Instant,
    offset_ticks: AtomicI64,
    floor_ticks: AtomicU64,
    anchor_seq: AtomicU64,
    writer: Mutex<()>,
}

impl NetworkClock {
    /// Anchors local elapsed time to `server_ticks` at construction.
    pub fn new(tick_rate: u64, server_ticks: u64) -> Self {
        let clock = Self {
            tick_rate,
            epoch: Instant::now(),
            offset_ticks: AtomicI64::new(0),
            floor_ticks: AtomicU64::new(server_ticks),
            anchor_seq: AtomicU64::new(0),
            writer: Mutex::new(()),
        };
        clock.offset_ticks.store(
            offset_for(server_ticks, clock.local_ticks()),
            Ordering::Relaxed,
        );
        clock
    }

    pub fn tick_rate(&self) -> u64 {
        self.tick_rate
    }

    fn local_ticks(&self) -> u64 {
        let elapsed_ns = self.epoch.elapsed().as_nanos();
        ((elapsed_ns * self.tick_rate as u128) / 1_000_000_000u128).min(u64::MAX as u128) as u64
    }

    fn estimate_ticks(&self, offset_ticks: i64) -> u64 {
        self.local_ticks().saturating_add_signed(offset_ticks)
    }

    /// Returns extrapolated device ticks without acquiring the writer mutex.
    pub fn now_ticks(&self) -> u64 {
        loop {
            let seq = self.anchor_seq.load(Ordering::Acquire);
            if seq & 1 == 1 {
                std::hint::spin_loop();
                continue;
            }
            let offset_ticks = self.offset_ticks.load(Ordering::Relaxed);
            let floor_ticks = self.floor_ticks.load(Ordering::Relaxed);
            fence(Ordering::Acquire);
            if self.anchor_seq.load(Ordering::Relaxed) != seq {
                continue;
            }
            let estimate = self.estimate_ticks(offset_ticks);
            if estimate <= floor_ticks {
                return floor_ticks;
            }
            if self
                .floor_ticks
                .compare_exchange(floor_ticks, estimate, Ordering::Relaxed, Ordering::Relaxed)
                .is_ok()
            {
                return estimate;
            }
        }
    }

    /// Applies a device-time reading observed at receipt, without lowering the
    /// monotonic floor. Reads stay nondecreasing, so an observation older than the
    /// current estimate holds time still until the extrapolation catches up.
    pub fn observe_server_time(&self, server_ticks: u64) {
        let _writer = self.writer.lock();
        log::debug!(
            "network radio clock observation: correction_us={:.3} server_ticks={server_ticks}",
            (server_ticks as f64 - self.now_ticks() as f64) * 1_000_000.0 / self.tick_rate as f64
        );
        self.store_anchor(server_ticks, false);
    }

    /// Resets the estimate and the monotonic floor. May move time backward, which is
    /// what a device clock reset requires.
    pub fn reanchor(&self, server_ticks: u64) {
        let _writer = self.writer.lock();
        self.store_anchor(server_ticks, true);
    }

    fn store_anchor(&self, server_ticks: u64, reset_floor: bool) {
        let seq = self.anchor_seq.load(Ordering::Relaxed);
        self.anchor_seq
            .store(seq.wrapping_add(1), Ordering::Relaxed);
        fence(Ordering::Release);
        self.offset_ticks.store(
            offset_for(server_ticks, self.local_ticks()),
            Ordering::Relaxed,
        );
        if reset_floor {
            self.floor_ticks.store(server_ticks, Ordering::Relaxed);
        }
        self.anchor_seq
            .store(seq.wrapping_add(2), Ordering::Release);
    }
}

fn offset_for(server_ticks: u64, local_ticks: u64) -> i64 {
    (server_ticks as i64).wrapping_sub(local_ticks as i64)
}

#[cfg(test)]
mod tests {
    use super::*;
    use std::time::Duration;

    const RATE: u64 = 1_000_000_000;

    #[test]
    fn clock_advances_from_anchor() {
        let clock = NetworkClock::new(RATE, 1_000_000_000);
        let t0 = clock.now_ticks();
        assert!(t0 >= 1_000_000_000);
        std::thread::sleep(Duration::from_millis(2));
        let t1 = clock.now_ticks();
        assert!(t1 > t0);
        assert!(t1 - t0 >= 2_000_000);
    }

    #[test]
    fn tick_rate_scales_the_extrapolation() {
        let clock = NetworkClock::new(1_000_000, 0);
        std::thread::sleep(Duration::from_millis(10));
        let elapsed = clock.now_ticks();
        assert!((10_000..100_000).contains(&elapsed), "{elapsed}");
    }

    #[test]
    fn heartbeat_corrects_a_slow_estimate() {
        let clock = NetworkClock::new(RATE, 1_000_000_000);
        let ahead = clock.now_ticks() + 10_000_000;
        clock.observe_server_time(ahead);
        assert!(clock.now_ticks() >= ahead);
    }

    #[test]
    fn heartbeat_older_than_the_estimate_never_moves_time_backward() {
        let clock = NetworkClock::new(RATE, 1_000_000_000);
        let t0 = clock.now_ticks();
        clock.observe_server_time(t0 - 500_000);
        assert!(clock.now_ticks() >= t0);
        std::thread::sleep(Duration::from_millis(2));
        assert!(clock.now_ticks() > t0);
    }

    #[test]
    fn backward_reanchor_keeps_clock_running_from_new_time() {
        let clock = NetworkClock::new(RATE, 10_000_000_000);
        assert!(clock.now_ticks() >= 10_000_000_000);

        let earlier = 2_000_000_000;
        clock.reanchor(earlier);
        let t0 = clock.now_ticks();
        assert!(t0 >= earlier && t0 < earlier + 1_000_000_000, "{t0}");
        std::thread::sleep(Duration::from_millis(2));
        let t1 = clock.now_ticks();
        assert!(
            t1 - t0 >= 2_000_000,
            "clock froze after re-anchor: {t0} {t1}"
        );
    }

    #[test]
    fn concurrent_readers_stay_monotonic_across_reanchors() {
        let clock = std::sync::Arc::new(NetworkClock::new(RATE, 5_000_000_000));
        let readers: Vec<_> = (0..4)
            .map(|_| {
                let clock = clock.clone();
                std::thread::spawn(move || {
                    let mut last = 0u64;
                    for _ in 0..50_000 {
                        let now = clock.now_ticks();
                        assert!(now >= last, "{now} < {last}");
                        last = now;
                    }
                })
            })
            .collect();
        for step in 0..100u64 {
            clock.observe_server_time(5_000_000_000 + step * 1_000);
        }
        for reader in readers {
            reader.join().expect("reader thread");
        }
    }
}
