//! The SBR complex QMF banks (ISO/IEC 14496-3 §4.6.18.4): 32-band analysis,
//! 64-band synthesis and the 32-band downsampled synthesis.
//!
//! The analysis modulation is a 64-point DFT once the half-band phase offsets
//! are pulled out as pre/post twiddles:
//! `e^{iπ(k+½)(2n−½)/64} = e^{i2πkn/64} · e^{−iπk/128} · e^{iπ(2n−½)/128}`.
//! The synthesis matrixing is a pair of 64-point DCT-IVs (see [`qmf_synthesis`]);
//! the downsampled synthesis kernel `(2n−127.5)/64` is the 64-band kernel at
//! half rate.
//!
//! The direct definitions are kept as the test oracle.

use super::tables::QMF_WINDOW;
use crate::dsp::{pow2_dct4, Pow2Dct4, Radix2Fft};
use std::f64::consts::PI;
use std::sync::OnceLock;

type Cpx = [f32; 2];

struct Plans {
    /// Unscaled inverse FFT (positive exponent) for the analysis.
    fft64: Radix2Fft,
    /// 64-point DCT-IV for the synthesis.
    dct64: &'static Pow2Dct4,
    /// Analysis pre-twiddle e^{iπ(2n−½)/128}, n < 64.
    ana_pre: Vec<Cpx>,
    /// Analysis post-twiddle 2·e^{−iπk/128}, k < 32.
    ana_post: Vec<Cpx>,
    /// The analysis window c(2m), time-reversed: `ana_win[q] = c(2·(319−q))`.
    ana_win: Vec<f32>,
    /// The decimated window c(2m) for the downsampled synthesis.
    ds_win: Vec<f32>,
    /// AVX detected once, for the synthesis window's 8-wide twin.
    #[cfg(all(feature = "simd", target_arch = "x86_64"))]
    avx: bool,
}

fn plans() -> &'static Plans {
    static P: OnceLock<Plans> = OnceLock::new();
    P.get_or_init(|| {
        let tw = |a: f64, scale: f64| [(a.cos() * scale) as f32, (a.sin() * scale) as f32];
        Plans {
            fft64: Radix2Fft::new(64, 1.0),
            dct64: pow2_dct4(64).expect("64 is a power of two"),
            ana_pre: (0..64)
                .map(|n| tw(PI * (2.0 * f64::from(n) - 0.5) / 128.0, 1.0))
                .collect(),
            ana_post: (0..32)
                .map(|k| tw(-PI * f64::from(k) / 128.0, 2.0))
                .collect(),
            ana_win: (0..320).map(|q| QMF_WINDOW[2 * (319 - q)]).collect(),
            ds_win: (0..320).map(|m| QMF_WINDOW[2 * m]).collect(),
            #[cfg(all(feature = "simd", target_arch = "x86_64"))]
            avx: std::is_x86_feature_detected!("avx"),
        }
    })
}

#[inline]
fn cmul(a: Cpx, b: Cpx) -> Cpx {
    [a[0] * b[0] - a[1] * b[1], a[0] * b[1] + a[1] * b[0]]
}

/// QMF analysis of `2·nts` slots of 32 samples; the core output is lifted to
/// ±32768 scale. `buf` (`288 + 1024` samples, kept by the caller across
/// frames) holds the last 288 input samples at its front, in time order: the
/// new samples land right after them, and one shift carries the history to
/// the next call — no zeroed stack buffer, no history copy in and out.
pub(super) fn qmf_analysis(input: &[f32], buf: &mut [f32], w: &mut [[Cpx; 32]; 32], nts: usize) {
    let p = plans();
    let ns = 64 * nts;
    let buf = &mut buf[..288 + ns];
    for (b, &s) in buf[288..].iter_mut().zip(&input[..ns]) {
        *b = s * 32768.0;
    }
    let mut a = [[0f32; 2]; 64];
    for (l, slot) in w.iter_mut().enumerate().take(2 * nts) {
        ana_fold(p, &buf[32 * l..32 * l + 320], &mut a);
        p.fft64.run_bitrev(&mut a);
        for (k, out) in slot.iter_mut().enumerate() {
            *out = cmul(a[k], p.ana_post[k]);
        }
    }
    buf.copy_within(ns..ns + 288, 0);
}

/// The analysis front end for one slot: window the 320-sample segment, fold it
/// to 64 and apply the pre-twiddle, `a[rev(n)] = u(n)·ana_pre[n]` — written in
/// the FFT's bit-reversed order, so `run_bitrev` needs no permutation pass.
#[inline]
fn ana_fold(p: &Plans, seg: &[f32], a: &mut [Cpx; 64]) {
    let seg = &seg[..320];
    #[cfg(all(feature = "simd", target_arch = "x86_64"))]
    if p.avx {
        // SAFETY: AVX detected in `plans()`; `seg`, `ana_win` (320) and
        // `ana_pre` (64) are exactly the lengths `ana_fold_avx` reads.
        unsafe { ana_fold_avx(p, seg, a) };
        return;
    }
    ana_fold_scalar(p, seg, a);
}

