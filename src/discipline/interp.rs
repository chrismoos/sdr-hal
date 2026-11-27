//! Reads a sample stream at fractional positions with a windowed-sinc kernel.

use num_complex::Complex32;

pub(crate) const TAPS: usize = 16;
/// Samples the kernel reaches before the integer part of a position.
pub(crate) const BEFORE: usize = TAPS / 2 - 1;
/// Samples the kernel reaches after the integer part of a position.
pub(crate) const AFTER: usize = TAPS / 2;
/// Fractional steps the kernel is stored at. A position is rounded to the nearest one,
/// which is an error below -60 dB for a signal that fills a quarter of the sample rate.
const PHASES: usize = 1024;
/// A position recomputed from the clock map can land a rounding error below a whole
/// sample, so one more sample is kept than the kernel reaches.
const ROUNDING_GUARD: usize = 1;
/// Silence kept ahead of a stream, so a position up to one sample before it is readable.
const LEAD_IN: usize = BEFORE + 1 + ROUNDING_GUARD;
const KAISER_BETA: f64 = 8.0;
/// Terms of the Bessel series, which converges to `f64` precision well before this
/// for the window's argument.
const BESSEL_TERMS: usize = 32;
/// Distances below this take the sinc limit of one instead of dividing by zero.
const SINC_ORIGIN: f64 = 1e-12;

fn bessel_i0(x: f64) -> f64 {
    let mut sum = 1.0;
    let mut term = 1.0;
    let half = x / 2.0;
    for k in 1..BESSEL_TERMS {
        term *= (half / k as f64) * (half / k as f64);
        sum += term;
    }
    sum
}

pub(crate) struct Kernel {
    rows: Vec<[f32; TAPS]>,
    run: crate::dsp::fir::FractionalRun<TAPS>,
}

impl Kernel {
    pub(crate) fn new() -> Self {
        let rows = (0..=PHASES)
            .map(|phase| {
                let fraction = phase as f64 / PHASES as f64;
                let mut row = [0f64; TAPS];
                for (tap, weight) in row.iter_mut().enumerate() {
                    let distance = tap as f64 - BEFORE as f64 - fraction;
                    let sinc = if distance.abs() < SINC_ORIGIN {
                        1.0
                    } else {
                        (std::f64::consts::PI * distance).sin() / (std::f64::consts::PI * distance)
                    };
                    let reach = distance / (TAPS as f64 / 2.0);
                    let window = if reach.abs() >= 1.0 {
                        0.0
                    } else {
                        bessel_i0(KAISER_BETA * (1.0 - reach * reach).sqrt())
                            / bessel_i0(KAISER_BETA)
                    };
                    *weight = sinc * window;
                }
                let sum: f64 = row.iter().sum();
                row.map(|weight| (weight / sum) as f32)
            })
            .collect();
        log::debug!(
            "clock: interpolator uses {}",
            crate::dsp::fir::interleaved_dot_name()
        );
        Self {
            rows,
            run: crate::dsp::fir::fractional_run(),
        }
    }
}

/// The recent samples of a stream, addressed by their index since the stream started.
pub(crate) struct Window {
    samples: Vec<Complex32>,
    /// Stream index of `samples[0]`.
    base: i64,
}

impl Window {
    pub(crate) fn new() -> Self {
        Self {
            samples: Vec::new(),
            base: 0,
        }
    }

    /// Starts a stream whose next pushed sample has index zero. Samples before it read
    /// as silence.
    pub(crate) fn restart(&mut self) {
        self.samples.clear();
        self.samples.resize(LEAD_IN, Complex32::new(0.0, 0.0));
        self.base = -(LEAD_IN as i64);
    }

    pub(crate) fn push(&mut self, samples: &[Complex32]) {
        self.samples.extend_from_slice(samples);
    }

    pub(crate) fn end(&self) -> i64 {
        self.base + self.samples.len() as i64
    }

    /// The last position [`Self::at`] can read.
    pub(crate) fn last_readable(&self) -> f64 {
        (self.end() - 1 - AFTER as i64) as f64
    }

    /// The stream at `position`, which must not be past [`Self::last_readable`] or
    /// before the samples kept by [`Self::discard_before`].
    #[cfg(test)]
    pub(crate) fn at(&self, kernel: &Kernel, position: f64) -> Complex32 {
        let mut out = [Complex32::new(0.0, 0.0)];
        self.read_run(kernel, position, 0.0, &mut out);
        out[0]
    }

    /// The stream at `position`, `position + step` and so on, one per slot of `out`.
    /// Every position is bound as [`Self::at`] binds one.
    pub(crate) fn read_run(
        &self,
        kernel: &Kernel,
        position: f64,
        step: f64,
        out: &mut [Complex32],
    ) {
        let start = position - (self.base + BEFORE as i64) as f64;
        (kernel.run)(&kernel.rows, &self.samples, start, step, out);
    }

    /// Renumbers the stream so the sample at index `count` becomes index zero.
    pub(crate) fn renumber(&mut self, count: i64) {
        self.base -= count;
    }

