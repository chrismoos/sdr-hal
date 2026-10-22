use super::{
    ComplexFir32, DecimatingComplexFir32, Fir32, PolyphaseComplexFir32, SymmetricComplexFir32,
};
use num_complex::Complex32;

#[test]
fn fir32_preserves_state_across_blocks() {
    let taps = [0.25, 0.5, 0.25];
    let mut one = Fir32::new(&taps);
    let mut split = Fir32::new(&taps);

    let all = one.process_block(&[1.0, 2.0, 3.0, 4.0]);
    let mut pieces = split.process_block(&[1.0, 2.0]);
    pieces.extend(split.process_block(&[3.0, 4.0]));

    assert_eq!(all, pieces);
    assert_eq!(all, vec![0.25, 1.0, 2.0, 3.0]);
}

#[test]
fn symmetric_block_path_matches_per_sample_path_exactly() {
    let even: Vec<f64> = (0..48)
        .map(|i| {
            let x = (i as f64 - 23.5) / 8.0;
            (-x * x).exp() * (1.0 + 0.1 * x.cos())
        })
        .collect();
    let odd: Vec<f64> = (0..49)
        .map(|i| {
            let x = (i as f64 - 24.0) / 8.0;
            (-x * x).exp()
        })
        .collect();

    let samples: Vec<Complex32> = (0..500)
        .map(|i| {
            let t = i as f32;
            Complex32::new((t * 0.37).sin() * 1.7, (t * 0.11).cos() - 0.25)
        })
        .collect();

    for taps in [even, odd] {
        for split in [1usize, 7, 47, 48, 64, 250] {
            let mut per_sample = SymmetricComplexFir32::new(&taps);
            let expected: Vec<Complex32> = samples
                .iter()
                .map(|&s| per_sample.process_sample(s))
                .collect();

            let mut blocked = SymmetricComplexFir32::new(&taps);
            let mut actual = Vec::new();
            for chunk in samples.chunks(split) {
                actual.extend(blocked.process_block(chunk));
            }

            assert_eq!(actual, expected, "taps={} split={split}", taps.len());

            // The ring must also be left in a state the per-sample API can
            // continue from.
            let tail = [Complex32::new(0.5, -0.5), Complex32::new(-1.25, 2.0)];
            let expected_tail: Vec<Complex32> =
                tail.iter().map(|&s| per_sample.process_sample(s)).collect();
            let actual_tail: Vec<Complex32> =
                tail.iter().map(|&s| blocked.process_sample(s)).collect();
            assert_eq!(
                actual_tail,
                expected_tail,
                "taps={} split={split}",
                taps.len()
            );
        }
    }
}

#[test]
fn complex_fir_matches_real_iq_filtering() {
    let taps = [0.25, 0.5, 0.25];
    let samples = [
        Complex32::new(1.0, -1.0),
        Complex32::new(2.0, -2.0),
        Complex32::new(3.0, -3.0),
    ];
    let mut complex = ComplexFir32::new(&taps);
    let mut real = Fir32::new(&taps);
    let mut imag = Fir32::new(&taps);

    let complex_out = complex.process_block(&samples);
    let real_out = real.process_block(&[1.0, 2.0, 3.0]);
    let imag_out = imag.process_block(&[-1.0, -2.0, -3.0]);

    for (actual, (re, im)) in complex_out.iter().zip(real_out.into_iter().zip(imag_out)) {
        assert_eq!(*actual, Complex32::new(re, im));
    }
}

#[test]
fn interpolation_uses_polyphase_taps() {
    let taps = [1.0, 2.0, 3.0, 4.0];
    let mut fir = Fir32::with_interpolate(&taps, 2);
    assert_eq!(fir.process_block(&[1.0, 10.0]), vec![2.0, 4.0, 26.0, 48.0]);
}

#[test]
fn polyphase_complex_fir_matches_interpolation_reference() {
    let taps = [1.0, 2.0, 3.0, 4.0];
    let mut fir = PolyphaseComplexFir32::with_interpolate(&taps, 2);
    let out = fir.process_block(&[Complex32::new(1.0, -1.0), Complex32::new(10.0, -10.0)]);
    let want = [2.0, 4.0, 26.0, 48.0];
    assert_eq!(out.len(), want.len());
    for (got, want) in out.iter().zip(want) {
        assert_eq!(*got, Complex32::new(want, -want));
    }
}