/// The scalar oracle of [`ana_fold`].
fn ana_fold_scalar(p: &Plans, seg: &[f32], a: &mut [Cpx; 64]) {
    // With x[n] newest first (x[n] = seg[319−n]), z(n) = x(n)·c(2n) is the
    // time-ordered segment times the reversed window; u(n) = Σ_j z(n+64j).
    let mut t = [0f32; 320];
    for ((tq, &x), &c) in t.iter_mut().zip(seg).zip(&p.ana_win) {
        *tq = x * c;
    }
    let rev = p.fft64.bitrev();
    for n in 0..64 {
        let u = t[319 - n] + t[255 - n] + t[191 - n] + t[127 - n] + t[63 - n];
        a[rev[n] as usize] = [u * p.ana_pre[n][0], u * p.ana_pre[n][1]];
    }
}

/// AVX twin of [`ana_fold`]. With `m = 63 − n` the fold is the forward sum
/// `t[256+m] + t[192+m] + t[128+m] + t[64+m] + t[m]` (the scalar's order), the
/// window product fused in; eight `m` per trip, then one lane reversal puts
/// them in `n` order and each `u` is doubled into its complex pair for the
/// pre-twiddle product. Same products and sums, so bit-identical
/// (`ana_fold_matches_scalar`).
///
/// # Safety
/// AVX must be available; `seg.len() >= 320`, `p.ana_win.len() == 320`,
/// `p.ana_pre.len() == 64`.
#[cfg(all(feature = "simd", target_arch = "x86_64"))]
#[target_feature(enable = "avx")]
unsafe fn ana_fold_avx(p: &Plans, seg: &[f32], a: &mut [Cpx; 64]) {
    use std::arch::x86_64::*;
    let (s, w) = (seg.as_ptr(), p.ana_win.as_ptr());
    let (pre, ap) = (
        p.ana_pre.as_ptr().cast::<f32>(),
        a.as_mut_ptr().cast::<f32>(),
    );
    let rev = p.fft64.bitrev();
    let tz = |k: usize| _mm256_mul_ps(_mm256_loadu_ps(s.add(k)), _mm256_loadu_ps(w.add(k)));
    let mut m = 0;
    while m < 64 {
        let mut u = tz(256 + m);
        u = _mm256_add_ps(u, tz(192 + m));
        u = _mm256_add_ps(u, tz(128 + m));
        u = _mm256_add_ps(u, tz(64 + m));
        u = _mm256_add_ps(u, tz(m));
        // Lane k holds u(63 − m − k); reverse so lane k is u(n0 + k).
        let r = _mm256_permute_ps::<0x1B>(_mm256_permute2f128_ps::<0x01>(u, u));
        let (lo, hi) = (_mm256_unpacklo_ps(r, r), _mm256_unpackhi_ps(r, r));
        let n0 = 56 - m;
        let d0 = _mm256_permute2f128_ps::<0x20>(lo, hi);
        let d1 = _mm256_permute2f128_ps::<0x31>(lo, hi);
        let e0 = _mm256_mul_ps(d0, _mm256_loadu_ps(pre.add(2 * n0)));
        let e1 = _mm256_mul_ps(d1, _mm256_loadu_ps(pre.add(2 * n0 + 8)));
        // Each complex value to its bit-reversed slot, 64 bits at a time.
        for (h, e) in [(0, e0), (4, e1)] {
            let lo = _mm_castps_pd(_mm256_castps256_ps128(e));
            let hi = _mm_castps_pd(_mm256_extractf128_ps::<1>(e));
            let slot = |q: usize| ap.add(2 * rev[n0 + h + q] as usize).cast::<f64>();
            _mm_storel_pd(slot(0), lo);
            _mm_storeh_pd(slot(1), lo);
            _mm_storel_pd(slot(2), hi);
            _mm_storeh_pd(slot(3), hi);
        }
        m += 8;
    }
}

/// Split a synthesis slot into `xr[k] = Re X[k]` and `xi[k] = (−1)^k·Im X[k]`
/// for `k < slot.len()`; the rest of `xr`/`xi` is left as is.
#[inline]
fn split_slot(p: &Plans, slot: &[Cpx], xr: &mut [f32; 64], xi: &mut [f32; 64]) {
    let bands = slot.len().min(64);
    #[cfg(all(feature = "simd", target_arch = "x86_64"))]
    if p.avx && bands % 8 == 0 {
        // SAFETY: AVX detected in `plans()`; `bands <= 64` complex values are
        // read and `bands` floats written to each 64-float output.
        unsafe { split_slot_avx(&slot[..bands], xr, xi) };
        return;
    }
    let _ = p;
    split_slot_scalar(&slot[..bands], xr, xi);
}

