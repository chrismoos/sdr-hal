//! Device clock arithmetic and selection for backends whose timestamp clock is
//! independent of the sample rate.

/// Ticks per sample when the clock divides the rate, otherwise the leftover ticks.
/// A zero rate cannot be checked and reports the whole clock as leftover.
pub(crate) fn tick_alignment(clock_hz: u64, sample_rate_hz: u64) -> Result<u64, u64> {
    if sample_rate_hz == 0 {
        return Err(clock_hz);
    }
    let remainder = clock_hz % sample_rate_hz;
    if remainder == 0 {
        Ok(clock_hz / sample_rate_hz)
    } else {
        Err(remainder)
    }
}

#[cfg(feature = "uhd-backend")]
mod select {
    use crate::error::Error;

    /// Each probe writes a hardware PLL, so the descent is bounded.
    const MAX_CLOCK_ATTEMPTS: usize = 16;
    /// Ceiling discovery stops here, well above any current device clock.
    const MAX_PROBE_CLOCK_HZ: u64 = 1_000_000_000;
    /// Driver rate readbacks land a fraction short of a whole Hz.
    const WHOLE_HZ_TOLERANCE: f64 = 1e-3;
    /// `f64` represents integers exactly below two to the 53rd.
    const MAX_EXACT_INTEGER: f64 = 9_007_199_254_740_992.0;

    /// Rounds a driver rate readback to whole Hz. A rate that is not a whole number of
    /// Hz has no whole-multiple clock, so it is rejected rather than rounded.
    pub(crate) fn whole_rate_hz(rate_hz: f64) -> Result<u64, Error> {
        if !rate_hz.is_finite() || rate_hz <= 0.0 {
            return Err(Error::invalid_setting(format!(
                "sample rate must be finite and positive, got {rate_hz}"
            )));
        }
        if rate_hz >= MAX_EXACT_INTEGER {
            return Err(Error::invalid_setting(format!(
                "sample rate {rate_hz} Hz is too large for exact clock selection"
            )));
        }
        let rounded = rate_hz.round();
        if (rate_hz - rounded).abs() > WHOLE_HZ_TOLERANCE {
            return Err(Error::invalid_setting(format!(
                "sample rate {rate_hz} Hz is not a whole number of Hz, so no device clock is a whole multiple of it. Configure master_clock_rate to choose one anyway."
            )));
        }
        Ok(rounded as u64)
    }

    fn gcd(mut a: u64, mut b: u64) -> u64 {
        while b != 0 {
            let next = a % b;
            a = b;
            b = next;
        }
        a
    }

    fn lcm(a: u64, b: u64) -> Option<u64> {
        if a == 0 || b == 0 {
            return None;
        }
        (a / gcd(a, b)).checked_mul(b)
    }

    /// Smallest clock that is a whole multiple of every rate. Any multiple of this is
    /// also a multiple of each rate, so the descent only has to test against it.
    pub(crate) fn rates_lcm_hz(rates_hz: &[f64]) -> Result<u64, Error> {
        let mut combined: Option<u64> = None;
        for rate_hz in rates_hz {
            let rate = whole_rate_hz(*rate_hz)?;
            combined = match combined {
                None => Some(rate),
                Some(current) => Some(lcm(current, rate).ok_or_else(|| {
                    Error::invalid_setting(
                        "configured sample rates have no common whole-Hz clock".to_string(),
                    )
                })?),
            };
        }
        combined.ok_or_else(|| {
            Error::invalid_setting("clock selection needs at least one sample rate".to_string())
        })
    }

    /// Reads and writes a device clock. Implementations drive real hardware, so
    /// [`select_device_clock`] keeps the number of calls small.
    pub(crate) trait ClockProbe {
        /// Clock the device is running now.
        fn current_hz(&mut self) -> Result<u64, Error>;
        /// Highest clock worth requesting.
        fn ceiling_hz(&mut self) -> Result<u64, Error>;
        /// Requests `candidate_hz` and returns what the device applied.
        fn try_clock(&mut self, candidate_hz: u64) -> Result<u64, Error>;
    }

