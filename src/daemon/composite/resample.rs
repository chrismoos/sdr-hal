//! Rational polyphase FIR conversion. The phase accumulator advances on the integer
//! rate lattice, so fragmented input never changes sample counts or filter phase.

use num_complex::Complex32;

use crate::dsp::fir::dot_re_im;

const MIN_TAPS_PER_PHASE: usize = 16;
pub(super) const MAX_TAPS_PER_PHASE: usize = 1024;
const BLACKMAN_TRANSITION_SPAN: f64 = 6.0;
const SIMD_LANES: usize = 4;

pub(super) fn tap_count(input_rate_hz: f64, output_rate_hz: f64, passband_hz: f64) -> usize {
    let transition = (input_rate_hz.min(output_rate_hz) / 2.0 - passband_hz) / input_rate_hz;
    ((BLACKMAN_TRANSITION_SPAN / transition).ceil() as usize)
        .max(MIN_TAPS_PER_PHASE)
        .next_multiple_of(SIMD_LANES)
}

pub(super) struct RationalResampler {
    phases: Vec<Vec<f32>>,
    taps_per_phase: usize,
    re: Vec<f32>,
    im: Vec<f32>,
    head: usize,
    numerator: usize,
    denominator: usize,
    phase: usize,
}

impl RationalResampler {
    pub(super) fn new(
        numerator: usize,
        denominator: usize,
        input_rate_hz: f64,
        passband_hz: f64,
    ) -> Self {
        let output_rate_hz = input_rate_hz * numerator as f64 / denominator as f64;
        let taps_per_phase = tap_count(input_rate_hz, output_rate_hz, passband_hz);
        let stopband_hz = input_rate_hz.min(output_rate_hz) / 2.0;
        let cutoff = (passband_hz + stopband_hz) / (2.0 * input_rate_hz);
        let phases = (0..numerator)
            .map(|phase| {
                let fraction = phase as f64 / numerator as f64;
                let mut taps = (0..taps_per_phase)
                    .map(|lag| {
                        let distance = lag as f64 + fraction - (taps_per_phase / 2) as f64;
                        let angle = std::f64::consts::TAU * cutoff * distance;
                        let sinc = if angle.abs() < f64::EPSILON {
                            2.0 * cutoff
                        } else {
                            angle.sin() / (std::f64::consts::PI * distance)
                        };
                        let window_angle =
                            std::f64::consts::TAU * (distance / taps_per_phase as f64 + 0.5);
                        let window =
                            0.42 - 0.5 * window_angle.cos() + 0.08 * (2.0 * window_angle).cos();
                        (sinc * window) as f32
                    })
                    .collect::<Vec<_>>();
                let sum: f32 = taps.iter().sum();
                for tap in &mut taps {
                    *tap /= sum;
                }
                taps
            })
            .collect();
        Self {
            phases,
            taps_per_phase,
            re: vec![0.0; taps_per_phase * 2],
            im: vec![0.0; taps_per_phase * 2],
            head: 0,
            numerator,
            denominator,
            phase: 0,
        }
    }

    pub(super) fn group_delay_samples(&self) -> usize {
        self.taps_per_phase / 2
    }

    /// Places the next input at absolute index `input` on the rate lattice and returns
    /// the absolute index of the first output it produces.
    pub(super) fn start_at_input(&mut self, input: u64) -> u64 {
        let (numerator, denominator) = (self.numerator as u128, self.denominator as u128);
        let position = input as u128 * numerator;
        let first_output = position.div_ceil(denominator);
        self.phase = (first_output * denominator - position) as usize;
        first_output as u64
    }

    pub(super) fn process_into(&mut self, input: &[Complex32], output: &mut Vec<Complex32>) {
        output.clear();
        output.reserve(
            input.len() * self.numerator / self.denominator
                + self.numerator.div_ceil(self.denominator),
        );
        for sample in input {
            self.head = (self.head + self.taps_per_phase - 1) % self.taps_per_phase;
            self.re[self.head] = sample.re;
            self.re[self.head + self.taps_per_phase] = sample.re;
            self.im[self.head] = sample.im;
            self.im[self.head + self.taps_per_phase] = sample.im;
            while self.phase < self.numerator {
                let (re, im) = dot_re_im(
                    &self.phases[self.phase],
                    &self.re[self.head..self.head + self.taps_per_phase],
                    &self.im[self.head..self.head + self.taps_per_phase],
                );
                output.push(Complex32::new(re, im));
                self.phase += self.denominator;
            }
            self.phase -= self.numerator;
        }
    }
}

#[cfg(test)]
mod tests {
    use super::*;

    const DEVICE_RATE: f64 = 9_720_000.0;
    const CLIENT_RATE: f64 = 4_915_200.0;
    const PASSBAND: f64 = 750_000.0;

    fn tone(length: usize, frequency: f64, rate: f64) -> Vec<Complex32> {
        (0..length)
            .map(|n| {
                let phase = std::f64::consts::TAU * frequency * n as f64 / rate;
                Complex32::new(phase.cos() as f32, phase.sin() as f32)
            })
            .collect()
    }

    fn magnitude(samples: &[Complex32], frequency: f64, rate: f64) -> f32 {
        samples
            .iter()
            .enumerate()
            .map(|(n, s)| {
                let phase = -std::f64::consts::TAU * frequency * n as f64 / rate;
                *s * Complex32::new(phase.cos() as f32, phase.sin() as f32)
            })
            .sum::<Complex32>()
            .norm()
            / samples.len() as f32
    }

    #[test]
    fn a_conversion_keeps_exact_counts_and_phase_across_short_reads() {
        for (up, down, rate, count) in [
            (2025, 1024, CLIENT_RATE, 24_576),
            (1024, 2025, DEVICE_RATE, 48_600),
        ] {
            let input = tone(count * 3, 600_000.0, rate);
            let mut whole = RationalResampler::new(up, down, rate, PASSBAND);
            let mut split = RationalResampler::new(up, down, rate, PASSBAND);
            let mut expected = Vec::new();
            whole.process_into(&input, &mut expected);
            let mut actual = Vec::new();
            let mut part = Vec::new();
            for chunk in input.chunks(2705) {
                split.process_into(chunk, &mut part);
                actual.extend_from_slice(&part);
            }
            assert_eq!(actual, expected);
            assert_eq!(actual.len(), input.len() * up / down);
            let steady = &actual[actual.len() / 2..];
            let amplitude = magnitude(steady, 600_000.0, rate * up as f64 / down as f64);
            assert!((amplitude - 1.0).abs() < 0.003, "amplitude={amplitude}");
        }
    }

    #[test]
    fn a_conversion_rejects_tx_images_and_rx_aliases() {
        let mut tx = RationalResampler::new(2025, 1024, CLIENT_RATE, PASSBAND);
        let mut output = Vec::new();
        tx.process_into(&tone(24_576 * 4, 600_000.0, CLIENT_RATE), &mut output);
        let image = magnitude(&output[48_600..], 600_000.0 - CLIENT_RATE, DEVICE_RATE);
        assert!(image < 0.001, "TX image={image}");
        let mut rx = RationalResampler::new(1024, 2025, DEVICE_RATE, PASSBAND);
        rx.process_into(&tone(48_600 * 4, 3_000_000.0, DEVICE_RATE), &mut output);
        let power = output[24_576..].iter().map(|s| s.norm_sqr()).sum::<f32>()
            / (output.len() - 24_576) as f32;
        assert!(power < 1e-6, "RX alias power={power}");
    }
}
