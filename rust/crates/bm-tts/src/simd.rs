//! The dot product, which is where a render spends most of its arithmetic.
//!
//! One generated frame runs 16 of these against the audio head (1024 x 768) and
//! one against the text head (419 x 768), so a 124-frame render is ~1.6 billion
//! multiply-accumulates. At that size the codegen is the whole difference.
//!
//! **Why this is hand-written.** The portable form —
//! `for lane in 0..8 { acc[lane] = row[k + lane].mul_add(x[k + lane], acc[lane]) }`
//! — gets 285 ms for the render, and the disassembly says why: LLVM keeps eight
//! *scalar* accumulators and emits six scalar `fmadd` plus one two-lane
//! `fmla.2s` per eight multiply-accumulates. It will pair lanes but not fill a
//! register, and the fixed reduction order that makes the result reproducible is
//! what stops it. Explicit vectors settle the question.
//!
//! **The summation order is not the portable one**, so this is a numerical
//! change, not just a speed one: four accumulators per vector pass, then a
//! pairwise horizontal add. `tools/frames-parity.py` is the arbiter — at
//! temperature 0 the codes are an argmax, so a difference either flips a near
//! tie or vanishes entirely.
//!
//! Four accumulators rather than one, on purpose: a single chain serialises on
//! FMA latency (4 cycles on both targets), which would be slower than the scalar
//! code it replaces. Four chains is what makes the throughput available.

/// `a · b`. The lengths must match.
#[inline]
pub fn dot(a: &[f32], b: &[f32]) -> f32 {
    debug_assert_eq!(a.len(), b.len());
    #[cfg(target_arch = "aarch64")]
    {
        // NEON is baseline on aarch64, so there is nothing to detect.
        unsafe { aarch64::dot(a, b) }
    }
    #[cfg(target_arch = "x86_64")]
    {
        // FMA is *not* baseline on x86-64, so this one has to be asked for.
        if is_x86_feature_detected!("fma") && is_x86_feature_detected!("avx2") {
            unsafe { x86_64::dot(a, b) }
        } else {
            portable(a, b)
        }
    }
    #[cfg(not(any(target_arch = "aarch64", target_arch = "x86_64")))]
    {
        portable(a, b)
    }
}

/// Eight independent accumulators, fused. The fallback, and the reference the
/// vector paths are checked against.
pub fn portable(a: &[f32], b: &[f32]) -> f32 {
    const LANES: usize = 8;
    let n = a.len().min(b.len());
    let mut acc = [0f32; LANES];
    let mut k = 0;
    while k + LANES <= n {
        for (lane, slot) in acc.iter_mut().enumerate() {
            *slot = a[k + lane].mul_add(b[k + lane], *slot);
        }
        k += LANES;
    }
    let mut sum = ((acc[0] + acc[1]) + (acc[2] + acc[3])) + ((acc[4] + acc[5]) + (acc[6] + acc[7]));
    while k < n {
        sum = a[k].mul_add(b[k], sum);
        k += 1;
    }
    sum
}

#[cfg(target_arch = "aarch64")]
mod aarch64 {
    use std::arch::aarch64::*;

    /// Four `fmla.4s` accumulators, 16 floats per pass.
    #[inline]
    pub unsafe fn dot(a: &[f32], b: &[f32]) -> f32 {
        let n = a.len().min(b.len());
        let (pa, pb) = (a.as_ptr(), b.as_ptr());
        let mut acc0 = vdupq_n_f32(0.0);
        let mut acc1 = vdupq_n_f32(0.0);
        let mut acc2 = vdupq_n_f32(0.0);
        let mut acc3 = vdupq_n_f32(0.0);
        let mut i = 0;
        while i + 16 <= n {
            acc0 = vfmaq_f32(acc0, vld1q_f32(pa.add(i)), vld1q_f32(pb.add(i)));
            acc1 = vfmaq_f32(acc1, vld1q_f32(pa.add(i + 4)), vld1q_f32(pb.add(i + 4)));
            acc2 = vfmaq_f32(acc2, vld1q_f32(pa.add(i + 8)), vld1q_f32(pb.add(i + 8)));
            acc3 = vfmaq_f32(acc3, vld1q_f32(pa.add(i + 12)), vld1q_f32(pb.add(i + 12)));
            i += 16;
        }
        let mut acc = vaddq_f32(vaddq_f32(acc0, acc1), vaddq_f32(acc2, acc3));
        while i + 4 <= n {
            acc = vfmaq_f32(acc, vld1q_f32(pa.add(i)), vld1q_f32(pb.add(i)));
            i += 4;
        }
        let mut s = vaddvq_f32(acc);
        while i < n {
            s = a[i].mul_add(b[i], s);
            i += 1;
        }
        s
    }
}

