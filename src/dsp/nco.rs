use num_complex::Complex32;

const PHASOR_NCO_RESYNC_SAMPLES: u32 = 1024;
const ROTOR_LANES: usize = 4;

/// A complex oscillator for frequency shifting. Multiplies IQ by `exp(j * phase)`
/// and keeps phase continuous across blocks.
/// Positive frequency offsets shift the spectrum upward. Phase inputs must be finite.
///
/// ```
/// use num_complex::Complex32;
/// use sdr_hal::dsp::PhasorNco;
///
/// let mut nco = PhasorNco::from_offset_hz(250, 1_000);
/// let unit = Complex32::new(1.0, 0.0);
/// assert!((nco.mix(unit) - unit).norm() < 1e-6);
/// assert!((nco.mix(unit) - Complex32::new(0.0, 1.0)).norm() < 1e-6);
/// ```
pub struct PhasorNco {
    epoch_phase_rad: f64,
    phase_step_rad: f64,
    /// Lane `n` holds the rotor for epoch position `group * ROTOR_LANES + n`.
    lanes: [Complex32; ROTOR_LANES],
    lane_step: Complex32,
    samples_since_resync: u32,
    active: bool,
}

fn phasor(phase_rad: f64) -> Complex32 {
    Complex32::new(phase_rad.cos() as f32, phase_rad.sin() as f32)
}

impl PhasorNco {
    /// Starts at zero phase with `phase_step_rad` radians per sample.
    pub fn new(phase_step_rad: f64) -> Self {
        assert!(phase_step_rad.is_finite(), "phase step must be finite");
        Self::starting_at(0.0, phase_step_rad, phase_step_rad != 0.0)
    }

    /// Uses `start_phase_rad` for the first sample, then advances by `phase_step_rad` per sample.
    pub fn with_start_phase(start_phase_rad: f64, phase_step_rad: f64) -> Self {
        assert!(
            start_phase_rad.is_finite() && phase_step_rad.is_finite(),
            "phase and phase step must be finite"
        );
        Self::starting_at(
            start_phase_rad,
            phase_step_rad,
            phase_step_rad != 0.0 || start_phase_rad != 0.0,
        )
    }

    fn starting_at(start_phase_rad: f64, phase_step_rad: f64, active: bool) -> Self {
        let mut nco = Self {
            epoch_phase_rad: start_phase_rad,
            phase_step_rad,
            lanes: [Complex32::new(1.0, 0.0); ROTOR_LANES],
            lane_step: phasor(phase_step_rad * ROTOR_LANES as f64),
            samples_since_resync: 0,
            active,
        };
        nco.load_lanes();
        nco
    }

    /// Sets a signed frequency shift in Hz at the supplied sample rate in Hz.
    /// A zero offset or zero rate produces an inactive oscillator.
    pub fn from_offset_hz(offset_hz: i64, sample_rate_hz: usize) -> Self {
        let step = if offset_hz == 0 || sample_rate_hz == 0 {
            0.0
        } else {
            2.0 * std::f64::consts::PI * offset_hz as f64 / sample_rate_hz as f64
        };
        Self::new(step)
    }

    /// Whether the oscillator rotates samples, either by its frequency step or its initial phase.
    #[inline]
    pub fn is_active(&self) -> bool {
        self.active
    }

    /// Multiplies one sample by the current phasor, then advances by one sample.
    #[inline]
    pub fn mix(&mut self, sample: Complex32) -> Complex32 {
        if !self.active {
            return sample;
        }
        sample * self.next_rotor()
    }

    /// Rotates every sample and advances oscillator state by the slice length.
    pub fn rotate_in_place(&mut self, samples: &mut [Complex32]) {
        if !self.active {
            return;
        }
        self.rotate_lanes(samples.len(), |index, rotor| samples[index] *= rotor);
    }

    /// Replaces `out` with `input` rotated, advancing by its length.
    pub fn rotate_into(&mut self, input: &[Complex32], out: &mut Vec<Complex32>) {
        out.clear();
        if !self.active {
            out.extend_from_slice(input);
            return;
        }
        out.resize(input.len(), Complex32::default());
        self.rotate_lanes(input.len(), |index, rotor| {
            out[index] = input[index] * rotor
        });
    }

    /// Adds `input` rotated and scaled by `gain` into `out`, advancing by its length.
    ///
    /// # Panics
    /// Panics when the lengths differ.
    pub fn rotate_accumulate(&mut self, input: &[Complex32], out: &mut [Complex32], gain: f32) {
        assert_eq!(input.len(), out.len());
        if !self.active {
            for (slot, &sample) in out.iter_mut().zip(input) {
                *slot += sample * gain;
            }
            return;
        }
        self.rotate_lanes(input.len(), |index, rotor| {
            out[index] += input[index] * rotor * gain;
        });
    }

