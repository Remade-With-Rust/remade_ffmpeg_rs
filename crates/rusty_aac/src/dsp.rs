//! AAC numerical core: inverse quantization and the IMDCT synthesis filterbank
//! (windows + overlap-add). These must match ISO 14496-3 exactly — they are the
//! parts where any deviation produces audibly wrong / incompatible output, so
//! they're written straight from the spec and checked by the MDCT↔IMDCT
//! perfect-reconstruction (TDAC) property.
//!
//! The IMDCT here is a direct O(N²) evaluation — correct and clear; it can be
//! swapped for an FFT-based fast path later without changing results.

// These primitives are exercised by the tests now and wired into the decode
// path in the spectral/synthesis stages; allow until then.
#![allow(dead_code)]

use std::f64::consts::PI;
use std::sync::OnceLock;

/// Inverse quantization of a spectral coefficient (ISO 14496-3 §10.3):
/// `x = sign(q) · |q|^(4/3)`. The scalefactor gain is applied separately.
pub fn dequant(q: i32) -> f32 {
    let m = (q.unsigned_abs() as f64).powf(4.0 / 3.0);
    (q.signum() as f64 * m) as f32
}

/// Scalefactor gain `2^(0.25·(sf − 100))`, the per-band multiplier applied to
/// dequantized coefficients.
pub fn sf_gain(sf: i32) -> f32 {
    2f64.powf(0.25 * (sf as f64 - 100.0)) as f32
}

/// IMDCT (ISO 14496-3 §4.6.11.2): `N/2` spectral coefficients → `N` time
/// samples. `out[i] = (2/N)·Σ_k spec[k]·cos((2π/N)(i+n0)(k+½))`, `n0=(N/2+1)/2`.
pub fn imdct(spec: &[f32]) -> Vec<f32> {
    let half = spec.len();
    let n = half * 2;
    if n == 0 {
        return Vec::new();
    }
    let n0 = (n / 2 + 1) as f64 / 2.0; // N/4 + 1/2
    let scale = 2.0 / n as f64;
    let w = 2.0 * PI / n as f64;
    let mut out = vec![0f32; n];
    for (i, o) in out.iter_mut().enumerate() {
        let mut acc = 0f64;
        let a = w * (i as f64 + n0);
        for (k, &s) in spec.iter().enumerate() {
            acc += s as f64 * (a * (k as f64 + 0.5)).cos();
        }
        *o = (scale * acc) as f32;
    }
    out
}

/// Forward MDCT — the analysis transform paired with [`imdct`]. Used to validate
/// the IMDCT via perfect reconstruction (and useful for an eventual encoder).
/// `N` time samples → `N/2` coefficients.
///
/// The `2.0` factor makes this the exact inverse of the spec's `2/N` IMDCT under
/// Princen-Bradley windows, so `imdct(mdct(·))` with overlap-add is identity.
pub fn mdct(time: &[f32]) -> Vec<f32> {
    let n = time.len();
    let half = n / 2;
    if n == 0 {
        return Vec::new();
    }
    let n0 = (n / 2 + 1) as f64 / 2.0;
    let w = 2.0 * PI / n as f64;
    let mut out = vec![0f32; half];
    for (k, o) in out.iter_mut().enumerate() {
        let mut acc = 0f64;
        for (i, &t) in time.iter().enumerate() {
            acc += t as f64 * (w * (i as f64 + n0) * (k as f64 + 0.5)).cos();
        }
        *o = (2.0 * acc) as f32;
    }
    out
}

/// In-place radix-2 Cooley–Tukey FFT (forward, `exp(-i2πkn/N)`); `re.len()` must be
/// a power of two and equal `im.len()`. Pure in-house `f64` — the fast MDCT's engine.
/// Uses precomputed per-stage twiddles (`tw_c`/`tw_s`, flattened over stages) so a
/// transform is pure multiply-add — no runtime trig or twiddle recurrence.
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

