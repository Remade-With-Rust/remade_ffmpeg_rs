//! DWAA / DWAB encoding.
//!
//! This is the `exr` crate's (1.74) DWA encoder, reproduced operation for
//! operation so that files are byte-identical to the ones `exr` writes for
//! the same image on x86-64: the same channel rules, the same colour
//! transform (`y, (b - y) / 1.8556, (r - y) / 1.5747`), the same forward
//! DCT (a basis-matrix product summed in input order, which is what `exr`
//! runs on every x86-64 CPU, SSE2 and AVX2 alike), the same perceptual
//! transfer table (built with the platform's `powf`/`ln`, as `exr` builds
//! it), OpenEXR's half-float quantizer, and zlib level 9 (miniz_oxide) for
//! the unknown, DC and RLE sections with static Huffman for AC.
//!
//! OpenEXR's own encoder differs in the colour transform and the DCT, so
//! its bytes differ; any DWA reader (OpenEXR included) decodes these files.

use std::cmp::Ordering;
use std::sync::OnceLock;

use super::dwa::{classify, rule, suffix_of, Rule, Scheme, ZIGZAG};
use super::{huffman, rle, zip, BlockGeom};
use crate::error::Result;
use crate::half::{f16_to_f32, f32_to_f16};
use crate::header::SampleType;

/// The rules the writer uses (OpenEXR's and `exr`'s defaults).
fn default_rules() -> Vec<Rule> {
    use SampleType::{F16, F32, U32};
    let mut v = Vec::with_capacity(15);
    for (s, csc) in [
        ("R", 0),
        ("G", 1),
        ("B", 2),
        ("Y", -1),
        ("BY", -1),
        ("RY", -1),
    ] {
        v.push(rule(s, Scheme::Lossy, F16, csc, false));
        v.push(rule(s, Scheme::Lossy, F32, csc, false));
    }
    for ty in [U32, F16, F32] {
        v.push(rule("A", Scheme::Rle, ty, -1, false));
    }
    v
}

/// The serialized rules that match at least one channel, with the u16
/// size prefix.
fn write_rules(rules: &[Rule], geom: &BlockGeom<'_>) -> Vec<u8> {
    let mut payload = Vec::new();
    for r in rules {
        if !geom
            .chans
            .iter()
            .any(|c| r.matches(suffix_of(c.name).1, c.ty))
        {
            continue;
        }
        payload.extend_from_slice(&r.suffix);
        payload.push(0);
        let csc = (r.csc + 1) as u8;
        let scheme = match r.scheme {
            Scheme::Unknown => 0u8,
            Scheme::Lossy => 1,
            Scheme::Rle => 2,
        };
        payload.push((csc << 4) | (scheme << 2) | u8::from(r.case_insensitive));
        payload.push(match r.ty {
            SampleType::U32 => 0,
            SampleType::F16 => 1,
            SampleType::F32 => 2,
        });
    }
    let mut out = Vec::with_capacity(payload.len() + 2);
    out.extend_from_slice(&((payload.len() + 2) as u16).to_le_bytes());
    out.extend_from_slice(&payload);
    out
}

/// Linear to perceptual, as `exr` computes it at run time (with `powf` and
/// `ln`); kept for the test that pins it to the stored table.
#[cfg(test)]
fn to_nonlinear_runtime() -> Vec<u16> {
    let ln_base = 9.025_013_291_56_f32.ln();
    (0..=u16::MAX)
        .map(|bits| {
            let v = f16_to_f32(bits);
            if !v.is_finite() {
                return 0;
            }
            let sign = if v < 0.0 { -1.0 } else { 1.0 };
            let v = v.abs();
            let nl = if v <= 1.0 {
                v.powf(1.0 / 2.2)
            } else {
                1.0 + v.ln() / ln_base
            };
            f32_to_f16(sign * nl)
        })
        .collect()
}

/// Linear to perceptual. `exr` builds this table at run time; on both
/// glibc and the Windows CRT the result is entry for entry OpenEXR's stored
/// table, which this uses so the output does not depend on the platform's
/// `powf`.
fn to_nonlinear() -> &'static [u16] {
    &crate::luts::get().dwa_to_nonlinear
}