/// The scalar oracle of [`split_slot`].
fn split_slot_scalar(slot: &[Cpx], xr: &mut [f32; 64], xi: &mut [f32; 64]) {
    for (k, x) in slot.iter().enumerate() {
        xr[k] = x[0];
        xi[k] = if k & 1 == 1 { -x[1] } else { x[1] };
    }
}

/// AVX twin of [`split_slot`]: eight complex values per trip, deinterleaved by
/// a cross-lane exchange and two in-lane shuffles; the odd-`k` negation is a
/// sign-bit XOR, exactly the scalar's `-x`.
///
/// # Safety
/// AVX must be available; `slot.len() % 8 == 0` and `slot.len() <= 64`.
#[cfg(all(feature = "simd", target_arch = "x86_64"))]
#[target_feature(enable = "avx")]
unsafe fn split_slot_avx(slot: &[Cpx], xr: &mut [f32; 64], xi: &mut [f32; 64]) {
    use std::arch::x86_64::*;
    let (src, rp, ip) = (
        slot.as_ptr().cast::<f32>(),
        xr.as_mut_ptr(),
        xi.as_mut_ptr(),
    );
    let odd = _mm256_setr_ps(0.0, -0.0, 0.0, -0.0, 0.0, -0.0, 0.0, -0.0);
    let mut k = 0;
    while k < slot.len() {
        let (a, b) = (
            _mm256_loadu_ps(src.add(2 * k)),
            _mm256_loadu_ps(src.add(2 * k + 8)),
        );
        // [c0 c1 | c4 c5] and [c2 c3 | c6 c7]: the shuffles then yield k order.
        let lo = _mm256_permute2f128_ps::<0x20>(a, b);
        let hi = _mm256_permute2f128_ps::<0x31>(a, b);
        _mm256_storeu_ps(rp.add(k), _mm256_shuffle_ps::<0x88>(lo, hi));
        _mm256_storeu_ps(
            ip.add(k),
            _mm256_xor_ps(_mm256_shuffle_ps::<0xDD>(lo, hi), odd),
        );
        k += 8;
    }
}

/// The synthesis butterfly: `v[i] = (s1[63−i] − s0[i])/64` and
/// `v[127−i] = (s1[63−i] + s0[i])/64`.
#[inline]
fn dct_post(p: &Plans, s0: &[f32; 64], s1: &[f32; 64], v: &mut [f32; 128]) {
    #[cfg(all(feature = "simd", target_arch = "x86_64"))]
    if p.avx {
        // SAFETY: AVX detected in `plans()`; fixed-size arrays.
        unsafe { dct_post_avx(s0, s1, v) };
        return;
    }
    let _ = p;
    dct_post_scalar(s0, s1, v);
}

/// The scalar oracle of [`dct_post`].
fn dct_post_scalar(s0: &[f32; 64], s1: &[f32; 64], v: &mut [f32; 128]) {
    for i in 0..64 {
        v[i] = (s1[63 - i] - s0[i]) * (1.0 / 64.0);
        v[127 - i] = (s1[63 - i] + s0[i]) * (1.0 / 64.0);
    }
}

/// AVX twin of [`dct_post`]: eight `i` per trip, `s1` read reversed and the
/// mirrored half stored reversed; same differences, sums and scale, so
/// bit-identical (`dct_post_matches_scalar`).
///
/// # Safety
/// AVX must be available.
#[cfg(all(feature = "simd", target_arch = "x86_64"))]
#[target_feature(enable = "avx")]
unsafe fn dct_post_avx(s0: &[f32; 64], s1: &[f32; 64], v: &mut [f32; 128]) {
    use std::arch::x86_64::*;
    let rev = |x: __m256| _mm256_permute_ps::<0x1B>(_mm256_permute2f128_ps::<0x01>(x, x));
    let (p0, p1, vp) = (s0.as_ptr(), s1.as_ptr(), v.as_mut_ptr());
    let c = _mm256_set1_ps(1.0 / 64.0);
    let mut i = 0;
    while i < 64 {
        let a = _mm256_loadu_ps(p0.add(i));
        let b = rev(_mm256_loadu_ps(p1.add(56 - i))); // lane k: s1[63 − i − k]
        _mm256_storeu_ps(vp.add(i), _mm256_mul_ps(_mm256_sub_ps(b, a), c));
        _mm256_storeu_ps(vp.add(120 - i), rev(_mm256_mul_ps(_mm256_add_ps(b, a), c)));
        i += 8;
    }
}

