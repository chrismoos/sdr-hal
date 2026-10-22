//! Conversions between SoapySDR's nanoseconds and Hz and the server's ticks and whole Hz.

use sdr_hal::error::Error;

const NANOS_PER_SECOND: u128 = 1_000_000_000;
/// Rates and frequencies within this many Hz of a whole number are taken as that number,
/// since applications compute them in floating point.
const WHOLE_HZ_TOLERANCE: f64 = 1e-3;

/// Device ticks for a nanosecond time, rounded to the nearest tick.
pub(super) fn ns_to_ticks(time_ns: i64, tick_rate: u64) -> Result<u64, Error> {
    let time_ns = u128::try_from(time_ns)
        .map_err(|_| Error::from(format!("device time {time_ns} ns is negative")))?;
    let ticks = (time_ns * tick_rate as u128 + NANOS_PER_SECOND / 2) / NANOS_PER_SECOND;
    u64::try_from(ticks)
        .map_err(|_| Error::from(format!("device time {time_ns} ns exceeds the tick range")))
}

/// Nanoseconds for a device tick count, rounded to the nearest nanosecond.
pub(super) fn ticks_to_ns(ticks: u64, tick_rate: u64) -> i64 {
    if tick_rate == 0 {
        return 0;
    }
    let ns = (ticks as u128 * NANOS_PER_SECOND + tick_rate as u128 / 2) / tick_rate as u128;
    i64::try_from(ns).unwrap_or(i64::MAX)
}

/// Ticks spanned by `samples` at `rate_hz`, matching how the server times a block.
pub(super) fn samples_to_ticks(samples: u64, tick_rate: u64, rate_hz: f64) -> u64 {
    if rate_hz <= 0.0 {
        return 0;
    }
    (samples as f64 * tick_rate as f64 / rate_hz).floor() as u64
}

/// A positive rate or frequency as whole Hz, which is what the server takes.
pub(super) fn whole_hz(value_hz: f64, what: &str) -> Result<u64, Error> {
    let rounded = value_hz.round();
    if !value_hz.is_finite()
        || rounded < 1.0
        || rounded >= u64::MAX as f64
        || (value_hz - rounded).abs() > WHOLE_HZ_TOLERANCE
    {
        return Err(format!("{what} {value_hz} Hz must be a positive whole number of Hz").into());
    }
    Ok(rounded as u64)
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn nanoseconds_and_ticks_round_trip_on_a_clock_that_is_not_one_ghz() {
        const TICK_RATE: u64 = 38_880_000;
        let ticks = ns_to_ticks(1_000_000_000, TICK_RATE).unwrap();
        assert_eq!(ticks, TICK_RATE);
        assert_eq!(ticks_to_ns(ticks, TICK_RATE), 1_000_000_000);
        assert_eq!(ns_to_ticks(26, TICK_RATE).unwrap(), 1);
        assert!(ns_to_ticks(-1, TICK_RATE).is_err());
    }

    #[test]
    fn floating_point_rates_resolve_to_whole_hz_and_fractions_are_refused() {
        assert_eq!(whole_hz(2_399_999.999_9, "rate").unwrap(), 2_400_000);
        assert_eq!(whole_hz(388_800.0, "rate").unwrap(), 388_800);
        assert!(whole_hz(1_000_000.5, "rate").is_err());
        assert!(whole_hz(0.0, "rate").is_err());
        assert!(whole_hz(f64::NAN, "rate").is_err());
    }

    #[test]
    fn sample_spans_follow_the_rate() {
        assert_eq!(samples_to_ticks(324, 31_104_000, 388_800.0), 25_920);
        assert_eq!(samples_to_ticks(10, 31_104_000, 0.0), 0);
    }
}
