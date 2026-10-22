//! The Vorbis IMDCT core: an f32 power-of-two DCT-IV (pre-rotation, an `L/2`
//! point complex FFT, post-rotation).
//!
//! **Provenance:** `Radix2Fft` and the DCT-IV factorisation are ported from
//! `rusty_aac::dsp` (`Radix2Fft`, `Pow2Dct4`) in this workspace — the transform
//! that ships in the AAC decoder, bit-identical across its scalar / AVX / NEON
//! paths. rusty_vorbis stays dependency-free, so the code is copied rather than
//! linked. Vorbis needs `L = n/2` for blocksizes 64..=8192, i.e. FFTs of
//! 16..=2048 points.
//!
//! The IMDCT of `n/2` coefficients is the unscaled DCT-IV `t` of length `n/2`
//! unfolded to `n` samples (lewton's `inverse_mdct`, Vorbis I §1.3.2 with no
//! normalisation):
//!
//! ```text
//! y[i] =  t[n/4 + i]        for i in 0..n/4
//! y[i] = -t[3n/4 - 1 - i]   for i in n/4..3n/4
//! y[i] = -t[i - 3n/4]       for i in 3n/4..n
//! ```

use std::f64::consts::PI;

/// In-place iterative radix-2 complex FFT in f32 (`Σ x·e^{-2πi jk/n}`).
/// The butterfly stages have an AVX twin on x86-64 (runtime-detected) and a
/// NEON twin on aarch64; both do exactly the scalar products and sums (no FMA),
/// so every path is bit-identical (`fft_simd_matches_scalar`).
pub(crate) struct Radix2Fft {
    n: usize,
    /// The bit-reversal permutation as its swaps (`i < rev(i)` only).
    swaps: Vec<(u16, u16)>,
    /// Per-stage twiddles, stage with half-length `h` at offset `h - 2`.
    stw: Vec<[f32; 2]>,
    #[cfg(all(feature = "simd", target_arch = "x86_64"))]
    avx: bool,
}

impl Radix2Fft {
    pub(crate) fn new(n: usize) -> Self {
        assert!(n.is_power_of_two() && (2..=1 << 16).contains(&n));
        let bits = n.trailing_zeros();
        let rev: Vec<u16> = (0..n)
            .map(|i| ((i as u32).reverse_bits() >> (32 - bits)) as u16)
            .collect();
        let swaps = (0..n)
            .filter_map(|i| {
                let j = rev[i] as usize;
                (i < j).then_some((i as u16, j as u16))
            })
            .collect();
        let tw: Vec<[f32; 2]> = (0..n / 2)
            .map(|t| {
                let a = -2.0 * PI * t as f64 / n as f64;
                [a.cos() as f32, a.sin() as f32]
            })
            .collect();
        let mut stw = Vec::with_capacity(n.saturating_sub(2));
        let mut half = 2;
        while half < n {
            let step = n / (2 * half);
            stw.extend((0..half).map(|j| tw[j * step]));
            half *= 2;
        }
        Self {
            n,
            swaps,
            stw,
            #[cfg(all(feature = "simd", target_arch = "x86_64"))]
            avx: std::is_x86_feature_detected!("avx"),
        }
    }

    #[inline]
    pub(crate) fn run(&self, buf: &mut [[f32; 2]]) {
        let buf = &mut buf[..self.n];
        #[cfg(all(feature = "simd", target_arch = "x86_64"))]
        if self.avx {
            // SAFETY: AVX detected at construction; `buf` is exactly `n` long and
            // `stw` holds every stage's twiddles (see `run_avx`).
            unsafe { self.run_avx(buf) };
            return;
        }
        #[cfg(all(feature = "simd", target_arch = "aarch64"))]
        {
            // SAFETY: NEON is baseline on aarch64; same bounds as `run_avx`.
            unsafe { self.run_neon(buf) };
            return;
        }
        #[allow(unreachable_code)]
        self.run_scalar(buf);
    }

    #[inline(always)]
    fn permute_and_first_stage(&self, buf: &mut [[f32; 2]]) {
        for &(i, j) in &self.swaps {
            buf.swap(i as usize, j as usize);
        }
        for pair in buf.chunks_exact_mut(2) {
            let (a, b) = (pair[0], pair[1]);
            pair[0] = [a[0] + b[0], a[1] + b[1]];
            pair[1] = [a[0] - b[0], a[1] - b[1]];
        }
    }

    /// The scalar reference (and the fallback without the twin ISA).
    pub(crate) fn run_scalar(&self, buf: &mut [[f32; 2]]) {
        let buf = &mut buf[..self.n];
        self.permute_and_first_stage(buf);
        let mut half = 2;
        while half < self.n {
            let w = &self.stw[half - 2..2 * half - 2];
            for block in buf.chunks_exact_mut(2 * half) {
                let (lo, hi) = block.split_at_mut(half);
                for ((a, b), w) in lo.iter_mut().zip(hi.iter_mut()).zip(w) {
                    let t = [b[0] * w[0] - b[1] * w[1], b[0] * w[1] + b[1] * w[0]];
                    let x = *a;
                    *a = [x[0] + t[0], x[1] + t[1]];
                    *b = [x[0] - t[0], x[1] - t[1]];
                }
            }
            half *= 2;
        }
    }