/// `exr`'s x86 forward-DCT basis: `basis[input][output]`, computed the way
/// `exr` computes it. The platform's `cosf` decides the last bit of three
/// of these (glibc and the Windows CRT disagree by one ulp), so `exr`'s DWA
/// bytes differ between platforms; computing them the same way keeps the
/// output identical to `exr`'s on each.
fn basis() -> &'static [[f32; 8]; 8] {
    static T: OnceLock<[[f32; 8]; 8]> = OnceLock::new();
    T.get_or_init(|| {
        // exr's truncated pi (OpenEXR's scalar DCT uses the same).
        #[allow(clippy::approx_constant)]
        const PI: f32 = 3.14159;
        const INV_SQRT_2: f32 = 0.707_106_77;
        let mut t = [[0.0f32; 8]; 8];
        for (input, row) in t.iter_mut().enumerate() {
            for (output, v) in row.iter_mut().enumerate() {
                let scale = if output == 0 { 0.5 * INV_SQRT_2 } else { 0.5 };
                *v = scale * (((2 * input + 1) as f32 * output as f32 * PI) / 16.0).cos();
            }
        }
        t
    })
}

/// One 8-point pass: `out[o] = sum_i in[i] * basis[i][o]`, summed from 0.0
/// in input order. `NAN_FIRST` adds `exr`'s AVX2 NaN rule (below).
#[inline(always)]
fn pass<const NAN_FIRST: bool>(b: &[[f32; 8]; 8], x: [f32; 8]) -> [f32; 8] {
    let mut out = [0.0f32; 8];
    for (i, &xi) in x.iter().enumerate() {
        for (o, v) in out.iter_mut().enumerate() {
            if NAN_FIRST && v.is_nan() {
                continue;
            }
            *v += xi * b[i][o];
        }
    }
    out
}

fn transform<const NAN_FIRST: bool>(d: &mut [f32; 64]) {
    let b = basis();
    for row in d.chunks_exact_mut(8) {
        let x: [f32; 8] = row.try_into().expect("8 values");
        row.copy_from_slice(&pass::<NAN_FIRST>(b, x));
    }
    for c in 0..8 {
        let x: [f32; 8] = std::array::from_fn(|k| d[k * 8 + c]);
        let y = pass::<NAN_FIRST>(b, x);
        for (k, v) in y.into_iter().enumerate() {
            d[k * 8 + c] = v;
        }
    }
}

/// Forward DCT, bit-identical to `exr` on AVX2 machines.
///
/// Finite blocks stay finite, and every x86 kernel `exr` has gives the same
/// bits for them. A block holding an infinity or NaN (possible only in a
/// linearly quantized channel) produces NaN sums, and which NaN payload
/// survives an addition of two NaNs depends on operand order in the machine
/// code: `exr`'s AVX2 kernel keeps the running sum's (the first NaN wins),
/// its SSE2 kernel the product's. This follows the AVX2 kernel, which is
/// what `exr` runs on any CPU from the last decade.
fn dct_forward(d: &mut [f32; 64]) {
    if d.iter().all(|v| v.is_finite()) {
        transform::<false>(d);
    } else {
        transform::<true>(d);
    }
}

struct QuantTables {
    y: [f32; 64],
    half_y: [u16; 64],
    cbcr: [f32; 64],
    half_cbcr: [u16; 64],
}

impl QuantTables {
    fn new(base_error: f32) -> Self {
        const JPEG_Y: [u8; 64] = [
            16, 11, 10, 16, 24, 40, 51, 61, 12, 12, 14, 19, 26, 58, 60, 55, 14, 13, 16, 24, 40, 57,
            69, 56, 14, 17, 22, 29, 51, 87, 80, 62, 18, 22, 37, 56, 68, 109, 103, 77, 24, 35, 55,
            64, 81, 104, 113, 92, 49, 64, 78, 87, 103, 121, 120, 101, 72, 92, 95, 98, 112, 100,
            103, 99,
        ];
        const JPEG_CBCR: [u8; 64] = [
            17, 18, 24, 47, 99, 99, 99, 99, 18, 21, 26, 66, 99, 99, 99, 99, 24, 26, 56, 99, 99, 99,
            99, 99, 47, 66, 99, 99, 99, 99, 99, 99, 99, 99, 99, 99, 99, 99, 99, 99, 99, 99, 99, 99,
            99, 99, 99, 99, 99, 99, 99, 99, 99, 99, 99, 99, 99, 99, 99, 99, 99, 99, 99, 99,
        ];
        let e = base_error.max(0.0);
        let mut t = Self {
            y: [0.0; 64],
            half_y: [0; 64],
            cbcr: [0.0; 64],
            half_cbcr: [0; 64],
        };
        for i in 0..64 {
            t.y[i] = e * f32::from(JPEG_Y[i]) / 10.0;
            t.half_y[i] = f32_to_f16(t.y[i]);
            t.cbcr[i] = e * f32::from(JPEG_CBCR[i]) / 17.0;
            t.half_cbcr[i] = f32_to_f16(t.cbcr[i]);
        }
        t
    }
}

