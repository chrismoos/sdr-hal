//! Causal FIR filters with zero-initialized history retained across calls.
//!
//! Real taps are converted to `f32`. Tap zero multiplies the newest input.
//! Interpolation emits phase order `0..L`, equivalent to zero insertion followed
//! by FIR filtering with gain `L`. Constructors do not normalize taps. Block
//! methods produce exactly the input length times `L` and do not flush the tail.
//! Supply zeros explicitly when the filter tail is needed.
//!
//! ```
//! use sdr_hal::dsp::fir::Fir32;
//!
//! let mut filter = Fir32::new(&[0.5, 0.5]);
//! assert_eq!(filter.process_block(&[1.0, 1.0]), vec![0.5, 1.0]);
//! assert_eq!(filter.process_block(&[0.0]), vec![0.5]);
//! ```

use num_complex::Complex32;

mod kernels;
pub(crate) use kernels::dot_re_im;
use kernels::symmetric_complex_accumulate;

/// Real-input FIR with optional integer interpolation.
pub struct Fir32 {
    taps: Vec<f32>,
    delay: Vec<f32>,
    head: usize,
    delay_len: usize,
    interpolate: usize,
}

impl Fir32 {
    /// Creates a rate-preserving filter with zero history.
    ///
    /// # Panics
    /// Panics if taps are empty.
    pub fn new(taps: &[f64]) -> Self {
        Self::with_interpolate(taps, 1)
    }

    /// Creates an interpolating filter with zeroed history. The tap array is padded
    /// with zeros to a multiple of `interpolate`.
    ///
    /// # Panics
    /// Panics if taps are empty or `interpolate` is zero.
    pub fn with_interpolate(taps: &[f64], interpolate: usize) -> Self {
        assert!(!taps.is_empty());
        assert!(interpolate > 0);

        let mut taps = taps.iter().map(|&tap| tap as f32).collect::<Vec<_>>();
        let rem = taps.len() % interpolate;
        if rem != 0 {
            taps.resize(taps.len() + (interpolate - rem), 0.0);
        }
        let delay_len = taps.len() / interpolate;
        Self {
            taps,
            delay: vec![0.0; delay_len * 2],
            head: 1 % delay_len,
            delay_len,
            interpolate,
        }
    }

    /// Filters one sample. The interpolation factor must be one.
    #[inline]
    pub fn process_sample(&mut self, sample: f32) -> f32 {
        assert_eq!(
            self.interpolate, 1,
            "single-output filtering requires interpolation factor one"
        );
        self.push_sample(sample);
        self.accumulate_phase_1()
    }

    /// Filters one sample and returns `interpolate` output samples in phase order.
    pub fn process_sample_interpolated(&mut self, sample: f32) -> Vec<f32> {
        let mut out = Vec::with_capacity(self.interpolate);
        self.process_sample_interpolated_into(sample, &mut out);
        out
    }

    /// Filters one sample and appends `interpolate` outputs to `out`. Existing entries are kept.
    #[inline]
    pub fn process_sample_interpolated_into(&mut self, sample: f32, out: &mut Vec<f32>) {
        for phase in 0..self.interpolate {
            if phase == 0 {
                self.push_sample(sample);
            }
            out.push(self.accumulate_phase(phase));
        }
    }

    /// Filters a block, returning `interpolate` outputs for each input sample.
    /// The filter keeps its history for the next call.
    pub fn process_block(&mut self, input: &[f32]) -> Vec<f32> {
        let mut out = Vec::with_capacity(input.len() * self.interpolate);
        self.process_block_into(input, &mut out);
        out
    }

    /// Filters a block into `out`, with `interpolate` outputs for each input sample.
    /// Reuses the output allocation and keeps filter history for the next call.
    pub fn process_block_into(&mut self, input: &[f32], out: &mut Vec<f32>) {
        out.clear();
        out.reserve(input.len() * self.interpolate);
        if self.interpolate == 1 {
            for &sample in input {
                self.push_sample(sample);
                out.push(self.accumulate_phase_1());
            }
            return;
        }

        for &sample in input {
            self.process_sample_interpolated_into(sample, out);
        }
    }

