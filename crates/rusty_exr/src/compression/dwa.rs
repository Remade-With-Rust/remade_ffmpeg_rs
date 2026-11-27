//! DWAA / DWAB decoding (OpenEXR's lossy DCT compressor).
//!
//! A DWA chunk holds four sections: "unknown" channels (zlib), the lossy
//! DCT's AC coefficients (Huffman or zlib) and DC coefficients (zlib with
//! the ZIP predictor), and RLE channels (zlib over run-length tokens).
//! Channels are routed to a scheme by suffix rules stored in the chunk;
//! R/G/B triples sharing a prefix are decoded together through an inverse
//! Rec. 709 colour transform.
//!
//! Bit-exactness: OpenEXR picks its inverse DCT at run time. On every
//! x86-64 CPU with AVX (which is what its reference build runs on) it uses
//! a hand-written AVX routine whose float operation order differs from its
//! scalar and SSE2 routines, and the rounded halves differ accordingly. The
//! IDCT below reproduces that AVX routine's operation order exactly, in
//! scalar IEEE-754 single precision, so decoded samples match OpenEXR on
//! those machines bit for bit (and on any platform this crate runs on).

use super::{huffman, BlockGeom, ChanGeom};
use crate::error::{Error, Result};
use crate::header::SampleType;

#[derive(Debug, Clone, Copy, PartialEq, Eq)]
pub(super) enum Scheme {
    Unknown,
    Lossy,
    Rle,
}

#[derive(Debug, Clone)]
pub(super) struct Rule {
    pub(super) suffix: Vec<u8>,
    pub(super) scheme: Scheme,
    pub(super) ty: SampleType,
    pub(super) csc: i32,
    pub(super) case_insensitive: bool,
}

impl Rule {
    pub(super) fn matches(&self, suffix: &[u8], ty: SampleType) -> bool {
        if self.ty != ty {
            return false;
        }
        if self.case_insensitive {
            self.suffix.eq_ignore_ascii_case(suffix)
        } else {
            self.suffix == suffix
        }
    }
}

pub(super) fn rule(suffix: &str, scheme: Scheme, ty: SampleType, csc: i32, ci: bool) -> Rule {
    Rule {
        suffix: suffix.as_bytes().to_vec(),
        scheme,
        ty,
        csc,
        case_insensitive: ci,
    }
}

fn legacy_rules() -> Vec<Rule> {
    use SampleType::{F16, F32, U32};
    use Scheme::{Lossy, Rle};
    let mut v = Vec::new();
    for (s, csc) in [
        ("r", 0),
        ("red", 0),
        ("g", 1),
        ("grn", 1),
        ("green", 1),
        ("b", 2),
        ("blu", 2),
        ("blue", 2),
        ("y", -1),
        ("by", -1),
        ("ry", -1),
    ] {
        v.push(rule(s, Lossy, F16, csc, true));
        v.push(rule(s, Lossy, F32, csc, true));
    }
    v.push(rule("a", Rle, U32, -1, true));
    v.push(rule("a", Rle, F16, -1, true));
    v.push(rule("a", Rle, F32, -1, true));
    v
}

fn read_rules(c: &mut crate::header::Cursor<'_>) -> Result<Vec<Rule>> {
    if c.remaining() <= 2 {
        return Err(Error::invalid("DWA rules truncated"));
    }
    let b = c.bytes(2)?;
    let size = u16::from_le_bytes([b[0], b[1]]) as usize;
    if size < 2 || size - 2 > c.remaining() {
        return Err(Error::invalid("DWA rule size"));
    }
    let mut data = c.bytes(size - 2)?;
    let mut rules = Vec::new();
    while !data.is_empty() {
        if data.len() <= 3 {
            return Err(Error::invalid("DWA rule truncated"));
        }
        let Some(nul) = data.iter().take(129).position(|&b| b == 0) else {
            return Err(Error::invalid("DWA rule suffix"));
        };
        if nul > data.len() - 3 {
            return Err(Error::invalid("DWA rule truncated"));
        }
        let suffix = data[..nul].to_vec();
        let value = data[nul + 1];
        let ty = data[nul + 2];
        data = &data[nul + 3..];
        let csc = i32::from(value >> 4) - 1;
        if !(-1..3).contains(&csc) {
            return Err(Error::invalid("DWA rule colour index"));
        }
        let scheme = match (value >> 2) & 3 {
            0 => Scheme::Unknown,
            1 => Scheme::Lossy,
            2 => Scheme::Rle,
            _ => return Err(Error::invalid("DWA rule scheme")),
        };
        let ty = match ty {
            0 => SampleType::U32,
            1 => SampleType::F16,
            2 => SampleType::F32,
            _ => return Err(Error::invalid("DWA rule pixel type")),
        };
        rules.push(Rule {
            suffix,
            scheme,
            ty,
            csc,
            case_insensitive: value & 1 != 0,
        });
    }
    Ok(rules)
}