// ---------------------------------------------------------------------------
// OpenEXR's half-float quantizer (`algoQuantize`): among the halves within
// the error tolerance of the coefficient, pick the one with the fewest set
// bits (which compresses best), breaking ties by distance.

/// Set bits in a value below 2^16 (the quantizer's candidates are 15-bit
/// halves). A table, because `popcnt` is not in the x86-64 baseline.
#[inline(always)]
fn ones(v: u32) -> u32 {
    const T: [u8; 256] = {
        let mut t = [0u8; 256];
        let mut i = 0;
        while i < 256 {
            t[i] = (i as u32).count_ones() as u8;
            i += 1;
        }
        t
    };
    debug_assert!(v < 0x1_0000);
    u32::from(T[(v & 0xff) as usize]) + u32::from(T[((v >> 8) & 0xff) as usize])
}

#[inline]
fn shr(v: u32, s: u32) -> u32 {
    if s >= 32 {
        0
    } else {
        v >> s
    }
}

#[inline]
fn h2f(bits: u32) -> f32 {
    f16_to_f32(bits as u16)
}

struct Best {
    value: u32,
    bits: u32,
    delta: f32,
}

impl Best {
    fn new(abssrc: u32, err_tol: f32) -> Self {
        Self {
            value: abssrc,
            bits: ones(abssrc),
            delta: err_tol,
        }
    }

    /// An alternate above the source (`delta = alt - src`).
    fn large(&mut self, alt: u32, err_tol: f32, src: f32) {
        self.test(alt, h2f(alt) - src, err_tol);
    }

    /// An alternate below the source (`delta = src - alt`).
    fn small(&mut self, alt: u32, err_tol: f32, src: f32) {
        self.test(alt, src - h2f(alt), err_tol);
    }

    #[inline]
    fn test(&mut self, alt: u32, delta: f32, err_tol: f32) {
        let bits = ones(alt);
        if bits < self.bits {
            if delta < err_tol {
                self.bits = bits;
                self.delta = delta;
                self.value = alt;
            }
        } else if bits == self.bits && delta < self.delta {
            self.value = alt;
            self.delta = delta;
            self.bits = bits;
        }
    }
}

fn quant_mask(tol_sig: u32) -> (u32, u32, u32, u32) {
    let npow2 = 1u32 << (32 - tol_sig.leading_zeros());
    let lowermask = npow2 - 1;
    let mask = !lowermask;
    (npow2, lowermask, mask, mask ^ npow2)
}

fn q_denorm_tol(abssrc: u32, tol_sig: u32, err_tol: f32, src: f32) -> u32 {
    let (npow2, _, mask, mask2) = quant_mask(tol_sig);
    let mut b = Best::new(abssrc, err_tol);
    b.small(abssrc & mask2, err_tol, src);
    b.small(abssrc & mask, err_tol, src);
    b.large((abssrc + npow2) & mask, err_tol, src);
    b.large((abssrc + (npow2 << 1)) & mask, err_tol, src);
    b.value
}

fn mask3_of(npow2: u32, mask2: u32, extrabit: u32) -> u32 {
    mask2 ^ (((npow2 << 1) * extrabit) | ((npow2 >> 1) * (1 - extrabit)))
}

fn q_generic(abssrc: u32, tol_sig: u32, err_tol: f32, src: f32) -> u32 {
    let (npow2, lowermask, mask, mask2) = quant_mask(tol_sig);
    let extrabit = u32::from(tol_sig > (abssrc & lowermask));
    let mask3 = mask3_of(npow2, mask2, extrabit);
    let mut b = Best::new(abssrc, err_tol);
    let order = if extrabit != 0 {
        [mask3, mask2, mask]
    } else if abssrc & npow2 != 0 {
        [mask2, mask3, mask]
    } else {
        [mask2, mask, mask3]
    };
    for m in order {
        b.small(abssrc & m, err_tol, src);
    }
    b.large((abssrc + npow2) & mask, err_tol, src);
    b.value
}