    #[inline]
    fn push_sample(&mut self, sample: f32) {
        self.head = if self.head == 0 {
            self.delay_len - 1
        } else {
            self.head - 1
        };
        self.delay[self.head] = sample;
        self.delay[self.head + self.delay_len] = sample;
    }

    #[inline]
    fn accumulate_phase_1(&self) -> f32 {
        debug_assert_eq!(self.interpolate, 1);
        debug_assert!(self.head + self.delay_len <= self.delay.len());
        debug_assert_eq!(self.delay_len, self.taps.len());

        let mut acc = 0.0f32;
        // SAFETY: Construction pads the tap phases and doubles the delay ring, keeping each pointer increment inside its allocation.
        let mut delay = unsafe { self.delay.as_ptr().add(self.head) };
        let mut tap = self.taps.as_ptr();
        for _ in 0..self.delay_len {
            // SAFETY: Construction pads the tap phases and doubles the delay ring, keeping each pointer increment inside its allocation.
            unsafe {
                acc += *delay * *tap;
                delay = delay.add(1);
                tap = tap.add(1);
            }
        }
        acc
    }

    #[inline]
    fn accumulate_phase(&self, phase: usize) -> f32 {
        debug_assert!(phase < self.interpolate);
        debug_assert!(self.head + self.delay_len <= self.delay.len());
        debug_assert_eq!(self.delay_len * self.interpolate, self.taps.len());

        let mut acc = 0.0f32;
        let mut tap_idx = phase;
        // SAFETY: Construction pads the tap phases and doubles the delay ring, keeping each pointer increment inside its allocation.
        let mut delay = unsafe { self.delay.as_ptr().add(self.head) };
        while tap_idx < self.taps.len() {
            // SAFETY: Construction pads the tap phases and doubles the delay ring, keeping each pointer increment inside its allocation.
            unsafe {
                acc += *delay * *self.taps.as_ptr().add(tap_idx);
                delay = delay.add(1);
            }
            tap_idx += self.interpolate;
        }
        acc * self.interpolate as f32
    }
}

#[derive(Clone, Copy, Default)]
struct ComplexDelay {
    re: f32,
    im: f32,
}

/// Complex-input FIR with real taps and optional integer interpolation.
pub struct ComplexFir32 {
    taps: Vec<f32>,
    delay: Vec<ComplexDelay>,
    head: usize,
    delay_len: usize,
    interpolate: usize,
}

impl ComplexFir32 {
    /// Creates a rate-preserving filter with zero history.
    ///
    /// # Panics
    /// Panics if taps are empty.
    pub fn new(taps: &[f64]) -> Self {
        Self::with_interpolate(taps, 1)
    }

    /// Creates an interpolating filter with zeroed history. The tap array is padded
    /// with zeros to a multiple of `interpolate`.
    ///
    /// # Panics
    /// Panics if taps are empty or `interpolate` is zero.
    pub fn with_interpolate(taps: &[f64], interpolate: usize) -> Self {
        assert!(!taps.is_empty());
        assert!(interpolate > 0);

        let mut taps = taps.iter().map(|&tap| tap as f32).collect::<Vec<_>>();
        let rem = taps.len() % interpolate;
        if rem != 0 {
            taps.resize(taps.len() + (interpolate - rem), 0.0);
        }
        let delay_len = taps.len() / interpolate;
        Self {
            taps,
            delay: vec![ComplexDelay::default(); delay_len * 2],
            head: 1 % delay_len,
            delay_len,
            interpolate,
        }
    }

    /// Filters one sample. The interpolation factor must be one.
    #[inline]
    pub fn process_sample(&mut self, sample: Complex32) -> Complex32 {
        assert_eq!(
            self.interpolate, 1,
            "single-output filtering requires interpolation factor one"
        );
        self.push_sample(sample);
        self.accumulate_phase_1()
    }

