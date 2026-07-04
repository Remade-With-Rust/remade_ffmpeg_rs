//! Vorbis filterbank: the Vorbis window + forward MDCT, matched to lewton's IMDCT.
//!
//! Vorbis windows both on encode (here) and decode (during overlap-add). The window
//! satisfies the Princen–Bradley condition `w[i]² + w[i+n/2]² = 1`, so
//! encode-window → forward-MDCT → IMDCT → decode-window → 50%-overlap-add perfectly
//! reconstructs the input. lewton's IMDCT is an *unnormalized* DCT-IV + unfold, so the
//! forward transform carries the `2/M` normalization (verified by the round-trip test).

use std::f32::consts::PI;
use std::f64::consts::PI as PI64;
use std::sync::OnceLock;

/// The Vorbis window slope (rising half), length `n_half = blocksize/2`:
/// `slope[x] = sin(π/2 · sin²(π/2 · (x+½)/n_half))`.
pub fn window_slope(n_half: usize) -> Vec<f32> {
    (0..n_half)
        .map(|x| {
            let v = (0.5 * PI * (x as f32 + 0.5) / n_half as f32).sin();
            (0.5 * PI * v * v).sin()
        })
        .collect()
}

/// The full symmetric Vorbis window of length `blocksize` (both neighbours same size).
pub fn vorbis_window(blocksize: usize) -> Vec<f32> {
    let n_half = blocksize / 2;
    let slope = window_slope(n_half);
    let mut w = vec![0.0f32; blocksize];
    w[..n_half].copy_from_slice(&slope);
    for i in n_half..blocksize {
        w[i] = slope[blocksize - 1 - i];
    }
    w
}

/// In-place radix-2 Cooley–Tukey FFT (forward, `exp(-i2πkn/N)`); `re.len()` must be a power of
/// two and equal `im.len()`. Pure in-house `f64` — the fast MDCT's engine. Uses precomputed
/// per-stage twiddles (`tw_c`/`tw_s`, flattened over stages) so a transform is pure multiply-add.
fn fft(re: &mut [f64], im: &mut [f64], tw_c: &[f64], tw_s: &[f64]) {
    let n = re.len();
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
    // Butterfly stages, indexing the flattened per-stage twiddle tables.
    let mut off = 0usize;
    let mut len = 2usize;
    while len <= n {
        let half = len / 2;
        let mut base = 0;
        while base < n {
            for k in 0..half {
                let (a, b) = (base + k, base + k + half);
                let (cr, ci) = (tw_c[off + k], tw_s[off + k]);
                let tr = cr * re[b] - ci * im[b];
                let ti = cr * im[b] + ci * re[b];
                re[b] = re[a] - tr;
                im[b] = im[a] - ti;
                re[a] += tr;
                im[a] += ti;
            }
            base += len;
        }
        off += half;
        len <<= 1;
    }
}

/// Precomputed MDCT twiddles for one block size: pre-rotation, post-rotation, and the FFT's
/// per-stage factors — so a transform touches no `cos`/`sin` at runtime.
struct MdctTwiddles {
    pre_c: Vec<f64>,
    pre_s: Vec<f64>,
    post_c: Vec<f64>,
    post_s: Vec<f64>,
    fft_c: Vec<f64>,
    fft_s: Vec<f64>,
}

impl MdctTwiddles {
    fn build(n: usize) -> MdctTwiddles {
        // N/4-FFT MDCT: fold N→L=N/2, an M=N/4 complex FFT, pre/post rotations
        // (shares the exact cosine basis with the direct oracle — see mdct_forward).
        let l = n / 2;
        let m = n / 4;
        let (mut pre_c, mut pre_s) = (vec![0f64; m], vec![0f64; m]);
        for p in 0..m {
            let th = PI64 * (4.0 * p as f64 + 1.0) / (4.0 * l as f64);
            pre_c[p] = th.cos();
            pre_s[p] = th.sin();
        }
        let (mut post_c, mut post_s) = (vec![0f64; m], vec![0f64; m]);
        for p in 0..m {
            let ph = PI64 * p as f64 / l as f64;
            post_c[p] = ph.cos();
            post_s[p] = ph.sin();
        }
        // FFT stage twiddles for the M-point transform (flattened over stages).
        let (mut fft_c, mut fft_s) = (Vec::new(), Vec::new());
        let mut len = 2usize;
        while len <= m {
            for k in 0..len / 2 {
                let ang = -2.0 * PI64 * k as f64 / len as f64;
                fft_c.push(ang.cos());
                fft_s.push(ang.sin());
            }
            len <<= 1;
        }
        MdctTwiddles {
            pre_c,
            pre_s,
            post_c,
            post_s,
            fft_c,
            fft_s,
        }
    }
}

