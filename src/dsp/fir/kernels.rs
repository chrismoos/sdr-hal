use num_complex::Complex32;

#[inline]
pub(super) fn symmetric_complex_accumulate(
    taps: &[f32],
    delay_re: &[f32],
    delay_im: &[f32],
    head: usize,
    last: usize,
) -> Complex32 {
    debug_assert_eq!(delay_re.len(), delay_im.len());
    debug_assert!(head + taps.len() <= delay_re.len());
    debug_assert!(last < delay_re.len() && last + 1 >= taps.len());
    #[cfg(target_arch = "aarch64")]
    {
        // SAFETY: The target architecture guarantees these SIMD instructions and callers provide the checked buffer ranges.
        return unsafe { symmetric_complex_accumulate_neon(taps, delay_re, delay_im, head, last) };
    }
    #[cfg(target_arch = "x86_64")]
    {
        // SAFETY: The target architecture guarantees these SIMD instructions and callers provide the checked buffer ranges.
        return unsafe { symmetric_complex_accumulate_sse(taps, delay_re, delay_im, head, last) };
    }
    #[allow(unreachable_code)]
    symmetric_complex_accumulate_scalar(taps, delay_re, delay_im, head, last)
}

#[inline]
fn symmetric_complex_accumulate_scalar(
    taps: &[f32],
    delay_re: &[f32],
    delay_im: &[f32],
    head: usize,
    last: usize,
) -> Complex32 {
    let mut re = 0.0f32;
    let mut im = 0.0f32;
    for i in 0..taps.len() {
        // SAFETY: The caller provides both tap windows in bounds. Vector chunks contain four elements and the tail uses scalar indexes.
        unsafe {
            let coeff = *taps.get_unchecked(i);
            re += (*delay_re.get_unchecked(head + i) + *delay_re.get_unchecked(last - i)) * coeff;
            im += (*delay_im.get_unchecked(head + i) + *delay_im.get_unchecked(last - i)) * coeff;
        }
    }
    Complex32::new(re, im)
}

#[cfg(target_arch = "aarch64")]
#[target_feature(enable = "neon")]
#[inline]
unsafe fn symmetric_complex_accumulate_neon(
    taps: &[f32],
    delay_re: &[f32],
    delay_im: &[f32],
    head: usize,
    last: usize,
) -> Complex32 {
    use std::arch::aarch64::{vaddq_f32, vaddvq_f32, vdupq_n_f32, vfmaq_f32, vld1q_f32};

    let chunks = taps.len() / 4;
    let mut acc_re = vdupq_n_f32(0.0);
    let mut acc_im = vdupq_n_f32(0.0);
    for chunk in 0..chunks {
        let i = chunk * 4;
        // SAFETY: The caller provides both tap windows in bounds. Vector chunks contain four elements and the tail uses scalar indexes.
        let coeff = unsafe { vld1q_f32(taps.as_ptr().add(i)) };
        // SAFETY: The caller provides both tap windows in bounds. Vector chunks contain four elements and the tail uses scalar indexes.
        let front_re = unsafe { vld1q_f32(delay_re.as_ptr().add(head + i)) };
        // SAFETY: The caller provides both tap windows in bounds. Vector chunks contain four elements and the tail uses scalar indexes.
        let front_im = unsafe { vld1q_f32(delay_im.as_ptr().add(head + i)) };
        // SAFETY: The caller provides both tap windows in bounds. Vector chunks contain four elements and the tail uses scalar indexes.
        let back_re = unsafe { reverse_f32x4_neon(vld1q_f32(delay_re.as_ptr().add(last - i - 3))) };
        // SAFETY: The caller provides both tap windows in bounds. Vector chunks contain four elements and the tail uses scalar indexes.
        let back_im = unsafe { reverse_f32x4_neon(vld1q_f32(delay_im.as_ptr().add(last - i - 3))) };
        acc_re = vfmaq_f32(acc_re, coeff, vaddq_f32(front_re, back_re));
        acc_im = vfmaq_f32(acc_im, coeff, vaddq_f32(front_im, back_im));
    }
    let mut re = vaddvq_f32(acc_re);
    let mut im = vaddvq_f32(acc_im);
    for i in (chunks * 4)..taps.len() {
        // SAFETY: The caller provides both tap windows in bounds. Vector chunks contain four elements and the tail uses scalar indexes.
        unsafe {
            let coeff = *taps.get_unchecked(i);
            re += (*delay_re.get_unchecked(head + i) + *delay_re.get_unchecked(last - i)) * coeff;
            im += (*delay_im.get_unchecked(head + i) + *delay_im.get_unchecked(last - i)) * coeff;
        }
    }
    Complex32::new(re, im)
}