    /// Always advances filter history. Returns an output only when `emit` is true.
    /// Requires interpolation factor one.
    #[inline]
    pub fn process_sample_if(&mut self, sample: Complex32, emit: bool) -> Option<Complex32> {
        assert_eq!(
            self.interpolate, 1,
            "single-output filtering requires interpolation factor one"
        );
        self.push_sample(sample);
        emit.then(|| self.accumulate_phase_1())
    }

    /// Filters one sample and returns `interpolate` output samples in phase order.
    pub fn process_sample_interpolated(&mut self, sample: Complex32) -> Vec<Complex32> {
        let mut out = Vec::with_capacity(self.interpolate);
        self.process_sample_interpolated_into(sample, &mut out);
        out
    }

    /// Filters one sample and appends `interpolate` outputs to `out`. Existing entries are kept.
    pub fn process_sample_interpolated_into(
        &mut self,
        sample: Complex32,
        out: &mut Vec<Complex32>,
    ) {
        for phase in 0..self.interpolate {
            if phase == 0 {
                self.push_sample(sample);
            }
            out.push(self.accumulate_phase(phase));
        }
    }

    /// Filters a block, returning `interpolate` outputs for each input sample.
    /// The filter keeps its history for the next call.
    pub fn process_block(&mut self, input: &[Complex32]) -> Vec<Complex32> {
        let mut out = Vec::with_capacity(input.len() * self.interpolate);
        self.process_block_into(input, &mut out);
        out
    }

    /// Filters a block into `out`, with `interpolate` outputs for each input sample.
    /// Reuses the output allocation and keeps filter history for the next call.
    pub fn process_block_into(&mut self, input: &[Complex32], out: &mut Vec<Complex32>) {
        out.clear();
        out.reserve(input.len() * self.interpolate);
        if self.interpolate == 1 {
            for &sample in input {
                self.push_sample(sample);
                out.push(self.accumulate_phase_1());
            }
            return;
        }

        for &sample in input {
            self.process_sample_interpolated_into(sample, out);
        }
    }

    #[inline]
    fn push_sample(&mut self, sample: Complex32) {
        self.head = if self.head == 0 {
            self.delay_len - 1
        } else {
            self.head - 1
        };
        let sample = ComplexDelay {
            re: sample.re,
            im: sample.im,
        };
        self.delay[self.head] = sample;
        self.delay[self.head + self.delay_len] = sample;
    }

    #[inline]
    fn accumulate_phase_1(&self) -> Complex32 {
        debug_assert_eq!(self.interpolate, 1);
        debug_assert!(self.head + self.delay_len <= self.delay.len());
        debug_assert_eq!(self.delay_len, self.taps.len());

        let mut re = 0.0f32;
        let mut im = 0.0f32;
        // SAFETY: Construction pads the tap phases and doubles the delay ring, keeping each pointer increment inside its allocation.
        let mut delay = unsafe { self.delay.as_ptr().add(self.head) };
        let mut tap = self.taps.as_ptr();
        for _ in 0..self.delay_len {
            // SAFETY: Construction pads the tap phases and doubles the delay ring, keeping each pointer increment inside its allocation.
            unsafe {
                let sample = *delay;
                let coeff = *tap;
                re += sample.re * coeff;
                im += sample.im * coeff;
                delay = delay.add(1);
                tap = tap.add(1);
            }
        }
        Complex32::new(re, im)
    }

    #[inline]
    fn accumulate_phase(&self, phase: usize) -> Complex32 {
        debug_assert!(phase < self.interpolate);
        debug_assert!(self.head + self.delay_len <= self.delay.len());
        debug_assert_eq!(self.delay_len * self.interpolate, self.taps.len());

        let mut re = 0.0f32;
        let mut im = 0.0f32;
        let mut tap_idx = phase;
        // SAFETY: Construction pads the tap phases and doubles the delay ring, keeping each pointer increment inside its allocation.
        let mut delay = unsafe { self.delay.as_ptr().add(self.head) };
        while tap_idx < self.taps.len() {
            // SAFETY: Construction pads the tap phases and doubles the delay ring, keeping each pointer increment inside its allocation.
            unsafe {
                let sample = *delay;
                let coeff = *self.taps.as_ptr().add(tap_idx);
                re += sample.re * coeff;
                im += sample.im * coeff;
                delay = delay.add(1);
            }
            tap_idx += self.interpolate;
        }
        let gain = self.interpolate as f32;
        Complex32::new(re * gain, im * gain)
    }
}

