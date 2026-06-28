//! Brick **Q2 (core)** — a radix-2 Cooley–Tukey FFT, the psychoacoustic model's
//! analysis transform. Pure in-house, no dependencies. Scalar Rust; this is the
//! single biggest cycle sink in a perceptual encoder and is flagged as a SIMD
//! hotspot (`lab::bricks::accel` → SIMD), but the scalar path is the reference the
//! SIMD twin will be checked against.

use std::f32::consts::PI;

/// In-place complex FFT. `re`/`im` are the real/imaginary parts; `re.len()` must
/// be a power of two and equal to `im.len()`. Forward transform (`exp(-i2πkn/N)`).
pub fn fft(re: &mut [f32], im: &mut [f32]) {
    let n = re.len();
    assert!(n.is_power_of_two(), "FFT length must be a power of two");
    assert_eq!(n, im.len());

    // Bit-reversal permutation.
    let mut j = 0usize;
    for i in 1..n {
        let mut bit = n >> 1;
        while j & bit != 0 {
            j ^= bit;
            bit >>= 1;
        }
        j |= bit;
        if i < j {
            re.swap(i, j);
            im.swap(i, j);
        }
    }

    // Butterfly stages.
    //
    // **SIMD Floor 1 / 1A:** the per-stage twiddle factors are precomputed once (via
    // the same recurrence that used to run inline) into `tw_r`/`tw_i`, instead of
    // being advanced by a serial recurrence *inside* the butterfly loop. That data
    // dependency (`cr,ci` carried k→k+1) blocked vectorization; hoisting it makes the
    // inner `k` loop independent and contiguous. Byte-identical (same recurrence
    // values, just computed once and reused across base-blocks) — pinned by
    // `hoisted_matches_inline`.
    let mut tw_r = vec![0f32; n / 2];
    let mut tw_i = vec![0f32; n / 2];
    let mut len = 2usize;
    while len <= n {
        let ang = -2.0 * PI / len as f32;
        let (wr, wi) = (ang.cos(), ang.sin());
        let half = len / 2;
        // Stage twiddles, computed once (the old per-base-block recurrence).
        tw_r[0] = 1.0;
        tw_i[0] = 0.0;
        for k in 1..half {
            tw_r[k] = tw_r[k - 1] * wr - tw_i[k - 1] * wi;
            tw_i[k] = tw_r[k - 1] * wi + tw_i[k - 1] * wr;
        }
        let mut base = 0;
        while base < n {
            for k in 0..half {
                let a = base + k;
                let b = a + half;
                let (cr, ci) = (tw_r[k], tw_i[k]);
                let tr = cr * re[b] - ci * im[b];
                let ti = cr * im[b] + ci * re[b];
                re[b] = re[a] - tr;
                im[b] = im[a] - ti;
                re[a] += tr;
                im[a] += ti;
            }
            base += len;
        }
        len <<= 1;
    }
}

/// Power spectrum `|X[k]|²` of a real signal windowed into `re` (with `im`
/// zeroed), for the first `n/2 + 1` bins. Convenience for the psymodel.
pub fn power_spectrum(re: &mut [f32], im: &mut [f32], out: &mut [f32]) {
    fft(re, im);
    for (k, o) in out.iter_mut().enumerate() {
        *o = re[k] * re[k] + im[k] * im[k];
    }
}

#[cfg(test)]
mod tests {
    use super::*;

    /// Naive DFT reference.
    fn dft(re: &[f32], im: &[f32]) -> (Vec<f32>, Vec<f32>) {
        let n = re.len();
        let mut or = vec![0f32; n];
        let mut oi = vec![0f32; n];
        for k in 0..n {
            for t in 0..n {
                let ang = -2.0 * PI * (k * t) as f32 / n as f32;
                or[k] += re[t] * ang.cos() - im[t] * ang.sin();
                oi[k] += re[t] * ang.sin() + im[t] * ang.cos();
            }
        }
        (or, oi)
    }