fn q_equal_exp(abssrc: u32, tol_sig: u32, err_tol: f32, src: f32) -> u32 {
    let npow2 = 0x0800u32;
    let lowermask = npow2 - 1;
    let mask = !lowermask;
    let mask2 = mask ^ npow2;
    let masked = abssrc & lowermask;
    let extrabit = u32::from(tol_sig > masked);
    let mask3 = mask3_of(npow2, mask2, extrabit);
    let mut b = Best::new(abssrc, err_tol);
    if masked == abssrc {
        b.small(abssrc & mask3, err_tol, src);
    } else {
        let mut alt0 = abssrc & mask2;
        let alt1 = abssrc & mask;
        if alt0 == alt1 {
            alt0 = abssrc & mask3;
        }
        b.small(alt0, err_tol, src);
        b.small(alt1, err_tol, src);
    }
    b.large((abssrc + npow2) & mask, err_tol, src);
    b.value
}

fn q_close_exp(abssrc: u32, tol_sig: u32, err_tol: f32, src: f32) -> u32 {
    let npow2 = 0x0400u32;
    let lowermask = npow2 - 1;
    let mask = !lowermask;
    let mask2 = mask ^ npow2;
    let extrabit = u32::from(tol_sig > (abssrc & lowermask));
    let mask3 = mask3_of(npow2, mask2, extrabit);
    let mut alt = [0u32; 3];
    if abssrc & npow2 == 0 {
        if extrabit != 0 {
            alt[0] = abssrc & mask3;
            alt[1] = abssrc & mask;
        } else {
            alt[0] = abssrc & mask;
            alt[1] = abssrc & mask3;
        }
    } else if extrabit != 0 {
        alt[0] = abssrc & mask3;
        alt[1] = abssrc & mask2;
        if src - h2f(alt[1]) >= err_tol {
            alt[1] = abssrc & mask;
        }
    } else {
        alt[0] = abssrc & mask2;
        alt[1] = abssrc & mask3;
        if src - h2f(alt[0]) >= err_tol {
            alt[0] = abssrc & mask;
        }
    }
    alt[2] = (abssrc + npow2) & mask;
    let mut b = Best::new(abssrc, err_tol);
    b.small(alt[0], err_tol, src);
    b.small(alt[1], err_tol, src);
    b.large(alt[2], err_tol, src);
    b.value
}

fn two_sided(abssrc: u32, alt0: u32, alt1: u32, err_tol: f32, src: f32) -> u32 {
    let bits0 = ones(alt0);
    let bits1 = ones(alt1);
    match bits1.cmp(&bits0) {
        Ordering::Less => {
            if h2f(alt1) - src < err_tol {
                return alt1;
            }
            if src - h2f(alt0) < err_tol {
                return alt0;
            }
        }
        Ordering::Equal => {
            let delta = src - h2f(alt0);
            let delta1 = h2f(alt1) - src;
            if delta < err_tol {
                return if delta1 < delta { alt1 } else { alt0 };
            }
            if delta1 < err_tol {
                return alt1;
            }
        }
        Ordering::Greater => {
            if src - h2f(alt0) < err_tol {
                return alt0;
            }
            if bits1 < ones(abssrc) && h2f(alt1) - src < err_tol {
                return alt1;
            }
        }
    }
    abssrc
}

fn q_equal_sig(abssrc: u32, npow2: u32, mask: u32, err_tol: f32, src: f32) -> u32 {
    let mut alt0 = abssrc & mask;
    let alt1 = (abssrc + npow2) & mask;
    let mut delta0 = src - h2f(alt0);
    if delta0 >= err_tol {
        let mask2 = mask ^ (npow2 | (npow2 >> 1));
        alt0 = abssrc & mask2;
        delta0 = src - h2f(alt0);
        if delta0 >= err_tol {
            let delta1 = h2f(alt1) - src;
            if delta1 < err_tol && ones(alt1) < ones(abssrc) {
                return alt1;
            }
            return abssrc;
        }
    }
    let bits0 = ones(alt0);
    let bits1 = ones(alt1);
    if bits1 < bits0 {
        if h2f(alt1) - src < err_tol {
            return alt1;
        }
    } else if bits1 == bits0 && h2f(alt1) - src < delta0 {
        return alt1;
    }
    alt0
}