/// Block-only complex FIR interpolator with real taps.
pub struct PolyphaseComplexFir32 {
    phase_taps: Vec<f32>,
    delay_re: Vec<f32>,
    delay_im: Vec<f32>,
    head: usize,
    delay_len: usize,
    row_len: usize,
    interpolate: usize,
}

impl PolyphaseComplexFir32 {
    /// Outputs produced per input sample.
    pub fn interpolate(&self) -> usize {
        self.interpolate
    }

    /// Creates an interpolating filter with zeroed history. The tap array is padded
    /// with zeros to a multiple of `interpolate`.
    ///
    /// # Panics
    /// Panics if taps are empty or `interpolate` is zero.
    pub fn with_interpolate(taps: &[f64], interpolate: usize) -> Self {
        assert!(!taps.is_empty());
        assert!(interpolate > 0);

        let mut taps = taps.iter().map(|&tap| tap as f32).collect::<Vec<_>>();
        let rem = taps.len() % interpolate;
        if rem != 0 {
            taps.resize(taps.len() + (interpolate - rem), 0.0);
        }
        let delay_len = taps.len() / interpolate;
        let row_len = delay_len.next_multiple_of(4);
        let mut phase_taps = vec![0.0f32; interpolate * row_len];
        for (idx, &tap) in taps.iter().enumerate() {
            let phase = idx % interpolate;
            let column = idx / interpolate;
            phase_taps[phase * row_len + column] = tap;
        }
        let delay_size = delay_len * 2 + (row_len - delay_len);
        Self {
            phase_taps,
            delay_re: vec![0.0; delay_size],
            delay_im: vec![0.0; delay_size],
            head: 1 % delay_len,
            delay_len,
            row_len,
            interpolate,
        }
    }

    /// Filters a block, returning `interpolate` outputs for each input sample.
    /// The filter keeps its history for the next call.
    pub fn process_block(&mut self, input: &[Complex32]) -> Vec<Complex32> {
        let mut out = Vec::with_capacity(input.len() * self.interpolate);
        self.process_block_into(input, &mut out);
        out
    }

    /// Filters a block into `out`, with `interpolate` outputs for each input sample.
    /// Reuses the output allocation and keeps filter history for the next call.
    pub fn process_block_into(&mut self, input: &[Complex32], out: &mut Vec<Complex32>) {
        out.clear();
        out.reserve(input.len() * self.interpolate);
        let gain = self.interpolate as f32;
        for &sample in input {
            self.push_sample(sample);
            for phase in 0..self.interpolate {
                let row = &self.phase_taps[phase * self.row_len..(phase + 1) * self.row_len];
                let (re, im) = dot_re_im(
                    row,
                    &self.delay_re[self.head..self.head + self.row_len],
                    &self.delay_im[self.head..self.head + self.row_len],
                );
                out.push(Complex32::new(re * gain, im * gain));
            }
        }
    }

    #[inline]
    fn push_sample(&mut self, sample: Complex32) {
        self.head = if self.head == 0 {
            self.delay_len - 1
        } else {
            self.head - 1
        };
        self.delay_re[self.head] = sample.re;
        self.delay_im[self.head] = sample.im;
        self.delay_re[self.head + self.delay_len] = sample.re;
        self.delay_im[self.head + self.delay_len] = sample.im;
    }
}

/// A complex FIR filter for symmetric real taps. The sample rate stays unchanged.
pub struct SymmetricComplexFir32 {
    half_taps: Vec<f32>,
    center_tap: Option<f32>,
    delay_re: Vec<f32>,
    delay_im: Vec<f32>,
    head: usize,
    delay_len: usize,
    block_re: Vec<f32>,
    block_im: Vec<f32>,
}

