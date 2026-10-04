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
}

fn plans() -> &'static Plans {
    static P: OnceLock<Plans> = OnceLock::new();
    P.get_or_init(|| {
        let tw = |a: f64, scale: f64| [(a.cos() * scale) as f32, (a.sin() * scale) as f32];
        Plans {
            fft64: Radix2Fft::new(64, 1.0),
            dct64: pow2_dct4(64).expect("64 is a power of two"),
            ana_pre: (0..64).map(|n| tw(PI * (2.0 * n as f64 - 0.5) / 128.0, 1.0)).collect(),
            ana_post: (0..32).map(|k| tw(-PI * k as f64 / 128.0, 2.0)).collect(),
            ana_win: (0..320).map(|q| QMF_WINDOW[2 * (319 - q)]).collect(),
            ds_win: (0..320).map(|m| QMF_WINDOW[2 * m]).collect(),
        }
    })
}

#[inline]
fn cmul(a: Cpx, b: Cpx) -> Cpx {
    [a[0] * b[0] - a[1] * b[1], a[0] * b[1] + a[1] * b[0]]
}

/// QMF analysis of `2·nts` slots of 32 samples. `hist` keeps the last 288
/// input samples (time order); the core output is lifted to ±32768 scale.
pub(super) fn qmf_analysis(input: &[f32], hist: &mut [f32], w: &mut [[Cpx; 32]; 32], nts: usize) {
    let p = plans();
    let ns = 64 * nts;
    let mut buf = [0f32; 288 + 1024];
    buf[..288].copy_from_slice(hist);
    for (b, &s) in buf[288..288 + ns].iter_mut().zip(&input[..ns]) {
        *b = s * 32768.0;
    }
    let mut a = [[0f32; 2]; 64];
    let mut t = [0f32; 320];
    for (l, slot) in w.iter_mut().enumerate().take(2 * nts) {
        // With x[n] newest first (x[n] = seg[319−n]), z(n) = x(n)·c(2n) is the
        // time-ordered segment times the reversed window; u(n) = Σ_j z(n+64j).
        let seg = &buf[32 * l..32 * l + 320];
        for ((tq, &x), &c) in t.iter_mut().zip(seg).zip(&p.ana_win) {
            *tq = x * c;
        }
        for n in 0..64 {
            let u = t[319 - n] + t[255 - n] + t[191 - n] + t[127 - n] + t[63 - n];
            a[n] = [u * p.ana_pre[n][0], u * p.ana_pre[n][1]];
        }
        p.fft64.run(&mut a);
        for (k, out) in slot.iter_mut().enumerate() {
            *out = cmul(a[k], p.ana_post[k]);
        }
    }
    hist.copy_from_slice(&buf[ns..ns + 288]);
}

/// 64-band (or downsampled 32-band) QMF synthesis of `2·nts` slots. `v` is a
/// FIFO of 20 blocks of `vlen` samples (newest first) with a moving offset,
/// so a slot costs no shift: the window reads ten blocks from `off`.
///
/// The 64-band matrixing is two 64-point DCT-IVs: with `s0 = DCT-IV(Re X)` and
/// `s1 = DCT-IV((−1)^k·Im X)`, `v[i] = (s1[63−i] − s0[i])/64` and
/// `v[127−i] = (s1[63−i] + s0[i])/64`. The downsampled bank is the same kernel
/// at half rate with the upper 32 bands empty: its `v` is every other sample.
pub(super) fn qmf_synthesis(out: &mut [f32], xs: &[[Cpx; 64]], v: &mut QmfSynthState, nts: usize, ds: bool) {
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
        for k in 0..bands {
            xr[k] = slot[k][0];
            xi[k] = if k & 1 == 1 { -slot[k][1] } else { slot[k][1] };
        }
        p.dct64.dct4(&xr, &mut s0, &mut scratch);
        p.dct64.dct4(&xi, &mut s1, &mut scratch);
        for i in 0..64 {
            v64[i] = (s1[63 - i] - s0[i]) * (1.0 / 64.0);
            v64[127 - i] = (s1[63 - i] + s0[i]) * (1.0 / 64.0);
        }
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
        let vb = &v.buf[off..off + span];
        let o = &mut out[l * bands..(l + 1) * bands];
        o.iter_mut().for_each(|x| *x = 0.0);
        for i in 0..5 {
            let (va, wa) = (&vb[2 * vlen * i..][..bands], &win[2 * bands * i..][..bands]);
            let (vb2, wb) = (&vb[2 * vlen * i + 3 * bands..][..bands], &win[2 * bands * i + bands..][..bands]);
            for j in 0..bands {
                o[j] += va[j] * wa[j];
                o[j] += vb2[j] * wb[j];
            }
        }
        o.iter_mut().for_each(|x| *x *= 1.0 / 32768.0);
    }
}