    /// Applies the largest device-accepted clock that is a whole multiple of every rate,
    /// and returns it. Restores the entry clock and errors when none qualifies.
    pub(crate) fn select_device_clock<P: ClockProbe>(
        probe: &mut P,
        rates_hz: &[f64],
    ) -> Result<u64, Error> {
        let step = rates_lcm_hz(rates_hz)?;
        let entry = probe.current_hz()?;
        let ceiling = probe.ceiling_hz()?.min(MAX_PROBE_CLOCK_HZ);
        if step > ceiling {
            return Err(unaligned_error(step, ceiling, 0));
        }

        let mut multiple = ceiling / step;
        let mut attempts = 0;
        while multiple >= 1 && attempts < MAX_CLOCK_ATTEMPTS {
            let candidate = match step.checked_mul(multiple) {
                Some(candidate) => candidate,
                None => {
                    multiple -= 1;
                    continue;
                }
            };
            let applied = probe.try_clock(candidate)?;
            attempts += 1;
            if applied != 0 && applied % step == 0 {
                return Ok(applied);
            }
            // A downward coercion rules out every multiple above what the device took,
            // so jump instead of stepping and turning a small step into thousands of
            // PLL writes.
            multiple = if applied < candidate && applied / step < multiple {
                applied / step
            } else {
                multiple - 1
            };
        }

        if let Err(e) = probe.try_clock(entry) {
            log::warn!("device clock: cannot restore {entry} Hz after selection failed: {e}");
        }
        Err(unaligned_error(step, ceiling, attempts))
    }

    fn unaligned_error(step: u64, ceiling: u64, attempts: usize) -> Error {
        Error::invalid_setting(format!(
            "no device clock up to {ceiling} Hz is a whole multiple of every configured sample rate, which needs a multiple of {step} Hz ({attempts} clocks tried). Configure master_clock_rate to choose one anyway."
        ))
    }
}