fn q_default(abssrc: u32, tol_sig: u32, err_tol: f32, src: f32) -> u32 {
    let (npow2, lowermask, mask, _) = quant_mask(tol_sig);
    let masked = abssrc & lowermask;
    match masked.cmp(&tol_sig) {
        Ordering::Greater => {
            let mask2 = mask ^ (npow2 | (npow2 >> 1));
            two_sided(
                abssrc,
                abssrc & mask2,
                (abssrc + npow2) & mask,
                err_tol,
                src,
            )
        }
        Ordering::Less => two_sided(abssrc, abssrc & mask, (abssrc + npow2) & mask, err_tol, src),
        Ordering::Equal => q_equal_sig(abssrc, npow2, mask, err_tol, src),
    }
}

fn quantize(src: u32, herr_tol: u32, err_tol: f32, src_float: f32) -> u32 {
    let sign = src & 0x8000;
    let abssrc = src & 0x7fff;
    let src_float = src_float.abs();
    let src_exp = src & 0x7c00;
    let tol_exp = herr_tol & 0x7c00;
    if src_exp == 0x7c00 {
        return src;
    }
    if src_float < err_tol {
        return 0;
    }
    let exp_diff = src_exp.wrapping_sub(tol_exp) >> 10;
    let mut tol_sig = shr((herr_tol & 0x03ff) | (1 << 10), exp_diff);
    if tol_exp == 0 {
        tol_sig = herr_tol & 0x03ff;
        if tol_sig == 0 {
            return src;
        }
        if exp_diff == 0 || exp_diff == 1 {
            return sign | q_generic(abssrc, tol_sig, err_tol, src_float);
        }
        tol_sig = shr(tol_sig, exp_diff).max(1);
        return sign | q_denorm_tol(abssrc, tol_sig, err_tol, src_float);
    }
    if tol_sig == 0 {
        return src;
    }
    if exp_diff > 1 || src_exp == 0 {
        return sign | q_default(abssrc, tol_sig, err_tol, src_float);
    }
    if exp_diff == 0 {
        return sign | q_equal_exp(abssrc, tol_sig, err_tol, src_float);
    }
    sign | q_close_exp(abssrc, tol_sig, err_tol, src_float)
}

// ---------------------------------------------------------------------------

/// Push one block's AC coefficients (zig-zag order) as run-length tokens.
fn rle_ac(z: &[u16; 64], ac: &mut Vec<u16>) {
    let mut i = 1;
    while i < 64 {
        if z[i] != 0 {
            ac.push(z[i]);
            i += 1;
            continue;
        }
        let mut run = 1;
        while i + run < 64 && z[i + run] == 0 {
            run += 1;
        }
        if run == 1 {
            ac.push(0);
        } else if i + run == 64 {
            ac.push(0xff00);
        } else {
            ac.push(0xff00 | run as u16);
        }
        i += run;
    }
}

/// `exr`'s edge mirroring for partial 8x8 blocks.
fn mirror(index: usize, len: usize) -> usize {
    let (mut v, n) = (index as isize, len as isize);
    if v >= n {
        v = n - (v - (n - 1));
    }
    if v < 0 {
        v = n - 1;
    }
    v as usize
}