pub(super) fn suffix_of(name: &str) -> (&[u8], &[u8]) {
    let b = name.as_bytes();
    match b.iter().rposition(|&c| c == b'.') {
        Some(i) => (&b[..=i], &b[i + 1..]),
        None => (&b[..0], b),
    }
}

/// Classify channels; returns per-channel schemes and the CSC triples.
pub(super) fn classify(chans: &[ChanGeom<'_>], rules: &[Rule]) -> (Vec<Scheme>, Vec<[usize; 3]>) {
    let mut schemes = vec![Scheme::Unknown; chans.len()];
    let mut prefixes: Vec<(&[u8], [i32; 3])> = Vec::new();
    for (ci, ch) in chans.iter().enumerate() {
        let (prefix, suffix) = suffix_of(ch.name);
        let pi = if let Some(i) = prefixes.iter().position(|(p, _)| *p == prefix) {
            i
        } else {
            prefixes.push((prefix, [-1; 3]));
            prefixes.len() - 1
        };
        for r in rules {
            if r.matches(suffix, ch.ty) {
                schemes[ci] = r.scheme;
                if r.csc >= 0 {
                    prefixes[pi].1[r.csc as usize] = ci as i32;
                }
            }
        }
    }
    let mut sets = Vec::new();
    for (_, idx) in prefixes {
        if idx.iter().any(|&i| i < 0) {
            continue;
        }
        let [r, g, b] = idx.map(|i| i as usize);
        // Equal sampling means equal geometry inside one chunk.
        let (rc, gc, bc) = (&chans[r], &chans[g], &chans[b]);
        if rc.x_sampling != gc.x_sampling
            || rc.x_sampling != bc.x_sampling
            || rc.y_sampling != gc.y_sampling
            || rc.y_sampling != bc.y_sampling
        {
            continue;
        }
        sets.push([r, g, b]);
    }
    (schemes, sets)
}

// ---------------------------------------------------------------------------
// The lossy DCT decoder.

pub(crate) const A: f32 = 3.535536e-01;
pub(crate) const B: f32 = 4.903927e-01;
pub(crate) const C: f32 = 4.619398e-01;
pub(crate) const D: f32 = 4.157349e-01;
pub(crate) const E: f32 = 2.777855e-01;
pub(crate) const F: f32 = 1.913422e-01;
pub(crate) const G: f32 = 9.754573e-02;

/// `dst[i] = src[ZIGZAG[i]]`.
pub(super) const ZIGZAG: [usize; 64] = [
    0, 1, 5, 6, 14, 15, 27, 28, 2, 4, 7, 13, 16, 26, 29, 42, 3, 8, 12, 17, 25, 30, 41, 43, 9, 11,
    18, 24, 31, 40, 44, 53, 10, 19, 23, 32, 39, 45, 52, 54, 20, 22, 33, 38, 46, 51, 55, 60, 21, 34,
    37, 47, 50, 56, 59, 61, 35, 36, 48, 49, 57, 58, 62, 63,
];

/// Addition and subtraction as an AVX `vaddps` / `vsubps` computes them
/// for `src1 op src2`. Finite inputs give finite sums, and then the order
/// of operands cannot matter. With NaNs (only possible when a coefficient
/// is infinite or NaN) the hardware returns `src1`'s NaN when both operands
/// are NaN; `NAN` selects that exact rule, so even NaN signs and payloads
/// match OpenEXR's assembly.
#[inline(always)]
fn vadd<const NAN: bool>(s1: f32, s2: f32) -> f32 {
    if NAN && s1.is_nan() {
        s1
    } else {
        s1 + s2
    }
}

#[inline(always)]
fn vsub<const NAN: bool>(s1: f32, s2: f32) -> f32 {
    if NAN && s1.is_nan() {
        s1
    } else {
        s1 - s2
    }
}

/// One row of OpenEXR's AVX row pass (full 8-point transform).
#[inline(always)]
fn row_pass<const NAN: bool>(r: &[f32]) -> [f32; 8] {
    const M1: [[f32; 4]; 4] = [[A, A, A, A], [C, F, -F, -C], [A, -A, -A, A], [F, -C, C, -F]];
    const M2: [[f32; 4]; 4] = [[B, D, E, G], [D, -G, -B, -E], [E, -B, G, D], [G, -E, D, -B]];
    let add = vadd::<NAN>;
    let mut out = [0f32; 8];
    for k in 0..4 {
        let e = add(
            add(r[0] * M1[0][k], r[2] * M1[1][k]),
            add(r[4] * M1[2][k], r[6] * M1[3][k]),
        );
        let o = add(
            add(r[1] * M2[0][k], r[3] * M2[1][k]),
            add(r[5] * M2[2][k], r[7] * M2[3][k]),
        );
        out[k] = add(e, o);
        out[7 - k] = vsub::<NAN>(e, o);
    }
    out
}

/// OpenEXR's `dctInverse8x8_avx_{zr}`: an inverse DCT where the last `zr`
/// rows of the (row-major, frequency-domain) block are known to be zero.
fn idct_avx(data: &mut [f32; 64], zr: usize) {
    if data.iter().all(|v| v.is_finite()) {
        idct_avx_impl::<false>(data, zr);
    } else {
        idct_avx_impl::<true>(data, zr);
    }
}

/// Operation order, and the operand order of every addition and
/// subtraction, follow the AVX assembly instruction by instruction.
fn idct_avx_impl<const NAN: bool>(data: &mut [f32; 64], zr: usize) {
    let add = vadd::<NAN>;
    let sub = vsub::<NAN>;
    let present = 8 - zr;
    let mut rows = [[0f32; 8]; 8];
    // Row pass over processed row pairs.
    let mut pair = 0;
    while pair < 8 {
        if pair < present {
            rows[pair] = row_pass::<NAN>(&data[pair * 8..pair * 8 + 8]);
            rows[pair + 1] = row_pass::<NAN>(&data[pair * 8 + 8..pair * 8 + 16]);
        }
        pair += 2;
    }
    if zr == 7 {
        for i in 0..8 {
            for j in 0..8 {
                data[i * 8 + j] = rows[0][j] * A;
            }
        }
        return;
    }
    let p = |i: usize| i < present;
    for j in 0..8 {
        let r = |i: usize| rows[i][j];
        // Odd outputs.
        let mut o0 = B * r(1);
        if p(3) {
            o0 = add(D * r(3), o0);
            if p(5) {
                let mut t = E * r(5);
                if p(7) {
                    t = add(G * r(7), t);
                }
                o0 = add(t, o0);
            }
        }
        let mut o1 = D * r(1);
        if p(3) {
            let mut t = G * r(3);
            if p(5) {
                t = add(B * r(5), t);
            }
            o1 = sub(o1, t);
        }
        if p(7) {
            o1 = sub(o1, E * r(7));
        }
        let mut o2 = E * r(1);
        if p(3) {
            o2 = sub(o2, B * r(3));
        }
        if p(5) {
            o2 = add(o2, G * r(5));
        }
        if p(7) {
            o2 = add(o2, D * r(7));
        }
        let mut o3 = G * r(1);
        if p(5) {
            o3 = add(D * r(5), o3);
        }
        if p(3) {
            let mut t = E * r(3);
            if p(7) {
                t = add(B * r(7), t);
            }
            o3 = sub(o3, t);
        }
        // Even outputs.
        let mut t11 = A * r(0);
        let mut t1 = t11;
        if p(4) {
            let a4 = A * r(4);
            t11 = add(t11, a4);
            t1 = sub(t1, a4);
        }
        let mut e0 = t11;
        let mut e3 = t11;
        let mut e1 = t1;
        let mut e2 = t1;
        if p(2) {
            let mut t7 = C * r(2);
            if p(6) {
                t7 = add(t7, F * r(6));
            }
            e0 = add(t11, t7);
            e3 = sub(t11, t7);
            let mut t7 = F * r(2);
            if p(6) {
                t7 = sub(t7, C * r(6));
            }
            e1 = add(t1, t7);
            e2 = sub(t1, t7);
        }
        data[j] = add(e0, o0);
        data[8 + j] = add(e1, o1);
        data[16 + j] = add(e2, o2);
        data[24 + j] = add(e3, o3);
        data[32 + j] = sub(e3, o3);
        data[40 + j] = sub(e2, o2);
        data[48 + j] = sub(e1, o1);
        data[56 + j] = sub(e0, o0);
    }
}

/// Imath's `float_to_half` (NaN keeps its high payload, sets bit 0 if
/// that is empty; finite values round to nearest even).
#[inline]
fn imath_f2h(f: f32) -> u16 {
    if f.is_nan() {
        let x = f.to_bits();
        let m = ((x & 0x007f_ffff) >> 13) as u16;
        return ((x >> 16) & 0x8000) as u16 | 0x7c00 | m | u16::from(m == 0);
    }
    crate::half::f32_to_f16(f)
}

/// Imath's `half_to_float` (NaN payload kept, quiet bit not forced).
#[inline]
pub(crate) fn imath_h2f_bits(h: u16) -> u32 {
    if h & 0x7c00 == 0x7c00 {
        (u32::from(h & 0x8000) << 16) | 0x7f80_0000 | (u32::from(h & 0x3ff) << 13)
    } else {
        crate::half::f16_to_f32(h).to_bits()
    }
}

/// F16C `vcvtps2ph` with round-to-nearest-even.
#[inline]
fn f16c_f2h(f: f32) -> u16 {
    crate::half::f32_to_f16(f)
}

/// `UNZIGZAG[k]` is the raster position of zig-zag index `k`
/// (`ZIGZAG[UNZIGZAG[k]] == k`).
pub(crate) const UNZIGZAG: [usize; 64] = {
    let mut t = [0usize; 64];
    let mut i = 0;
    while i < 64 {
        t[ZIGZAG[i]] = i;
        i += 1;
    }
    t
};

/// `zr` value marking a component whose block has no AC coefficient (a
/// flat block: every sample is the DC term times `A * A`).
pub(crate) const ZR_CONSTANT: u8 = 8;

/// Trailing all-zero rows of a block whose last non-zero AC coefficient
/// is zig-zag index `lnz` (OpenEXR's choice of IDCT variant), or
/// [`ZR_CONSTANT`] when there is none.
fn zero_rows(lnz: usize) -> u8 {
    match lnz {
        0 => ZR_CONSTANT,
        1 => 7,
        2 => 6,
        3..=8 => 5,
        9 => 4,
        10..=19 => 3,
        20 => 2,
        21..=34 => 1,
        _ => 0,
    }
}

/// The scalar block pipeline (and the oracle of `simd::dwa_block`): per
/// component dequantize (`raster` holds half coefficients in raster order),
/// inverse DCT, then the inverse colour transform for a triple, then
/// rounding to halves. `constant` is true when every component is flat.
pub(crate) fn block_scalar(
    raster: &[[u16; 64]; 3],
    zr: [u8; 3],
    n: usize,
    constant: bool,
    halves: &mut [[u16; 64]; 3],
) {
    let mut dct = [[0f32; 64]; 3];
    for comp in 0..n {
        let d = &mut dct[comp];
        let z = &raster[comp];
        if zr[comp] == ZR_CONSTANT {
            let v = crate::half::f16_to_f32(z[0]) * A * A;
            *d = [v; 64];
        } else {
            for i in 0..64 {
                d[i] = crate::half::f16_to_f32(z[i]);
            }
            idct_avx(d, usize::from(zr[comp]));
        }
    }
    if n == 3 {
        let count = if constant { 1 } else { 64 };
        let [d0, d1, d2] = &mut dct;
        for i in 0..count {
            let (s0, s1, s2) = (d0[i], d1[i], d2[i]);
            d0[i] = s0 + 1.5747 * s2;
            d1[i] = s0 - 0.1873 * s1 - 0.4682 * s2;
            d2[i] = s0 + 1.8556 * s1;
        }
    }
    for comp in 0..n {
        if constant {
            halves[comp] = [imath_f2h(dct[comp][0]); 64];
        } else {
            for i in 0..64 {
                halves[comp][i] = f16c_f2h(dct[comp][i]);
            }
        }
    }
}

struct AcReader<'a> {
    ac: &'a [u16],
    pos: usize,
}