/// 64-band (or downsampled 32-band) QMF synthesis of `2·nts` slots. `v` is a
/// FIFO of 20 blocks of `vlen` samples (newest first) with a moving offset,
/// so a slot costs no shift: the window reads ten blocks from `off`.
///
/// The 64-band matrixing is two 64-point DCT-IVs: with `s0 = DCT-IV(Re X)` and
/// `s1 = DCT-IV((−1)^k·Im X)`, `v[i] = (s1[63−i] − s0[i])/64` and
/// `v[127−i] = (s1[63−i] + s0[i])/64`. The downsampled bank is the same kernel
/// at half rate with the upper 32 bands empty: its `v` is every other sample.
pub(super) fn qmf_synthesis(
    out: &mut [f32],
    xs: &[[Cpx; 64]],
    v: &mut QmfSynthState,
    nts: usize,
    ds: bool,
) {
    let p = plans();
    let bands = if ds { 32 } else { 64 };
    let vlen = 2 * bands;
    let span = 10 * vlen;
    let win: &[f32] = if ds { &p.ds_win } else { &QMF_WINDOW };
    let (mut xr, mut xi) = ([0f32; 64], [0f32; 64]);
    let (mut s0, mut s1) = ([0f32; 64], [0f32; 64]);
    let mut v64 = [0f32; 128];
    let mut scratch = [[0f32; 2]; 32];
    for (l, slot) in xs.iter().enumerate().take(2 * nts) {
        if v.off < vlen {
            // Wrap: keep the newest span − vlen samples at the far end.
            let keep = span - vlen;
            let len = v.buf.len();
            v.buf.copy_within(v.off..v.off + keep, len - keep);
            v.off = len - keep;
        }
        v.off -= vlen;
        let off = v.off;
        split_slot(p, &slot[..bands], &mut xr, &mut xi);
        p.dct64.dct4(&xr, &mut s0, &mut scratch);
        p.dct64.dct4(&xi, &mut s1, &mut scratch);
        dct_post(p, &s0, &s1, &mut v64);
        let dst = &mut v.buf[off..off + vlen];
        if ds {
            for (n, d) in dst.iter_mut().enumerate() {
                *d = v64[2 * n];
            }
        } else {
            dst.copy_from_slice(&v64);
        }
        // g[2b·i + j] = v[2·vlen·i + j], g[2b·i + b + j] = v[2·vlen·i + 3b + j];
        // out[j] = Σ g·c over the ten blocks.
        qmf_window(
            p,
            &mut out[l * bands..(l + 1) * bands],
            &v.buf[off..off + span],
            &win[..10 * bands],
        );
    }
}

/// The synthesis window: `o[j] = (Σ_i va_i[j]·wa_i[j] + vb_i[j]·wb_i[j])/32768`
/// over the five block pairs of `vb` (`o.len()` bands, `vb.len() == 20·bands`,
/// `win.len() == 10·bands`), summed in that order from 0.
#[inline]
fn qmf_window(p: &Plans, o: &mut [f32], vb: &[f32], win: &[f32]) {
    let bands = o.len();
    let (vb, win) = (&vb[..20 * bands], &win[..10 * bands]);
    // A running `o[j] +=` reloads and re-stores every column for each of the
    // ten terms; the twins hold eight columns in two registers across all of
    // them, same products and same add order per column, so bit-identical
    // (`qmf_window_matches_scalar`).
    #[cfg(all(feature = "simd", target_arch = "x86_64"))]
    if bands % 8 == 0 && p.avx {
        crate::prof::count(crate::prof::Kernel::QmfWindow, true, bands);
        // SAFETY: AVX detected in `plans()`; bounds as for the SSE twin below.
        unsafe { qmf_window_avx(o, vb, win) };
        return;
    }
    #[cfg(all(feature = "simd", target_arch = "x86_64"))]
    if bands % 8 == 0 {
        crate::prof::count(crate::prof::Kernel::QmfWindow, true, bands);
        // SAFETY: SSE is baseline on x86-64; `vb`/`win` are re-bounded above
        // to the 20·bands / 10·bands that `qmf_window_sse` reads.
        unsafe { qmf_window_sse(o, vb, win) };
        return;
    }
    #[cfg(all(feature = "simd", target_arch = "aarch64"))]
    if bands % 8 == 0 {
        crate::prof::count(crate::prof::Kernel::QmfWindow, true, bands);
        // SAFETY: NEON is baseline on aarch64; bounds as above.
        unsafe { qmf_window_neon(o, vb, win) };
        return;
    }
    let _ = p; // only the x86-64 twins consult the detected ISA
    crate::prof::count(crate::prof::Kernel::QmfWindow, false, bands);
    qmf_window_scalar(o, vb, win);
}