/// The Vorbis block sizes (2048 long, 256 short) get cached twiddles; any other size (tests)
/// builds a fresh set on demand.
fn mdct_twiddles(n: usize) -> Option<&'static MdctTwiddles> {
    static LONG: OnceLock<MdctTwiddles> = OnceLock::new();
    static SHORT: OnceLock<MdctTwiddles> = OnceLock::new();
    match n {
        2048 => Some(LONG.get_or_init(|| MdctTwiddles::build(2048))),
        256 => Some(SHORT.get_or_init(|| MdctTwiddles::build(256))),
        _ => None,
    }
}

/// Forward MDCT (O(N log N)): `blocksize` windowed samples → `blocksize/2` coefficients, matched
/// to lewton's IMDCT. The textbook N/4 method — fold the N real inputs to L=N/2 (TDAC), pack into
/// M=N/4 complex with a pre-rotation, one **M-point** FFT, then a post-rotation unpacks the N/2
/// coefficients (4× fewer FFT points than a length-N transform). The `2/M` (=`2/l`) Vorbis scale
/// replaces AAC's `×2`; the cosine basis is identical, so this matches [`mdct_direct`] within f32
/// rounding. `blocksize` is a power of two ≥ 4.
pub fn mdct_forward(xw: &[f32]) -> Vec<f32> {
    let n = xw.len();
    let (l, m) = (n / 2, n / 4);
    if n < 4 {
        return mdct_direct_f64(xw);
    }
    let owned;
    let tw = match mdct_twiddles(n) {
        Some(t) => t,
        None => {
            owned = MdctTwiddles::build(n);
            &owned
        }
    };
    // TDAC fold xw[0..N] → y[mm] (computed on the fly), for the two indices each p needs.
    let (l2, l32) = (l / 2, 3 * l / 2);
    let fold = |mm: usize| -> f64 {
        if mm < l2 {
            -(xw[l32 - 1 - mm] as f64) - (xw[mm + l32] as f64)
        } else {
            (xw[mm - l2] as f64) - (xw[l32 - 1 - mm] as f64)
        }
    };
    // Pack + pre-rotate into M complex: v[p] = (y[2p] + i·y[L-1-2p])·e^{-iπ(4p+1)/4L}.
    let (mut re, mut im) = (vec![0f64; m], vec![0f64; m]);
    for p in 0..m {
        let (yr, yi) = (fold(2 * p), fold(l - 1 - 2 * p));
        re[p] = yr * tw.pre_c[p] + yi * tw.pre_s[p];
        im[p] = yi * tw.pre_c[p] - yr * tw.pre_s[p];
    }
    fft(&mut re, &mut im, &tw.fft_c, &tw.fft_s);
    // Post-rotate W = V·e^{-iπp/L}; X[2p]=Re(W), X[L-1-2p]=-Im(W); output scaled ×(2/M).
    let scale = 2.0 / l as f64;
    let mut out = vec![0.0f32; l];
    for p in 0..m {
        let (vr, vi) = (re[p], im[p]);
        out[2 * p] = (scale * (vr * tw.post_c[p] + vi * tw.post_s[p])) as f32;
        out[l - 1 - 2 * p] = (scale * (vr * tw.post_s[p] - vi * tw.post_c[p])) as f32;
    }
    out
}

/// Direct O(N²) MDCT in `f64` — the tiny-size (`N<4`) fallback the FFT path can't handle.
fn mdct_direct_f64(xw: &[f32]) -> Vec<f32> {
    let big_n = xw.len();
    let m = big_n / 2;
    let scale = 2.0 / m as f64;
    let inv_m = PI64 / m as f64;
    let mut out = vec![0.0f32; m];
    for (k, o) in out.iter_mut().enumerate() {
        let kf = k as f64 + 0.5;
        let mut acc = 0.0f64;
        for (n, &s) in xw.iter().enumerate() {
            acc += s as f64 * (inv_m * (n as f64 + 0.5 + m as f64 / 2.0) * kf).cos();
        }
        *o = (acc * scale) as f32;
    }
    out
}

/// Direct O(N²) MDCT with a per-call `cos` — the scalar oracle the table path is checked against.
#[cfg(test)]
pub fn mdct_direct(xw: &[f32]) -> Vec<f32> {
    let big_n = xw.len();
    let m = big_n / 2;
    let scale = 2.0 / m as f32;
    let inv_m = PI / m as f32;
    let mut out = vec![0.0f32; m];
    for (k, o) in out.iter_mut().enumerate() {
        let kf = k as f32 + 0.5;
        let mut acc = 0.0f32;
        for (n, &s) in xw.iter().enumerate() {
            acc += s * (inv_m * (n as f32 + 0.5 + m as f32 / 2.0) * kf).cos();
        }
        *o = acc * scale;
    }
    out
}