    /// Drops the samples no position at or after `position` reads.
    pub(crate) fn discard_before(&mut self, position: f64) {
        let keep_from = position.floor() as i64 - (BEFORE + ROUNDING_GUARD) as i64;
        let drop = (keep_from - self.base).clamp(0, self.samples.len() as i64) as usize;
        if drop > 0 {
            self.samples.drain(..drop);
            self.base += drop as i64;
        }
    }
}

#[cfg(test)]
mod tests {
    use super::*;

    const RATE: f64 = 4_915_200.0;
    const TONE_HZ: f64 = 600_000.0;

    fn tone_at(index: f64) -> Complex32 {
        let phase = std::f64::consts::TAU * TONE_HZ * index / RATE;
        Complex32::new(phase.cos() as f32, phase.sin() as f32)
    }

    fn window_with_tone(length: usize) -> Window {
        let mut window = Window::new();
        window.restart();
        let samples: Vec<_> = (0..length).map(|index| tone_at(index as f64)).collect();
        window.push(&samples);
        window
    }

    #[test]
    fn a_whole_position_returns_the_sample_itself() {
        let kernel = Kernel::new();
        let window = window_with_tone(64);
        for index in [20.0, 31.0, 40.0] {
            let sample = window.at(&kernel, index);
            assert!((sample - tone_at(index)).norm() < 1e-6, "index={index}");
        }
    }

    #[test]
    fn a_fractional_position_matches_the_signal_between_samples() {
        let kernel = Kernel::new();
        let window = window_with_tone(256);
        let mut worst = 0f32;
        for step in 0..2000 {
            let position = 40.0 + step as f64 * 0.0731;
            worst = worst.max((window.at(&kernel, position) - tone_at(position)).norm());
        }
        assert!(worst < 2e-3, "worst error {worst}");
    }

    #[test]
    fn discarding_keeps_what_later_positions_read() {
        let kernel = Kernel::new();
        let mut window = window_with_tone(256);
        let expected = window.at(&kernel, 100.4);
        window.discard_before(100.4);
        assert_eq!(window.at(&kernel, 100.4), expected);
        assert!(window.samples.len() < 256 - 80);
    }

    #[test]
    fn a_position_a_rounding_error_below_the_discard_point_is_still_readable() {
        let kernel = Kernel::new();
        let mut window = window_with_tone(256);
        window.discard_before(100.000_000_1);
        let sample = window.at(&kernel, 99.999_999_9);
        assert!((sample - tone_at(100.0)).norm() < 1e-3);
    }

    #[test]
    fn positions_before_the_first_sample_read_silence_then_the_signal() {
        let kernel = Kernel::new();
        let window = window_with_tone(64);
        assert!(window.at(&kernel, 0.0).norm() > 0.99);
        assert_eq!(window.last_readable(), (64 - 1 - AFTER) as f64);
    }

    #[test]
    fn the_selected_dot_product_matches_the_scalar_one() {
        let kernel = Kernel::new();
        let dot = crate::dsp::fir::interleaved_dot();
        println!(
            "kernel under test: {}",
            crate::dsp::fir::interleaved_dot_name()
        );
        let mut state = 0x2545_F491_4F6C_DD1Du64;
        let mut next = || {
            state ^= state << 13;
            state ^= state >> 7;
            state ^= state << 17;
            (state >> 40) as f32 / (1u64 << 23) as f32 - 1.0
        };
        let samples: Vec<Complex32> = (0..TAPS + 5)
            .map(|_| Complex32::new(next(), next()))
            .collect();
        for phase in [0, 1, 257, 511, 512, 1023, PHASES] {
            let row = &kernel.rows[phase];
            for offset in 0..5 {
                let fast = dot(row, &samples[offset..]);
                let slow = crate::dsp::fir::interleaved_dot_scalar(row, &samples[offset..]);
                assert!(
                    (fast - slow).norm() < 1e-5,
                    "phase={phase} offset={offset} {fast} {slow}"
                );
            }
        }
    }

    #[test]
    fn a_run_reads_each_position_as_a_single_read_does() {
        let kernel = Kernel::new();
        let mut window = Window::new();
        window.restart();
        let samples: Vec<Complex32> = (0..400)
            .map(|index| {
                let angle = index as f32 * 0.37;
                Complex32::new(angle.cos(), (angle * 1.3).sin())
            })
            .collect();
        window.push(&samples);
        for step in [1.0, 1.000_000_22, 0.999_998, 1.37] {
            let start = 3.25;
            let mut run = vec![Complex32::new(0.0, 0.0); 200];
            window.read_run(&kernel, start, step, &mut run);
            for (index, sample) in run.iter().enumerate() {
                let position: f64 = start + index as f64 * step;
                let whole = position.floor();
                let row = &kernel.rows[((position - whole) * PHASES as f64 + 0.5) as usize];
                let first = (whole as i64 - BEFORE as i64 - window.base) as usize;
                let expected = crate::dsp::fir::interleaved_dot_scalar(
                    row,
                    &window.samples[first..first + TAPS],
                );
                assert!(
                    (*sample - expected).norm() < 1e-5,
                    "step={step} index={index} {sample} {expected}"
                );
            }
        }
    }
}