/// The scalar oracle of [`qmf_window`].
fn qmf_window_scalar(o: &mut [f32], vb: &[f32], win: &[f32]) {
    let bands = o.len();
    let vlen = 2 * bands;
    o.fill(0.0);
    for i in 0..5 {
        let (va, wa) = (&vb[2 * vlen * i..][..bands], &win[2 * bands * i..][..bands]);
        let (vb2, wb) = (
            &vb[2 * vlen * i + 3 * bands..][..bands],
            &win[2 * bands * i + bands..][..bands],
        );
        for j in 0..bands {
            o[j] += va[j] * wa[j];
            o[j] += vb2[j] * wb[j];
        }
    }
    for x in o.iter_mut() {
        *x *= 1.0 / 32768.0;
    }
}

/// SSE twin of [`qmf_window`].
///
/// # Safety
/// `o.len() % 8 == 0`, `vb.len() >= 20·o.len()`, `win.len() >= 10·o.len()`: the
/// trip for columns `j..j+8` reads `vb[4b·i + j..][..8]`, `vb[4b·i + 3b + j..][..8]`,
/// `win[2b·i + j..][..8]` and `win[2b·i + b + j..][..8]` for `i < 5`.
#[cfg(all(feature = "simd", target_arch = "x86_64"))]
unsafe fn qmf_window_sse(o: &mut [f32], vb: &[f32], win: &[f32]) {
    use std::arch::x86_64::*;
    let b = o.len();
    let (op, v, w) = (o.as_mut_ptr(), vb.as_ptr(), win.as_ptr());
    let scale = _mm_set1_ps(1.0 / 32768.0);
    let mut j = 0;
    while j < b {
        let (mut a0, mut a1) = (_mm_setzero_ps(), _mm_setzero_ps());
        for i in 0..5 {
            let (va, wa) = (v.add(4 * b * i + j), w.add(2 * b * i + j));
            let (vc, wc) = (v.add(4 * b * i + 3 * b + j), w.add(2 * b * i + b + j));
            a0 = _mm_add_ps(a0, _mm_mul_ps(_mm_loadu_ps(va), _mm_loadu_ps(wa)));
            a1 = _mm_add_ps(
                a1,
                _mm_mul_ps(_mm_loadu_ps(va.add(4)), _mm_loadu_ps(wa.add(4))),
            );
            a0 = _mm_add_ps(a0, _mm_mul_ps(_mm_loadu_ps(vc), _mm_loadu_ps(wc)));
            a1 = _mm_add_ps(
                a1,
                _mm_mul_ps(_mm_loadu_ps(vc.add(4)), _mm_loadu_ps(wc.add(4))),
            );
        }
        _mm_storeu_ps(op.add(j), _mm_mul_ps(a0, scale));
        _mm_storeu_ps(op.add(j + 4), _mm_mul_ps(a1, scale));
        j += 8;
    }
}

/// AVX twin of [`qmf_window`]: one 8-column register per trip, and VEX folds
/// the unaligned window operand into the multiply.
///
/// # Safety
///
/// As [`qmf_window_sse`], and AVX must be available.
#[cfg(all(feature = "simd", target_arch = "x86_64"))]
#[target_feature(enable = "avx")]
unsafe fn qmf_window_avx(o: &mut [f32], vb: &[f32], win: &[f32]) {
    use std::arch::x86_64::*;
    let b = o.len();
    let (op, v, w) = (o.as_mut_ptr(), vb.as_ptr(), win.as_ptr());
    let scale = _mm256_set1_ps(1.0 / 32768.0);
    let mut j = 0;
    while j < b {
        let mut a = _mm256_setzero_ps();
        for i in 0..5 {
            let (va, wa) = (v.add(4 * b * i + j), w.add(2 * b * i + j));
            let (vc, wc) = (v.add(4 * b * i + 3 * b + j), w.add(2 * b * i + b + j));
            a = _mm256_add_ps(a, _mm256_mul_ps(_mm256_loadu_ps(va), _mm256_loadu_ps(wa)));
            a = _mm256_add_ps(a, _mm256_mul_ps(_mm256_loadu_ps(vc), _mm256_loadu_ps(wc)));
        }
        _mm256_storeu_ps(op.add(j), _mm256_mul_ps(a, scale));
        j += 8;
    }
}