/// Window a block in place (encode-side analysis window).
pub fn apply_window(block: &mut [f32], window: &[f32]) {
    for (s, w) in block.iter_mut().zip(window) {
        *s *= *w;
    }
}

#[cfg(test)]
mod tests {
    use super::*;
    use std::f32::consts::FRAC_PI_4;

    /// The cached-table + vectorized MDCT must match the direct O(N²) oracle within f32 rounding.
    #[test]
    fn table_mdct_matches_direct() {
        for &bs in &[256usize, 2048] {
            let sig: Vec<f32> = (0..bs)
                .map(|i| (0.03 * i as f32).sin() + 0.2 * (0.11 * i as f32).cos())
                .collect();
            let fast = mdct_forward(&sig);
            let direct = mdct_direct(&sig);
            for (a, b) in fast.iter().zip(&direct) {
                assert!((a - b).abs() <= 1e-3 * (1.0 + b.abs()), "bs={bs}: {a} vs {b}");
            }
        }
    }

    /// Princen–Bradley: `w[i]² + w[i+n/2]² == 1`.
    #[test]
    fn window_satisfies_princen_bradley() {
        for &bs in &[256usize, 2048] {
            let w = vorbis_window(bs);
            let half = bs / 2;
            for i in 0..half {
                let s = w[i] * w[i] + w[i + half] * w[i + half];
                assert!((s - 1.0).abs() < 1e-5, "bs={bs} i={i} sum={s}");
            }
        }
    }

    // --- lewton's IMDCT, ported verbatim for round-trip validation ---

    fn dct_iv_slow(buffer: &mut [f32]) {
        let x = buffer.to_vec();
        let n = buffer.len();
        let nmask = (n << 3) - 1;
        let mcos: Vec<f32> = (0..8 * n)
            .map(|i| (FRAC_PI_4 * (i as f32) / (n as f32)).cos())
            .collect();
        for i in 0..n {
            let mut acc = 0.0;
            for j in 0..n {
                acc += x[j] * mcos[((2 * i + 1) * (2 * j + 1)) & nmask];
            }
            buffer[i] = acc;
        }
    }

    fn inverse_mdct_slow(buffer: &mut [f32]) {
        let n = buffer.len();
        let n4 = n >> 2;
        let n2 = n >> 1;
        let n3_4 = n - n4;
        let mut temp = buffer[0..n2].to_vec();
        dct_iv_slow(&mut temp);
        buffer[..n4].copy_from_slice(&temp[n4..2 * n4]);
        for i in n4..n3_4 {
            buffer[i] = -temp[n3_4 - i - 1];
        }
        for i in n3_4..n {
            buffer[i] = -temp[i - n3_4];
        }
    }

    /// M coefficients → N=2M time samples via lewton's exact IMDCT.
    fn imdct_lewton(coeffs: &[f32]) -> Vec<f32> {
        let m = coeffs.len();
        let mut buffer = vec![0.0f32; 2 * m];
        buffer[0..m].copy_from_slice(coeffs);
        inverse_mdct_slow(&mut buffer);
        buffer
    }

    /// End-to-end TDAC: window → forward MDCT → lewton IMDCT → window → overlap-add
    /// must reconstruct the input in steady state. Validates the MDCT *and* the 2/M scale.
    #[test]
    fn mdct_imdct_reconstructs_via_overlap_add() {
        for &bs in &[256usize, 2048] {
            let hop = bs / 2;
            let w = vorbis_window(bs);
            let siglen = bs * 4;
            // A deterministic mixed-tone signal (no RNG in this env).
            let signal: Vec<f32> = (0..siglen)
                .map(|i| {
                    let t = i as f32;
                    0.6 * (0.02 * t).sin() + 0.3 * (0.11 * t + 0.7).sin() + 0.1 * (0.37 * t).cos()
                })
                .collect();
            let mut out = vec![0.0f32; siglen];
            let mut pos = 0;
            while pos + bs <= siglen {
                let mut block = signal[pos..pos + bs].to_vec();
                apply_window(&mut block, &w);
                let coeffs = mdct_forward(&block);
                let mut y = imdct_lewton(&coeffs);
                apply_window(&mut y, &w);
                for i in 0..bs {
                    out[pos + i] += y[i];
                }
                pos += hop;
            }
            // Steady-state region (fully overlapped) must match the input.
            let mut max_err = 0.0f32;
            for i in bs..(siglen - bs) {
                max_err = max_err.max((out[i] - signal[i]).abs());
            }
            assert!(max_err < 1e-3, "bs={bs} max reconstruction error {max_err}");
        }
    }
}