    /// Replaces `out` with `(rotated_left + rotated_right * right_gain) * output_scale`.
    /// Advances both oscillators by the input length and reuses output capacity.
    ///
    /// # Panics
    /// Panics when the input lengths differ.
    pub fn rotate_sum_into(
        &mut self,
        left: &[Complex32],
        right_nco: &mut Self,
        right: &[Complex32],
        right_gain: f32,
        output_scale: f32,
        out: &mut Vec<Complex32>,
    ) {
        assert_eq!(left.len(), right.len());
        out.clear();
        out.reserve(left.len());
        out.extend(left.iter().zip(right).map(|(&left_sample, &right_sample)| {
            (self.mix(left_sample) + right_nco.mix(right_sample) * right_gain) * output_scale
        }));
    }

    /// A rotor does not depend on how the input is split across calls.
    #[inline(always)]
    fn rotate_lanes(&mut self, len: usize, mut apply: impl FnMut(usize, Complex32)) {
        let mut index = 0usize;
        while index < len {
            let aligned = (self.samples_since_resync as usize).is_multiple_of(ROTOR_LANES);
            if !aligned || len - index < ROTOR_LANES {
                apply(index, self.next_rotor());
                index += 1;
                continue;
            }
            let epoch_groups =
                (PHASOR_NCO_RESYNC_SAMPLES - self.samples_since_resync) as usize / ROTOR_LANES;
            let groups = ((len - index) / ROTOR_LANES).min(epoch_groups);
            for _ in 0..groups {
                for (lane, rotor) in self.lanes.iter_mut().enumerate() {
                    apply(index + lane, *rotor);
                    *rotor *= self.lane_step;
                }
                index += ROTOR_LANES;
            }
            self.samples_since_resync += (groups * ROTOR_LANES) as u32;
            if self.samples_since_resync == PHASOR_NCO_RESYNC_SAMPLES {
                self.resync();
            }
        }
    }

    #[inline]
    fn next_rotor(&mut self) -> Complex32 {
        let lane = self.samples_since_resync as usize % ROTOR_LANES;
        let rotor = self.lanes[lane];
        self.samples_since_resync += 1;
        if self.samples_since_resync == PHASOR_NCO_RESYNC_SAMPLES {
            self.resync();
        } else if lane == ROTOR_LANES - 1 {
            for rotor in &mut self.lanes {
                *rotor *= self.lane_step;
            }
        }
        rotor
    }

    fn resync(&mut self) {
        self.epoch_phase_rad = (self.epoch_phase_rad
            + self.phase_step_rad * f64::from(PHASOR_NCO_RESYNC_SAMPLES))
        .rem_euclid(2.0 * std::f64::consts::PI);
        self.samples_since_resync = 0;
        self.load_lanes();
    }

    fn load_lanes(&mut self) {
        for (lane, rotor) in self.lanes.iter_mut().enumerate() {
            *rotor = phasor(self.epoch_phase_rad + self.phase_step_rad * lane as f64);
        }
    }
}

#[cfg(test)]
mod tests {
    use super::PhasorNco;
    use num_complex::Complex32;
    use std::f64::consts::PI;
    #[test]
    fn phasor_nco_tracks_exact_phase_across_resync() {
        let (offset, rate) = (123_400i64, 1_228_800usize);
        let mut nco = PhasorNco::from_offset_hz(offset, rate);
        assert!(nco.is_active());
        let step = 2.0 * PI * offset as f64 / rate as f64;
        let mut max_err = 0.0f32;
        for n in 0..5000usize {
            let got = nco.mix(Complex32::new(1.0, 0.0));
            let phase = step * n as f64;
            let want = Complex32::new(phase.cos() as f32, phase.sin() as f32);
            max_err = max_err.max((got - want).norm());
        }
        assert!(max_err < 1e-3, "phasor drifted from exact NCO: {max_err}");
    }

    #[test]
    fn phasor_nco_with_start_phase_tracks_exact_phase() {
        let start = 0.3f64;
        let step = -2.0 * PI * 87_650.0 / 1_228_800.0;
        let mut nco = PhasorNco::with_start_phase(start, step);
        assert!(nco.is_active());
        let mut max_err = 0.0f32;
        for n in 0..5000usize {
            let got = nco.mix(Complex32::new(1.0, 0.0));
            let phase = start + step * n as f64;
            let want = Complex32::new(phase.cos() as f32, phase.sin() as f32);
            max_err = max_err.max((got - want).norm());
        }
        assert!(max_err < 1e-3, "phasor drifted from exact NCO: {max_err}");

        let mut constant = PhasorNco::with_start_phase(PI / 2.0, 0.0);
        assert!(constant.is_active());
        let got = constant.mix(Complex32::new(1.0, 0.0));
        assert!((got - Complex32::new(0.0, 1.0)).norm() < 1e-6);
    }