    /// AVX twin: four butterflies per 256-bit op for `half >= 4`; the complex
    /// multiply is `b·Re(w) -/+ swap(b)·Im(w)` via `addsub` — the same two
    /// products and one sum per lane as the scalar path (bit-identical).
    ///
    /// # Safety
    /// AVX must be available and `buf.len() == self.n`. Each vector touches
    /// `buf[s+j..s+j+4]` and `buf[s+j+half..+4]` with `s + 2·half <= n`, `j + 4 <=
    /// half`, and twiddles `stw[half-2+j..+4]` with `2·half - 2 <= n - 2`.
    #[cfg(all(feature = "simd", target_arch = "x86_64"))]
    #[target_feature(enable = "avx")]
    unsafe fn run_avx(&self, buf: &mut [[f32; 2]]) {
        use std::arch::x86_64::*;
        let n = self.n;
        if n < 4 {
            self.run_scalar(buf);
            return;
        }
        for &(i, j) in &self.swaps {
            buf.swap(i as usize, j as usize);
        }
        let p = buf.as_mut_ptr().cast::<f32>();
        // First stage, four points per op (a0 + a1 even slot, a0 - a1 odd slot).
        let mut s = 0;
        while s < n {
            let x = _mm256_loadu_ps(p.add(2 * s));
            let y = _mm256_castpd_ps(_mm256_permute_pd::<0b0101>(_mm256_castps_pd(x)));
            let r = _mm256_blend_ps::<0b1100_1100>(_mm256_add_ps(x, y), _mm256_sub_ps(y, x));
            _mm256_storeu_ps(p.add(2 * s), r);
            s += 4;
        }
        let mut half = 2;
        while half < n {
            if half == 2 {
                let w = _mm_loadu_ps(self.stw.as_ptr().cast::<f32>());
                let (wr, wi) = (_mm_moveldup_ps(w), _mm_movehdup_ps(w));
                let mut s = 0;
                while s < n {
                    let pa = p.add(2 * s);
                    let pb = p.add(2 * s + 4);
                    let a = _mm_loadu_ps(pa);
                    let b = _mm_loadu_ps(pb);
                    let bs = _mm_permute_ps::<0b10_11_00_01>(b);
                    let t = _mm_addsub_ps(_mm_mul_ps(b, wr), _mm_mul_ps(bs, wi));
                    _mm_storeu_ps(pa, _mm_add_ps(a, t));
                    _mm_storeu_ps(pb, _mm_sub_ps(a, t));
                    s += 4;
                }
            } else {
                let wbase = self.stw.as_ptr().add(half - 2).cast::<f32>();
                let mut s = 0;
                while s < n {
                    let mut j = 0;
                    while j < half {
                        let pa = p.add(2 * (s + j));
                        let pb = p.add(2 * (s + j + half));
                        let a = _mm256_loadu_ps(pa);
                        let b = _mm256_loadu_ps(pb);
                        let w = _mm256_loadu_ps(wbase.add(2 * j));
                        let wr = _mm256_moveldup_ps(w);
                        let wi = _mm256_movehdup_ps(w);
                        let bs = _mm256_permute_ps::<0b10_11_00_01>(b);
                        let t = _mm256_addsub_ps(_mm256_mul_ps(b, wr), _mm256_mul_ps(bs, wi));
                        _mm256_storeu_ps(pa, _mm256_add_ps(a, t));
                        _mm256_storeu_ps(pb, _mm256_sub_ps(a, t));
                        j += 4;
                    }
                    s += 2 * half;
                }
            }
            half *= 2;
        }
    }

    /// NEON twin: two butterflies per 128-bit op; the even-lane sign of the
    /// complex multiply is a sign-bit flip (exact), so sums match the scalar.
    ///
    /// # Safety
    /// `buf.len() == self.n`; bounds as for `run_avx` with two-wide vectors.
    #[cfg(all(feature = "simd", target_arch = "aarch64"))]
    #[target_feature(enable = "neon")]
    unsafe fn run_neon(&self, buf: &mut [[f32; 2]]) {
        use std::arch::aarch64::*;
        let n = self.n;
        self.permute_and_first_stage(buf);
        let p = buf.as_mut_ptr() as *mut f32;
        let mask = [0x8000_0000u32, 0, 0x8000_0000, 0];
        let neg_even = vld1q_u32(mask.as_ptr());
        let mut half = 2;
        while half < n {
            let wbase = self.stw.as_ptr().add(half - 2) as *const f32;
            let mut s = 0;
            while s < n {
                let mut j = 0;
                while j < half {
                    let pa = p.add(2 * (s + j));
                    let pb = p.add(2 * (s + j + half));
                    let a = vld1q_f32(pa);
                    let b = vld1q_f32(pb);
                    let w = vld1q_f32(wbase.add(2 * j));
                    let wr = vtrn1q_f32(w, w);
                    let wi = vtrn2q_f32(w, w);
                    let x = vmulq_f32(b, wr);
                    let y = vmulq_f32(vrev64q_f32(b), wi);
                    let y = vreinterpretq_f32_u32(veorq_u32(vreinterpretq_u32_f32(y), neg_even));
                    let t = vaddq_f32(x, y);
                    vst1q_f32(pa, vaddq_f32(a, t));
                    vst1q_f32(pb, vsubq_f32(a, t));
                    j += 2;
                }
                s += 2 * half;
            }
            half *= 2;
        }
    }
}