/// Synthesis FIFO: room for several slots so the shift happens rarely.
#[derive(Clone)]
pub(super) struct QmfSynthState {
    buf: Vec<f32>,
    off: usize,
}

impl QmfSynthState {
    pub(super) fn new() -> QmfSynthState {
        let len = 1280 + 32 * 128;
        QmfSynthState { buf: vec![0.0; len], off: len - 1280 }
    }
}

#[cfg(test)]
mod tests {
    use super::*;

    /// The standard's direct analysis (oracle).
    fn analysis_direct(input: &[f32], hist: &mut [f64], w: &mut [[Cpx; 32]; 32], nts: usize) {
        let ns = 64 * nts;
        let mut buf = vec![0f64; 288 + ns];
        buf[..288].copy_from_slice(hist);
        for (b, &s) in buf[288..].iter_mut().zip(&input[..ns]) {
            *b = s as f64 * 32768.0;
        }
        for (l, slot) in w.iter_mut().enumerate().take(2 * nts) {
            let end = 320 + 32 * l;
            let mut u = [0f64; 64];
            for n in 0..320 {
                u[n & 63] += buf[end - 1 - n] * QMF_WINDOW[2 * n] as f64;
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
                    acc += (x[0] as f64 * a.cos() - x[1] as f64 * a.sin()) / 64.0;
                }
                v[n] = acc;
            }
            let step = if ds { 2 } else { 1 };
            for (j, o) in out[l * bands..(l + 1) * bands].iter_mut().enumerate() {
                let mut acc = 0f64;
                for i in 0..5 {
                    acc += v[2 * vlen * i + j] * QMF_WINDOW[step * (2 * bands * i + j)] as f64;
                    acc += v[2 * vlen * i + 3 * bands + j] * QMF_WINDOW[step * (2 * bands * i + bands + j)] as f64;
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
    fn fast_banks_match_the_direct_definitions() {
        let (mut hf, mut hd) = (vec![0f32; 288], vec![0f64; 288]);
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
            let e = of.iter().zip(&od).fold(0f32, |m, (a, b)| m.max((a - b).abs()));
            assert!(e < 2e-5, "synthesis error {e}");
            let (mut of, mut od) = (vec![0f32; 1024], vec![0f32; 1024]);
            qmf_synthesis(&mut of, &xs, &mut dsf, 16, true);
            synthesis_direct(&mut od, &xs, &mut dsd, 16, true);
            let e = of.iter().zip(&od).fold(0f32, |m, (a, b)| m.max((a - b).abs()));
            assert!(e < 2e-5, "downsampled synthesis error {e}");
        }
        // Subband samples are at ±32768·(window gain) scale.
        assert!(worst < 0.05, "analysis error {worst}");
    }

    /// Analysis then synthesis with nothing in between is a unity-gain 2x
    /// upsampler (64-band) or a unity-gain identity (32-band downsampled).
    #[test]
    fn qmf_round_trip_is_unity_gain() {
        let mut hist = vec![0f32; 288];
        let mut w = [[[0f32; 2]; 32]; 32];
        let mut v = QmfSynthState::new();
        let mut ds = QmfSynthState::new();
        let (mut up, mut same) = (Vec::new(), Vec::new());
        let f = 1000.0 / 22050.0;
        for frame in 0..6 {
            let input: Vec<f32> = (0..1024)
                .map(|i| (0.5 * (2.0 * PI * f * (frame * 1024 + i) as f64).sin()) as f32)
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
        let input: Vec<f32> = (0..6 * 1024).map(|i| (0.5 * (2.0 * PI * f * i as f64).sin()) as f32).collect();
        let best = (0..600)
            .map(|d| {
                let (mut s, mut e) = (0f64, 0f64);
                for t in 2048..6 * 1024 {
                    let r = input[t - d] as f64;
                    s += r * r;
                    e += (same[t] as f64 - r).powi(2);
                }
                10.0 * (s / e).log10()
            })
            .fold(f64::MIN, f64::max);
        assert!(best > 50.0, "downsampled round trip {best:.1} dB");
    }
}