    #[test]
    fn phasor_nco_fused_rotate_sum_matches_separate_passes() {
        let left = (0..5000)
            .map(|n| Complex32::new((n as f32 * 0.017).sin(), (n as f32 * 0.031).cos()))
            .collect::<Vec<_>>();
        let right = (0..5000)
            .map(|n| Complex32::new((n as f32 * 0.043).cos(), (n as f32 * 0.029).sin()))
            .collect::<Vec<_>>();

        for (left_offset, right_offset) in
            [(123_400, -617_000), (0, -617_000), (123_400, 0), (0, 0)]
        {
            let mut separate_left = PhasorNco::from_offset_hz(left_offset, 4_915_200);
            let mut separate_right = PhasorNco::from_offset_hz(right_offset, 4_915_200);
            let mut fused_left = PhasorNco::from_offset_hz(left_offset, 4_915_200);
            let mut fused_right = PhasorNco::from_offset_hz(right_offset, 4_915_200);
            let mut got = Vec::new();
            for _ in 0..17 {
                separate_right.mix(Complex32::new(0.0, 0.0));
                fused_right.mix(Complex32::new(0.0, 0.0));
            }

            for range in [0..733, 733..2167, 2167..5000] {
                let mut expected_left = left[range.clone()].to_vec();
                let mut expected_right = right[range.clone()].to_vec();
                separate_left.rotate_in_place(&mut expected_left);
                separate_right.rotate_in_place(&mut expected_right);
                let expected = expected_left
                    .iter()
                    .zip(&expected_right)
                    .map(|(&left_sample, &right_sample)| (left_sample + right_sample * 0.37) * 0.23)
                    .collect::<Vec<_>>();

                fused_left.rotate_sum_into(
                    &left[range.clone()],
                    &mut fused_right,
                    &right[range],
                    0.37,
                    0.23,
                    &mut got,
                );
                assert_eq!(got, expected);
            }
        }
    }

    #[test]
    fn block_rotation_tracks_exact_phase_and_ignores_how_input_is_split() {
        let (offset, rate) = (-1_234_567i64, 12_288_000usize);
        let step = 2.0 * PI * offset as f64 / rate as f64;
        let input: Vec<Complex32> = (0..5_000)
            .map(|n| Complex32::new((n as f32 * 0.37).sin(), (n as f32 * 0.11).cos()))
            .collect();
        let mut whole = input.clone();
        PhasorNco::from_offset_hz(offset, rate).rotate_in_place(&mut whole);

        let mut in_place = input.clone();
        let mut into = Vec::new();
        let mut accumulated = vec![Complex32::new(0.25, -0.5); input.len()];
        let mut mixed = Vec::new();
        let (mut a, mut b, mut c, mut d) = (
            PhasorNco::from_offset_hz(offset, rate),
            PhasorNco::from_offset_hz(offset, rate),
            PhasorNco::from_offset_hz(offset, rate),
            PhasorNco::from_offset_hz(offset, rate),
        );
        let mut start = 0;
        for len in [1, 3, 7, 1021, 4, 5, 1030, 2, 2927].iter().cycle() {
            if start >= input.len() {
                break;
            }
            let end = (start + len).min(input.len());
            a.rotate_in_place(&mut in_place[start..end]);
            let mut part = Vec::new();
            b.rotate_into(&input[start..end], &mut part);
            into.extend_from_slice(&part);
            c.rotate_accumulate(&input[start..end], &mut accumulated[start..end], 2.0);
            mixed.extend(input[start..end].iter().map(|&sample| d.mix(sample)));
            start = end;
        }
        assert_eq!(in_place, whole);
        assert_eq!(into, whole);
        assert_eq!(mixed, whole);
        for (n, &sample) in input.iter().enumerate() {
            let phase = step * n as f64;
            let exact = sample * Complex32::new(phase.cos() as f32, phase.sin() as f32);
            assert!(
                (whole[n] - exact).norm() < 2e-5,
                "sample {n} drifted from exact phase"
            );
            let summed = Complex32::new(0.25, -0.5) + whole[n] * 2.0;
            assert!((accumulated[n] - summed).norm() < 1e-5, "accumulate {n}");
        }
    }

    #[test]
    fn an_inactive_oscillator_copies_and_scales_without_rotating() {
        let mut nco = PhasorNco::from_offset_hz(0, 1_000);
        let input = [Complex32::new(1.0, 2.0), Complex32::new(-3.0, 0.5)];
        let mut out = Vec::new();
        nco.rotate_into(&input, &mut out);
        assert_eq!(out, input);
        let mut sum = [Complex32::new(1.0, 1.0); 2];
        nco.rotate_accumulate(&input, &mut sum, 0.5);
        assert_eq!(sum, [Complex32::new(1.5, 2.0), Complex32::new(-0.5, 1.25)]);
    }
}