#[cfg(feature = "uhd-backend")]
pub(crate) use select::{ClockProbe, select_device_clock};

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn alignment_reports_ticks_per_sample_or_the_leftover() {
        assert_eq!(tick_alignment(30_720_000, 1_920_000), Ok(16));
        assert_eq!(tick_alignment(39_321_600, 1_966_080), Ok(20));
        // The pairing that ran with a silent warning before.
        assert_eq!(tick_alignment(39_321_600, 2_000_000), Err(1_321_600));
        assert_eq!(tick_alignment(1_000_000_000, 1_228_800), Err(985_600));
        assert_eq!(tick_alignment(39_321_600, 0), Err(39_321_600));
    }

    #[cfg(feature = "uhd-backend")]
    mod selection {
        use super::super::select::*;
        use crate::error::ErrorKind;

        #[test]
        fn readbacks_round_to_whole_hz_but_fractional_rates_are_refused() {
            assert_eq!(whole_rate_hz(2_000_000.0).unwrap(), 2_000_000);
            assert_eq!(whole_rate_hz(1_999_999.9999999998).unwrap(), 2_000_000);
            // A B210 produced exactly this rate, and nothing divides it.
            assert_eq!(
                whole_rate_hz(4_571_428.571428572).unwrap_err().kind(),
                ErrorKind::InvalidSetting
            );
            for bad in [0.0, -1.0, f64::NAN, f64::INFINITY, 9_007_199_254_740_992.0] {
                assert!(whole_rate_hz(bad).is_err(), "{bad}");
            }
        }

        #[test]
        fn common_multiples_combine_and_overflow_is_refused() {
            assert_eq!(rates_lcm_hz(&[4_915_200.0]).unwrap(), 4_915_200);
            assert_eq!(
                rates_lcm_hz(&[1_228_800.0, 1_920_000.0]).unwrap(),
                30_720_000
            );
            // Above any device clock, which is why the caller must be able to pin one.
            assert_eq!(
                rates_lcm_hz(&[2_000_000.0, 1_228_800.0]).unwrap(),
                768_000_000
            );
            assert!(rates_lcm_hz(&[]).is_err());
            assert!(rates_lcm_hz(&[5_000_000_001.0, 4_999_999_999.0]).is_err());
        }

        /// Models a device that coerces a request down to the nearest clock it accepts.
        struct FakeProbe {
            ceiling: u64,
            accepted: Vec<u64>,
            calls: Vec<u64>,
            current: u64,
        }

        impl FakeProbe {
            fn new(entry: u64, ceiling: u64, accepted: Vec<u64>) -> Self {
                Self {
                    ceiling,
                    accepted,
                    calls: Vec::new(),
                    current: entry,
                }
            }
        }

        impl ClockProbe for FakeProbe {
            fn current_hz(&mut self) -> Result<u64, crate::error::Error> {
                Ok(self.current)
            }
            fn ceiling_hz(&mut self) -> Result<u64, crate::error::Error> {
                Ok(self.ceiling)
            }
            fn try_clock(&mut self, candidate_hz: u64) -> Result<u64, crate::error::Error> {
                self.calls.push(candidate_hz);
                let applied = self
                    .accepted
                    .iter()
                    .copied()
                    .filter(|rate| *rate <= candidate_hz)
                    .max()
                    .unwrap_or(0);
                self.current = applied;
                Ok(applied)
            }
        }

        #[test]
        fn the_largest_accepted_multiple_wins_on_the_first_probe() {
            let accepted = (1..=2).map(|k| k * 30_720_000).collect();
            let mut probe = FakeProbe::new(32_000_000, 61_440_000, accepted);
            let chosen = select_device_clock(&mut probe, &[1_228_800.0, 1_920_000.0]).unwrap();
            assert_eq!(chosen, 61_440_000);
            assert_eq!(probe.calls, vec![61_440_000]);
        }

        #[test]
        fn a_coerced_readback_jumps_the_descent_instead_of_stepping() {
            let mut probe = FakeProbe::new(32_000_000, 61_440_000, vec![7_680_000]);
            let chosen = select_device_clock(&mut probe, &[1_920_000.0]).unwrap();
            assert_eq!(chosen, 7_680_000);
            assert!(
                probe.calls.len() <= 4,
                "descent took {} probes: {:?}",
                probe.calls.len(),
                probe.calls
            );
        }

        #[test]
        fn nothing_aligned_restores_the_entry_clock() {
            let mut probe = FakeProbe::new(39_321_600, 61_440_000, vec![39_321_600]);
            let error = select_device_clock(&mut probe, &[2_000_000.0]).unwrap_err();
            assert!(error.to_string().contains("master_clock_rate"), "{error}");
            assert_eq!(probe.calls.last(), Some(&39_321_600));
            assert_eq!(probe.current, 39_321_600);
        }

        /// Models a device that always lands one Hz below the request, so the descent
        /// can never jump and has to be stopped by the attempt cap.
        struct StubbornProbe {
            calls: usize,
        }

        impl ClockProbe for StubbornProbe {
            fn current_hz(&mut self) -> Result<u64, crate::error::Error> {
                Ok(32_000_000)
            }
            fn ceiling_hz(&mut self) -> Result<u64, crate::error::Error> {
                Ok(1_000_000_000)
            }
            fn try_clock(&mut self, candidate_hz: u64) -> Result<u64, crate::error::Error> {
                self.calls += 1;
                Ok(candidate_hz.saturating_sub(1))
            }
        }

        #[test]
        fn a_tiny_common_multiple_stops_at_the_attempt_cap() {
            let mut probe = StubbornProbe { calls: 0 };
            assert!(select_device_clock(&mut probe, &[1_000.0]).is_err());
            // Sixteen probes, then one restore. Without the cap this would be a million.
            assert_eq!(probe.calls, 17);
        }

        #[test]
        fn a_device_that_accepts_nothing_gives_up_at_once() {
            let mut probe = FakeProbe::new(32_000_000, 61_440_000, Vec::new());
            assert!(select_device_clock(&mut probe, &[1_920_000.0]).is_err());
            assert_eq!(probe.calls.len(), 2, "one probe and one restore");
        }

        #[test]
        fn a_common_multiple_above_the_ceiling_never_touches_the_device() {
            let mut probe = FakeProbe::new(32_000_000, 61_440_000, vec![32_000_000]);
            let error = select_device_clock(&mut probe, &[2_000_000.0, 1_228_800.0]).unwrap_err();
            assert!(error.to_string().contains("768000000"), "{error}");
            assert!(probe.calls.is_empty());
        }
    }
}