/// Precomputed MDCT twiddles for one block size: pre-rotation, post-rotation, and
/// the FFT's per-stage factors — so a transform touches no `cos`/`sin` at runtime.
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
        // (derived + verified against the direct oracle — see mdct_fast).
        let l = n / 2;
        let m = n / 4;
        let (mut pre_c, mut pre_s) = (vec![0f64; m], vec![0f64; m]);
        for p in 0..m {
            let th = PI * (4.0 * p as f64 + 1.0) / (4.0 * l as f64);
            pre_c[p] = th.cos();
            pre_s[p] = th.sin();
        }
        let (mut post_c, mut post_s) = (vec![0f64; m], vec![0f64; m]);
        for p in 0..m {
            let ph = PI * p as f64 / l as f64;
            post_c[p] = ph.cos();
            post_s[p] = ph.sin();
        }
        // FFT stage twiddles for the M-point transform (flattened over stages).
        let (mut fft_c, mut fft_s) = (Vec::new(), Vec::new());
        let mut len = 2usize;
        while len <= m {
            for k in 0..len / 2 {
                let ang = -2.0 * PI * k as f64 / len as f64;
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

/// The AAC block sizes (2048 long, 256 short) get cached twiddles; any other size
/// (tests) builds a fresh set.
fn mdct_twiddles(n: usize) -> Option<&'static MdctTwiddles> {
    static LONG: OnceLock<MdctTwiddles> = OnceLock::new();
    static SHORT: OnceLock<MdctTwiddles> = OnceLock::new();
    match n {
        2048 => Some(LONG.get_or_init(|| MdctTwiddles::build(2048))),
        256 => Some(SHORT.get_or_init(|| MdctTwiddles::build(256))),
        _ => None,
    }
}

/// Fast forward MDCT (O(N log N)) — numerically matches the direct [`mdct`] (kept as
/// the scalar oracle). The textbook N/4 method: fold the N real inputs to L=N/2 (TDAC),
/// pack into M=N/4 complex with a pre-rotation, one **M-point** FFT, then a post-rotation
/// unpacks the N/2 coefficients. 4× fewer FFT points than a length-N transform. `N` is a
/// power of two (≥4); time samples → `N/2` coeffs.
pub fn mdct_fast(x: &[f32]) -> Vec<f32> {
    let n = x.len();
    if n < 4 {
        return mdct(x); // tiny sizes: fall back to the direct oracle
    }
    let (l, m) = (n / 2, n / 4);
    let owned;
    let tw = match mdct_twiddles(n) {
        Some(t) => t,
        None => {
            owned = MdctTwiddles::build(n);
            &owned
        }
    };
    // TDAC fold x[0..N] → y[mm] (computed on the fly), for the two indices each p needs.
    let (l2, l32) = (l / 2, 3 * l / 2);
    let fold = |mm: usize| -> f64 {
        if mm < l2 {
            -(x[l32 - 1 - mm] as f64) - (x[mm + l32] as f64)
        } else {
            (x[mm - l2] as f64) - (x[l32 - 1 - mm] as f64)
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
    // Post-rotate W = V·e^{-iπp/L}; X[2p]=Re(W), X[L-1-2p]=-Im(W); output scaled ×2.
    let mut out = vec![0f32; l];
    for p in 0..m {
        let (vr, vi) = (re[p], im[p]);
        out[2 * p] = (2.0 * (vr * tw.post_c[p] + vi * tw.post_s[p])) as f32;
        out[l - 1 - 2 * p] = (2.0 * (vr * tw.post_s[p] - vi * tw.post_c[p])) as f32;
    }
    out
}

/// AAC sine analysis/synthesis window: `w[n] = sin(π/N·(n+½))`.
pub fn sine_window(n: usize) -> Vec<f32> {
    (0..n)
        .map(|i| (PI / n as f64 * (i as f64 + 0.5)).sin() as f32)
        .collect()
}

/// Modified Bessel function of the first kind, order 0 (series form).
fn bessel_i0(x: f64) -> f64 {
    let mut sum = 1.0;
    let mut term = 1.0;
    let half_x = x / 2.0;
    for k in 1..50 {
        term *= (half_x / k as f64).powi(2);
        sum += term;
        if term < 1e-12 * sum {
            break;
        }
    }
    sum
}

/// Kaiser-Bessel-derived window of length `n` (ISO 14496-3 §4.6.11.2.4).
/// `alpha` is 4 for long blocks (N=2048), 6 for short (N=256).
pub fn kbd_window(n: usize, alpha: f64) -> Vec<f32> {
    let half = n / 2;
    // Cumulative Kaiser window over the first half.
    let mut cumulative = vec![0f64; half + 1];
    let mut running = 0.0;
    for p in 0..=half {
        let r = 2.0 * p as f64 / half as f64 - 1.0; // -1..1
        running += bessel_i0(PI * alpha * (1.0 - r * r).max(0.0).sqrt());
        cumulative[p] = running;
    }
    let total = cumulative[half];
    let mut w = vec![0f32; n];
    for i in 0..half {
        w[i] = (cumulative[i] / total).sqrt() as f32;
        w[n - 1 - i] = w[i];
    }
    w
}

#[cfg(test)]
mod tests {
    use super::*;

    /// The fast FFT-based MDCT must match the direct O(N²) oracle at both block
    /// sizes, on raw and encoder-scaled (×32768) input.
    #[test]
    fn mdct_fast_matches_direct() {
        for &n in &[256usize, 2048] {
            for &scale in &[1.0f32, 32768.0] {
                let x: Vec<f32> = (0..n)
                    .map(|i| {
                        scale
                            * (0.3 * (i as f64 * 0.021).sin() + 0.2 * (i as f64 * 0.005).cos())
                                as f32
                    })
                    .collect();
                let (a, b) = (mdct(&x), mdct_fast(&x));
                let tol = 2e-3 * scale.max(1.0);
                for k in 0..n / 2 {
                    assert!(
                        (a[k] - b[k]).abs() < tol,
                        "n={n} scale={scale} k={k}: {} vs {}",
                        a[k],
                        b[k]
                    );
                }
            }
        }
    }

    #[test]
    fn dequant_matches_spec_curve() {
        assert_eq!(dequant(0), 0.0);
        assert_eq!(dequant(1), 1.0);
        assert!((dequant(2) - 2f32.powf(4.0 / 3.0)).abs() < 1e-5);
        assert!((dequant(-3) + 3f32.powf(4.0 / 3.0)).abs() < 1e-4);
        // Monotonic and sign-preserving.
        assert!(dequant(10) > dequant(9));
        assert!(dequant(-5) < 0.0);
    }

    #[test]
    fn sf_gain_is_quarter_db_steps() {
        assert!((sf_gain(100) - 1.0).abs() < 1e-6); // sf 100 → unity
        assert!((sf_gain(104) - 2.0).abs() < 1e-5); // +4 → ×2
        assert!((sf_gain(96) - 0.5).abs() < 1e-6); // −4 → ×0.5
    }

    /// w[n]² + w[n+N/2]² = 1 is the Princen-Bradley condition both AAC windows
    /// must satisfy for the filterbank to reconstruct perfectly.
    fn assert_princen_bradley(w: &[f32]) {
        let half = w.len() / 2;
        for n in 0..half {
            let s = w[n] * w[n] + w[n + half] * w[n + half];
            assert!((s - 1.0).abs() < 1e-4, "PB violated at {n}: {s}");
        }
    }

    #[test]
    fn sine_window_satisfies_princen_bradley() {
        assert_princen_bradley(&sine_window(256));
    }

    #[test]
    fn kbd_window_satisfies_princen_bradley() {
        assert_princen_bradley(&kbd_window(256, 4.0));
        assert_princen_bradley(&kbd_window(256, 6.0));
    }

    /// The decisive correctness check: windowed MDCT → IMDCT → windowed
    /// overlap-add reconstructs the original signal in the steady-state region
    /// (TDAC). If the IMDCT/window math is wrong, this fails.
    #[test]
    fn mdct_imdct_overlap_add_perfectly_reconstructs() {
        let n = 64usize;
        let half = n / 2;
        let w = sine_window(n);
        // A deterministic, broadband test signal.
        let len = 4 * n;
        let signal: Vec<f32> = (0..len)
            .map(|i| (0.3 * (i as f64 * 0.21).sin() + 0.2 * (i as f64 * 0.05).cos()) as f32)
            .collect();

        let mut out = vec![0f32; len];
        let mut p = 0;
        while p + n <= len {
            let framed: Vec<f32> = (0..n).map(|i| signal[p + i] * w[i]).collect();
            let synth = imdct(&mdct(&framed));
            for i in 0..n {
                out[p + i] += synth[i] * w[i];
            }
            p += half;
        }

        // Interior (≥ one full frame from each edge) must match the input.
        for i in n..(len - n) {
            assert!(
                (out[i] - signal[i]).abs() < 1e-3,
                "reconstruction error at {i}: {} vs {}",
                out[i],
                signal[i]
            );
        }
    }
}
