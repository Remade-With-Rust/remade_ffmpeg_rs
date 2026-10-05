//! AAC numerical core: inverse quantization and the IMDCT synthesis filterbank
//! (windows + overlap-add). These must match ISO 14496-3 exactly — they are the
//! parts where any deviation produces audibly wrong / incompatible output, so
//! they're written straight from the spec and checked by the MDCT↔IMDCT
//! perfect-reconstruction (TDAC) property.
//!
//! The direct O(N²) transforms are kept as oracles; the decoder runs the fast
//! paths (an f32 radix-2 DCT-IV for power-of-two lengths, the mixed-radix plan
//! for 960/480/120).

// These primitives are exercised by the tests now and wired into the decode
// path in the spectral/synthesis stages; allow until then.
#![allow(dead_code)]

use std::f64::consts::PI;
use std::mem::MaybeUninit;
use std::sync::OnceLock;

/// Inverse quantization of a spectral coefficient (ISO 14496-3 §10.3):
/// `x = sign(q) · |q|^(4/3)`. The scalefactor gain is applied separately.
pub fn dequant(q: i32) -> f32 {
    // `|q|^(4/3)` for every magnitude a legal stream can produce (escape codes
    // reach 8191, pulses add up to 15), precomputed with the same f64 `powf` and
    // stored as f32. `(±m) as f32 == ±(m as f32)`, so the lookup is bit-identical
    // to evaluating the formula — which was a libm call per coefficient and ~40%
    // of LC decode. Larger (corrupt-stream) magnitudes take the formula.
    dequant_with(pow43_table(), q)
}

/// The `|q|^(4/3)` table, for callers that hoist it out of a coefficient loop.
pub fn pow43_table() -> &'static [f32] {
    static POW43: OnceLock<Vec<f32>> = OnceLock::new();
    POW43.get_or_init(|| (0..POW43_LEN).map(|i| dequant_formula(i as i32)).collect())
}

/// [`dequant`] against an already-fetched [`pow43_table`].
#[inline(always)]
pub fn dequant_with(t: &[f32], q: i32) -> f32 {
    match t.get(q.unsigned_abs() as usize) {
        Some(&m) => {
            if q < 0 {
                -m
            } else {
                m
            }
        }
        None => dequant_formula(q),
    }
}

/// Magnitudes covered by the `dequant` table.
const POW43_LEN: usize = 8192 + 16;

/// The defining formula (the table's source and oracle).
fn dequant_formula(q: i32) -> f32 {
    let m = f64::from(q.unsigned_abs()).powf(4.0 / 3.0);
    (f64::from(q.signum()) * m) as f32
}

/// Scalefactor gain `2^(0.25·(sf − 100))`, the per-band multiplier applied to
/// dequantized coefficients.
pub fn sf_gain(sf: i32) -> f32 {
    // Regular scalefactors span 0..=255: tabulated with the same formula (a libm
    // `pow` per band otherwise); anything else takes the formula.
    static T: OnceLock<[f32; 256]> = OnceLock::new();
    let t = T.get_or_init(|| std::array::from_fn(|i| sf_gain_formula(i as i32)));
    match t.get(sf as usize) {
        Some(&g) if sf >= 0 => g,
        _ => sf_gain_formula(sf),
    }
}

