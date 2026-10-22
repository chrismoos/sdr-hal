//! Filter tap design.

use std::f64::consts::PI;

/// Widest tap count [`lowpass_for_transition`] returns. A narrower transition than this
/// supports is met with the widest filter rather than an unbounded one.
pub const MAX_LOWPASS_TAPS: usize = 1023;

/// Blackman-windowed sinc low-pass taps, normalized to unit DC gain.
///
/// `cutoff_fraction` is the -6 dB point as a fraction of the sample rate, so it stays
/// below 0.5. The tap count must be odd, which keeps the taps exactly mirrored for
/// [`SymmetricComplexFir32`](super::fir::SymmetricComplexFir32).
///
/// # Panics
/// Panics unless `taps` is odd and at least 3, and `cutoff_fraction` is in `(0, 0.5)`.
pub fn blackman_sinc_lowpass(taps: usize, cutoff_fraction: f64) -> Vec<f64> {
    assert!(taps >= 3 && taps % 2 == 1, "tap count must be odd and >= 3");
    assert!(
        cutoff_fraction > 0.0 && cutoff_fraction < 0.5,
        "cutoff fraction must be within (0, 0.5)"
    );

    let last = (taps - 1) as f64;
    let center = last / 2.0;
    let mut out = Vec::with_capacity(taps);
    for n in 0..taps {
        let x = n as f64 - center;
        let sinc = if x == 0.0 {
            2.0 * cutoff_fraction
        } else {
            (2.0 * PI * cutoff_fraction * x).sin() / (PI * x)
        };
        let phase = 2.0 * PI * n as f64 / last;
        let window = 0.42 - 0.5 * phase.cos() + 0.08 * (2.0 * phase).cos();
        out.push(sinc * window);
    }

    let gain: f64 = out.iter().sum();
    if gain.abs() > f64::EPSILON {
        for tap in &mut out {
            *tap /= gain;
        }
    }
    out
}

/// Tap count whose Blackman transition fits within `transition_fraction` of the sample
/// rate, reaching about -60 dB by the far edge. Always odd, and clamped to
/// [`MAX_LOWPASS_TAPS`].
pub fn taps_for_transition(transition_fraction: f64) -> usize {
    // A Blackman window reaches -60 dB after about 2.8 / N in normalized frequency.
    const BLACKMAN_TRANSITION_TAPS: f64 = 2.8;
    if transition_fraction.is_nan() || transition_fraction <= 0.0 {
        return MAX_LOWPASS_TAPS;
    }
    let needed = (BLACKMAN_TRANSITION_TAPS / transition_fraction).ceil();
    let taps = if needed >= MAX_LOWPASS_TAPS as f64 {
        MAX_LOWPASS_TAPS
    } else {
        (needed as usize).max(3)
    };
    taps | 1
}

/// Low-pass taps that pass `cutoff_fraction` and reach the stopband by
/// `stopband_fraction`, both as fractions of the sample rate. The tap count follows the
/// gap between them, so a caller that needs a sharper filter pays for it in taps.
///
/// # Panics
/// Panics unless `0 < cutoff_fraction < stopband_fraction <= 0.5`.
pub fn lowpass_for_transition(cutoff_fraction: f64, stopband_fraction: f64) -> Vec<f64> {
    assert!(
        cutoff_fraction > 0.0 && cutoff_fraction < stopband_fraction && stopband_fraction <= 0.5,
        "need 0 < cutoff {cutoff_fraction} < stopband {stopband_fraction} <= 0.5"
    );
    let taps = taps_for_transition(stopband_fraction - cutoff_fraction);
    blackman_sinc_lowpass(taps, cutoff_fraction)
}

/// Anti-alias taps for changing rate by `ratio`, where `occupied_fraction` of the faster
/// rate must survive. Everything above `0.5 / ratio` folds into the slower band, so that
/// is where the stopband has to start.
///
/// # Panics
/// Panics if `ratio` is below two, or `occupied_fraction` leaves no transition room.
pub fn rate_change_lowpass(ratio: usize, occupied_fraction: f64) -> Vec<f64> {
    assert!(ratio >= 2, "a rate change needs a ratio of at least two");
    lowpass_for_transition(occupied_fraction, 0.5 / ratio as f64)
}

#[cfg(test)]
mod tests {
    use super::*;

    fn response_db(taps: &[f64], frequency_fraction: f64) -> f64 {
        let center = (taps.len() - 1) as f64 / 2.0;
        let (mut re, mut im) = (0.0f64, 0.0f64);
        for (n, &tap) in taps.iter().enumerate() {
            let phase = -2.0 * PI * frequency_fraction * (n as f64 - center);
            re += tap * phase.cos();
            im += tap * phase.sin();
        }
        20.0 * re.hypot(im).max(f64::MIN_POSITIVE).log10()
    }

    #[test]
    fn taps_are_mirrored_and_unit_gain_at_dc() {
        let taps = blackman_sinc_lowpass(63, 0.1);
        for (front, back) in taps.iter().zip(taps.iter().rev()) {
            assert!((front - back).abs() < 1e-12, "{front} vs {back}");
        }
        assert!((taps.iter().sum::<f64>() - 1.0).abs() < 1e-12);
        assert!(response_db(&taps, 0.0).abs() < 1e-9);
    }

    #[test]
    fn the_tap_count_follows_the_transition_width() {
        assert!(taps_for_transition(0.1) < taps_for_transition(0.01));
        assert_eq!(taps_for_transition(0.0), MAX_LOWPASS_TAPS);
        assert_eq!(taps_for_transition(-1.0), MAX_LOWPASS_TAPS);
        assert_eq!(taps_for_transition(1e-9), MAX_LOWPASS_TAPS);
        for width in [0.5, 0.2, 0.05, 0.01, 0.002] {
            let count = taps_for_transition(width);
            assert_eq!(count % 2, 1, "{count} taps for width {width}");
        }
    }

    #[test]
    fn a_designed_transition_actually_reaches_the_stopband() {
        for (cutoff, stopband) in [(0.01, 0.05), (0.002, 0.05), (0.04, 0.05), (0.1, 0.25)] {
            let taps = lowpass_for_transition(cutoff, stopband);
            let passband = response_db(&taps, cutoff * 0.5);
            let reached = response_db(&taps, stopband);
            assert!(passband > -1.5, "passband {passband:.2} dB at {cutoff}");
            assert!(
                reached < -55.0,
                "{} taps reached only {reached:.1} dB at {stopband}",
                taps.len()
            );
        }
    }

    #[test]
    fn a_decimation_filter_stops_the_band_that_folds() {
        // A 20 kHz carrier in a 3.888 MHz composite decimated by ten.
        let occupied = 20_000.0 / 3_888_000.0;
        let taps = rate_change_lowpass(10, occupied / 2.0);
        assert!(response_db(&taps, 0.0).abs() < 1e-9);
        assert!(response_db(&taps, occupied / 2.0) > -6.1);
        let fold = response_db(&taps, 0.05);
        assert!(fold < -55.0, "fold edge only {fold:.1} dB");
        assert!(response_db(&taps, 0.25) < -60.0);
    }

    #[test]
    fn a_full_band_decimation_filter_costs_many_more_taps() {
        let narrow = rate_change_lowpass(10, 0.0026);
        let full = rate_change_lowpass(10, 0.04);
        assert!(
            full.len() > narrow.len() * 3,
            "{} vs {} taps",
            full.len(),
            narrow.len()
        );
        assert!(response_db(&full, 0.05) < -55.0);
    }
}