#[cfg(target_arch = "aarch64")]
#[target_feature(enable = "neon")]
#[inline]
unsafe fn reverse_f32x4_neon(
    v: std::arch::aarch64::float32x4_t,
) -> std::arch::aarch64::float32x4_t {
    use std::arch::aarch64::{vcombine_f32, vget_high_f32, vget_low_f32, vrev64q_f32};

    let swapped_pairs = vrev64q_f32(v);
    vcombine_f32(vget_high_f32(swapped_pairs), vget_low_f32(swapped_pairs))
}

#[cfg(target_arch = "x86_64")]
#[target_feature(enable = "sse")]
#[inline]
unsafe fn symmetric_complex_accumulate_sse(
    taps: &[f32],
    delay_re: &[f32],
    delay_im: &[f32],
    head: usize,
    last: usize,
) -> Complex32 {
    use std::arch::x86_64::{_mm_add_ps, _mm_loadu_ps, _mm_mul_ps, _mm_setzero_ps, _mm_shuffle_ps};

    let chunks = taps.len() / 4;
    let mut acc_re = _mm_setzero_ps();
    let mut acc_im = _mm_setzero_ps();
    for chunk in 0..chunks {
        let i = chunk * 4;
        // SAFETY: The caller provides both tap windows in bounds. Vector chunks contain four elements and the tail uses scalar indexes.
        let coeff = unsafe { _mm_loadu_ps(taps.as_ptr().add(i)) };
        // SAFETY: The caller provides both tap windows in bounds. Vector chunks contain four elements and the tail uses scalar indexes.
        let front_re = unsafe { _mm_loadu_ps(delay_re.as_ptr().add(head + i)) };
        // SAFETY: The caller provides both tap windows in bounds. Vector chunks contain four elements and the tail uses scalar indexes.
        let front_im = unsafe { _mm_loadu_ps(delay_im.as_ptr().add(head + i)) };
        // SAFETY: The caller provides both tap windows in bounds. Vector chunks contain four elements and the tail uses scalar indexes.
        let back_re = unsafe { _mm_loadu_ps(delay_re.as_ptr().add(last - i - 3)) };
        // SAFETY: The caller provides both tap windows in bounds. Vector chunks contain four elements and the tail uses scalar indexes.
        let back_im = unsafe { _mm_loadu_ps(delay_im.as_ptr().add(last - i - 3)) };
        let back_re = _mm_shuffle_ps::<0x1b>(back_re, back_re);
        let back_im = _mm_shuffle_ps::<0x1b>(back_im, back_im);
        acc_re = _mm_add_ps(acc_re, _mm_mul_ps(coeff, _mm_add_ps(front_re, back_re)));
        acc_im = _mm_add_ps(acc_im, _mm_mul_ps(coeff, _mm_add_ps(front_im, back_im)));
    }

    // SAFETY: The caller provides both tap windows in bounds. Vector chunks contain four elements and the tail uses scalar indexes.
    let mut re = unsafe { hsum_f32x4_sse(acc_re) };
    // SAFETY: The caller provides both tap windows in bounds. Vector chunks contain four elements and the tail uses scalar indexes.
    let mut im = unsafe { hsum_f32x4_sse(acc_im) };

    for i in (chunks * 4)..taps.len() {
        // SAFETY: The caller provides both tap windows in bounds. Vector chunks contain four elements and the tail uses scalar indexes.
        unsafe {
            let coeff = *taps.get_unchecked(i);
            re += (*delay_re.get_unchecked(head + i) + *delay_re.get_unchecked(last - i)) * coeff;
            im += (*delay_im.get_unchecked(head + i) + *delay_im.get_unchecked(last - i)) * coeff;
        }
    }
    Complex32::new(re, im)
}