/// Decode one lossy channel set (1 channel, or an R/G/B triple with CSC).
/// `outs` receive `width * height` half values each, row-major.
fn decode_lossy(
    ac: &mut AcReader<'_>,
    dc: &[u16],
    dc_pos: &mut usize,
    width: usize,
    height: usize,
    to_linear: Option<&[u16; 65536]>,
    outs: &mut [&mut [u16]],
) -> Result<()> {
    let n = outs.len();
    let nbx = width.div_ceil(8);
    let nby = height.div_ceil(8);
    let need = n * nbx * nby;
    if dc.len() - *dc_pos < need {
        return Err(Error::invalid("DWA DC data too short"));
    }
    let dc_base = *dc_pos;
    *dc_pos += need;
    // Coefficients in raster (de-zig-zagged) order: the un-RLE scatters each
    // AC value straight to its raster slot.
    let mut raster = [[0u16; 64]; 3];
    let mut zr = [ZR_CONSTANT; 3];
    let mut halves = [[0u16; 64]; 3];
    // Float scratch for the SIMD block kernel, owned here so no block pays
    // for zeroing it.
    let mut work = [[0f32; 64]; 3];
    let isa = crate::simd::isa();
    for by in 0..nby {
        for bx in 0..nbx {
            let mut constant = true;
            for comp in 0..n {
                let z = &mut raster[comp];
                *z = [0; 64];
                z[0] = dc[dc_base + comp * nbx * nby + by * nbx + bx];
                // Un-RLE the AC coefficients (over a local view of the
                // remaining tokens: as a field update through `&mut`, the
                // read position kept several induction variables live).
                let toks = &ac.ac[ac.pos..];
                let mut i = 0usize;
                let mut k = 1usize;
                let mut lnz = 0usize;
                while k < 64 {
                    let Some(&v) = toks.get(i) else {
                        return Err(Error::invalid("DWA AC data too short"));
                    };
                    i += 1;
                    if v & 0xff00 == 0xff00 {
                        let c = usize::from(v & 0xff);
                        k += if c == 0 { 64 } else { c };
                    } else {
                        lnz = k;
                        z[UNZIGZAG[k]] = v;
                        k += 1;
                    }
                }
                ac.pos += i;
                zr[comp] = zero_rows(lnz);
                if lnz != 0 {
                    constant = false;
                }
            }
            if constant || !crate::simd::dwa_block(isa, &raster, zr, n, &mut halves, &mut work) {
                block_scalar(&raster, zr, n, constant, &mut halves);
            }
            let max_y = (height - by * 8).min(8);
            let max_x = (width - bx * 8).min(8);
            for comp in 0..n {
                let out = &mut outs[comp];
                if max_x == 8 {
                    // Whole-width block: fixed 8-sample rows, unrolled.
                    for yy in 0..max_y {
                        let row = (by * 8 + yy) * width + bx * 8;
                        let src: &[u16; 8] = halves[comp][yy * 8..yy * 8 + 8]
                            .try_into()
                            .expect("8 samples");
                        let dst: &mut [u16; 8] =
                            (&mut out[row..row + 8]).try_into().expect("8 samples");
                        match to_linear {
                            Some(t) => {
                                for j in 0..8 {
                                    dst[j] = t[usize::from(src[j])];
                                }
                            }
                            None => *dst = *src,
                        }
                    }
                    continue;
                }
                for yy in 0..max_y {
                    let row = (by * 8 + yy) * width + bx * 8;
                    let src = &halves[comp][yy * 8..yy * 8 + max_x];
                    let dst = &mut out[row..row + max_x];
                    match to_linear {
                        Some(t) => {
                            for (d, &s) in dst.iter_mut().zip(src) {
                                *d = t[s as usize];
                            }
                        }
                        None => dst.copy_from_slice(src),
                    }
                }
            }
        }
    }
    Ok(())
}