impl SymmetricComplexFir32 {
    /// Creates a zero-history filter.
    ///
    /// # Panics
    /// Panics if taps are empty or mirrored `f32` tap pairs differ by more than `1e-6`.
    pub fn new(taps: &[f64]) -> Self {
        assert!(!taps.is_empty());

        let taps = taps.iter().map(|&tap| tap as f32).collect::<Vec<_>>();
        for i in 0..taps.len() / 2 {
            let a = taps[i];
            let b = taps[taps.len() - 1 - i];
            assert!(
                (a - b).abs() <= 1e-6,
                "SymmetricComplexFir32 requires mirrored taps"
            );
        }

        let delay_len = taps.len();
        let half_len = delay_len / 2;
        let center_tap = (delay_len % 2 == 1).then(|| taps[half_len]);
        Self {
            half_taps: taps[..half_len].to_vec(),
            center_tap,
            delay_re: vec![0.0; delay_len * 2],
            delay_im: vec![0.0; delay_len * 2],
            head: 1 % delay_len,
            delay_len,
            block_re: Vec::new(),
            block_im: Vec::new(),
        }
    }

    /// Number of taps, which is also the filter's delay-line length.
    pub fn tap_count(&self) -> usize {
        self.delay_len
    }

    /// Filters one sample.
    #[inline]
    pub fn process_sample(&mut self, sample: Complex32) -> Complex32 {
        self.push_sample(sample);
        self.accumulate()
    }

    /// Always advances filter history. Returns an output only when `emit` is true.
    #[inline]
    pub fn process_sample_if(&mut self, sample: Complex32, emit: bool) -> Option<Complex32> {
        self.push_sample(sample);
        emit.then(|| self.accumulate())
    }

    /// Consumes a block and returns the same number of outputs. Retains history for the next call.
    pub fn process_block(&mut self, input: &[Complex32]) -> Vec<Complex32> {
        let mut out = Vec::with_capacity(input.len());
        self.process_block_into(input, &mut out);
        out
    }

    /// Replaces `out` with one output per input and retains history.
    /// Matches [`Self::process_sample`] bit for bit and reuses output capacity.
    pub fn process_block_into(&mut self, input: &[Complex32], out: &mut Vec<Complex32>) {
        out.clear();
        out.reserve(input.len());
        if input.is_empty() {
            return;
        }

        self.stage_block(input);
        let half = self.half_taps.len();
        for oldest in 0..input.len() {
            let mut value = symmetric_complex_accumulate(
                &self.half_taps,
                &self.block_re,
                &self.block_im,
                oldest,
                oldest + self.delay_len - 1,
            );
            if let Some(coeff) = self.center_tap {
                value.re += self.block_re[oldest + half] * coeff;
                value.im += self.block_im[oldest + half] * coeff;
            }
            out.push(value);
        }
        self.reload_ring();
    }

    fn stage_block(&mut self, input: &[Complex32]) {
        let history = self.delay_len - 1;
        self.block_re.clear();
        self.block_im.clear();
        self.block_re.reserve(history + input.len());
        self.block_im.reserve(history + input.len());
        for lag in (0..history).rev() {
            self.block_re.push(self.delay_re[self.head + lag]);
            self.block_im.push(self.delay_im[self.head + lag]);
        }
        for sample in input {
            self.block_re.push(sample.re);
            self.block_im.push(sample.im);
        }
    }

    /// Refill the ring from the staged tail so the per-sample API picks up
    /// where the block ended.
    fn reload_ring(&mut self) {
        let newest = self.block_re.len() - 1;
        self.head = 0;
        for lag in 0..self.delay_len {
            let re = self.block_re[newest - lag];
            let im = self.block_im[newest - lag];
            self.delay_re[lag] = re;
            self.delay_re[lag + self.delay_len] = re;
            self.delay_im[lag] = im;
            self.delay_im[lag + self.delay_len] = im;
        }
    }