#[cfg(target_arch = "x86_64")]
#[target_feature(enable = "sse")]
#[inline]
unsafe fn hsum_f32x4_sse(v: std::arch::x86_64::__m128) -> f32 {
    use std::arch::x86_64::{_mm_add_ps, _mm_add_ss, _mm_cvtss_f32, _mm_movehl_ps, _mm_shuffle_ps};

    let high = _mm_movehl_ps(v, v);
    let pair_sums = _mm_add_ps(v, high);
    let lane_1 = _mm_shuffle_ps::<0x55>(pair_sums, pair_sums);
    _mm_cvtss_f32(_mm_add_ss(pair_sums, lane_1))
}

#[inline]
pub(crate) fn dot_re_im(taps: &[f32], re: &[f32], im: &[f32]) -> (f32, f32) {
    debug_assert!(re.len() >= taps.len() && im.len() >= taps.len());
    #[cfg(target_arch = "aarch64")]
    {
        // SAFETY: The target architecture guarantees these SIMD instructions and callers provide the checked buffer ranges.
        return unsafe { dot_re_im_neon(taps, re, im) };
    }
    #[cfg(target_arch = "x86_64")]
    {
        // SAFETY: The target architecture guarantees these SIMD instructions and callers provide the checked buffer ranges.
        return unsafe { dot_re_im_sse(taps, re, im) };
    }
    #[allow(unreachable_code)]
    dot_re_im_scalar(taps, re, im)
}

#[inline]
fn dot_re_im_scalar(taps: &[f32], re: &[f32], im: &[f32]) -> (f32, f32) {
    let mut sum_re = 0.0f32;
    let mut sum_im = 0.0f32;
    for i in 0..taps.len() {
        // SAFETY: Both input slices cover all taps. Vector chunks contain four elements and the tail stays below the tap count.
        unsafe {
            let coeff = *taps.get_unchecked(i);
            sum_re += *re.get_unchecked(i) * coeff;
            sum_im += *im.get_unchecked(i) * coeff;
        }
    }
    (sum_re, sum_im)
}

#[cfg(target_arch = "aarch64")]
#[target_feature(enable = "neon")]
#[inline]
unsafe fn dot_re_im_neon(taps: &[f32], re: &[f32], im: &[f32]) -> (f32, f32) {
    use std::arch::aarch64::{vaddvq_f32, vdupq_n_f32, vfmaq_f32, vld1q_f32};

    let chunks = taps.len() / 4;
    let mut acc_re = vdupq_n_f32(0.0);
    let mut acc_im = vdupq_n_f32(0.0);
    for chunk in 0..chunks {
        let i = chunk * 4;
        // SAFETY: Both input slices cover all taps. Vector chunks contain four elements and the tail stays below the tap count.
        let coeff = unsafe { vld1q_f32(taps.as_ptr().add(i)) };
        // SAFETY: Both input slices cover all taps. Vector chunks contain four elements and the tail stays below the tap count.
        let lane_re = unsafe { vld1q_f32(re.as_ptr().add(i)) };
        // SAFETY: Both input slices cover all taps. Vector chunks contain four elements and the tail stays below the tap count.
        let lane_im = unsafe { vld1q_f32(im.as_ptr().add(i)) };
        acc_re = vfmaq_f32(acc_re, coeff, lane_re);
        acc_im = vfmaq_f32(acc_im, coeff, lane_im);
    }
    let mut sum_re = vaddvq_f32(acc_re);
    let mut sum_im = vaddvq_f32(acc_im);
    for i in (chunks * 4)..taps.len() {
        // SAFETY: Both input slices cover all taps. Vector chunks contain four elements and the tail stays below the tap count.
        unsafe {
            let coeff = *taps.get_unchecked(i);
            sum_re += *re.get_unchecked(i) * coeff;
            sum_im += *im.get_unchecked(i) * coeff;
        }
    }
    (sum_re, sum_im)
}