// ---------------------------------------------------------------------------

fn u64_at(c: &mut crate::header::Cursor<'_>) -> Result<u64> {
    c.u64()
}

/// Buffers one worker reuses from one DWA chunk to the next.
#[derive(Default)]
pub(crate) struct DwaScratch {
    unknown: Vec<u8>,
    bytes: Vec<u8>,
    ac: Vec<u16>,
    dc: Vec<u16>,
    rle: Vec<u8>,
    bufs: [Vec<u16>; 3],
}

/// Decode one DWA chunk into `scratch.w[..geom.raw_size / 2]` (channel-planar
/// little-endian words).
pub(crate) fn decompress(
    geom: &BlockGeom<'_>,
    data: &[u8],
    scratch: &mut super::Scratch,
) -> Result<()> {
    const HEADER: usize = 11 * 8;
    if data.len() < HEADER {
        return Err(Error::invalid("DWA header truncated"));
    }
    let mut c = crate::header::Cursor::new(data);
    let mut s = [0u64; 11];
    for v in &mut s {
        *v = u64_at(&mut c)?;
    }
    let [version, unk_unc, unk_comp, ac_comp, dc_comp, rle_comp, rle_unc, rle_raw, ac_count, dc_count, ac_compression] =
        s;
    if s.iter().take(10).any(|&v| v > i64::MAX as u64) {
        return Err(Error::invalid("DWA section size"));
    }
    let total = data.len() as u64;
    let comp_sum = unk_comp
        .checked_add(ac_comp)
        .and_then(|v| v.checked_add(dc_comp))
        .and_then(|v| v.checked_add(rle_comp))
        .ok_or_else(|| Error::invalid("DWA section sizes"))?;
    if total < HEADER as u64 + comp_sum {
        return Err(Error::invalid("DWA sections exceed the chunk"));
    }
    let raw = geom.raw_size as u64;
    if unk_unc > raw || rle_raw > raw || unk_unc + rle_raw > raw {
        return Err(Error::invalid("DWA uncompressed sizes exceed the chunk"));
    }
    if version > 2 {
        return Err(Error::unsupported(format!("DWA version {version}")));
    }
    let rules = if version < 2 {
        legacy_rules()
    } else {
        read_rules(&mut c)?
    };
    let rest = &data[c.pos..];
    let (sec_unk, rest) = split(rest, unk_comp)?;
    let (sec_ac, rest) = split(rest, ac_comp)?;
    let (sec_dc, rest) = split(rest, dc_comp)?;
    let (sec_rle, _) = split(rest, rle_comp)?;

    let chans = &geom.chans;
    let (schemes, sets) = classify(chans, &rules);
    let sizes: Vec<u64> = chans.iter().map(|ch| ch.bytes() as u64).collect();
    let need = |sch: Scheme| -> u64 {
        chans
            .iter()
            .zip(&schemes)
            .zip(&sizes)
            .filter(|((_, s), _)| **s == sch)
            .map(|(_, &n)| n)
            .sum()
    };
    validate(need(Scheme::Unknown), unk_comp, unk_unc, 0)?;
    validate(need(Scheme::Rle), rle_comp, rle_raw, rle_unc)?;

    // Buffer bounds OpenEXR checks against.
    let pixels = (geom.lines * geom.width) as u64;
    let nlossy = schemes.iter().filter(|&&s| s == Scheme::Lossy).count() as u64;
    let blocks = (geom.lines as u64).div_ceil(8) * (geom.width as u64).div_ceil(8);
    let rle_buf: u64 = chans
        .iter()
        .zip(&schemes)
        .filter(|(_, &s)| s == Scheme::Rle)
        .map(|(ch, _)| 2 * pixels * ch.ty.bytes() as u64)
        .sum();

    let super::Scratch {
        w: words_buf,
        huf,
        dwa: ds,
        inflater,
        ..
    } = scratch;
    let unknown: &[u8] = if unk_comp > 0 {
        super::inflate_exact_reuse(inflater, sec_unk, &mut ds.unknown, unk_unc as usize)?;
        &ds.unknown[..unk_unc as usize]
    } else {
        &[]
    };
    let ac: &[u16] = if ac_comp > 0 {
        if ac_count > blocks * 63 * nlossy {
            return Err(Error::invalid("DWA AC count"));
        }
        match ac_compression {
            0 => huffman::decompress_into(sec_ac, ac_count as usize, &mut ds.ac, huf)?,
            1 => {
                let n = ac_count as usize * 2;
                super::inflate_exact_reuse(inflater, sec_ac, &mut ds.bytes, n)?;
                ds.ac.clear();
                ds.ac.extend(
                    ds.bytes[..n]
                        .chunks_exact(2)
                        .map(|x| u16::from_le_bytes([x[0], x[1]])),
                );
            }
            _ => return Err(Error::invalid("DWA AC compression")),
        }
        &ds.ac
    } else {
        &[]
    };
    let dc: &[u16] = if dc_comp > 0 {
        if dc_count > blocks * nlossy {
            return Err(Error::invalid("DWA DC count"));
        }
        let n = dc_count as usize;
        super::inflate_exact_reuse(inflater, sec_dc, &mut ds.bytes, 2 * n)?;
        let b = &mut ds.bytes[..2 * n];
        super::zip::undo_predictor(b);
        // The predicted bytes hold every value's low byte, then every high
        // byte (the ZIP byte split of 2n bytes, whose halves are n each).
        let (lo, hi) = b.split_at(n);
        ds.dc.clear();
        ds.dc.extend(lo.iter().zip(hi).map(|(&l, &h)| u16::from_le_bytes([l, h])));
        &ds.dc
    } else {
        if dc_count != 0 {
            return Err(Error::invalid("DWA DC count without data"));
        }
        &[]
    };
    let rle: &[u8] = if rle_raw > 0 {
        if rle_unc > rle_buf || rle_raw > rle_buf {
            return Err(Error::invalid("DWA RLE sizes"));
        }
        let n = rle_unc as usize;
        super::inflate_exact_reuse(inflater, sec_rle, &mut ds.bytes, n)?;
        super::rle::unpack_into(&ds.bytes[..n], rle_raw as usize, &mut ds.rle)?;
        &ds.rle
    } else {
        &[]
    };

    // Output: channel-planar 16-bit words (a 32-bit sample is its low word
    // then its high word) in the worker's reused buffer. Every word of it is
    // written below (lossy, RLE and unknown channels cover the whole chunk;
    // lossy UINT rows zero their tails), so it is not re-zeroed. HALF lossy
    // channels are decoded straight into it.
    let mut offs = Vec::with_capacity(chans.len());
    let mut o = 0usize;
    for ch in chans {
        offs.push(o);
        o += ch.bytes() / 2;
    }
    let words = super::grow(words_buf, geom.raw_size / 2);
    let mut done = vec![false; chans.len()];
    let mut acr = AcReader { ac, pos: 0 };
    let mut dc_pos = 0usize;
    let luts = crate::luts::get();
    // As a fixed-size array, so the per-sample lookup needs no bounds check.
    let to_linear: &[u16; 65536] = luts.dwa_to_linear[..].try_into().expect("64K table");

    for set in &sets {
        if set.iter().any(|&i| schemes[i] != Scheme::Lossy) {
            return Err(Error::invalid("DWA colour set not lossy"));
        }
        let ch = &chans[set[0]];
        let (w, h) = (ch.width, ch.height);
        let direct = set.iter().all(|&i| chans[i].ty == SampleType::F16)
            && set[0] != set[1]
            && set[0] != set[2]
            && set[1] != set[2];
        if direct {
            // Channels in a set share geometry (checked in `classify`), and
            // distinct channels own disjoint word ranges.
            let mut outs = disjoint3(words, set.map(|i| (offs[i], w * h)));
            decode_lossy(
                &mut acr,
                dc,
                &mut dc_pos,
                w,
                h,
                Some(to_linear),
                &mut outs,
            )?;
        } else {
            let [b0, b1, b2] = &mut ds.bufs;
            let mut outs: [&mut [u16]; 3] = [
                super::grow(b0, w * h),
                super::grow(b1, w * h),
                super::grow(b2, w * h),
            ];
            decode_lossy(
                &mut acr,
                dc,
                &mut dc_pos,
                w,
                h,
                Some(to_linear),
                &mut outs,
            )?;
            for (k, &ci) in set.iter().enumerate() {
                store_words(words, &chans[ci], offs[ci], &ds.bufs[k][..w * h]);
            }
        }
        for &ci in set {
            done[ci] = true;
        }
    }

    let mut rle_planes = 0usize;
    let mut unk_pos = 0usize;
    for (ci, ch) in chans.iter().enumerate() {
        if done[ci] {
            // RLE/unknown buffers still advance for their own schemes only.
            continue;
        }
        if ch.width == 0 || ch.height == 0 {
            continue;
        }
        match schemes[ci] {
            Scheme::Lossy => {
                let lin = if ch.p_linear {
                    None
                } else {
                    Some(to_linear)
                };
                let n = ch.width * ch.height;
                if ch.ty == SampleType::F16 {
                    let out = &mut words[offs[ci]..offs[ci] + n];
                    decode_lossy(
                        &mut acr,
                        dc,
                        &mut dc_pos,
                        ch.width,
                        ch.height,
                        lin,
                        &mut [out],
                    )?;
                } else {
                    let buf = super::grow(&mut ds.bufs[0], n);
                    decode_lossy(
                        &mut acr,
                        dc,
                        &mut dc_pos,
                        ch.width,
                        ch.height,
                        lin,
                        &mut [&mut *buf],
                    )?;
                    store_words(words, ch, offs[ci], buf);
                }
            }
            Scheme::Rle => {
                let bpe = ch.ty.bytes();
                let plane = ch.width * ch.height;
                let src = &rle[rle_planes..rle_planes + bpe * plane];
                let dst = &mut words[offs[ci]..offs[ci] + bpe * plane / 2];
                unsplit_planes(src, plane, dst);
                rle_planes += bpe * plane;
            }
            Scheme::Unknown => {
                let n = ch.bytes();
                if unk_pos + n > unknown.len() {
                    return Err(Error::invalid("DWA unknown data too short"));
                }
                let dst = &mut words[offs[ci]..offs[ci] + n / 2];
                for (d, s) in dst
                    .iter_mut()
                    .zip(unknown[unk_pos..unk_pos + n].chunks_exact(2))
                {
                    *d = u16::from_le_bytes([s[0], s[1]]);
                }
                unk_pos += n;
            }
        }
    }
    Ok(())
}