/// Encode one standalone channel (1 component) or one R/G/B triple.
fn encode_group(
    comps: &[Vec<u16>],
    width: usize,
    height: usize,
    q: &QuantTables,
    ac: &mut Vec<u16>,
    dc: &mut Vec<u16>,
) {
    if width == 0 || height == 0 {
        return;
    }
    let n = comps.len();
    let bx = width.div_ceil(8);
    let by = height.div_ceil(8);
    let mut group_dc: Vec<Vec<u16>> = (0..n).map(|_| Vec::with_capacity(bx * by)).collect();
    let mut blocks = vec![[[0.0f32; 64]; 3]; bx];
    let cols: Vec<[usize; 8]> = (0..bx)
        .map(|x| std::array::from_fn(|k| mirror(x * 8 + k, width)))
        .collect();
    for yb in 0..by {
        let rows: [usize; 8] = std::array::from_fn(|k| mirror(yb * 8 + k, height));
        for (xb, blk) in blocks.iter_mut().enumerate() {
            for (c, comp) in comps.iter().enumerate() {
                for (r, &sy) in rows.iter().enumerate() {
                    let line = &comp[sy * width..(sy + 1) * width];
                    for (k, &sx) in cols[xb].iter().enumerate() {
                        blk[c][r * 8 + k] = f16_to_f32(line[sx]);
                    }
                }
            }
            if n == 3 {
                for i in 0..64 {
                    let (r, g, b) = (blk[0][i], blk[1][i], blk[2][i]);
                    let y = 0.2126 * r + 0.7152 * g + 0.0722 * b;
                    blk[0][i] = y;
                    blk[1][i] = (b - y) / 1.8556;
                    blk[2][i] = (r - y) / 1.5747;
                }
            }
            for d in blk.iter_mut().take(n) {
                dct_forward(d);
            }
            for (c, d) in blk.iter().enumerate().take(n) {
                let (tol, htol) = if c == 0 {
                    (&q.y, &q.half_y)
                } else {
                    (&q.cbcr, &q.half_cbcr)
                };
                let mut z = [0u16; 64];
                for i in 0..64 {
                    let src = f32_to_f16(d[i]);
                    z[ZIGZAG[i]] =
                        quantize(u32::from(src), u32::from(htol[i]), tol[i], f16_to_f32(src))
                            as u16;
                }
                group_dc[c].push(z[0]);
                rle_ac(&z, ac);
            }
        }
    }
    for g in group_dc {
        dc.extend(g);
    }
}

/// A channel's samples as halves (floats demoted, clamped to the half
/// range), optionally through the perceptual transfer.
fn halves(planar: &[u8], ty: SampleType, nonlinear: bool) -> Vec<u16> {
    let mut v: Vec<u16> = match ty {
        SampleType::F16 => planar
            .chunks_exact(2)
            .map(|b| u16::from_le_bytes([b[0], b[1]]))
            .collect(),
        _ => planar
            .chunks_exact(4)
            .map(|b| {
                f32_to_f16(f32::from_le_bytes([b[0], b[1], b[2], b[3]]).clamp(-65504.0, 65504.0))
            })
            .collect(),
    };
    if nonlinear {
        let t = to_nonlinear();
        for s in &mut v {
            *s = t[*s as usize];
        }
    }
    v
}

fn zlib9(data: &[u8]) -> Vec<u8> {
    if data.is_empty() {
        Vec::new()
    } else {
        miniz_oxide::deflate::compress_to_vec_zlib(data, 9)
    }
}