/// NEON twin of [`qmf_window`].
///
/// # Safety
///
/// As [`qmf_window_sse`] (NEON is baseline on aarch64).
#[cfg(all(feature = "simd", target_arch = "aarch64"))]
unsafe fn qmf_window_neon(o: &mut [f32], vb: &[f32], win: &[f32]) {
    use std::arch::aarch64::*;
    let b = o.len();
    let (op, v, w) = (o.as_mut_ptr(), vb.as_ptr(), win.as_ptr());
    let scale = vdupq_n_f32(1.0 / 32768.0);
    let mut j = 0;
    while j < b {
        let (mut a0, mut a1) = (vdupq_n_f32(0.0), vdupq_n_f32(0.0));
        for i in 0..5 {
            let (va, wa) = (v.add(4 * b * i + j), w.add(2 * b * i + j));
            let (vc, wc) = (v.add(4 * b * i + 3 * b + j), w.add(2 * b * i + b + j));
            // Separate multiply and add (never vfmaq): the scalar rounds twice.
            a0 = vaddq_f32(a0, vmulq_f32(vld1q_f32(va), vld1q_f32(wa)));
            a1 = vaddq_f32(a1, vmulq_f32(vld1q_f32(va.add(4)), vld1q_f32(wa.add(4))));
            a0 = vaddq_f32(a0, vmulq_f32(vld1q_f32(vc), vld1q_f32(wc)));
            a1 = vaddq_f32(a1, vmulq_f32(vld1q_f32(vc.add(4)), vld1q_f32(wc.add(4))));
        }
        vst1q_f32(op.add(j), vmulq_f32(a0, scale));
        vst1q_f32(op.add(j + 4), vmulq_f32(a1, scale));
        j += 8;
    }
}

/// Synthesis FIFO: room for several slots so the shift happens rarely.
#[derive(Clone)]
pub(super) struct QmfSynthState {
    buf: Vec<f32>,
    off: usize,
}

impl QmfSynthState {
    pub(super) fn new() -> Self {
        let len = 1280 + 32 * 128;
        Self {
            buf: vec![0.0; len],
            off: len - 1280,
        }
    }
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn ana_fold_matches_scalar() {
        let mut seed = 0x2545_f491u32;
        let mut rnd = || {
            seed = seed.wrapping_mul(1_664_525).wrapping_add(1_013_904_223);
            (seed >> 8) as f32 / (1 << 23) as f32 * 2.0 - 1.0
        };
        let p = plans();
        for _ in 0..200 {
            let seg: Vec<f32> = (0..320).map(|_| rnd() * 32768.0).collect();
            let (mut a, mut b) = ([[0f32; 2]; 64], [[0f32; 2]; 64]);
            ana_fold(p, &seg, &mut a);
            ana_fold_scalar(p, &seg, &mut b);
            let bits = |x: &[Cpx; 64]| x.iter().flatten().map(|v| v.to_bits()).collect::<Vec<_>>();
            assert_eq!(bits(&a), bits(&b));
        }
    }

    #[test]
    fn split_slot_matches_scalar() {
        let p = plans();
        let mut seed = 0x6c07_8965u32;
        let mut rnd = || {
            seed = seed.wrapping_mul(1_664_525).wrapping_add(1_013_904_223);
            (seed >> 8) as f32 / (1 << 23) as f32 * 2.0 - 1.0
        };
        for bands in [32usize, 64] {
            let mut slot: Vec<Cpx> = (0..bands).map(|_| [rnd(), rnd()]).collect();
            slot[1][1] = 0.0; // the sign flip of +0 must give -0
            slot[3][1] = -0.0;
            let (mut ar, mut ai, mut br, mut bi) = ([7f32; 64], [7f32; 64], [7f32; 64], [7f32; 64]);
            split_slot(p, &slot, &mut ar, &mut ai);
            split_slot_scalar(&slot, &mut br, &mut bi);
            let bits = |x: &[f32; 64]| x.map(f32::to_bits);
            assert_eq!(bits(&ar), bits(&br), "re bands {bands}");
            assert_eq!(bits(&ai), bits(&bi), "im bands {bands}");
        }
    }

    #[test]
    fn dct_post_matches_scalar() {
        let p = plans();
        let mut seed = 0x1234_5679u32;
        let mut rnd = || {
            seed = seed.wrapping_mul(1_664_525).wrapping_add(1_013_904_223);
            (seed >> 8) as f32 / (1 << 23) as f32 * 2.0 - 1.0
        };
        for _ in 0..100 {
            let s0: [f32; 64] = std::array::from_fn(|_| rnd() * 1e4);
            let s1: [f32; 64] = std::array::from_fn(|_| rnd() * 1e4);
            let (mut a, mut b) = ([0f32; 128], [0f32; 128]);
            dct_post(p, &s0, &s1, &mut a);
            dct_post_scalar(&s0, &s1, &mut b);
            assert_eq!(a.map(f32::to_bits), b.map(f32::to_bits));
        }
    }