    #[inline]
    fn push_sample(&mut self, sample: Complex32) {
        self.head = if self.head == 0 {
            self.delay_len - 1
        } else {
            self.head - 1
        };
        self.delay_re[self.head] = sample.re;
        self.delay_re[self.head + self.delay_len] = sample.re;
        self.delay_im[self.head] = sample.im;
        self.delay_im[self.head + self.delay_len] = sample.im;
    }

    #[inline]
    fn accumulate(&mut self) -> Complex32 {
        let half = self.half_taps.len();
        let head = self.head;
        let last = head + self.delay_len - 1;

        let mut out = symmetric_complex_accumulate(
            &self.half_taps,
            &self.delay_re,
            &self.delay_im,
            head,
            last,
        );
        if let Some(coeff) = self.center_tap {
            out.re += self.delay_re[head + half] * coeff;
            out.im += self.delay_im[head + half] * coeff;
        }

        out
    }
}

/// Decimating complex FIR with symmetric real taps.
///
/// Every input advances the filter history. The first input of a freshly built filter
/// produces an output, and every `decimate`-th input after it does too, so a block
/// whose length is a whole number of decimation periods keeps the output grid aligned
/// to the input grid across calls.
pub struct DecimatingComplexFir32 {
    filter: SymmetricComplexFir32,
    decimate: usize,
    phase: usize,
}

impl DecimatingComplexFir32 {
    /// Creates a zero-history filter.
    ///
    /// # Panics
    /// Panics if `decimate` is zero, or on the tap conditions of
    /// [`SymmetricComplexFir32::new`].
    pub fn new(taps: &[f64], decimate: usize) -> Self {
        assert!(decimate > 0, "decimation factor must be positive");
        Self {
            filter: SymmetricComplexFir32::new(taps),
            decimate,
            phase: 0,
        }
    }

    /// Inputs consumed per output.
    pub fn decimate(&self) -> usize {
        self.decimate
    }

    /// Places the output grid as if `inputs` samples had already been consumed, so the
    /// next output falls where a filter fed from the start would emit it. History is kept.
    pub fn set_inputs_consumed(&mut self, inputs: u64) {
        self.phase = (inputs % self.decimate as u64) as usize;
    }

    /// Group delay in input samples. Subtract this from a timestamp to name the input
    /// sample whose energy centers the first output.
    pub fn group_delay_samples(&self) -> usize {
        self.filter.tap_count() / 2
    }

    /// How many outputs `input_len` further inputs produce from the current phase.
    pub fn outputs_for(&self, input_len: usize) -> usize {
        if input_len == 0 {
            return 0;
        }
        let offset = (self.decimate - self.phase) % self.decimate;
        if offset >= input_len {
            return 0;
        }
        (input_len - offset).div_ceil(self.decimate)
    }

    /// Filters and decimates a block, returning the kept outputs.
    pub fn process_block(&mut self, input: &[Complex32]) -> Vec<Complex32> {
        let mut out = Vec::with_capacity(self.outputs_for(input.len()));
        self.process_block_into(input, &mut out);
        out
    }

    /// Replaces `out` with the kept outputs and retains history for the next call.
    pub fn process_block_into(&mut self, input: &[Complex32], out: &mut Vec<Complex32>) {
        out.clear();
        out.reserve(self.outputs_for(input.len()));
        for &sample in input {
            let emit = self.phase == 0;
            self.phase = (self.phase + 1) % self.decimate;
            if let Some(value) = self.filter.process_sample_if(sample, emit) {
                out.push(value);
            }
        }
    }
}

#[cfg(test)]
mod tests;

#[cfg(test)]
mod contract_tests {
    use super::*;
    #[test]
    #[should_panic(expected = "single-output filtering requires interpolation factor one")]
    fn interpolating_real_filter_rejects_single_output_in_release() {
        Fir32::with_interpolate(&[1.0, 1.0], 2).process_sample(1.0);
    }
    #[test]
    #[should_panic(expected = "single-output filtering requires interpolation factor one")]
    fn interpolating_complex_filter_rejects_single_output_in_release() {
        ComplexFir32::with_interpolate(&[1.0, 1.0], 2).process_sample(Complex32::default());
    }
}
