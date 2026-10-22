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