/// Unscaled power-of-two DCT-IV: `out[m] = Σ x[k]·cos(π/L·(m+½)(k+½))`.
pub(crate) struct Dct4 {
    l: usize,
    pre: Vec<[f32; 2]>,
    post: Vec<[f32; 2]>,
    fft: Radix2Fft,
}

impl Dct4 {
    /// `l`: power of two, 32..=4096 (Vorbis `n/2`).
    pub(crate) fn new(l: usize) -> Self {
        assert!(l.is_power_of_two() && l >= 4);
        let m = l / 2;
        let pre = (0..m)
            .map(|p| {
                let th = PI * (4.0 * p as f64 + 1.0) / (4.0 * l as f64);
                [th.cos() as f32, th.sin() as f32]
            })
            .collect();
        let post = (0..m)
            .map(|p| {
                let ph = PI * p as f64 / l as f64;
                [ph.cos() as f32, ph.sin() as f32]
            })
            .collect();
        Self {
            l,
            pre,
            post,
            fft: Radix2Fft::new(m),
        }
    }

    pub(crate) fn len(&self) -> usize {
        self.l
    }

    /// `out[..L] = DCT-IV(x[..L])`, using `scratch[..L/2]`.
    pub(crate) fn run(&self, x: &[f32], out: &mut [f32], scratch: &mut [[f32; 2]]) {
        let m = self.l / 2;
        let x = &x[..self.l];
        let v = &mut scratch[..m];
        // Pre-rotation: v[p] = (x[2p] + i·x[L-1-2p])·e^{-iθ_p}.
        for (((vp, a), b), &[c, s]) in v
            .iter_mut()
            .zip(x.chunks_exact(2))
            .zip(x.rchunks_exact(2))
            .zip(&self.pre)
        {
            let (yr, yi) = (a[0], b[1]);
            *vp = [yr * c + yi * s, yi * c - yr * s];
        }
        self.fft.run(v);
        // out[2q] from v[q]; out[2q+1] = out[L-1-2p] from v[p], p = m-1-q.
        let post = v.iter().zip(&self.post);
        for ((o, (&[vr, vi], &[c, s])), (&[ur, ui], &[uc, us])) in out[..self.l]
            .chunks_exact_mut(2)
            .zip(post.clone())
            .zip(post.rev())
        {
            o[0] = vr * c + vi * s;
            o[1] = ur * us - ui * uc;
        }
    }
}

#[cfg(test)]
mod tests {
    use super::*;

    fn dct4_f64(x: &[f32]) -> Vec<f64> {
        let l = x.len();
        (0..l)
            .map(|m| {
                (0..l)
                    .map(|k| {
                        f64::from(x[k])
                            * (PI / l as f64 * (m as f64 + 0.5) * (k as f64 + 0.5)).cos()
                    })
                    .sum()
            })
            .collect()
    }

    #[test]
    fn dct4_matches_direct() {
        for &l in &[32usize, 64, 128, 1024] {
            let x: Vec<f32> = (0..l)
                .map(|i| ((i * 7919 % 211) as f32 / 105.0 - 1.0) * (1.0 + (i % 5) as f32))
                .collect();
            let d = Dct4::new(l);
            let mut out = vec![0.0; l];
            let mut scratch = vec![[0.0; 2]; l / 2];
            d.run(&x, &mut out, &mut scratch);
            let want = dct4_f64(&x);
            let peak = want.iter().fold(0.0f64, |a, &b| a.max(b.abs()));
            for (o, w) in out.iter().zip(&want) {
                assert!((f64::from(*o) - w).abs() < 1e-5 * peak, "l={l}");
            }
        }
    }

    #[test]
    fn fft_simd_matches_scalar() {
        for &n in &[16usize, 64, 256, 2048] {
            let f = Radix2Fft::new(n);
            let src: Vec<[f32; 2]> = (0..n)
                .map(|i| {
                    [
                        ((i * 37 % 101) as f32) * 0.01 - 0.5,
                        ((i * 53 % 97) as f32) * 0.02 - 1.0,
                    ]
                })
                .collect();
            let mut a = src.clone();
            let mut b = src;
            f.run(&mut a);
            f.run_scalar(&mut b);
            assert_eq!(a, b, "n={n}");
        }
    }
}