/// Compress one chunk (raw layout in). `level` is the DWA compression level
/// (OpenEXR's and `exr`'s default is 45).
pub(crate) fn compress(geom: &BlockGeom<'_>, raw: &[u8], level: f32) -> Result<Vec<u8>> {
    let rules = default_rules();
    let rule_bytes = write_rules(&rules, geom);
    let (schemes, sets) = classify(&geom.chans, &rules);
    let planar = geom.raw_to_planar(raw);
    let mut starts = Vec::with_capacity(geom.chans.len());
    let mut s = 0usize;
    for c in &geom.chans {
        starts.push(s);
        s += c.bytes();
    }
    let chan = |i: usize| &planar[starts[i]..starts[i] + geom.chans[i].bytes()];

    let mut unknown = Vec::new();
    let mut rle_raw = Vec::new();
    for (i, c) in geom.chans.iter().enumerate() {
        match schemes[i] {
            Scheme::Unknown => unknown.extend_from_slice(chan(i)),
            Scheme::Rle => {
                let src = chan(i);
                let bps = c.ty.bytes();
                let n = src.len() / bps;
                let base = rle_raw.len();
                rle_raw.resize(base + src.len(), 0);
                for (k, sample) in src.chunks_exact(bps).enumerate() {
                    for (b, &v) in sample.iter().enumerate() {
                        rle_raw[base + b * n + k] = v;
                    }
                }
            }
            Scheme::Lossy => {}
        }
    }

    let q = QuantTables::new(level / 100_000.0);
    let mut ac = Vec::new();
    let mut dc = Vec::new();
    let mut grouped = vec![false; geom.chans.len()];
    for set in &sets {
        let comps: Vec<Vec<u16>> = set
            .iter()
            .map(|&i| halves(chan(i), geom.chans[i].ty, true))
            .collect();
        let c0 = &geom.chans[set[0]];
        encode_group(&comps, c0.width, c0.height, &q, &mut ac, &mut dc);
        for &i in set {
            grouped[i] = true;
        }
    }
    for (i, c) in geom.chans.iter().enumerate() {
        if grouped[i] || schemes[i] != Scheme::Lossy {
            continue;
        }
        let comps = [halves(chan(i), c.ty, !c.p_linear)];
        encode_group(&comps, c.width, c.height, &q, &mut ac, &mut dc);
    }

    let unknown_z = zlib9(&unknown);
    let ac_z = huffman::compress(&ac)?;
    let dc_z = if dc.is_empty() {
        Vec::new()
    } else {
        let bytes: Vec<u8> = dc.iter().flat_map(|v| v.to_le_bytes()).collect();
        let mut t = vec![0u8; bytes.len()];
        zip::deinterleave(&bytes, &mut t);
        zip::apply_predictor(&mut t);
        zlib9(&t)
    };
    let (rle_tokens_len, rle_z) = if rle_raw.is_empty() {
        (0, Vec::new())
    } else {
        let t = rle::pack(&rle_raw);
        (t.len(), zlib9(&t))
    };

    let counters = [
        2u64,
        unknown.len() as u64,
        unknown_z.len() as u64,
        ac_z.len() as u64,
        dc_z.len() as u64,
        rle_z.len() as u64,
        rle_tokens_len as u64,
        rle_raw.len() as u64,
        ac.len() as u64,
        dc.len() as u64,
        0, // AC: static Huffman
    ];
    let mut out = Vec::with_capacity(
        88 + rule_bytes.len() + unknown_z.len() + ac_z.len() + dc_z.len() + rle_z.len(),
    );
    for v in counters {
        out.extend_from_slice(&v.to_le_bytes());
    }
    out.extend_from_slice(&rule_bytes);
    out.extend_from_slice(&unknown_z);
    out.extend_from_slice(&ac_z);
    out.extend_from_slice(&dc_z);
    out.extend_from_slice(&rle_z);
    Ok(out)
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn transfer_table_matches_exr() {
        // exr's run-time table vs OpenEXR's stored one (which the writer
        // uses): identical with glibc and the Windows CRT; any other libm
        // may differ by a half ulp in a few entries.
        let ours = to_nonlinear_runtime();
        let theirs = to_nonlinear();
        let mut diff = 0;
        for (a, b) in ours.iter().zip(theirs) {
            if a != b {
                diff += 1;
                assert!(a.abs_diff(*b) <= 1, "{a:#x} vs {b:#x}");
            }
        }
        #[cfg(any(windows, all(target_os = "linux", target_env = "gnu")))]
        assert_eq!(diff, 0);
        let _ = diff;
    }

    #[test]
    fn rle_ac_tokens() {
        let mut z = [0u16; 64];
        let mut ac = Vec::new();
        rle_ac(&z, &mut ac);
        assert_eq!(ac, [0xff00]);
        z[1] = 5;
        z[3] = 7;
        z[10] = 9;
        ac.clear();
        rle_ac(&z, &mut ac);
        assert_eq!(ac, [5, 0, 7, 0xff06, 9, 0xff00]);
    }

    #[test]
    fn mirror_matches_exr() {
        assert_eq!(mirror(5, 3), 0);
        assert_eq!(mirror(6, 3), 2);
        assert_eq!(mirror(3, 1), 0);
        assert_eq!(mirror(9, 10), 9);
        assert_eq!(mirror(10, 10), 9);
        assert_eq!(mirror(11, 10), 8);
    }

    #[test]
    fn quantizer_keeps_specials_and_zeroes_small() {
        let q = QuantTables::new(45.0 / 100_000.0);
        // Inf / NaN pass through.
        assert_eq!(
            quantize(0x7c00, u32::from(q.half_y[0]), q.y[0], f32::INFINITY),
            0x7c00
        );
        // Below tolerance -> 0.
        let tiny = f32_to_f16(q.y[5] * 0.5);
        assert_eq!(
            quantize(
                u32::from(tiny),
                u32::from(q.half_y[5]),
                q.y[5],
                f16_to_f32(tiny)
            ),
            0
        );
        // The result is always within tolerance of the source.
        for bits in (0u32..0x7c00).step_by(37) {
            for i in [0, 9, 63] {
                let r = quantize(bits, u32::from(q.half_y[i]), q.y[i], h2f(bits));
                assert!((h2f(r) - h2f(bits)).abs() <= q.y[i], "{bits:#x} -> {r:#x}");
            }
        }
    }
}