    #[test]
    fn qmf_window_matches_scalar() {
        let mut seed = 0x9e37_79b9u32;
        let mut rnd = || {
            seed = seed.wrapping_mul(1_664_525).wrapping_add(1_013_904_223);
            (seed >> 8) as f32 / (1 << 23) as f32 * 2.0 - 1.0
        };
        for bands in [32usize, 64] {
            for _ in 0..50 {
                let vb: Vec<f32> = (0..20 * bands).map(|_| rnd() * 30000.0).collect();
                let win: Vec<f32> = (0..10 * bands).map(|_| rnd()).collect();
                let (mut a, mut b) = (vec![0f32; bands], vec![0f32; bands]);
                qmf_window(plans(), &mut a, &vb, &win);
                qmf_window_scalar(&mut b, &vb, &win);
                let bits = |x: &[f32]| x.iter().map(|v| v.to_bits()).collect::<Vec<_>>();
                assert_eq!(bits(&a), bits(&b), "bands {bands}");
                // The dispatcher prefers AVX; hold the SSE twin to the oracle too.
                #[cfg(all(feature = "simd", target_arch = "x86_64"))]
                {
                    // SAFETY: SSE is baseline; lengths are exactly 20·b / 10·b.
                    unsafe { qmf_window_sse(&mut a, &vb, &win) };
                    assert_eq!(bits(&a), bits(&b), "sse bands {bands}");
                }
            }
        }
    }

    /// The standard's direct analysis (oracle).
    fn analysis_direct(input: &[f32], hist: &mut [f64], w: &mut [[Cpx; 32]; 32], nts: usize) {
        let ns = 64 * nts;
        let mut buf = vec![0f64; 288 + ns];
        buf[..288].copy_from_slice(hist);
        for (b, &s) in buf[288..].iter_mut().zip(&input[..ns]) {
            *b = f64::from(s) * 32768.0;
        }
        for (l, slot) in w.iter_mut().enumerate().take(2 * nts) {
            let end = 320 + 32 * l;
            let mut u = [0f64; 64];
            for n in 0..320 {
                u[n & 63] += buf[end - 1 - n] * f64::from(QMF_WINDOW[2 * n]);
            }
            for (k, out) in slot.iter_mut().enumerate() {
                let (mut re, mut im) = (0f64, 0f64);
                for (n, un) in u.iter().enumerate() {
                    let a = PI * (k as f64 + 0.5) * (2.0 * n as f64 - 0.5) / 64.0;
                    re += un * 2.0 * a.cos();
                    im += un * 2.0 * a.sin();
                }
                *out = [re as f32, im as f32];
            }
        }
        hist.copy_from_slice(&buf[ns..]);
    }

    /// The standard's direct synthesis (oracle); `v` newest first.
    fn synthesis_direct(out: &mut [f32], xs: &[[Cpx; 64]], v: &mut [f64], nts: usize, ds: bool) {
        let bands = if ds { 32 } else { 64 };
        let vlen = 2 * bands;
        let total = 10 * vlen;
        for (l, slot) in xs.iter().enumerate().take(2 * nts) {
            v.copy_within(0..total - vlen, vlen);
            for n in 0..vlen {
                let mut acc = 0f64;
                for (k, x) in slot.iter().enumerate().take(bands) {
                    let a = if ds {
                        PI * (k as f64 + 0.5) * (2.0 * n as f64 - 127.5) / 64.0
                    } else {
                        PI * (k as f64 + 0.5) * (2.0 * n as f64 - 255.0) / 128.0
                    };
                    acc += (f64::from(x[0]) * a.cos() - f64::from(x[1]) * a.sin()) / 64.0;
                }
                v[n] = acc;
            }
            let step = if ds { 2 } else { 1 };
            for (j, o) in out[l * bands..(l + 1) * bands].iter_mut().enumerate() {
                let mut acc = 0f64;
                for i in 0..5 {
                    acc += v[2 * vlen * i + j] * f64::from(QMF_WINDOW[step * (2 * bands * i + j)]);
                    acc += v[2 * vlen * i + 3 * bands + j]
                        * f64::from(QMF_WINDOW[step * (2 * bands * i + bands + j)]);
                }
                *o = (acc / 32768.0) as f32;
            }
        }
    }

    fn noise(n: usize, seed: u32) -> Vec<f32> {
        let mut s = seed;
        (0..n)
            .map(|_| {
                s = s.wrapping_mul(1664525).wrapping_add(1013904223);
                (s >> 8) as f32 / (1u32 << 24) as f32 - 0.5
            })
            .collect()
    }