/// `words[2 i]`, `words[2 i + 1]` = the low and high words of
/// `imath_h2f_bits(halves[i])`. The SIMD twin converts whole NaN-free
/// groups of 8; the scalar loop (its oracle) takes a declined group and the
/// tail, handing back to the twin at the next group boundary.
fn halves_to_f32_words(isa: crate::simd::Isa, halves: &[u16], words: &mut [u16]) {
    let n = halves.len();
    let mut i = crate::simd::dwa_f32_words(isa, halves, words);
    while i < n {
        let b = imath_h2f_bits(halves[i]);
        words[2 * i] = b as u16;
        words[2 * i + 1] = (b >> 16) as u16;
        i += 1;
        if i % 8 == 0 && i + 8 <= n {
            i += crate::simd::dwa_f32_words(isa, &halves[i..], &mut words[2 * i..]);
        }
    }
}

/// Three disjoint `len`-word ranges of `words` starting at the given
/// (pairwise distinct, non-overlapping) offsets, in argument order.
fn disjoint3(words: &mut [u16], r: [(usize, usize); 3]) -> [&mut [u16]; 3] {
    let mut order = [0usize, 1, 2];
    order.sort_by_key(|&k| r[k].0);
    let mut rest = words;
    let mut base = 0usize;
    let mut got: [Option<&mut [u16]>; 3] = [None, None, None];
    for k in order {
        let (off, len) = r[k];
        let tail = std::mem::take(&mut rest);
        let (_, tail) = tail.split_at_mut(off - base);
        let (mine, tail) = tail.split_at_mut(len);
        got[k] = Some(mine);
        rest = tail;
        base = off + len;
    }
    got.map(|g| g.expect("all three ranges assigned"))
}