    /// The pre-1A FFT: twiddle advanced by a serial recurrence inside the loop.
    /// Kept as the bit-identity oracle for the hoisted version.
    fn fft_inline_ref(re: &mut [f32], im: &mut [f32]) {
        let n = re.len();
        let mut j = 0usize;
        for i in 1..n {
            let mut bit = n >> 1;
            while j & bit != 0 {
                j ^= bit;
                bit >>= 1;
            }
            j |= bit;
            if i < j {
                re.swap(i, j);
                im.swap(i, j);
            }
        }
        let mut len = 2usize;
        while len <= n {
            let ang = -2.0 * PI / len as f32;
            let (wr, wi) = (ang.cos(), ang.sin());
            let half = len / 2;
            let mut base = 0;
            while base < n {
                let (mut cr, mut ci) = (1.0f32, 0.0f32);
                for k in 0..half {
                    let a = base + k;
                    let b = a + half;
                    let tr = cr * re[b] - ci * im[b];
                    let ti = cr * im[b] + ci * re[b];
                    re[b] = re[a] - tr;
                    im[b] = im[a] - ti;
                    re[a] += tr;
                    im[a] += ti;
                    let ncr = cr * wr - ci * wi;
                    ci = cr * wi + ci * wr;
                    cr = ncr;
                }
                base += len;
            }
            len <<= 1;
        }
    }

    /// **1A gate.** Hoisting the twiddle recurrence must be BIT-identical to the old
    /// inline recurrence — the same float values, just computed once and reused.
    #[test]
    fn hoisted_matches_inline() {
        let mut s = 0xC0FF_EE11u32;
        let mut rng = || {
            s = s.wrapping_mul(1_664_525).wrapping_add(1_013_904_223);
            (s >> 8) as f32 / (1u32 << 24) as f32 - 0.5
        };
        for &n in &[2usize, 4, 8, 16, 64, 256, 1024] {
            let re0: Vec<f32> = (0..n).map(|_| rng()).collect();
            let im0: Vec<f32> = (0..n).map(|_| rng()).collect();
            let (mut ra, mut ia) = (re0.clone(), im0.clone());
            let (mut rb, mut ib) = (re0.clone(), im0.clone());
            fft(&mut ra, &mut ia);
            fft_inline_ref(&mut rb, &mut ib);
            assert_eq!(ra, rb, "re mismatch at n={n}");
            assert_eq!(ia, ib, "im mismatch at n={n}");
        }
    }

    #[test]
    fn matches_naive_dft() {
        let n = 64;
        let mut s = 0x1234_5678u32;
        let mut re: Vec<f32> = (0..n)
            .map(|_| {
                s = s.wrapping_mul(1_664_525).wrapping_add(1_013_904_223);
                (s >> 8) as f32 / (1u32 << 24) as f32 - 0.5
            })
            .collect();
        let mut im = vec![0f32; n];
        let (rr, ri) = dft(&re, &im);
        fft(&mut re, &mut im);
        for k in 0..n {
            assert!(
                (re[k] - rr[k]).abs() < 1e-3,
                "bin {k} re: {} vs {}",
                re[k],
                rr[k]
            );
            assert!((im[k] - ri[k]).abs() < 1e-3, "bin {k} im");
        }
    }

    #[test]
    fn pure_tone_concentrates_in_one_bin() {
        let n = 256;
        let bin = 8;
        let mut re: Vec<f32> = (0..n)
            .map(|t| (2.0 * PI * bin as f32 * t as f32 / n as f32).cos())
            .collect();
        let mut im = vec![0f32; n];
        fft(&mut re, &mut im);
        let mag: Vec<f32> = (0..n)
            .map(|k| (re[k] * re[k] + im[k] * im[k]).sqrt())
            .collect();
        // Energy at bin and its mirror; ~zero elsewhere.
        assert!(mag[bin] > 100.0, "tone bin weak: {}", mag[bin]);
        for k in 0..n {
            if k != bin && k != n - bin {
                assert!(mag[k] < 1.0, "leak at bin {k}: {}", mag[k]);
            }
        }
    }

    #[test]
    fn parseval_energy_conserved() {
        let n = 128;
        let mut s = 99u32;
        let orig: Vec<f32> = (0..n)
            .map(|_| {
                s = s.wrapping_mul(1_664_525).wrapping_add(1_013_904_223);
                (s >> 8) as f32 / (1u32 << 24) as f32 - 0.5
            })
            .collect();
        let mut re = orig.clone();
        let mut im = vec![0f32; n];
        fft(&mut re, &mut im);
        let time_energy: f32 = orig.iter().map(|x| x * x).sum();
        let freq_energy: f32 =
            (0..n).map(|k| re[k] * re[k] + im[k] * im[k]).sum::<f32>() / n as f32;
        assert!(
            (time_energy - freq_energy).abs() < 1e-2,
            "{time_energy} vs {freq_energy}"
        );
    }
}