#[test]
fn polyphase_complex_fir_matches_general_complex_fir() {
    for (tap_count, interpolate) in [(48usize, 4usize), (23, 2), (12, 1)] {
        let taps = (0..tap_count)
            .map(|n| (n as f64 * 0.37).sin() / (1.0 + n as f64 * 0.11))
            .collect::<Vec<_>>();
        let input = (0..1024)
            .map(|n| {
                Complex32::new(
                    (n as f32 * 0.113).sin() * 1.5,
                    (n as f32 * 0.071).cos() * 0.8,
                )
            })
            .collect::<Vec<_>>();

        let mut reference = ComplexFir32::with_interpolate(&taps, interpolate);
        let mut fast = PolyphaseComplexFir32::with_interpolate(&taps, interpolate);

        let (first, second) = input.split_at(400);
        let mut want = reference.process_block(first);
        want.extend(reference.process_block(second));
        let mut got = fast.process_block(first);
        got.extend(fast.process_block(second));

        assert_eq!(got.len(), want.len());
        for (idx, (got, want)) in got.iter().zip(&want).enumerate() {
            let err = (got - want).norm();
            assert!(
                err <= 1e-5 * (1.0 + want.norm()),
                "taps={tap_count} interpolate={interpolate} sample {idx}: got {got}, want {want}, err {err}"
            );
        }
    }
}

#[test]
fn symmetric_complex_fir_matches_general_complex_fir() {
    let taps = [0.125, 0.25, 0.375, 0.375, 0.25, 0.125];
    let samples = [
        Complex32::new(1.0, -2.0),
        Complex32::new(0.5, 3.0),
        Complex32::new(-4.0, 0.25),
        Complex32::new(2.5, -1.5),
        Complex32::new(0.0, 0.75),
        Complex32::new(5.0, -0.5),
        Complex32::new(-1.0, 1.0),
    ];
    let mut general = ComplexFir32::new(&taps);
    let mut symmetric = SymmetricComplexFir32::new(&taps);

    assert_eq!(
        general.process_block(&samples),
        symmetric.process_block(&samples)
    );
}

#[test]
fn decimation_preserves_state_across_blocks() {
    let taps = crate::dsp::design::rate_change_lowpass(4, 0.05);
    let input: Vec<Complex32> = (0..64)
        .map(|n| Complex32::new(n as f32, -(n as f32)))
        .collect();

    let mut whole = DecimatingComplexFir32::new(&taps, 4);
    let mut split = DecimatingComplexFir32::new(&taps, 4);
    let all = whole.process_block(&input);
    let mut pieces = split.process_block(&input[..7]);
    pieces.extend(split.process_block(&input[7..30]));
    pieces.extend(split.process_block(&input[30..]));

    assert_eq!(all.len(), 16);
    assert_eq!(all, pieces);
}

#[test]
fn outputs_for_predicts_the_kept_count_from_any_phase() {
    let taps = crate::dsp::design::rate_change_lowpass(5, 0.05);
    let mut filter = DecimatingComplexFir32::new(&taps, 5);
    let sample = Complex32::new(1.0, 0.0);
    for len in [0usize, 1, 3, 5, 7, 11] {
        let predicted = filter.outputs_for(len);
        let produced = filter.process_block(&vec![sample; len]).len();
        assert_eq!(predicted, produced, "len {len}");
    }
}

#[test]
fn decimating_by_one_matches_the_symmetric_filter() {
    let taps = crate::dsp::design::blackman_sinc_lowpass(21, 0.2);
    let input: Vec<Complex32> = (0..40)
        .map(|n| Complex32::new((n as f32 * 0.3).sin(), (n as f32 * 0.1).cos()))
        .collect();
    let plain = SymmetricComplexFir32::new(&taps).process_block(&input);
    let decimated = DecimatingComplexFir32::new(&taps, 1).process_block(&input);
    assert_eq!(plain, decimated);
}

#[test]
fn decimation_rejects_a_tone_that_would_fold_into_the_kept_band() {
    const DECIMATE: usize = 10;
    let taps = crate::dsp::design::rate_change_lowpass(DECIMATE, 0.01);
    let tone = |fraction: f64| -> f32 {
        let input: Vec<Complex32> = (0..4000)
            .map(|n| {
                let phase = std::f64::consts::TAU * fraction * n as f64;
                Complex32::new(phase.cos() as f32, phase.sin() as f32)
            })
            .collect();
        let out = DecimatingComplexFir32::new(&taps, DECIMATE).process_block(&input);
        // Skip the filter transient before measuring the steady-state level.
        let steady = &out[taps.len() / DECIMATE + 1..];
        steady.iter().map(|s| s.norm()).sum::<f32>() / steady.len() as f32
    };

    let kept = tone(0.003);
    let folded = tone(0.11);
    assert!(kept > 0.9, "kept tone attenuated to {kept}");
    assert!(folded < 0.01, "folding tone only reached {folded}");
}

#[test]
fn group_delay_is_the_symmetric_filter_center() {
    let taps = crate::dsp::design::blackman_sinc_lowpass(63, 0.2);
    assert_eq!(
        DecimatingComplexFir32::new(&taps, 2).group_delay_samples(),
        31
    );
}