/// Store a lossy channel's decoded halves (`width * height`, row-major) as
/// its planar words at `off`: HALF as is, FLOAT as Imath's half-to-float,
/// UINT as OpenEXR leaves it (each row's halves in the first half of the
/// row's words, the rest zero).
fn store_words(words: &mut [u16], ch: &ChanGeom<'_>, off: usize, halves: &[u16]) {
    let n = ch.width * ch.height;
    match ch.ty {
        SampleType::F16 => words[off..off + n].copy_from_slice(&halves[..n]),
        SampleType::F32 => {
            halves_to_f32_words(crate::simd::isa(), &halves[..n], &mut words[off..off + 2 * n]);
        }
        SampleType::U32 => {
            let w = ch.width;
            if w == 0 {
                return;
            }
            for (row, src) in words[off..off + 2 * n]
                .chunks_exact_mut(2 * w)
                .zip(halves.chunks_exact(w))
            {
                let (head, tail) = row.split_at_mut(w);
                head.copy_from_slice(src);
                tail.fill(0);
            }
        }
    }
}

/// Rebuild samples of `bpe = src.len() / plane` bytes from byte planes
/// (byte `k` of sample `i` is `src[k * plane + i]`) as little-endian words
/// (a 4-byte sample is two words, low first). The fixed-width arms (2 and 4:
/// every sample type) are zipped loops the compiler vectorizes; the indexed
/// loop is the general form and their oracle.
fn unsplit_planes(src: &[u8], plane: usize, dst: &mut [u16]) {
    match src.len().checked_div(plane) {
        Some(2) => {
            let (a, b) = src.split_at(plane);
            for ((d, &x), &y) in dst.iter_mut().zip(a).zip(&b[..plane]) {
                *d = u16::from_le_bytes([x, y]);
            }
        }
        Some(4) => {
            let (a, r) = src.split_at(plane);
            let (b, r) = r.split_at(plane);
            let (c, e) = r.split_at(plane);
            for ((((d, &x), &y), &z), &w) in dst
                .chunks_exact_mut(2)
                .zip(a)
                .zip(b)
                .zip(c)
                .zip(&e[..plane])
            {
                d[0] = u16::from_le_bytes([x, y]);
                d[1] = u16::from_le_bytes([z, w]);
            }
        }
        _ => unsplit_planes_indexed(src, plane, dst),
    }
}