#[cfg(target_arch = "x86_64")]
#[target_feature(enable = "sse")]
#[inline]
unsafe fn dot_re_im_sse(taps: &[f32], re: &[f32], im: &[f32]) -> (f32, f32) {
    use std::arch::x86_64::{_mm_add_ps, _mm_loadu_ps, _mm_mul_ps, _mm_setzero_ps};

    let chunks = taps.len() / 4;
    let mut acc_re = _mm_setzero_ps();
    let mut acc_im = _mm_setzero_ps();
    for chunk in 0..chunks {
        let i = chunk * 4;
        // SAFETY: Both input slices cover all taps. Vector chunks contain four elements and the tail stays below the tap count.
        let coeff = unsafe { _mm_loadu_ps(taps.as_ptr().add(i)) };
        // SAFETY: Both input slices cover all taps. Vector chunks contain four elements and the tail stays below the tap count.
        let lane_re = unsafe { _mm_loadu_ps(re.as_ptr().add(i)) };
        // SAFETY: Both input slices cover all taps. Vector chunks contain four elements and the tail stays below the tap count.
        let lane_im = unsafe { _mm_loadu_ps(im.as_ptr().add(i)) };
        acc_re = _mm_add_ps(acc_re, _mm_mul_ps(coeff, lane_re));
        acc_im = _mm_add_ps(acc_im, _mm_mul_ps(coeff, lane_im));
    }
    // SAFETY: Both input slices cover all taps. Vector chunks contain four elements and the tail stays below the tap count.
    let mut sum_re = unsafe { hsum_f32x4_sse(acc_re) };
    // SAFETY: Both input slices cover all taps. Vector chunks contain four elements and the tail stays below the tap count.
    let mut sum_im = unsafe { hsum_f32x4_sse(acc_im) };
    for i in (chunks * 4)..taps.len() {
        // SAFETY: Both input slices cover all taps. Vector chunks contain four elements and the tail stays below the tap count.
        unsafe {
            let coeff = *taps.get_unchecked(i);
            sum_re += *re.get_unchecked(i) * coeff;
            sum_im += *im.get_unchecked(i) * coeff;
        }
    }
    (sum_re, sum_im)
}

#[cfg(target_arch = "x86_64")]
pub(super) const SSE_LANES: usize = 4;

#[cfg(target_arch = "x86_64")]
#[target_feature(enable = "avx2")]
pub(super) unsafe fn symmetric_complex_block_x86(
    taps: &[f32],
    re: &[f32],
    im: &[f32],
    samples: usize,
    out: &mut Vec<Complex32>,
) -> usize {
    const AVX_LANES: usize = 8;
    let processed = samples / AVX_LANES * AVX_LANES;
    let history = taps.len() * 2 - 1;
    for start in (0..processed).step_by(AVX_LANES) {
        let end = start + AVX_LANES + history;
        let re = &re[start..end];
        let im = &im[start..end];
        // SAFETY: The checked slices include eight output windows.
        let (re, im) = unsafe { (symmetric_avx2(taps, re), symmetric_avx2(taps, im)) };
        out.extend(
            re.into_iter()
                .zip(im)
                .map(|(re, im)| Complex32::new(re, im)),
        );
    }
    processed
}