    #[test]
    #[cfg_attr(miri, ignore = "too slow to interpret: over 2 minutes under Miri")]
    fn fast_banks_match_the_direct_definitions() {
        let (mut hf, mut hd) = (vec![0f32; 288 + 1024], vec![0f64; 288]);
        let (mut wf, mut wd) = ([[[0f32; 2]; 32]; 32], [[[0f32; 2]; 32]; 32]);
        let mut sf = QmfSynthState::new();
        let mut sd = vec![0f64; 1280];
        let mut dsf = QmfSynthState::new();
        let mut dsd = vec![0f64; 640];
        let mut worst = 0f32;
        for frame in 0..5 {
            let input = noise(1024, frame);
            qmf_analysis(&input, &mut hf, &mut wf, 16);
            analysis_direct(&input, &mut hd, &mut wd, 16);
            for (a, b) in wf.iter().flatten().zip(wd.iter().flatten()) {
                worst = worst.max((a[0] - b[0]).abs()).max((a[1] - b[1]).abs());
            }
            let mut xs = vec![[[0f32; 2]; 64]; 38];
            for (l, s) in wd.iter().enumerate() {
                xs[l][..32].copy_from_slice(s);
                for k in 32..64 {
                    xs[l][k] = [s[k - 32][1] * 0.3, s[k - 32][0] * -0.2];
                }
            }
            let (mut of, mut od) = (vec![0f32; 2048], vec![0f32; 2048]);
            qmf_synthesis(&mut of, &xs, &mut sf, 16, false);
            synthesis_direct(&mut od, &xs, &mut sd, 16, false);
            let e = of
                .iter()
                .zip(&od)
                .fold(0f32, |m, (a, b)| m.max((a - b).abs()));
            assert!(e < 2e-5, "synthesis error {e}");
            let (mut of, mut od) = (vec![0f32; 1024], vec![0f32; 1024]);
            qmf_synthesis(&mut of, &xs, &mut dsf, 16, true);
            synthesis_direct(&mut od, &xs, &mut dsd, 16, true);
            let e = of
                .iter()
                .zip(&od)
                .fold(0f32, |m, (a, b)| m.max((a - b).abs()));
            assert!(e < 2e-5, "downsampled synthesis error {e}");
        }
        // Subband samples are at ±32768·(window gain) scale.
        assert!(worst < 0.05, "analysis error {worst}");
    }

    /// Analysis then synthesis with nothing in between is a unity-gain 2x
    /// upsampler (64-band) or a unity-gain identity (32-band downsampled).
    #[test]
    #[cfg_attr(miri, ignore = "too slow to interpret: over 2 minutes under Miri")]
    fn qmf_round_trip_is_unity_gain() {
        let mut hist = vec![0f32; 288 + 1024];
        let mut w = [[[0f32; 2]; 32]; 32];
        let mut v = QmfSynthState::new();
        let mut ds = QmfSynthState::new();
        let (mut up, mut same) = (Vec::new(), Vec::new());
        let f = 1000.0 / 22050.0;
        for frame in 0..6 {
            let input: Vec<f32> = (0..1024)
                .map(|i| (0.5 * (2.0 * PI * f * f64::from(frame * 1024 + i)).sin()) as f32)
                .collect();
            qmf_analysis(&input, &mut hist, &mut w, 16);
            let mut xs = vec![[[0f32; 2]; 64]; 38];
            for (l, s) in w.iter().enumerate() {
                xs[l][..32].copy_from_slice(s);
            }
            let mut out = vec![0f32; 2048];
            qmf_synthesis(&mut out, &xs, &mut v, 16, false);
            up.extend_from_slice(&out);
            let mut out = vec![0f32; 1024];
            qmf_synthesis(&mut out, &xs, &mut ds, 16, true);
            same.extend_from_slice(&out);
        }
        let peak = up[4096..].iter().fold(0f32, |a, &b| a.max(b.abs()));
        assert!((peak - 0.5).abs() < 0.01, "peak {peak}");
        // The downsampled bank reconstructs the input (delayed) near-perfectly.
        let input: Vec<f32> = (0..6 * 1024)
            .map(|i| (0.5 * (2.0 * PI * f * f64::from(i)).sin()) as f32)
            .collect();
        let best = (0..600)
            .map(|d| {
                let (mut s, mut e) = (0f64, 0f64);
                for t in 2048..6 * 1024 {
                    let r = f64::from(input[t - d]);
                    s += r * r;
                    e += (f64::from(same[t]) - r).powi(2);
                }
                10.0 * (s / e).log10()
            })
            .fold(f64::MIN, f64::max);
        assert!(best > 50.0, "downsampled round trip {best:.1} dB");
    }
}