/// The indexed form of [`unsplit_planes`] (even `bpe`).
fn unsplit_planes_indexed(src: &[u8], plane: usize, dst: &mut [u16]) {
    if plane == 0 {
        return;
    }
    let bpe = src.len() / plane;
    for i in 0..plane {
        for k in (0..bpe).step_by(2) {
            dst[i * bpe / 2 + k / 2] =
                u16::from_le_bytes([src[k * plane + i], src[(k + 1) * plane + i]]);
        }
    }
}

fn split(d: &[u8], n: u64) -> Result<(&[u8], &[u8])> {
    if n > d.len() as u64 {
        return Err(Error::invalid("DWA section past end of chunk"));
    }
    Ok(d.split_at(n as usize))
}

fn validate(required: u64, compressed: u64, uncompressed: u64, extra: u64) -> Result<()> {
    if required > 0 {
        if uncompressed < required || compressed == 0 {
            return Err(Error::invalid("DWA section size"));
        }
    } else if uncompressed > 0 || compressed > 0 || extra > 0 {
        return Err(Error::invalid("DWA section without channels"));
    }
    Ok(())
}

#[cfg(test)]
mod tests {
    use super::*;

    fn lcg(seed: &mut u32) -> u32 {
        *seed = seed.wrapping_mul(1_103_515_245).wrapping_add(12345);
        *seed >> 8
    }