#[cfg(target_arch = "x86_64")]
#[target_feature(enable = "avx2")]
#[inline]
unsafe fn symmetric_avx2(taps: &[f32], samples: &[f32]) -> [f32; 8] {
    use std::arch::x86_64::{
        _mm256_add_ps, _mm256_loadu_ps, _mm256_mul_ps, _mm256_set1_ps, _mm256_setzero_ps,
        _mm256_storeu_ps,
    };

    let mut sums = [_mm256_setzero_ps(); SSE_LANES];
    let last = taps.len() * 2 - 1;
    for (chunk, coefficients) in taps.chunks_exact(SSE_LANES).enumerate() {
        for (lane, &coefficient) in coefficients.iter().enumerate() {
            let tap = chunk * SSE_LANES + lane;
            // SAFETY: Each load spans eight outputs inside the checked input window.
            let (front, back) = unsafe {
                (
                    _mm256_loadu_ps(samples.as_ptr().add(tap)),
                    _mm256_loadu_ps(samples.as_ptr().add(last - tap)),
                )
            };
            let pair = _mm256_add_ps(front, back);
            sums[lane] =
                _mm256_add_ps(sums[lane], _mm256_mul_ps(_mm256_set1_ps(coefficient), pair));
        }
    }
    // The SSE reduction order keeps block and single-sample results bit-identical.
    let sum = _mm256_add_ps(
        _mm256_add_ps(sums[0], sums[2]),
        _mm256_add_ps(sums[1], sums[3]),
    );
    let mut result = [0.0; 8];
    // SAFETY: The destination holds all eight lanes.
    unsafe { _mm256_storeu_ps(result.as_mut_ptr(), sum) };
    result
}

#[cfg(target_arch = "x86_64")]
#[target_feature(enable = "avx")]
#[inline]
pub(super) fn polyphase_avx_4x(
    taps: &[[f32; 8]; 12],
    re: &[f32; 12],
    im: &[f32; 12],
) -> [Complex32; 4] {
    use std::arch::x86_64::{
        _mm_set1_ps, _mm256_add_ps, _mm256_castps128_ps256, _mm256_insertf128_ps, _mm256_loadu_ps,
        _mm256_mul_ps, _mm256_setzero_ps, _mm256_storeu_ps,
    };

    let mut sums = [_mm256_setzero_ps(); SSE_LANES];
    for (chunk, coefficients) in taps.chunks_exact(SSE_LANES).enumerate() {
        for (lane, coeff) in coefficients.iter().enumerate() {
            let tap = chunk * SSE_LANES + lane;
            let iq = _mm256_insertf128_ps::<1>(
                _mm256_castps128_ps256(_mm_set1_ps(re[tap])),
                _mm_set1_ps(im[tap]),
            );
            // SAFETY: Each coefficient row holds all eight lanes.
            let coeff = unsafe { _mm256_loadu_ps(coeff.as_ptr()) };
            sums[lane] = _mm256_add_ps(sums[lane], _mm256_mul_ps(iq, coeff));
        }
    }
    // Match the SSE reduction order for each phase.
    let sum = _mm256_add_ps(
        _mm256_add_ps(sums[0], sums[2]),
        _mm256_add_ps(sums[1], sums[3]),
    );
    let scaled = _mm256_mul_ps(sum, std::arch::x86_64::_mm256_set1_ps(4.0));
    let mut iq = [0.0; 8];
    // SAFETY: The destination holds all eight lanes.
    unsafe { _mm256_storeu_ps(iq.as_mut_ptr(), scaled) };
    std::array::from_fn(|phase| Complex32::new(iq[phase], iq[phase + SSE_LANES]))
}

/// Dot product of real `taps` with the first `taps.len()` interleaved complex
/// `samples`. The tap count must be a multiple of eight.
#[cfg(test)]
pub(crate) type InterleavedDot = fn(taps: &[f32], samples: &[Complex32]) -> Complex32;