/// The defining formula (the table's source and oracle).
fn sf_gain_formula(sf: i32) -> f32 {
    2f64.powf(0.25 * (f64::from(sf) - 100.0)) as f32
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
            acc += f64::from(s) * (a * (k as f64 + 0.5)).cos();
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
            acc += f64::from(t) * (w * (i as f64 + n0) * (k as f64 + 0.5)).cos();
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
    fn build(n: usize) -> Self {
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
        Self {
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
    let tw = if let Some(t) = mdct_twiddles(n) {
        t
    } else {
        owned = MdctTwiddles::build(n);
        &owned
    };
    // TDAC fold x[0..N] → y[mm] (computed on the fly), for the two indices each p needs.
    let (l2, l32) = (l / 2, 3 * l / 2);
    let fold = |mm: usize| -> f64 {
        if mm < l2 {
            -f64::from(x[l32 - 1 - mm]) - f64::from(x[mm + l32])
        } else {
            f64::from(x[mm - l2]) - f64::from(x[l32 - 1 - mm])
        }
    };
    // Pack + pre-rotate into M complex: v[p] = (y[2p] + i·y[L-1-2p])·e^{-iπ(4p+1)/4L}.
    // Every element is computed, so the FFT buffers are filled from fresh
    // capacity rather than zero-filled and overwritten.
    let (mut re, mut im) = (Vec::<f64>::with_capacity(m), Vec::<f64>::with_capacity(m));
    let (rp, ip) = (re.spare_capacity_mut(), im.spare_capacity_mut());
    for p in 0..m {
        let (yr, yi) = (fold(2 * p), fold(l - 1 - 2 * p));
        rp[p].write(yr * tw.pre_c[p] + yi * tw.pre_s[p]);
        ip[p].write(yi * tw.pre_c[p] - yr * tw.pre_s[p]);
    }
    // SAFETY: the loop wrote all `m` elements of both buffers.
    unsafe {
        re.set_len(m);
        im.set_len(m);
    }
    fft(&mut re, &mut im, &tw.fft_c, &tw.fft_s);
    // Post-rotate W = V·e^{-iπp/L}; X[2p]=Re(W), X[L-1-2p]=-Im(W); output scaled ×2.
    let mut out = Vec::<f32>::with_capacity(l);
    let op = out.spare_capacity_mut();
    for p in 0..m {
        let (vr, vi) = (re[p], im[p]);
        op[2 * p].write((2.0 * (vr * tw.post_c[p] + vi * tw.post_s[p])) as f32);
        op[l - 1 - 2 * p].write((2.0 * (vr * tw.post_s[p] - vi * tw.post_c[p])) as f32);
    }
    // SAFETY: L = 2M; 2p covers the even indices and L-1-2p the odd ones, so
    // the loop wrote all `l` elements.
    unsafe { out.set_len(l) };
    out
}

/// Fast IMDCT (O(N log N)) — numerically matches the direct [`imdct`] (kept as the
/// scalar oracle). `N/2` coefficients → `N` time samples.
///
/// Derivation: the MDCT factors as `C = D·F`, where `F` is the TDAC fold
/// (`N → L = N/2`, the `fold` closure in [`mdct_fast`]) and `D` is the `L`-point
/// DCT-IV. The spec IMDCT is `(2/N)·Cᵀ = (2/N)·Fᵀ·D` (DCT-IV is symmetric), so it is
/// the SAME pre-rotation / `N/4`-point FFT / post-rotation core run on the
/// coefficients (which yields `2·D·X`), followed by the transpose of the fold — an
/// unfold that writes every output sample exactly once.
pub fn imdct_fast(spec: &[f32]) -> Vec<f32> {
    let l = spec.len();
    let n = l * 2;
    if n < 8 {
        return imdct(spec);
    }
    let m = n / 4;
    let owned;
    let tw = if let Some(t) = mdct_twiddles(n) {
        t
    } else {
        owned = MdctTwiddles::build(n);
        &owned
    };
    let (mut re, mut im) = (vec![0f64; m], vec![0f64; m]);
    for p in 0..m {
        let (yr, yi) = (f64::from(spec[2 * p]), f64::from(spec[l - 1 - 2 * p]));
        re[p] = yr * tw.pre_c[p] + yi * tw.pre_s[p];
        im[p] = yi * tw.pre_c[p] - yr * tw.pre_s[p];
    }
    fft(&mut re, &mut im, &tw.fft_c, &tw.fft_s);
    // z = D·X (the core produces 2·D·X; the ×2 and the spec's 2/N fold into 1/N).
    let scale = 1.0 / n as f64;
    let mut z = vec![0f64; l];
    for p in 0..m {
        let (vr, vi) = (re[p], im[p]);
        z[2 * p] = 2.0 * (vr * tw.post_c[p] + vi * tw.post_s[p]) * scale;
        z[l - 1 - 2 * p] = 2.0 * (vr * tw.post_s[p] - vi * tw.post_c[p]) * scale;
    }
    // Unfold = Fᵀ: the fold read y[mm] from these x positions, so scatter back.
    let (l2, l32) = (l / 2, 3 * l / 2);
    let mut out = vec![0f32; n];
    for (mm, &v) in z.iter().enumerate() {
        if mm < l2 {
            out[l32 - 1 - mm] = -v as f32;
            out[mm + l32] = -v as f32;
        } else {
            out[mm - l2] = v as f32;
            out[l32 - 1 - mm] = -v as f32;
        }
    }
    out
}

// ---------------------------------------------------------------------------
// Mixed-radix (2/3/5) transforms for the non-power-of-two frame lengths
// (960/120 for GA, 480 for LD/ELD) — and the half-length IMDCT the decoder's
// synthesis works in.
// ---------------------------------------------------------------------------

/// A complex FFT plan for any size whose prime factors are 2, 3 and 5.
struct MixedFft {
    n: usize,
    /// e^{-2πi t/n}, t in 0..n.
    w: Vec<(f64, f64)>,
}

impl MixedFft {
    fn new(n: usize) -> Self {
        let w = (0..n)
            .map(|t| {
                let a = -2.0 * PI * t as f64 / n as f64;
                (a.cos(), a.sin())
            })
            .collect();
        Self { n, w }
    }

    fn radix(n: usize) -> usize {
        if n % 4 == 0 {
            4
        } else if n % 2 == 0 {
            2
        } else if n % 3 == 0 {
            3
        } else if n % 5 == 0 {
            5
        } else {
            n // prime remainder: direct DFT
        }
    }

    /// out[k] = `Σ_j` x[off + j·stride] · e^{-2πi jk/n}, recursive decimation in time.
    fn rec(&self, x: &[(f64, f64)], off: usize, stride: usize, n: usize, out: &mut [(f64, f64)]) {
        if n == 1 {
            out[0] = x[off];
            return;
        }
        let r = Self::radix(n);
        let m = n / r;
        for j1 in 0..r {
            self.rec(
                x,
                off + j1 * stride,
                stride * r,
                m,
                &mut out[j1 * m..(j1 + 1) * m],
            );
        }
        let step = self.n / n; // W_n^x = W_N^(x·step)
        let mut t = [(0f64, 0f64); 5];
        let mut tmp = vec![(0f64, 0f64); if r > 5 { r } else { 0 }];
        for k1 in 0..m {
            let buf: &mut [(f64, f64)] = if r <= 5 { &mut t[..r] } else { &mut tmp[..] };
            for (j1, b) in buf.iter_mut().enumerate() {
                let (wr, wi) = self.w[(j1 * k1 * step) % self.n];
                let (vr, vi) = out[j1 * m + k1];
                *b = (vr * wr - vi * wi, vr * wi + vi * wr);
            }
            for k2 in 0..r {
                let (mut sr, mut si) = (0f64, 0f64);
                for (j1, &(br, bi)) in buf.iter().enumerate() {
                    let (wr, wi) = self.w[((j1 * k2) % r) * (self.n / r)];
                    sr += br * wr - bi * wi;
                    si += br * wi + bi * wr;
                }
                out[k1 + m * k2] = (sr, si);
            }
        }
    }

    fn forward(&self, x: &[(f64, f64)]) -> Vec<(f64, f64)> {
        let mut out = vec![(0f64, 0f64); self.n];
        self.rec(x, 0, 1, self.n, &mut out);
        out
    }
}

/// Twiddles + FFT for an `L`-coefficient DCT-IV core (the MDCT/IMDCT engine).
struct Dct4Plan {
    l: usize,
    pre: Vec<(f64, f64)>,
    post: Vec<(f64, f64)>,
    fft: MixedFft,
}

impl Dct4Plan {
    fn new(l: usize) -> Self {
        let m = l / 2;
        let pre = (0..m)
            .map(|p| {
                let th = PI * (4.0 * p as f64 + 1.0) / (4.0 * l as f64);
                (th.cos(), th.sin())
            })
            .collect();
        let post = (0..m)
            .map(|p| {
                let ph = PI * p as f64 / l as f64;
                (ph.cos(), ph.sin())
            })
            .collect();
        Self {
            l,
            pre,
            post,
            fft: MixedFft::new(m),
        }
    }

    /// Returns `2·D·y` (D = the L-point DCT-IV), exactly the core `mdct_fast` uses.
    fn run(&self, y: impl Fn(usize) -> f64) -> Vec<f64> {
        let (l, m) = (self.l, self.l / 2);
        let v: Vec<(f64, f64)> = (0..m)
            .map(|p| {
                let (yr, yi) = (y(2 * p), y(l - 1 - 2 * p));
                let (c, s) = self.pre[p];
                (yr * c + yi * s, yi * c - yr * s)
            })
            .collect();
        let f = self.fft.forward(&v);
        let mut z = vec![0f64; l];
        for p in 0..m {
            let (vr, vi) = f[p];
            let (c, s) = self.post[p];
            z[2 * p] = 2.0 * (vr * c + vi * s);
            z[l - 1 - 2 * p] = 2.0 * (vr * s - vi * c);
        }
        z
    }
}

/// Run `f` with the mixed-radix DCT-IV plan for length `l`. The lengths the
/// codec uses (960/120-sample frames, 480 for LD/ELD, and their halves) are
/// cached for the life of the process in a fixed table; any other length is
/// built for the call and dropped. (A map of leaked plans keyed by any `l`
/// would grow without bound if a caller ever fed it unbounded lengths.)
fn with_dct4_plan<R>(l: usize, f: impl FnOnce(&Dct4Plan) -> R) -> R {
    const CACHED: [usize; 6] = [60, 120, 240, 480, 960, 1920];
    static PLANS: [OnceLock<Dct4Plan>; CACHED.len()] = [const { OnceLock::new() }; CACHED.len()];
    match CACHED.iter().position(|&c| c == l) {
        Some(i) => f(PLANS[i].get_or_init(|| Dct4Plan::new(l))),
        None => f(&Dct4Plan::new(l)),
    }
}

/// Half-length IMDCT for any even `L` with 2/3/5 factors: `L` coefficients →
/// the MIDDLE `L` samples of the `2L`-sample spec IMDCT (with the spec's `2/N`
/// scale), i.e. samples `L/2 .. 3L/2`. This is the buffer FFmpeg's synthesis
/// windows from; the outer quarters follow by TDAC symmetry. `gain` scales the
/// result (the decoder passes 1/32768 to land in float [-1, 1]).
pub fn imdct_half(spec: &[f32], out: &mut [f32], gain: f64) {
    let l = spec.len();
    if let Some(plan) = pow2_dct4(l) {
        return plan.imdct_half(spec, out, gain as f32);
    }
    let z = with_dct4_plan(l, |plan| plan.run(|i| f64::from(spec[i])));
    // z = 2·D·X; the spec IMDCT is (2/N)·Fᵀ·D·X with N = 2L, so scale by 1/(2L)·…
    // middle[k] = −(D·X)[L−1−k]·(2/N)·…  — see `imdct_fast`'s unfold.
    let scale = gain / (2 * l) as f64;
    for k in 0..l {
        out[k] = (-z[l - 1 - k] * scale) as f32;
    }
}

/// An in-place iterative radix-2 complex FFT in f32: `sign` -1 computes
/// `Σ x·e^{-2πi jk/n}`, +1 the unscaled inverse.
///
/// It carries every IMDCT and both SBR QMF banks (~46% of HE-AAC decode CPU), so
/// the butterfly stages have SIMD twins: AVX on x86-64 (runtime-detected, four
/// butterflies per 256-bit op) and NEON on aarch64 (baseline, two per op). Both
/// perform exactly the scalar products and sums (no FMA), so all three paths are
/// **bit-identical** (`fft_simd_matches_scalar`).
pub struct Radix2Fft {
    n: usize,
    /// The bit-reversal permutation as its swaps (`i < rev(i)` only), so the
    /// permutation costs no compare per element.
    swaps: Vec<(u16, u16)>,
    /// `rev[i]`: where input `i` sits after the permutation. A producer that
    /// writes its element `i` to `rev[i]` hands [`Radix2Fft::run_bitrev`] the
    /// permuted order directly, and the swap pass (a pure data move) vanishes.
    rev: Vec<u16>,
    /// Per-stage twiddles laid out contiguously: the stage with half-length `h`
    /// (h = 2, 4, ..., n/2) holds `w[j·n/(2h)]` for `j < h` at offset `h - 2`, so a
    /// vector of butterflies loads its twiddles in one go.
    stw: Vec<[f32; 2]>,
    /// The AVX twin is usable (detected once, when the plan is built).
    #[cfg(all(feature = "simd", target_arch = "x86_64"))]
    avx: bool,
}

impl Radix2Fft {
    pub(crate) fn new(n: usize, sign: f64) -> Self {
        assert!(n.is_power_of_two() && n >= 2);
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
                let a = sign * 2.0 * PI * t as f64 / n as f64;
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
            rev,
            stw,
            #[cfg(all(feature = "simd", target_arch = "x86_64"))]
            avx: std::is_x86_feature_detected!("avx"),
        }
    }

    pub(crate) fn run(&self, buf: &mut [[f32; 2]]) {
        self.run_from(buf, false);
    }

    /// [`Self::run`] on input already in bit-reversed order (element `i` at
    /// `bitrev()[i]`): the same transform without the permutation pass.
    pub(crate) fn run_bitrev(&self, buf: &mut [[f32; 2]]) {
        self.run_from(buf, true);
    }

    /// The bit-reversal table: input `i` belongs at `bitrev()[i]`.
    #[inline]
    pub(crate) fn bitrev(&self) -> &[u16] {
        &self.rev
    }

    #[inline]
    fn run_from(&self, buf: &mut [[f32; 2]], permuted: bool) {
        let buf = &mut buf[..self.n];
        #[cfg(all(feature = "simd", target_arch = "x86_64"))]
        if self.avx {
            crate::prof::count(crate::prof::Kernel::Fft, true, self.n);
            // SAFETY: AVX detected at construction; `buf` is exactly `n` long and
            // `stw` holds every stage's twiddles (see `run_avx`).
            unsafe { self.run_avx(buf, permuted) };
            return;
        }
        #[cfg(all(feature = "simd", target_arch = "aarch64"))]
        {
            crate::prof::count(crate::prof::Kernel::Fft, true, self.n);
            // SAFETY: NEON is baseline on aarch64; same bounds as `run_avx`.
            unsafe { self.run_neon(buf, permuted) };
            return;
        }
        // On SIMD targets a twin above returns first; this is the scalar fallback.
        #[allow(unreachable_code)]
        {
            crate::prof::count(crate::prof::Kernel::Fft, false, self.n);
            self.run_scalar_from(buf, permuted);
        }
    }

    /// Bit-reversal permutation (unless the input is already `permuted`) plus
    /// the twiddle-free first stage (shared by every path).
    #[inline(always)]
    fn permute_and_first_stage(&self, buf: &mut [[f32; 2]], permuted: bool) {
        if !permuted {
            for &(i, j) in &self.swaps {
                buf.swap(i as usize, j as usize);
            }
        }
        for pair in buf.chunks_exact_mut(2) {
            let (a, b) = (pair[0], pair[1]);
            pair[0] = [a[0] + b[0], a[1] + b[1]];
            pair[1] = [a[0] - b[0], a[1] - b[1]];
        }
    }

    /// One radix-2 stage of half-length `half`, scalar.
    #[inline(always)]
    fn stage_scalar(&self, buf: &mut [[f32; 2]], half: usize) {
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
    }

    /// The scalar reference (and the fallback on CPUs without the twin ISA).
    pub(crate) fn run_scalar(&self, buf: &mut [[f32; 2]]) {
        self.run_scalar_from(buf, false);
    }

    fn run_scalar_from(&self, buf: &mut [[f32; 2]], permuted: bool) {
        let buf = &mut buf[..self.n];
        self.permute_and_first_stage(buf, permuted);
        let mut half = 2;
        while half < self.n {
            self.stage_scalar(buf, half);
            half *= 2;
        }
    }

    /// AVX twin: stages with `half >= 4` run four butterflies per 256-bit op; the
    /// complex multiply is `b·Re(w) -/+ swap(b)·Im(w)` via `addsub`, the same two
    /// products and one sum per lane as the scalar path (bit-identical).
    ///
    /// # Safety
    /// AVX must be available, and `buf.len() == self.n`. Each vector touches
    /// `buf[s + j .. s + j + 4]` and `buf[s + j + half .. +4]` with
    /// `s + 2·half <= n` and `j + 4 <= half`, and twiddles `stw[half - 2 + j .. +4]`
    /// with `2·half - 2 <= stw.len() = n - 2`. The `half == 2` stage reads and
    /// writes `buf[s..s+4]` for `s + 4 <= n` and the twiddles `stw[0..2]`.
    #[cfg(all(feature = "simd", target_arch = "x86_64"))]
    #[target_feature(enable = "avx")]
    unsafe fn run_avx(&self, buf: &mut [[f32; 2]], permuted: bool) {
        use std::arch::x86_64::*;
        let n = self.n;
        let p = buf.as_mut_ptr().cast::<f32>();
        if n >= 4 {
            if !permuted {
                for &(i, j) in &self.swaps {
                    buf.swap(i as usize, j as usize);
                }
            }
            // First stage, four points per op: swapping the two complex values
            // of each 128-bit lane gives y = [a1, a0, ...]; x + y is a0 + a1 in
            // the even slot and y - x is a0 - a1 in the odd one - the scalar's
            // exact sums.
            let mut s = 0;
            while s < n {
                let x = _mm256_loadu_ps(p.add(2 * s));
                let y = _mm256_castpd_ps(_mm256_permute_pd::<0b0101>(_mm256_castps_pd(x)));
                let r = _mm256_blend_ps::<0b1100_1100>(_mm256_add_ps(x, y), _mm256_sub_ps(y, x));
                _mm256_storeu_ps(p.add(2 * s), r);
                s += 4;
            }
        } else {
            self.permute_and_first_stage(buf, permuted);
        }
        let mut half = 2;
        while half < n {
            if half == 2 {
                // Two butterflies per 128-bit op; both twiddles are the same in
                // every block, so the twiddle vector is loaded once.
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

    /// NEON twin: two butterflies per 128-bit op from `half >= 2`; the even-lane
    /// sign of the complex multiply is applied by flipping the sign bit (exact),
    /// so the sums match the scalar path bit for bit.
    ///
    /// # Safety
    /// `buf.len() == self.n`; bounds as for `run_avx` with two-wide vectors.
    #[cfg(all(feature = "simd", target_arch = "aarch64"))]
    #[target_feature(enable = "neon")]
    unsafe fn run_neon(&self, buf: &mut [[f32; 2]], permuted: bool) {
        use std::arch::aarch64::*;
        let n = self.n;
        self.permute_and_first_stage(buf, permuted);
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

/// The DCT-IV core for power-of-two lengths in f32: pre-rotation, an `L/2`-point
/// FFT, post-rotation — the same factorisation as [`Dct4Plan`], allocation-free.
pub struct Pow2Dct4 {
    l: usize,
    pre: Vec<[f32; 2]>,
    post: Vec<[f32; 2]>,
    fft: Radix2Fft,
}

impl Pow2Dct4 {
    fn new(l: usize) -> Self {
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
            fft: Radix2Fft::new(m, -1.0),
        }
    }

    /// Pre-rotation: `v[p] = (x[2p] + i·x[L-1-2p])·e^{-iθ_p}`. Written over
    /// `chunks_exact` / `rchunks_exact` views (the even entries from the front,
    /// the odd ones mirrored from the back) so there is no index arithmetic or
    /// bounds check per element and LLVM vectorises the stride-2 and reversed
    /// accesses.
    #[inline(always)]
    fn rotate_in(&self, x: &[f32], v: &mut [[f32; 2]]) {
        let x = &x[..self.l];
        for (((vp, a), b), &[c, s]) in v
            .iter_mut()
            .zip(x.chunks_exact(2))
            .zip(x.rchunks_exact(2))
            .zip(&self.pre)
        {
            let (yr, yi) = (a[0], b[1]);
            *vp = [yr * c + yi * s, yi * c - yr * s];
        }
    }

    /// [`Self::rotate_in`] into scratch that need not be initialised: writes
    /// `v[..L/2]` and returns it as initialised, so callers skip a zero fill
    /// whose every element this overwrites. A separate body on purpose: routing
    /// `dct4` (the QMF synthesis, ~96K calls on HE v1) through this form cost
    /// HE v1 +1.66M / HE v2 +0.84M while LC saved 970K.
    #[inline(always)]
    fn rotate_in_uninit<'a>(
        &self,
        x: &[f32],
        v: &'a mut [MaybeUninit<[f32; 2]>],
    ) -> &'a mut [[f32; 2]] {
        let m = self.l / 2;
        let v = &mut v[..m];
        let x = &x[..self.l];
        for (((vp, a), b), &[c, s]) in v
            .iter_mut()
            .zip(x.chunks_exact(2))
            .zip(x.rchunks_exact(2))
            .zip(&self.pre)
        {
            let (yr, yi) = (a[0], b[1]);
            vp.write([yr * c + yi * s, yi * c - yr * s]);
        }
        // SAFETY: `pre` has `m` entries and `x` gives `m` pairs, so the loop
        // wrote every element of `v`.
        unsafe { &mut *(std::ptr::from_mut::<[MaybeUninit<[f32; 2]>]>(v) as *mut [[f32; 2]]) }
    }

    /// The unscaled DCT-IV: `out[m] = Σ x[k]·cos(π/L·(m+½)(k+½))`, using
    /// `scratch` (at least L/2 entries) for the FFT.
    pub(crate) fn dct4(&self, x: &[f32], out: &mut [f32], scratch: &mut [[f32; 2]]) {
        let m = self.l / 2;
        let v = &mut scratch[..m];
        self.rotate_in(x, v);
        self.fft.run(v);
        // out[2q] comes from v[q]; out[2q+1] = out[L-1-2p] from v[p], p = m-1-q.
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

    /// As [`imdct_half`]: `out[k] = -(D·X)[L-1-k]·gain/L`.
    fn imdct_half(&self, spec: &[f32], out: &mut [f32], gain: f32) {
        // Stack scratch, left uninitialised: `rotate_in_uninit` writes all of
        // `buf[..L/2]` before anything reads it, so a zero fill (once ~4K Ir per
        // call) was pure waste. 1024 covers every plan (`pow2_dct4` builds
        // L <= 2048); a longer one would panic on the slice, not misbehave.
        let mut buf = [MaybeUninit::<[f32; 2]>::uninit(); 1024];
        self.imdct_half_in(spec, out, gain, &mut buf);
    }

    #[inline(always)]
    fn imdct_half_in(
        &self,
        spec: &[f32],
        out: &mut [f32],
        gain: f32,
        buf: &mut [MaybeUninit<[f32; 2]>],
    ) {
        let v = self.rotate_in_uninit(spec, buf);
        self.fft.run(v);
        let scale = gain / self.l as f32;
        // out[2q] = -(D·X)[L-1-2q]·s from v[q]; out[2q+1] = -(D·X)[2p]·s, p = m-1-q.
        let post = v.iter().zip(&self.post);
        for ((o, (&[vr, vi], &[c, s])), (&[ur, ui], &[uc, us])) in out[..self.l]
            .chunks_exact_mut(2)
            .zip(post.clone())
            .zip(post.rev())
        {
            o[0] = -(vr * s - vi * c) * scale;
            o[1] = -(ur * uc + ui * us) * scale;
        }
    }

    /// The pre-restructure indexed forms, kept as the bit-exact oracle.
    #[cfg(test)]
    fn dct4_reference(&self, x: &[f32], out: &mut [f32]) {
        let (l, m) = (self.l, self.l / 2);
        let mut v = vec![[0f32; 2]; m];
        for (p, vp) in v.iter_mut().enumerate() {
            let (yr, yi) = (x[2 * p], x[l - 1 - 2 * p]);
            let [c, s] = self.pre[p];
            *vp = [yr * c + yi * s, yi * c - yr * s];
        }
        self.fft.run_scalar(&mut v);
        for (p, &[vr, vi]) in v.iter().enumerate() {
            let [c, s] = self.post[p];
            out[2 * p] = vr * c + vi * s;
            out[l - 1 - 2 * p] = vr * s - vi * c;
        }
    }

    #[cfg(test)]
    fn imdct_half_reference(&self, spec: &[f32], out: &mut [f32], gain: f32) {
        let (l, m) = (self.l, self.l / 2);
        let mut v = vec![[0f32; 2]; m];
        for (p, vp) in v.iter_mut().enumerate() {
            let (yr, yi) = (spec[2 * p], spec[l - 1 - 2 * p]);
            let [c, s] = self.pre[p];
            *vp = [yr * c + yi * s, yi * c - yr * s];
        }
        self.fft.run_scalar(&mut v);
        let scale = gain / l as f32;
        for (p, &[vr, vi]) in v.iter().enumerate() {
            let [c, s] = self.post[p];
            out[l - 1 - 2 * p] = -(vr * c + vi * s) * scale;
            out[2 * p] = -(vr * s - vi * c) * scale;
        }
    }
}

/// The cached power-of-two DCT-IV plan for `l` (16..=2048), if `l` qualifies.
pub fn pow2_dct4(l: usize) -> Option<&'static Pow2Dct4> {
    static PLANS: [OnceLock<Pow2Dct4>; 12] = [const { OnceLock::new() }; 12];
    if !l.is_power_of_two() || !(16..=2048).contains(&l) {
        return None;
    }
    Some(PLANS[l.trailing_zeros() as usize].get_or_init(|| Pow2Dct4::new(l)))
}

/// Forward MDCT with the same normalisation as [`mdct`] (`2·Σ`), any 2/3/5 size.
pub fn mdct_any(x: &[f32]) -> Vec<f32> {
    let n = x.len();
    let (l, l2, l32) = (n / 2, n / 4, 3 * n / 4);
    let fold = |mm: usize| -> f64 {
        if mm < l2 {
            -f64::from(x[l32 - 1 - mm]) - f64::from(x[mm + l32])
        } else {
            f64::from(x[mm - l2]) - f64::from(x[l32 - 1 - mm])
        }
    };
    with_dct4_plan(l, |plan| plan.run(fold))
        .iter()
        .map(|&v| v as f32)
        .collect()
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
        term *= (half_x / f64::from(k)).powi(2);
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
    #[cfg_attr(miri, ignore = "too slow to interpret: over 2 minutes under Miri")]
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

    /// The fast IMDCT must match the direct O(N²) oracle at every size the codecs
    /// use (2048/256 AAC-LC, 1920/240 for 960-frame and 1024/960 for LD/ELD once
    /// they exist fall back to the oracle until they get their own core), on
    /// broadband input at the decoder's ~16-bit scale.
    #[test]
    #[cfg_attr(miri, ignore = "too slow to interpret: over 2 minutes under Miri")]
    fn imdct_fast_matches_direct() {
        for &l in &[16usize, 128, 1024] {
            let x: Vec<f32> = (0..l)
                .map(|k| {
                    let k = k as f64;
                    (8000.0 * (k * 0.37).sin() * (-k / l as f64).exp() + 300.0 * (k * 1.9).cos())
                        as f32
                })
                .collect();
            let (a, b) = (imdct(&x), imdct_fast(&x));
            let peak = a.iter().fold(0f32, |m, v| m.max(v.abs()));
            for i in 0..2 * l {
                assert!(
                    (a[i] - b[i]).abs() <= 1e-4 * peak.max(1.0),
                    "l={l} i={i}: {} vs {}",
                    a[i],
                    b[i]
                );
            }
        }
    }

    /// The half IMDCT must equal the middle half of the direct oracle for every
    /// frame length the decoder uses, power-of-two or not.
    #[test]
    #[cfg_attr(miri, ignore = "too slow to interpret: over 2 minutes under Miri")]
    fn imdct_half_matches_middle_of_direct() {
        for &l in &[1024usize, 960, 512, 480, 128, 120] {
            let x: Vec<f32> = (0..l)
                .map(|k| (5000.0 * ((k as f64) * 0.61).sin() / (1.0 + k as f64 * 0.01)) as f32)
                .collect();
            let full = imdct(&x);
            let mut half = vec![0f32; l];
            imdct_half(&x, &mut half, 1.0);
            let peak = full.iter().fold(0f32, |m, v| m.max(v.abs()));
            for k in 0..l {
                assert!(
                    (full[l / 2 + k] - half[k]).abs() <= 1e-4 * peak.max(1.0),
                    "l={l} k={k}: {} vs {}",
                    full[l / 2 + k],
                    half[k]
                );
            }
        }
    }

    #[test]
    #[cfg_attr(miri, ignore = "too slow to interpret: over 2 minutes under Miri")]
    fn mdct_any_matches_direct() {
        for &n in &[1920usize, 240, 2048] {
            let x: Vec<f32> = (0..n)
                .map(|i| ((i as f64 * 0.013).sin() * 0.7) as f32)
                .collect();
            let (a, b) = (mdct(&x), mdct_any(&x));
            for k in 0..n / 2 {
                assert!(
                    (a[k] - b[k]).abs() < 2e-3,
                    "n={n} k={k}: {} vs {}",
                    a[k],
                    b[k]
                );
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

#[cfg(test)]
mod fft_twin {
    use super::*;

    /// The FFT SIMD twin (AVX / NEON, whichever this build dispatches to) must
    /// equal the scalar reference bit for bit at every size the codec uses.
    #[test]
    fn run_bitrev_matches_run() {
        let mut seed = 0x0bad_5eedu32;
        let mut rnd = || {
            seed = seed.wrapping_mul(1_664_525).wrapping_add(1_013_904_223);
            (seed >> 8) as f32 / (1 << 23) as f32 * 2.0 - 1.0
        };
        for n in [4usize, 8, 32, 64, 512] {
            let fft = Radix2Fft::new(n, 1.0);
            let x: Vec<[f32; 2]> = (0..n).map(|_| [rnd(), rnd()]).collect();
            let mut want = x.clone();
            fft.run(&mut want);
            let mut got = vec![[0f32; 2]; n];
            for (i, &v) in x.iter().enumerate() {
                got[fft.bitrev()[i] as usize] = v;
            }
            fft.run_bitrev(&mut got);
            let bits = |b: &[[f32; 2]]| b.iter().flatten().map(|v| v.to_bits()).collect::<Vec<_>>();
            assert_eq!(bits(&got), bits(&want), "n {n}");
        }
    }

    #[test]
    fn fft_simd_matches_scalar() {
        for bits in 1..=10 {
            let n = 1usize << bits;
            for sign in [-1.0, 1.0] {
                let fft = Radix2Fft::new(n, sign);
                let mut seed = 0x1234_5678u32 ^ n as u32;
                let mut r = || {
                    seed = seed.wrapping_mul(1664525).wrapping_add(1013904223);
                    ((seed >> 8) as f32 / (1u32 << 24) as f32 - 0.5) * 65536.0
                };
                let input: Vec<[f32; 2]> = (0..n).map(|_| [r(), r()]).collect();
                let (mut want, mut got) = (input.clone(), input.clone());
                fft.run_scalar(&mut want);
                fft.run(&mut got);
                for k in 0..n {
                    for c in 0..2 {
                        assert_eq!(
                            got[k][c].to_bits(),
                            want[k][c].to_bits(),
                            "n={n} sign={sign} k={k}"
                        );
                    }
                }
            }
        }
    }
}

#[cfg(test)]
mod sf_gain_table {
    #[test]
    fn sf_gain_table_matches_formula() {
        for sf in -300..600 {
            assert_eq!(
                super::sf_gain(sf).to_bits(),
                super::sf_gain_formula(sf).to_bits(),
                "sf={sf}"
            );
        }
    }
}

#[cfg(test)]
mod dequant_table {
    /// The table lookup must equal the formula bit for bit, on both sides of the
    /// table boundary.
    #[test]
    fn dequant_table_matches_formula() {
        for q in -9000i32..=9000 {
            assert_eq!(
                super::dequant(q).to_bits(),
                super::dequant_formula(q).to_bits(),
                "q={q}"
            );
        }
        for q in [i32::MIN + 1, -100_000, 100_000, i32::MAX] {
            assert_eq!(
                super::dequant(q).to_bits(),
                super::dequant_formula(q).to_bits(),
                "q={q}"
            );
        }
    }
}

#[cfg(test)]
mod dct4_twin {
    use super::*;

    /// The restructured rotations (and the FFT twin under them) must equal the
    /// original indexed forms bit for bit.
    #[test]
    fn dct4_and_imdct_half_match_reference() {
        for bits in 4..=11 {
            let l = 1usize << bits;
            let plan = pow2_dct4(l).unwrap();
            let x: Vec<f32> = (0..l)
                .map(|i| ((i as f32 * 0.37).sin() * 12345.0) + i as f32)
                .collect();
            let (mut got, mut want) = (vec![0f32; l], vec![0f32; l]);
            plan.dct4(&x, &mut got, &mut vec![[0f32; 2]; l / 2]);
            plan.dct4_reference(&x, &mut want);
            assert!(
                got.iter()
                    .zip(&want)
                    .all(|(a, b)| a.to_bits() == b.to_bits()),
                "dct4 l={l}"
            );
            plan.imdct_half(&x, &mut got, 0.37);
            plan.imdct_half_reference(&x, &mut want, 0.37);
            assert!(
                got.iter()
                    .zip(&want)
                    .all(|(a, b)| a.to_bits() == b.to_bits()),
                "imdct_half l={l}"
            );
        }
    }
}

#[cfg(test)]
mod kernel_price {
    use super::*;

    /// Per-call cost of the transform kernels, to price a SIMD twin against the
    /// stage it lives in. `cargo test --release -- --ignored --nocapture kernel_price`
    #[test]
    #[ignore = "manual microbenchmark: prints per-call timings"]
    fn kernel_price() {
        for n in [32usize, 64, 512] {
            let fft = Radix2Fft::new(n, -1.0);
            let mut buf: Vec<[f32; 2]> = (0..n)
                .map(|i| [(i as f32).sin(), (i as f32).cos()])
                .collect();
            let iters = 2_000_000 / n;
            let mut best = f64::MAX;
            for _ in 0..7 {
                let t = std::time::Instant::now();
                for _ in 0..iters {
                    fft.run(std::hint::black_box(&mut buf));
                }
                best = best.min(t.elapsed().as_secs_f64() / iters as f64);
            }
            eprintln!("Radix2Fft::run n={n:4}: {:8.1} ns/call", best * 1e9);
        }
        for len in [64usize, 512] {
            let (src0, src1): (Vec<f32>, Vec<f32>) = (
                (0..len).map(|i| i as f32).collect(),
                (0..len).map(|i| -(i as f32)).collect(),
            );
            let win: Vec<f32> = (0..2 * len).map(|i| (i as f32 * 0.01).sin()).collect();
            let mut dst = vec![0f32; 2 * len];
            let iters = 4_000_000 / len;
            let mut best = f64::MAX;
            for _ in 0..7 {
                let t = std::time::Instant::now();
                for _ in 0..iters {
                    crate::decode::synth::fmul_window(
                        std::hint::black_box(&mut dst),
                        &src0,
                        &src1,
                        &win,
                        len,
                    );
                }
                best = best.min(t.elapsed().as_secs_f64() / iters as f64);
            }
            eprintln!("fmul_window len={len:4}: {:8.1} ns/call", best * 1e9);
        }
        for l in [64usize, 512, 1024] {
            let plan = pow2_dct4(l).unwrap();
            let x: Vec<f32> = (0..l).map(|i| (i as f32 * 0.3).sin()).collect();
            let mut out = vec![0f32; l];
            let iters = 2_000_000 / l;
            let mut best = f64::MAX;
            for _ in 0..7 {
                let t = std::time::Instant::now();
                for _ in 0..iters {
                    plan.imdct_half(std::hint::black_box(&x), &mut out, 1.0);
                }
                best = best.min(t.elapsed().as_secs_f64() / iters as f64);
            }
            eprintln!(
                "Pow2Dct4::imdct_half l={l:4}: {:8.1} ns/call (incl. FFT l/2)",
                best * 1e9
            );
        }
    }
}