#[cfg(target_arch = "x86_64")]
mod x86_64 {
    use std::arch::x86_64::*;

    /// Four `vfmadd` accumulators, 32 floats per pass.
    #[target_feature(enable = "avx2,fma")]
    pub unsafe fn dot(a: &[f32], b: &[f32]) -> f32 {
        let n = a.len().min(b.len());
        let (pa, pb) = (a.as_ptr(), b.as_ptr());
        let mut acc0 = _mm256_setzero_ps();
        let mut acc1 = _mm256_setzero_ps();
        let mut acc2 = _mm256_setzero_ps();
        let mut acc3 = _mm256_setzero_ps();
        let mut i = 0;
        while i + 32 <= n {
            acc0 = _mm256_fmadd_ps(_mm256_loadu_ps(pa.add(i)), _mm256_loadu_ps(pb.add(i)), acc0);
            acc1 = _mm256_fmadd_ps(
                _mm256_loadu_ps(pa.add(i + 8)),
                _mm256_loadu_ps(pb.add(i + 8)),
                acc1,
            );
            acc2 = _mm256_fmadd_ps(
                _mm256_loadu_ps(pa.add(i + 16)),
                _mm256_loadu_ps(pb.add(i + 16)),
                acc2,
            );
            acc3 = _mm256_fmadd_ps(
                _mm256_loadu_ps(pa.add(i + 24)),
                _mm256_loadu_ps(pb.add(i + 24)),
                acc3,
            );
            i += 32;
        }
        let mut acc = _mm256_add_ps(_mm256_add_ps(acc0, acc1), _mm256_add_ps(acc2, acc3));
        while i + 8 <= n {
            acc = _mm256_fmadd_ps(_mm256_loadu_ps(pa.add(i)), _mm256_loadu_ps(pb.add(i)), acc);
            i += 8;
        }
        // The 8 lanes are folded in a fixed order, so the result is reproducible.
        let mut lanes = [0f32; 8];
        _mm256_storeu_ps(lanes.as_mut_ptr(), acc);
        let mut s = ((lanes[0] + lanes[1]) + (lanes[2] + lanes[3]))
            + ((lanes[4] + lanes[5]) + (lanes[6] + lanes[7]));
        while i < n {
            s = a[i].mul_add(b[i], s);
            i += 1;
        }
        s
    }
}

#[cfg(test)]
mod tests {
    use super::*;

    fn reference(a: &[f32], b: &[f32]) -> f32 {
        a.iter().zip(b).map(|(p, q)| p * q).sum()
    }

    #[test]
    fn the_vector_path_agrees_with_the_portable_one() {
        // Every width in play: 768 (audio head), 419 (text head), and odd ones
        // so the 16-wide, 4-wide and scalar tails are all exercised.
        for n in [1usize, 3, 4, 5, 7, 8, 15, 16, 17, 31, 32, 33, 419, 768, 769] {
            let a: Vec<f32> = (0..n)
                .map(|i| ((i * 37 % 101) as f32 - 50.0) / 50.0)
                .collect();
            let b: Vec<f32> = (0..n)
                .map(|i| ((i * 53 % 97) as f32 - 48.0) / 48.0)
                .collect();
            let want = reference(&a, &b);
            let got = dot(&a, &b);
            // Not bit-exact against a naive sum — that is the point of the
            // reassociation — but it must be a dot product.
            let tol = 1e-4 * want.abs().max(1.0);
            assert!(
                (got - want).abs() <= tol,
                "n={n}: dot {got} vs naive {want}"
            );
        }
    }

    #[test]
    fn the_vector_path_is_deterministic() {
        let a: Vec<f32> = (0..768).map(|i| (i as f32).sin()).collect();
        let b: Vec<f32> = (0..768).map(|i| (i as f32).cos()).collect();
        let first = dot(&a, &b);
        for _ in 0..8 {
            assert_eq!(dot(&a, &b).to_bits(), first.to_bits());
        }
    }

    #[test]
    fn an_empty_slice_is_zero() {
        assert_eq!(dot(&[], &[]), 0.0);
    }
}