/// The instruction set [`interleaved_dot`] selects on this CPU.
pub(crate) fn interleaved_dot_name() -> &'static str {
    #[cfg(target_arch = "aarch64")]
    {
        return "NEON";
    }
    #[cfg(target_arch = "x86_64")]
    {
        if std::arch::is_x86_feature_detected!("avx2") && std::arch::is_x86_feature_detected!("fma")
        {
            return "AVX2 with FMA";
        }
        return "SSE2";
    }
    #[allow(unreachable_code)]
    "scalar"
}

/// The fastest [`InterleavedDot`] this CPU runs.
#[cfg(test)]
pub(crate) fn interleaved_dot() -> InterleavedDot {
    #[cfg(target_arch = "aarch64")]
    {
        return |taps, samples| {
            assert!(taps.len().is_multiple_of(8) && samples.len() >= taps.len());
            // SAFETY: NEON is part of the aarch64 baseline, and the assert covers every read.
            unsafe { interleaved_dot_neon(taps, samples) }
        };
    }
    #[cfg(target_arch = "x86_64")]
    {
        if std::arch::is_x86_feature_detected!("avx2") && std::arch::is_x86_feature_detected!("fma")
        {
            return |taps, samples| {
                assert!(taps.len().is_multiple_of(8) && samples.len() >= taps.len());
                // SAFETY: The caller of `interleaved_dot` detected AVX2 and FMA, and the assert covers every read.
                unsafe { interleaved_dot_avx2(taps, samples) }
            };
        }
        return |taps, samples| {
            assert!(taps.len().is_multiple_of(8) && samples.len() >= taps.len());
            // SAFETY: SSE2 is part of the x86_64 baseline, and the assert covers every read.
            unsafe { interleaved_dot_sse2(taps, samples) }
        };
    }
    #[allow(unreachable_code)]
    interleaved_dot_scalar
}

pub(crate) fn interleaved_dot_scalar(taps: &[f32], samples: &[Complex32]) -> Complex32 {
    let mut re = 0f32;
    let mut im = 0f32;
    for (sample, weight) in samples[..taps.len()].iter().zip(taps) {
        re += sample.re * weight;
        im += sample.im * weight;
    }
    Complex32::new(re, im)
}

#[cfg(target_arch = "aarch64")]
#[target_feature(enable = "neon")]
unsafe fn interleaved_dot_neon(taps: &[f32], samples: &[Complex32]) -> Complex32 {
    use std::arch::aarch64::{vaddq_f32, vaddvq_f32, vdupq_n_f32, vfmaq_f32, vld1q_f32, vld2q_f32};

    let data = samples.as_ptr().cast::<f32>();
    let (mut re_a, mut im_a) = (vdupq_n_f32(0.0), vdupq_n_f32(0.0));
    let (mut re_b, mut im_b) = (vdupq_n_f32(0.0), vdupq_n_f32(0.0));
    for tap in (0..taps.len()).step_by(8) {
        // SAFETY: `tap + 8 <= taps.len() <= samples.len()`, and a `Complex32` is two `f32`.
        unsafe {
            let first = vld2q_f32(data.add(2 * tap));
            let second = vld2q_f32(data.add(2 * tap + 8));
            let weights_a = vld1q_f32(taps.as_ptr().add(tap));
            let weights_b = vld1q_f32(taps.as_ptr().add(tap + 4));
            re_a = vfmaq_f32(re_a, first.0, weights_a);
            im_a = vfmaq_f32(im_a, first.1, weights_a);
            re_b = vfmaq_f32(re_b, second.0, weights_b);
            im_b = vfmaq_f32(im_b, second.1, weights_b);
        }
    }
    Complex32::new(
        vaddvq_f32(vaddq_f32(re_a, re_b)),
        vaddvq_f32(vaddq_f32(im_a, im_b)),
    )
}