    #[test]
    fn f32_words_twins_match_scalar() {
        // Every half, in an order that puts NaNs in some groups and not in
        // others, at several lengths and offsets.
        let mut seed = 99u32;
        let mut all: Vec<u16> = (0..=u16::MAX).collect();
        for i in (1..all.len()).rev() {
            let j = lcg(&mut seed) as usize % (i + 1);
            all.swap(i, j);
        }
        // A run of NaN-free values so whole clean groups occur too.
        all.extend((0..4000u16).map(|v| v % 0x7c00));
        let mut isas = crate::simd::available();
        isas.push(crate::simd::Isa::Scalar);
        for start in [0usize, 3, 8] {
            for len in [0usize, 1, 7, 8, 9, 15, 16, 17, 100, all.len() - start] {
                let h = &all[start..start + len];
                let want: Vec<u16> = h
                    .iter()
                    .flat_map(|&v| {
                        let b = imath_h2f_bits(v);
                        [b as u16, (b >> 16) as u16]
                    })
                    .collect();
                for &isa in &isas {
                    let mut got = vec![0xaaaau16; 2 * len];
                    halves_to_f32_words(isa, h, &mut got);
                    assert_eq!(got, want, "{isa:?} start {start} len {len}");
                }
            }
        }
    }

    #[test]
    fn unsplit_planes_matches_indexed() {
        let mut seed = 7u32;
        for bpe in [2usize, 4, 6, 8] {
            for plane in [0usize, 1, 2, 15, 16, 17, 33, 1000] {
                let src: Vec<u8> = (0..bpe * plane).map(|_| lcg(&mut seed) as u8).collect();
                let mut a = vec![0u16; bpe * plane / 2];
                let mut b = vec![1u16; bpe * plane / 2];
                unsplit_planes(&src, plane, &mut a);
                unsplit_planes_indexed(&src, plane, &mut b);
                assert_eq!(a, b, "bpe {bpe} plane {plane}");
            }
        }
    }
}