#[cfg(target_arch = "x86_64")]
#[target_feature(enable = "sse2")]
unsafe fn interleaved_dot_sse2(taps: &[f32], samples: &[Complex32]) -> Complex32 {
    use std::arch::x86_64::{_mm_add_ps, _mm_loadu_ps, _mm_mul_ps, _mm_setzero_ps, _mm_shuffle_ps};

    /// Lanes 0 and 2 of each operand, which are the real parts of two samples.
    const REAL_LANES: i32 = 0b10_00_10_00;
    /// Lanes 1 and 3 of each operand, the imaginary parts.
    const IMAGINARY_LANES: i32 = 0b11_01_11_01;
    let data = samples.as_ptr().cast::<f32>();
    let (mut re, mut im) = (_mm_setzero_ps(), _mm_setzero_ps());
    for tap in (0..taps.len()).step_by(4) {
        // SAFETY: `tap + 4 <= taps.len() <= samples.len()`, and a `Complex32` is two `f32`.
        unsafe {
            let low = _mm_loadu_ps(data.add(2 * tap));
            let high = _mm_loadu_ps(data.add(2 * tap + 4));
            let weights = _mm_loadu_ps(taps.as_ptr().add(tap));
            re = _mm_add_ps(
                re,
                _mm_mul_ps(_mm_shuffle_ps::<REAL_LANES>(low, high), weights),
            );
            im = _mm_add_ps(
                im,
                _mm_mul_ps(_mm_shuffle_ps::<IMAGINARY_LANES>(low, high), weights),
            );
        }
    }
    // SAFETY: SSE is part of what this function enables.
    unsafe { Complex32::new(hsum_f32x4_sse(re), hsum_f32x4_sse(im)) }
}

#[cfg(target_arch = "x86_64")]
#[target_feature(enable = "avx2,fma")]
unsafe fn interleaved_dot_avx2(taps: &[f32], samples: &[Complex32]) -> Complex32 {
    use std::arch::x86_64::{
        _mm_add_ps, _mm256_castps256_ps128, _mm256_extractf128_ps, _mm256_fmadd_ps,
        _mm256_loadu_ps, _mm256_permutevar8x32_ps, _mm256_setr_epi32, _mm256_setzero_ps,
        _mm256_shuffle_ps,
    };

    const REAL_LANES: i32 = 0b10_00_10_00;
    const IMAGINARY_LANES: i32 = 0b11_01_11_01;
    // A 256-bit shuffle works inside each 128-bit half, so eight samples come out in
    // the order 0 1 4 5 2 3 6 7. The taps are put in that order to match.
    let tap_order = _mm256_setr_epi32(0, 1, 4, 5, 2, 3, 6, 7);
    let data = samples.as_ptr().cast::<f32>();
    let (mut re, mut im) = (_mm256_setzero_ps(), _mm256_setzero_ps());
    for tap in (0..taps.len()).step_by(8) {
        // SAFETY: `tap + 8 <= taps.len() <= samples.len()`, and a `Complex32` is two `f32`.
        unsafe {
            let low = _mm256_loadu_ps(data.add(2 * tap));
            let high = _mm256_loadu_ps(data.add(2 * tap + 8));
            let weights =
                _mm256_permutevar8x32_ps(_mm256_loadu_ps(taps.as_ptr().add(tap)), tap_order);
            re = _mm256_fmadd_ps(_mm256_shuffle_ps::<REAL_LANES>(low, high), weights, re);
            im = _mm256_fmadd_ps(_mm256_shuffle_ps::<IMAGINARY_LANES>(low, high), weights, im);
        }
    }
    // SAFETY: SSE is part of what this function enables.
    unsafe {
        Complex32::new(
            hsum_f32x4_sse(_mm_add_ps(
                _mm256_castps256_ps128(re),
                _mm256_extractf128_ps::<1>(re),
            )),
            hsum_f32x4_sse(_mm_add_ps(
                _mm256_castps256_ps128(im),
                _mm256_extractf128_ps::<1>(im),
            )),
        )
    }
}

/// Reads `samples` at `out.len()` positions `start`, `start + step`, and so on, each
/// with the `rows` entry nearest its fractional part. Position `p` reads the `N`
/// samples from `floor(p)`. The last row is the fraction one.
pub(crate) type FractionalRun<const N: usize> =
    fn(rows: &[[f32; N]], samples: &[Complex32], start: f64, step: f64, out: &mut [Complex32]);

/// The fastest [`FractionalRun`] this CPU runs. `N` must be a multiple of eight.
pub(crate) fn fractional_run<const N: usize>() -> FractionalRun<N> {
    assert!(N.is_multiple_of(8));
    #[cfg(target_arch = "aarch64")]
    {
        return |rows, samples, start, step, out| {
            // SAFETY: NEON is part of the aarch64 baseline.
            unsafe { fractional_run_neon(rows, samples, start, step, out) }
        };
    }
    #[cfg(target_arch = "x86_64")]
    {
        if std::arch::is_x86_feature_detected!("avx2") && std::arch::is_x86_feature_detected!("fma")
        {
            return |rows, samples, start, step, out| {
                // SAFETY: The caller of `fractional_run` detected AVX2 and FMA.
                unsafe { fractional_run_avx2(rows, samples, start, step, out) }
            };
        }
        return |rows, samples, start, step, out| {
            // SAFETY: SSE2 is part of the x86_64 baseline.
            unsafe { fractional_run_sse2(rows, samples, start, step, out) }
        };
    }
    #[allow(unreachable_code)]
    {
        |rows, samples, start, step, out| {
            fractional_run_with(rows, samples, start, step, out, |row, window| {
                interleaved_dot_scalar(row, window)
            })
        }
    }
}

/// The loop every [`FractionalRun`] shares. It is inlined into a caller that enables
/// an instruction set, so `dot` is compiled for that set without a call per sample.
#[inline(always)]
fn fractional_run_with<const N: usize>(
    rows: &[[f32; N]],
    samples: &[Complex32],
    start: f64,
    step: f64,
    out: &mut [Complex32],
    dot: impl Fn(&[f32], &[Complex32]) -> Complex32,
) {
    assert!(start >= 0.0 && step >= 0.0);
    let phases = (rows.len() - 1) as f64;
    for (index, slot) in out.iter_mut().enumerate() {
        let position = start + index as f64 * step;
        // A cast truncates, which is the floor of a position that is not negative.
        let whole = position as usize;
        let row = &rows[((position - whole as f64) * phases + 0.5) as usize];
        *slot = dot(row, &samples[whole..whole + N]);
    }
}

#[cfg(target_arch = "aarch64")]
#[target_feature(enable = "neon")]
unsafe fn fractional_run_neon<const N: usize>(
    rows: &[[f32; N]],
    samples: &[Complex32],
    start: f64,
    step: f64,
    out: &mut [Complex32],
) {
    fractional_run_with(rows, samples, start, step, out, |row, window| {
        // SAFETY: The loop passes exactly `N` samples, and `N` is a multiple of eight.
        unsafe { interleaved_dot_neon(row, window) }
    });
}

#[cfg(target_arch = "x86_64")]
#[target_feature(enable = "sse2")]
unsafe fn fractional_run_sse2<const N: usize>(
    rows: &[[f32; N]],
    samples: &[Complex32],
    start: f64,
    step: f64,
    out: &mut [Complex32],
) {
    fractional_run_with(rows, samples, start, step, out, |row, window| {
        // SAFETY: The loop passes exactly `N` samples, and `N` is a multiple of eight.
        unsafe { interleaved_dot_sse2(row, window) }
    });
}

#[cfg(target_arch = "x86_64")]
#[target_feature(enable = "avx2,fma")]
unsafe fn fractional_run_avx2<const N: usize>(
    rows: &[[f32; N]],
    samples: &[Complex32],
    start: f64,
    step: f64,
    out: &mut [Complex32],
) {
    fractional_run_with(rows, samples, start, step, out, |row, window| {
        // SAFETY: The loop passes exactly `N` samples, and `N` is a multiple of eight.
        unsafe { interleaved_dot_avx2(row, window) }
    });
}
