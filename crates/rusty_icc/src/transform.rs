//! Colour transforms between profiles.

use std::sync::atomic::{AtomicUsize, Ordering};
use std::sync::{Arc, OnceLock};

use crate::color::delta_e_2000;
use crate::curve::InvCurve;
use crate::error::{Error, Result};
use crate::fast::FastShaper;
use crate::lut::{ClutEval, FixedClut};
use crate::pipeline::{
    build_chain, u8_point, Chain, GamutChain, LutOutChain, SegChain, ShaperChain, ShaperLink,
};
use crate::profile::{ColorSpace, Profile};
use crate::simd;

/// ICC rendering intent.
#[derive(Debug, Clone, Copy, PartialEq, Eq, Hash, Default)]
pub enum Intent {
    /// Perceptual (`A2B0`/`B2A0`).
    #[default]
    Perceptual = 0,
    /// Media-relative colorimetric (`A2B1`/`B2A1`).
    RelativeColorimetric = 1,
    /// Saturation (`A2B2`/`B2A2`).
    Saturation = 2,
    /// ICC-absolute colorimetric: relative colorimetric tags plus media
    /// white point scaling.
    AbsoluteColorimetric = 3,
}

/// Pixel layout: channel order and alpha. Samples are interleaved.
///
/// Value conventions: device channels (grey, RGB, CMYK ink) are `0..=MAX`
/// for integers and `0.0..=1.0` for `f32`. L*a*b* uses the ICC v4
/// encodings for integers (`u8`: `L * 255 / 100`, `a + 128`; `u16`:
/// `L * 65535 / 100`, `(a + 128) * 257`) and natural units for `f32`
/// (`L` in `0..=100`, `a`/`b` around zero). Alpha is copied through
/// unchanged, or set opaque when the source has none.
#[derive(Debug, Clone, Copy, PartialEq, Eq, Hash)]
pub enum Layout {
    /// Grey.
    Gray,
    /// Grey, alpha.
    GrayAlpha,
    /// Red, green, blue.
    Rgb,
    /// Red, green, blue, alpha.
    Rgba,
    /// Cyan, magenta, yellow, black.
    Cmyk,
    /// Cyan, magenta, yellow, black, alpha.
    Cmyka,
    /// L*, a*, b*.
    Lab,
    /// L*, a*, b*, alpha.
    Laba,
}

impl Layout {
    /// Samples per pixel.
    #[must_use]
    pub const fn channels(self) -> usize {
        self.color_channels() + self.has_alpha() as usize
    }

    /// Colour samples per pixel (alpha excluded).
    #[must_use]
    pub const fn color_channels(self) -> usize {
        match self {
            Self::Gray | Self::GrayAlpha => 1,
            Self::Rgb | Self::Rgba | Self::Lab | Self::Laba => 3,
            Self::Cmyk | Self::Cmyka => 4,
        }
    }

    /// `true` when the last sample is alpha.
    #[must_use]
    pub const fn has_alpha(self) -> bool {
        matches!(
            self,
            Self::GrayAlpha | Self::Rgba | Self::Cmyka | Self::Laba
        )
    }

    /// The colour space this layout carries.
    #[must_use]
    pub const fn color_space(self) -> ColorSpace {
        match self {
            Self::Gray | Self::GrayAlpha => ColorSpace::Gray,
            Self::Rgb | Self::Rgba => ColorSpace::Rgb,
            Self::Cmyk | Self::Cmyka => ColorSpace::Cmyk,
            Self::Lab | Self::Laba => ColorSpace::Lab,
        }
    }

    const fn is_lab(self) -> bool {
        matches!(self, Self::Lab | Self::Laba)
    }
}

/// Transform options.
#[derive(Debug, Clone, Copy, PartialEq, Eq, Default)]
pub struct Options {
    /// Black-point compensation (the Adobe / `LittleCMS` algorithm). Ignored
    /// for absolute colorimetric; as in `LittleCMS`, perceptual and
    /// saturation transforms into ICC v4 profiles always compensate.
    pub black_point_compensation: bool,
}

/// How a transform evaluates `f32` pixels ([`Transform::with_precision`]).
/// Integer pixels are exact either way.
#[derive(Debug, Clone, Copy, PartialEq, Eq, Hash, Default)]
#[non_exhaustive]
pub enum Precision {
    /// The exact pipeline (the default): every `f32` pixel goes through the
    /// profiles' curves, matrices and tables in `f64`.
    #[default]
    Exact,
    /// Tables and the device-link grid in `f32`, as fast colour engines do:
    /// matrix-shaper RGB to RGB through 4096-entry forward and 16384-entry
    /// inverse curve tables (within 1e-5 of the exact path per channel,
    /// output clamped to `0..=1` as the exact path is), every other non-Lab
    /// transform through its device-link grid (33 points per axis for three
    /// inputs, 17 for four) interpolated tetrahedrally (within CIEDE2000 1.0
    /// of the exact path on the measured corpus). Deterministic: the same
    /// `f32` operations in the same order on every target, with or without
    /// SIMD. Transforms whose tables cannot meet the bound (a pure power-law
    /// output curve such as Adobe RGB's, Lab on either side, grey) keep the
    /// exact path.
    Fast,
}

mod sealed {
    pub trait Sealed {}
    impl Sealed for u8 {}
    impl Sealed for u16 {}
    impl Sealed for f32 {}
}

/// Sample type kinds (implementation detail of [`Sample`]).
#[doc(hidden)]
#[derive(Debug, Clone, Copy, PartialEq, Eq)]
pub enum SampleKind {
    U8,
    U16,
    F32,
}

/// A pixel sample type: `u8`, `u16` or `f32`.
pub trait Sample: Copy + Send + Sync + 'static + sealed::Sealed {
    #[doc(hidden)]
    const KIND: SampleKind;
    /// Entries of a per-value input table (a power of two), 0 for `f32`.
    #[doc(hidden)]
    const TABLE_LEN: usize;
    #[doc(hidden)]
    fn to_unit(self) -> f64;
    #[doc(hidden)]
    fn from_unit(v: f64) -> Self;
    #[doc(hidden)]
    fn opaque() -> Self;
    #[doc(hidden)]
    fn index(self) -> usize;
    /// The slice as `f32` samples, when that is what it holds.
    #[doc(hidden)]
    fn f32s(s: &[Self]) -> Option<&[f32]> {
        let _ = s;
        None
    }
    #[doc(hidden)]
    fn f32s_mut(s: &mut [Self]) -> Option<&mut [f32]> {
        let _ = s;
        None
    }
    /// The slice as `u8` samples, when that is what it holds.
    #[doc(hidden)]
    fn u8s(s: &[Self]) -> Option<&[u8]> {
        let _ = s;
        None
    }
    #[doc(hidden)]
    fn u8s_mut(s: &mut [Self]) -> Option<&mut [u8]> {
        let _ = s;
        None
    }
    /// The slice as `u16` samples, when that is what it holds.
    #[doc(hidden)]
    fn u16s(s: &[Self]) -> Option<&[u16]> {
        let _ = s;
        None
    }
    #[doc(hidden)]
    fn u16s_mut(s: &mut [Self]) -> Option<&mut [u16]> {
        let _ = s;
        None
    }
}

impl Sample for u8 {
    const KIND: SampleKind = SampleKind::U8;
    const TABLE_LEN: usize = 256;
    fn to_unit(self) -> f64 {
        f64::from(self) / 255.0
    }
    fn from_unit(v: f64) -> Self {
        // No `clamp01(v)` first: the float-to-integer cast saturates and
        // maps NaN to 0, so every `v` gives the value the clamped form gave
        // (`v >= 1` lands at or above 255.5 and `v < 0` below 0.5, which
        // truncate or saturate to the same ends; proven over a sweep in
        // `from_unit_matches_the_clamped_form`).
        (v * 255.0 + 0.5) as Self
    }
    fn opaque() -> Self {
        Self::MAX
    }
    fn index(self) -> usize {
        usize::from(self)
    }
    fn u8s(s: &[Self]) -> Option<&[u8]> {
        Some(s)
    }
    fn u8s_mut(s: &mut [Self]) -> Option<&mut [u8]> {
        Some(s)
    }
}

impl Sample for u16 {
    const KIND: SampleKind = SampleKind::U16;
    const TABLE_LEN: usize = 65536;
    fn to_unit(self) -> f64 {
        f64::from(self) / 65535.0
    }
    fn from_unit(v: f64) -> Self {
        // As for `u8`: the saturating cast already does the clamp's job.
        (v * 65535.0 + 0.5) as Self
    }
    fn opaque() -> Self {
        Self::MAX
    }
    fn index(self) -> usize {
        usize::from(self)
    }
    fn u16s(s: &[Self]) -> Option<&[u16]> {
        Some(s)
    }
    fn u16s_mut(s: &mut [Self]) -> Option<&mut [u16]> {
        Some(s)
    }
}

impl Sample for f32 {
    const KIND: SampleKind = SampleKind::F32;
    const TABLE_LEN: usize = 0;
    fn to_unit(self) -> f64 {
        f64::from(self)
    }
    fn from_unit(v: f64) -> Self {
        v as Self
    }
    fn opaque() -> Self {
        1.0
    }
    fn index(self) -> usize {
        0
    }
    fn f32s(s: &[Self]) -> Option<&[f32]> {
        Some(s)
    }
    fn f32s_mut(s: &mut [Self]) -> Option<&mut [f32]> {
        Some(s)
    }
}

/// Entries of the inverse-curve output tables, indexed by `sqrt(linear)`
/// so the steep start of a gamma inverse is sampled densely.
pub(crate) const OUT_TABLE: usize = 16384;

/// Matrix-shaper engine for integer samples. Tables are flat, one fixed-size
/// row per channel (`TABLE_LEN` input entries, `OUT_TABLE` output entries),
/// so the hot loop slices a row once and its indexing is provably in bounds.
struct ShaperEngine {
    link: ShaperLink,
    /// Per input channel, the first channel whose curve is the same (so its
    /// table row holds the same values): the SIMD kernels read that row, so
    /// three channels of one curve share one row's cache lines.
    row_of: [usize; 3],
    m: [[f32; 3]; 3],
    off: [f32; 3],
    in8: Box<[f32]>,
    in16: OnceLock<Box<[f32]>>,
    out: Box<[[f32; OUT_TABLE]]>,
    /// For a one-channel 8-bit source: the colour values the loop writes for
    /// every input byte (first `GREY_OUT` of each entry), from [`Self::run`]
    /// over a ramp, so a lookup gives exactly what the loop gives. Built by
    /// the first call with at least 256 pixels.
    grey8: OnceLock<Box<[[u8; 3]; 256]>>,
    /// Per output channel, the first channel whose output curve is the same
    /// (so its output table row holds the same values).
    out_of: [usize; 3],
    /// For three 8-bit output channels: the exact 8-bit codes of each output
    /// table by threshold ([`U8Codes`]), `None` when a table does not
    /// qualify. Built by the first 8-bit call the AVX2 kernel serves.
    codes8: OnceLock<Option<[Arc<U8Codes>; 3]>>,
    /// [`Precision::Fast`] tables, built by the first fast `f32` call;
    /// `None` when they miss the error budget (the exact path then serves).
    fast: OnceLock<Option<FastShaper>>,
}

/// The 8-bit code of every linear value through one `sqrt`-indexed output
/// table, by threshold. [`ShaperEngine::lookup_out`] then
/// [`Sample::from_unit`] is a non-decreasing step function of the value
/// (checked when this is built: see [`U8Codes::new`]), so the code of `v`
/// is the number of steps at or below `v`. The steps are found exactly, by
/// bisection over the `f32` bit patterns, and filed in buckets of the
/// value's bit pattern (`2^b` per octave) holding at most one step each: a
/// bucket's entry is its first code plus one (bits 23 and up) and the low
/// bits of its step (bits 0..23; `1 << shift` when it has none).
pub(crate) struct U8Codes {
    pub(crate) table: Box<[u32]>,
    /// Subtracted from the clamped value's bit pattern (then clamped at 0)
    /// before the bucket (`>> shift`) and the in-bucket position (low bits).
    pub(crate) base: i32,
    pub(crate) shift: u32,
}

impl U8Codes {
    /// `from_unit(lookup_out(table, v))` for `u8`: the function filed.
    fn code(table: &[f32; OUT_TABLE], v: f32) -> u8 {
        u8::from_unit(f64::from(ShaperEngine::lookup_out(table, v)))
    }

    /// `None` unless the table's codes are a non-decreasing function of the
    /// clamped value (proven here, see the comments) and fit the buckets.
    fn new(t: &[f32; OUT_TABLE]) -> Option<Self> {
        const ONE: u32 = 0x3F80_0000;
        let u = |y: f32| u8::from_unit(f64::from(y));
        // Within cell `i` the value is `t[i] + (t[i+1] - t[i]) * f`, with `f`
        // in 0..=1 a non-decreasing function of the value (the clamp, `sqrt`,
        // the product and the truncation all round monotonically); with a
        // non-negative difference every rounding step after it is monotone.
        if !t.windows(2).all(|w| w[1] - w[0] >= 0.0) {
            return None;
        }
        // Across the boundary into cell `i`: cell `i - 1` gives at most its
        // value at the largest position below `i` (an `f32`, so at most the
        // float before `i`, and that minus `i - 1` is exact); cell `i`
        // starts at `t[i]`. The codes must not fall there.
        for i in 1..OUT_TABLE - 1 {
            let f = f32::from_bits((i as f32).to_bits() - 1) - (i - 1) as f32;
            let left = t[i - 1] + (t[i] - t[i - 1]) * f;
            if u(left) > u(t[i]) {
                return None;
            }
        }
        // The steps: for each code above the code of 0, the smallest value in
        // 0..=1 reaching it (bisection on the bit pattern, which orders the
        // non-negative floats).
        let at = |bits: u32| Self::code(t, f32::from_bits(bits));
        let (c0, c1) = (at(0), at(ONE));
        let mut steps = Vec::with_capacity(usize::from(c1 - c0));
        for k in c0 + 1..=c1 {
            let (mut lo, mut hi) = (0u32, ONE);
            while hi - lo > 1 {
                let mid = lo + (hi - lo) / 2;
                if at(mid) >= k {
                    hi = mid;
                } else {
                    lo = mid;
                }
            }
            steps.push(hi);
        }
        // Two codes at one value cannot be told apart by one step.
        if steps.windows(2).any(|w| w[0] == w[1]) {
            return None;
        }
        // Buckets from the octave of the first step (every value below it is
        // code `c0`): `base` is one less than that octave's first bit
        // pattern, so every step's offset is at least 1, and every value below
        // the octave lands at offset 0, under every step.
        let octave = steps.first().map_or(ONE, |&b| b & 0x7F80_0000);
        if octave < 64 << 23 {
            return None;
        }
        let base = octave - 1;
        for b in 7..=12u32 {
            let shift = 23 - b;
            let n = ((ONE - base) >> shift) as usize + 1;
            let mut table = vec![0u32; n];
            let mut next = 0usize;
            let mut ok = true;
            for (j, e) in table.iter_mut().enumerate() {
                let start = (j as u32) << shift;
                let end = start + (1 << shift);
                let first = u32::from(c0) + next as u32;
                let mut low = 1u32 << shift;
                if let Some(&st) = steps.get(next) {
                    if st - base < end {
                        low = st - base - start;
                        next += 1;
                        if steps.get(next).is_some_and(|&s2| s2 - base < end) {
                            ok = false;
                            break;
                        }
                    }
                }
                *e = ((first + 1) << 23) | low;
            }
            if ok && next == steps.len() {
                return Some(Self {
                    table: table.into_boxed_slice(),
                    base: base as i32,
                    shift,
                });
            }
        }
        None
    }

    /// The code of `v` by the table: the scalar form of the kernel's lookup.
    #[cfg(test)]
    fn lookup(&self, v: f32) -> u8 {
        let c = if v > 0.0 { v.min(1.0) } else { 0.0 };
        let off = (c.to_bits() as i32 - self.base).max(0) as u32;
        let e = self.table[(off >> self.shift) as usize];
        let low = off & ((1 << self.shift) - 1);
        ((e >> 23) - u32::from(low < (e & 0x7F_FFFF))) as u8
    }
}

impl ShaperEngine {
    fn table(link: &ShaperLink, n: usize) -> Box<[f32]> {
        let curves = &link.in_curves;
        let mut table = Vec::with_capacity(curves.len() * n);
        for (k, c) in curves.iter().enumerate() {
            // The channels of an RGB profile nearly always share one curve:
            // the same curve gives the same values, so its row is copied.
            if let Some(j) = curves[..k].iter().position(|p| p.same_as(c)) {
                table.extend_from_within(j * n..(j + 1) * n);
            } else {
                table.extend((0..n).map(|i| c.eval(i as f64 / (n - 1) as f64) as f32));
            }
        }
        table.into_boxed_slice()
    }

    fn new(link: ShaperLink) -> Self {
        let in8 = Self::table(&link, 256);
        let mut out = vec![[0.0f32; OUT_TABLE]; link.out_curves.len()].into_boxed_slice();
        for (k, c) in link.out_curves.iter().enumerate() {
            let (done, rest) = out.split_at_mut(k);
            let row = &mut rest[0];
            // As for the input tables: the same curve's row is copied.
            if let Some(j) = link.out_curves[..k].iter().position(|p| p.same_as(c)) {
                *row = done[j];
                continue;
            }
            // The same values either way; the prepared form skips a `pow`
            // per entry for sRGB-shaped curves.
            let prepared = InvCurve::new(c);
            for (i, v) in row.iter_mut().enumerate() {
                let t = i as f64 / (OUT_TABLE - 1) as f64;
                let y = t * t;
                *v = prepared
                    .as_ref()
                    .map_or_else(|| c.eval_inverse(y), |p| p.eval(y)) as f32;
            }
        }
        let m = link.m.map(|r| r.map(|v| v as f32));
        let off = link.off.map(|v| v as f32);
        let row_of = core::array::from_fn(|k| {
            let cs = &link.in_curves;
            cs.get(k)
                .and_then(|c| cs[..k].iter().position(|p| p.same_as(c)))
                .unwrap_or(k)
        });
        let out_of = core::array::from_fn(|k| {
            let cs = &link.out_curves;
            cs.get(k)
                .and_then(|c| cs[..k].iter().position(|p| p.same_as(c)))
                .unwrap_or(k)
        });
        Self {
            out_of,
            codes8: OnceLock::new(),
            fast: OnceLock::new(),
            link,
            row_of,
            m,
            off,
            in8,
            in16: OnceLock::new(),
            out,
            grey8: OnceLock::new(),
        }
    }

    /// [`Self::run_loop`] for one 8-bit colour channel in and `k` out (`k` as
    /// the caller dispatches: 3 for three output channels, 1 otherwise), through
    /// [`ShaperEngine::grey8`]. `false`, with nothing done, while the table
    /// is not built and `src` has fewer than 256 pixels (building it costs
    /// 256 pixels of the loop).
    #[allow(clippy::too_many_arguments)]
    fn run_grey8(
        &self,
        src: &[u8],
        dst: &mut [u8],
        sc: usize,
        dc: usize,
        src_alpha: bool,
        dst_alpha: bool,
        k: usize,
    ) -> bool {
        let table = match self.grey8.get() {
            Some(t) => t,
            None if src.len() / sc >= 256 => self.grey8.get_or_init(|| {
                let ramp: [u8; 256] = core::array::from_fn(|v| v as u8);
                let mut flat = [0u8; 768];
                let mut t = Box::new([[0u8; 3]; 256]);
                if k == 3 {
                    self.run_loop::<u8, 1, 3>(&self.in8, &ramp, &mut flat, 1, 3, false, false);
                    for (row, v) in t.iter_mut().zip(flat.chunks_exact(3)) {
                        row.copy_from_slice(v);
                    }
                } else {
                    self.run_loop::<u8, 1, 1>(
                        &self.in8,
                        &ramp,
                        &mut flat[..256],
                        1,
                        1,
                        false,
                        false,
                    );
                    for (row, v) in t.iter_mut().zip(&flat[..256]) {
                        row[0] = *v;
                    }
                }
                t
            }),
            None => return false,
        };
        if k == 3 {
            grey8_loop::<3>(table, src, dst, sc, dc, src_alpha, dst_alpha);
        } else {
            grey8_loop::<1>(table, src, dst, sc, dc, src_alpha, dst_alpha);
        }
        true
    }

    /// [`U8Codes`] of the three output tables (one per distinct curve,
    /// shared), or `None` when one does not qualify.
    fn build_codes8(&self) -> Option<[Arc<U8Codes>; 3]> {
        if self.out.len() != 3 {
            return None;
        }
        let mut made: [Option<Arc<U8Codes>>; 3] = [None, None, None];
        for k in 0..3 {
            let j = self.out_of[k];
            made[k] = Some(match &made[j] {
                Some(c) if j < k => Arc::clone(c),
                _ => Arc::new(U8Codes::new(&self.out[k])?),
            });
        }
        let [Some(a), Some(b), Some(c)] = made else {
            return None;
        };
        Some([a, b, c])
    }

    /// The fast `f32` path, when its tables qualify: `false`, with nothing
    /// done, otherwise. The AVX2 kernel takes the leading multiple of 8
    /// pixels and the scalar twin the rest (the same values either way).
    fn run_fast_f32(
        &self,
        src: &[f32],
        dst: &mut [f32],
        sc_dc: (usize, usize),
        alpha: (bool, bool),
        exact: &ShaperChain<'_>,
    ) -> bool {
        let fast = self
            .fast
            .get_or_init(|| FastShaper::new(&self.link.in_curves, &self.link.out_curves));
        let Some(f) = fast else {
            return false;
        };
        let view = f.view(self.m, self.off);
        let mut near = Vec::new();
        let done = simd::shaper_f32_fast(&view, src, dst, sc_dc, alpha, &mut near);
        let (s, d) = (&src[done * sc_dc.0..], &mut dst[done * sc_dc.1..]);
        view.run(s, d, sc_dc, alpha, (done, &mut near));
        // The few pixels whose fast linear value sits next to an output
        // curve's break: the exact path's values.
        for k in near {
            let sp = &src[k * sc_dc.0..];
            let out = exact.eval([f64::from(sp[0]), f64::from(sp[1]), f64::from(sp[2])]);
            for (c, v) in out.iter().enumerate() {
                dst[k * sc_dc.1 + c] = *v as f32;
            }
        }
        true
    }

    fn in16(&self) -> &[f32] {
        self.in16.get_or_init(|| Self::table(&self.link, 65536))
    }

    #[inline]
    fn lookup_out(table: &[f32; OUT_TABLE], lin: f32) -> f32 {
        let lin = if lin > 0.0 { lin.min(1.0) } else { 0.0 };
        let pos = lin.sqrt() * (OUT_TABLE - 1) as f32;
        // `lin` is clamped to 0..=1 (NaN to 0), so `pos` is in
        // 0..=OUT_TABLE-1 and the i32 conversion (one `cvttss2si`) equals
        // the saturating `as usize`: matrix-shaper -12.4% Ir. The same trick
        // on `Sample::from_unit`'s f64 -> u8/u16 cast measured worse
        // (+262K here, +0.2..0.35% on the grid paths), so it is not used.
        let i = (pos as i32 as usize).min(OUT_TABLE - 2);
        let f = pos - i as f32;
        table[i] + (table[i + 1] - table[i]) * f
    }

    /// The integer path: [`Self::run_loop`], or for one 8-bit channel in
    /// its table ([`Self::run_grey8`]).
    #[allow(clippy::too_many_arguments)]
    #[inline(never)]
    fn run<T: Sample, const IN: usize, const OUT: usize>(
        &self,
        tables: &[f32],
        src: &[T],
        dst: &mut [T],
        sc: usize,
        dc: usize,
        src_alpha: bool,
        dst_alpha: bool,
    ) {
        if IN == 1 {
            if let Some(s) = T::u8s(src) {
                if let Some(d) = T::u8s_mut(dst) {
                    if self.run_grey8(s, d, sc, dc, src_alpha, dst_alpha, OUT) {
                        return;
                    }
                }
            }
        }
        let done = if IN == 3 && OUT == 3 {
            self.run_simd(tables, src, dst, (sc, dc), (src_alpha, dst_alpha))
        } else {
            0
        };
        let (src, dst) = (&src[done * sc..], &mut dst[done * dc..]);
        self.run_loop::<T, IN, OUT>(tables, src, dst, sc, dc, src_alpha, dst_alpha);
    }

    /// [`Self::run_loop`] for three channels in and out, in the AVX2 kernel
    /// ([`simd::shaper_u8`], [`simd::shaper_u16`]) on the leading multiple of
    /// 8 pixels: the same values, bit for bit. Returns the pixels done (0
    /// without AVX2, on other targets, or for other layouts).
    fn run_simd<T: Sample>(
        &self,
        tables: &[f32],
        src: &[T],
        dst: &mut [T],
        sc_dc: (usize, usize),
        alpha: (bool, bool),
    ) -> usize {
        let [o0, o1, o2] = match &self.out[..] {
            [a, b, c] => [a, b, c],
            _ => return 0,
        };
        let row = |k: usize, n: usize| tables.get(self.row_of[k] * n..(self.row_of[k] + 1) * n);
        if let (Some(s), Some(d)) = (T::u8s(src), T::u8s_mut(dst)) {
            let rows =
                [row(0, 256), row(1, 256), row(2, 256)].map(|r| r.and_then(|r| r.try_into().ok()));
            let [Some(r0), Some(r1), Some(r2)] = rows else {
                return 0;
            };
            let t = simd::ShaperTables {
                rows: [r0, r1, r2],
                outs: [o0, o1, o2],
                m: self.m,
                off: self.off,
            };
            if s.len() < 8 * sc_dc.0 || !simd::avx2() {
                return 0;
            }
            let codes = self.codes8.get_or_init(|| self.build_codes8());
            let codes = codes.as_ref().map(|c| {
                c.each_ref().map(|u| simd::Codes8 {
                    table: &u.table,
                    base: u.base,
                    shift: u.shift,
                })
            });
            return simd::shaper_u8(&t, codes.as_ref(), s, d, sc_dc, alpha);
        }
        if let (Some(s), Some(d)) = (T::u16s(src), T::u16s_mut(dst)) {
            let rows = [row(0, 65536), row(1, 65536), row(2, 65536)]
                .map(|r| r.and_then(|r| r.try_into().ok()));
            let [Some(r0), Some(r1), Some(r2)] = rows else {
                return 0;
            };
            let t = simd::ShaperTables {
                rows: [r0, r1, r2],
                outs: [o0, o1, o2],
                m: self.m,
                off: self.off,
            };
            return simd::shaper_u16(&t, s, d, sc_dc, alpha);
        }
        0
    }

    /// The integer hot loop, monomorphised on the colour channel counts so
    /// every per-channel index is a constant.
    #[allow(clippy::too_many_arguments)]
    fn run_loop<T: Sample, const IN: usize, const OUT: usize>(
        &self,
        tables: &[f32],
        src: &[T],
        dst: &mut [T],
        sc: usize,
        dc: usize,
        src_alpha: bool,
        dst_alpha: bool,
    ) {
        let n = T::TABLE_LEN;
        let rows: [&[f32]; IN] = core::array::from_fn(|c| &tables[c * n..(c + 1) * n]);
        let outs: [&[f32; OUT_TABLE]; OUT] = core::array::from_fn(|o| &self.out[o]);
        for (sp, dp) in src.chunks_exact(sc).zip(dst.chunks_exact_mut(dc)) {
            let mut lin = [0.0f32; 3];
            for c in 0..IN {
                lin[c] = rows[c][sp[c].index() & (n - 1)];
            }
            for o in 0..OUT {
                let row = self.m[o];
                let v = row[0] * lin[0] + row[1] * lin[1] + row[2] * lin[2] + self.off[o];
                dp[o] = T::from_unit(f64::from(Self::lookup_out(outs[o], v)));
            }
            if dst_alpha {
                dp[dc - 1] = if src_alpha { sp[sc - 1] } else { T::opaque() };
            }
        }
    }
}

enum Engine {
    Shaper(Box<ShaperEngine>),
    Grid(ClutEval),
}

/// A colour transform from one profile (and layout) to another.
///
/// `u8`/`u16` data runs through a matrix-shaper fast path when both ends
/// are matrix/TRC (or grey TRC) profiles and nothing else intervenes;
/// otherwise through a device-link grid (33 points per axis for 3 inputs,
/// 17 for 4, 4096 for 1) interpolated tetrahedrally (3-D) or linearly over
/// tetrahedral (4-D). `f32` data always runs the exact pipeline.
pub struct Transform {
    src_layout: Layout,
    dst_layout: Layout,
    chain: Arc<Chain>,
    engine: Engine,
    /// [`Chain::head_on_u8_points`], built by the first `f32` call whose
    /// pixels are 8-bit points.
    u8_head: OnceLock<Option<Box<[[f64; 256]]>>>,
    /// The grid's `(cell, fraction)` for every 16-bit value
    /// ([`Axis16`]), built once 16-bit calls have brought at least
    /// [`AXIS16_MIN_PIXELS`] pixels (counted in `pixels16`).
    axis16: OnceLock<Axis16>,
    pixels16: AtomicUsize,
    /// The grid's `(cell, fraction)` for every 8-bit value, built by the
    /// first 8-bit call under [`AXIS8_CALL_PIXELS`] pixels and kept for
    /// the next ones (a bigger call builds its own, as it always has).
    axis8: OnceLock<Box<AxisTable>>,
    precision: Precision,
}

/// Pixels from which an 8-bit grid call builds its own table (256 axis
/// evaluations, under 1 instruction per pixel from here) instead of reading
/// the transform's: a call of 64 pixels spent 60 per pixel building it.
const AXIS8_CALL_PIXELS: usize = 4096;

/// Pixels a 16-bit grid call needs to build [`Axis16`] (65536 axis
/// evaluations, about what 16384 four-channel pixels spend on them).
const AXIS16_MIN_PIXELS: usize = 16384;

/// [`ClutEval::axis`] of every 16-bit sample on an axis of the device-link
/// grid (every axis has the same point count): the cells and fractions
/// apart, 640 KiB, so a 16-bit lookup reads its `(cell, fraction)` per
/// channel instead of converting, dividing, clamping and truncating.
struct Axis16 {
    cell: Box<[u16]>,
    frac: Box<[f64]>,
}

impl Axis16 {
    fn new(points: usize) -> Self {
        let (cell, frac) = (0..=u16::MAX)
            .map(|v| {
                let (c, f) = ClutEval::axis(points, v.to_unit());
                // At most 4096 points per axis, so the cell fits.
                (u16::try_from(c).unwrap_or(u16::MAX), f)
            })
            .unzip::<_, _, Vec<_>, Vec<_>>();
        Self {
            cell: cell.into_boxed_slice(),
            frac: frac.into_boxed_slice(),
        }
    }
}

impl core::fmt::Debug for Transform {
    fn fmt(&self, f: &mut core::fmt::Formatter<'_>) -> core::fmt::Result {
        f.debug_struct("Transform")
            .field("src_layout", &self.src_layout)
            .field("dst_layout", &self.dst_layout)
            .field(
                "engine",
                &match self.engine {
                    Engine::Shaper(_) => "matrix-shaper",
                    Engine::Grid(_) => "grid",
                },
            )
            .finish_non_exhaustive()
    }
}

fn grid_points(in_ch: usize) -> usize {
    match in_ch {
        1 => 4096,
        2 => 129,
        3 => 33,
        _ => 17,
    }
}

impl Transform {
    /// A transform from `src` (pixels in `src_layout`) to `dst` (pixels in
    /// `dst_layout`) with `intent`.
    ///
    /// # Errors
    /// [`Error::InvalidTransform`] when a layout does not match its
    /// profile's colour space; [`Error::Unsupported`] /
    /// [`Error::Malformed`] when a profile cannot be used in the needed
    /// direction.
    pub fn new(
        src: &Profile,
        src_layout: Layout,
        dst: &Profile,
        dst_layout: Layout,
        intent: Intent,
        options: Options,
    ) -> Result<Self> {
        let b = options.black_point_compensation;
        Self::from_chain(
            build_chain(&[src, dst], &[intent, intent], &[b, b])?,
            src_layout,
            dst_layout,
        )
    }

    /// A soft-proofing transform: `src` to the `proof` device with
    /// `intent`, then the proof device's colours to `dst` with
    /// `proof_intent` (`LittleCMS`'s `cmsCreateProofingTransform` chain
    /// `src, proof, proof, dst` with intents `intent, intent, relative,
    /// proof_intent`). Use [`Intent::AbsoluteColorimetric`] as
    /// `proof_intent` to simulate the proof paper white.
    ///
    /// # Errors
    /// As [`Transform::new`].
    #[allow(clippy::too_many_arguments)]
    pub fn new_proofing(
        src: &Profile,
        src_layout: Layout,
        proof: &Profile,
        dst: &Profile,
        dst_layout: Layout,
        intent: Intent,
        proof_intent: Intent,
        options: Options,
    ) -> Result<Self> {
        let b = options.black_point_compensation;
        Self::from_chain(
            build_chain(
                &[src, proof, proof, dst],
                &[intent, intent, Intent::RelativeColorimetric, proof_intent],
                &[b, b, false, false],
            )?,
            src_layout,
            dst_layout,
        )
    }

    fn from_chain(chain: Chain, src_layout: Layout, dst_layout: Layout) -> Result<Self> {
        if src_layout.color_space() != chain.in_space {
            return Err(Error::InvalidTransform(
                "source layout does not match the source profile colour space",
            ));
        }
        if dst_layout.color_space() != chain.out_space {
            return Err(Error::InvalidTransform(
                "destination layout does not match the destination colour space",
            ));
        }
        let engine = if let Some(link) = chain.shaper.clone() {
            Engine::Shaper(Box::new(ShaperEngine::new(link)))
        } else {
            let g = grid_points(chain.in_ch);
            let data = chain.sample_grid(g);
            Engine::Grid(ClutEval::from_f32(vec![g; chain.in_ch], chain.out_ch, data))
        };
        Ok(Self {
            src_layout,
            dst_layout,
            chain: Arc::new(chain),
            engine,
            u8_head: OnceLock::new(),
            axis16: OnceLock::new(),
            pixels16: AtomicUsize::new(0),
            axis8: OnceLock::new(),
            precision: Precision::Exact,
        })
    }

    /// This transform with `f32` pixels evaluated at `precision`
    /// ([`Precision::Exact`] by default). Integer pixels are unaffected.
    #[must_use]
    pub fn with_precision(mut self, precision: Precision) -> Self {
        self.precision = precision;
        self
    }

    /// How `f32` pixels are evaluated.
    #[must_use]
    pub const fn precision(&self) -> Precision {
        self.precision
    }

    /// [`Precision::Fast`] for `f32` pixels: `false`, with nothing done,
    /// when this transform has no fast path (the exact one then serves).
    fn run_fast_f32(
        &self,
        src: &[f32],
        dst: &mut [f32],
        sc_dc: (usize, usize),
        (in_ch, out_ch): (usize, usize),
        alpha: (bool, bool),
    ) -> bool {
        match &self.engine {
            Engine::Shaper(e) if in_ch == 3 && out_ch == 3 => match self.chain.as_shaper() {
                Some(exact) => e.run_fast_f32(src, dst, sc_dc, alpha, &exact),
                None => false,
            },
            _ => false,
        }
    }

    /// The source layout.
    #[must_use]
    pub const fn src_layout(&self) -> Layout {
        self.src_layout
    }

    /// The destination layout.
    #[must_use]
    pub const fn dst_layout(&self) -> Layout {
        self.dst_layout
    }

    /// `true` when integer data takes the matrix-shaper fast path.
    #[must_use]
    pub const fn is_matrix_shaper(&self) -> bool {
        matches!(self.engine, Engine::Shaper(_))
    }

    /// Transforms interleaved pixels from `src` into `dst`. `dst` must hold
    /// exactly as many pixels as `src`.
    ///
    /// # Errors
    /// [`Error::BufferSize`] when the lengths do not match the layouts.
    pub fn transform<T: Sample>(&self, src: &[T], dst: &mut [T]) -> Result<()> {
        let sc = self.src_layout.channels();
        let dc = self.dst_layout.channels();
        if !src.len().is_multiple_of(sc) || dst.len() != src.len() / sc * dc {
            return Err(Error::BufferSize {
                src: src.len(),
                dst: dst.len(),
            });
        }
        let src_alpha = self.src_layout.has_alpha();
        let dst_alpha = self.dst_layout.has_alpha();
        let in_ch = self.src_layout.color_channels();
        let out_ch = self.dst_layout.color_channels();
        match (&self.engine, T::KIND) {
            (Engine::Shaper(e), SampleKind::U8 | SampleKind::U16) => {
                let tables = if T::KIND == SampleKind::U8 {
                    &e.in8[..]
                } else {
                    e.in16()
                };
                let a = (src, dst, sc, dc, src_alpha, dst_alpha);
                match (in_ch, out_ch) {
                    (3, 3) => e.run::<T, 3, 3>(tables, a.0, a.1, a.2, a.3, a.4, a.5),
                    (3, 1) => e.run::<T, 3, 1>(tables, a.0, a.1, a.2, a.3, a.4, a.5),
                    (1, 3) => e.run::<T, 1, 3>(tables, a.0, a.1, a.2, a.3, a.4, a.5),
                    _ => e.run::<T, 1, 1>(tables, a.0, a.1, a.2, a.3, a.4, a.5),
                }
            }
            (Engine::Grid(g), SampleKind::U8 | SampleKind::U16) => {
                let (ch, alpha) = ((sc, dc, in_ch, out_ch), (src_alpha, dst_alpha));
                if let Some(s) = T::u8s(src) {
                    if let Some(d) = T::u8s_mut(dst) {
                        if s.len() / sc >= AXIS8_CALL_PIXELS {
                            run_grid_u8(g, s, d, ch, alpha);
                        } else {
                            run_grid_u8_shared(g, &self.axis8, s, d, ch, alpha);
                        }
                        return Ok(());
                    }
                }
                if let Some(s) = T::u16s(src) {
                    if let Some(d) = T::u16s_mut(dst) {
                        let same_axes = (1..in_ch).all(|i| g.points(i) == g.points(0));
                        let table = match self.axis16.get() {
                            Some(t) => Some(t),
                            None if same_axes
                                && self.pixels16.fetch_add(s.len() / sc, Ordering::Relaxed)
                                    + s.len() / sc
                                    >= AXIS16_MIN_PIXELS =>
                            {
                                Some(self.axis16.get_or_init(|| Axis16::new(g.points(0))))
                            }
                            None => None,
                        };
                        match table {
                            Some(t) if same_axes => run_grid_u16_table(g, t, s, d, ch, alpha),
                            _ => run_grid_u16(g, s, d, ch, alpha),
                        }
                        return Ok(());
                    }
                }
                run_grid(g, src, dst, ch, alpha);
            }
            _ => {
                let src_lab = self.src_layout.is_lab();
                let dst_lab = self.dst_layout.is_lab();
                if let Some(s) = T::f32s(src) {
                    if let Some(d) = T::f32s_mut(dst) {
                        let alpha = (src_alpha, dst_alpha);
                        if self.precision == Precision::Fast
                            && self.run_fast_f32(s, d, (sc, dc), (in_ch, out_ch), alpha)
                        {
                            return Ok(());
                        }
                        let head = if !src_lab && mostly_u8_points(s, sc, in_ch) {
                            self.u8_head
                                .get_or_init(|| self.chain.head_on_u8_points())
                                .as_deref()
                        } else {
                            None
                        };
                        if let Some(f) = self.chain.as_shaper() {
                            match head {
                                Some([r0, r1, r2, ..]) => {
                                    run_shaper_f32_u8_points(
                                        &f,
                                        [r0, r1, r2],
                                        s,
                                        d,
                                        (sc, dc),
                                        alpha,
                                    );
                                }
                                _ => run_shaper_f32(&f, s, d, (sc, dc), alpha),
                            }
                            return Ok(());
                        }
                        if let Some(l) = self.chain.as_lut_out::<4>() {
                            run_lut_out_f32(&l, head, s, d, (sc, dc), alpha);
                        } else if let Some(l) = self.chain.as_lut_out::<3>() {
                            run_lut_out_f32(&l, head, s, d, (sc, dc), alpha);
                        } else {
                            let ch = (sc, dc, in_ch, out_ch);
                            match head {
                                Some(rows) => run_chain_f32_u8_points(
                                    &self.chain,
                                    rows,
                                    s,
                                    d,
                                    ch,
                                    alpha,
                                    dst_lab,
                                ),
                                None => {
                                    run_chain_f32(&self.chain, s, d, ch, alpha, (src_lab, dst_lab))
                                }
                            }
                        }
                        return Ok(());
                    }
                }
                let mut input = [0.0f64; 16];
                let mut out = [0.0f64; 16];
                for (sp, dp) in src.chunks_exact(sc).zip(dst.chunks_exact_mut(dc)) {
                    for c in 0..in_ch {
                        input[c] = sp[c].to_unit();
                    }
                    if src_lab {
                        input[0] /= 100.0;
                        input[1] = (input[1] + 128.0) / 255.0;
                        input[2] = (input[2] + 128.0) / 255.0;
                    }
                    self.chain.eval(&input, &mut out);
                    if dst_lab {
                        out[0] *= 100.0;
                        out[1] = out[1] * 255.0 - 128.0;
                        out[2] = out[2] * 255.0 - 128.0;
                    }
                    for c in 0..out_ch {
                        dp[c] = T::from_unit(out[c]);
                    }
                    if dst_alpha {
                        dp[dc - 1] = if src_alpha { sp[sc - 1] } else { T::opaque() };
                    }
                }
            }
        }
        Ok(())
    }
}

/// The loop of [`ShaperEngine::run_grey8`]: `K` colour values per pixel from
/// the table, the alpha channel as [`ShaperEngine::run`] writes it.
#[allow(clippy::too_many_arguments)]
fn grey8_loop<const K: usize>(
    table: &[[u8; 3]; 256],
    src: &[u8],
    dst: &mut [u8],
    sc: usize,
    dc: usize,
    src_alpha: bool,
    dst_alpha: bool,
) {
    for (sp, dp) in src.chunks_exact(sc).zip(dst.chunks_exact_mut(dc)) {
        let row = &table[usize::from(sp[0])];
        dp[..K].copy_from_slice(&row[..K]);
        if dst_alpha {
            dp[dc - 1] = if src_alpha { sp[sc - 1] } else { u8::MAX };
        }
    }
}

/// `f32` pixels between two RGB matrix/TRC profiles. Not generic, so the
/// whole per-pixel body (curves, matrices, inverse curves) is compiled here
/// as one loop instead of a call into the op interpreter per pixel.
fn run_shaper_f32(
    f: &ShaperChain<'_>,
    src: &[f32],
    dst: &mut [f32],
    (sc, dc): (usize, usize),
    (src_alpha, dst_alpha): (bool, bool),
) {
    // sRGB-shaped curves at both ends: their own loop, with the curve
    // arithmetic in line. Each loop in its own frame, so the general one
    // keeps the code it had (one shared frame cost every other pair of
    // curves 3 Ir per pixel, the general loop after the test in its frame
    // 1 Ir per pixel).
    if !run_segments_f32(f, src, dst, (sc, dc), (src_alpha, dst_alpha)) {
        run_curves_f32(f, src, dst, (sc, dc), (src_alpha, dst_alpha));
    }
}

/// [`run_shaper_f32`] for any curves.
#[inline(never)]
fn run_curves_f32(
    f: &ShaperChain<'_>,
    src: &[f32],
    dst: &mut [f32],
    (sc, dc): (usize, usize),
    (src_alpha, dst_alpha): (bool, bool),
) {
    for (sp, dp) in src.chunks_exact(sc).zip(dst.chunks_exact_mut(dc)) {
        let out = f.eval([f64::from(sp[0]), f64::from(sp[1]), f64::from(sp[2])]);
        dp[0] = out[0] as f32;
        dp[1] = out[1] as f32;
        dp[2] = out[2] as f32;
        if dst_alpha {
            dp[dc - 1] = if src_alpha { sp[sc - 1] } else { 1.0 };
        }
    }
}

/// [`run_shaper_f32`] when the chain is a [`SegChain`] (`true`); `false`,
/// with nothing done, otherwise.
#[inline(never)]
fn run_segments_f32(
    f: &ShaperChain<'_>,
    src: &[f32],
    dst: &mut [f32],
    (sc, dc): (usize, usize),
    (src_alpha, dst_alpha): (bool, bool),
) -> bool {
    let Some(seg) = f.segments() else {
        return false;
    };
    for (sp, dp) in src.chunks_exact(sc).zip(dst.chunks_exact_mut(dc)) {
        let out = seg.eval([f64::from(sp[0]), f64::from(sp[1]), f64::from(sp[2])]);
        dp[0] = out[0] as f32;
        dp[1] = out[1] as f32;
        dp[2] = out[2] as f32;
        if dst_alpha {
            dp[dc - 1] = if src_alpha { sp[sc - 1] } else { 1.0 };
        }
    }
    true
}

/// [`run_shaper_f32`] when the device curves of every sample that is an
/// 8-bit point come from `rows` ([`Chain::head_on_u8_points`]); any other
/// sample is evaluated (the same value either way). One loop per kind of
/// chain, each in its own frame, so the pixel loops keep their code.
fn run_shaper_f32_u8_points(
    f: &ShaperChain<'_>,
    rows: [&[f64; 256]; 3],
    src: &[f32],
    dst: &mut [f32],
    sc_dc: (usize, usize),
    alpha: (bool, bool),
) {
    match f.segments() {
        Some(seg) => run_segments_f32_u8_points(&seg, rows, src, dst, sc_dc, alpha),
        None => run_curves_f32_u8_points(f, rows, src, dst, sc_dc, alpha),
    }
}

/// [`run_shaper_f32_u8_points`] for a [`SegChain`].
#[inline(never)]
fn run_segments_f32_u8_points(
    seg: &SegChain,
    rows: [&[f64; 256]; 3],
    src: &[f32],
    dst: &mut [f32],
    (sc, dc): (usize, usize),
    (src_alpha, dst_alpha): (bool, bool),
) {
    for (sp, dp) in src.chunks_exact(sc).zip(dst.chunks_exact_mut(dc)) {
        let v = match (u8_point(sp[0]), u8_point(sp[1]), u8_point(sp[2])) {
            (Some(a), Some(b), Some(c)) => [rows[0][a], rows[1][b], rows[2][c]],
            _ => seg.curves([f64::from(sp[0]), f64::from(sp[1]), f64::from(sp[2])]),
        };
        let out = seg.rest(v);
        dp[0] = out[0] as f32;
        dp[1] = out[1] as f32;
        dp[2] = out[2] as f32;
        if dst_alpha {
            dp[dc - 1] = if src_alpha { sp[sc - 1] } else { 1.0 };
        }
    }
}

/// [`run_shaper_f32_u8_points`] for any other [`ShaperChain`].
#[inline(never)]
fn run_curves_f32_u8_points(
    f: &ShaperChain<'_>,
    rows: [&[f64; 256]; 3],
    src: &[f32],
    dst: &mut [f32],
    (sc, dc): (usize, usize),
    (src_alpha, dst_alpha): (bool, bool),
) {
    for (sp, dp) in src.chunks_exact(sc).zip(dst.chunks_exact_mut(dc)) {
        let v = match (u8_point(sp[0]), u8_point(sp[1]), u8_point(sp[2])) {
            (Some(a), Some(b), Some(c)) => [rows[0][a], rows[1][b], rows[2][c]],
            _ => f.curves([f64::from(sp[0]), f64::from(sp[1]), f64::from(sp[2])]),
        };
        let out = f.rest(v);
        dp[0] = out[0] as f32;
        dp[1] = out[1] as f32;
        dp[2] = out[2] as f32;
        if dst_alpha {
            dp[dc - 1] = if src_alpha { sp[sc - 1] } else { 1.0 };
        }
    }
}

/// `f32` pixels through a [`LutOutChain`], one loop per output count; with
/// `head` ([`Chain::head_on_u8_points`]) the curves of every sample that is
/// an 8-bit point come from that table.
fn run_lut_out_f32<const N: usize>(
    l: &LutOutChain<'_, N>,
    head: Option<&[[f64; 256]]>,
    src: &[f32],
    dst: &mut [f32],
    sc_dc: (usize, usize),
    alpha: (bool, bool),
) {
    match head {
        Some([r0, r1, r2, ..]) => run_lut_out_u8_points(l, [r0, r1, r2], src, dst, sc_dc, alpha),
        _ => run_lut_out_plain(l, src, dst, sc_dc, alpha),
    }
}

/// [`run_lut_out_f32`] without a table.
#[inline(never)]
fn run_lut_out_plain<const N: usize>(
    l: &LutOutChain<'_, N>,
    src: &[f32],
    dst: &mut [f32],
    (sc, dc): (usize, usize),
    (src_alpha, dst_alpha): (bool, bool),
) {
    let mut out = [0.0f64; 16];
    for (sp, dp) in src.chunks_exact(sc).zip(dst.chunks_exact_mut(dc)) {
        let v = l.curves([f64::from(sp[0]), f64::from(sp[1]), f64::from(sp[2])]);
        l.rest(v, &mut out);
        for c in 0..N {
            dp[c] = out[c] as f32;
        }
        if dst_alpha {
            dp[dc - 1] = if src_alpha { sp[sc - 1] } else { 1.0 };
        }
    }
}

/// [`run_lut_out_f32`] with the table: per sample the row entry when the
/// sample is an 8-bit point, the curve otherwise (the same value).
#[inline(never)]
fn run_lut_out_u8_points<const N: usize>(
    l: &LutOutChain<'_, N>,
    rows: [&[f64; 256]; 3],
    src: &[f32],
    dst: &mut [f32],
    (sc, dc): (usize, usize),
    (src_alpha, dst_alpha): (bool, bool),
) {
    let mut out = [0.0f64; 16];
    for (sp, dp) in src.chunks_exact(sc).zip(dst.chunks_exact_mut(dc)) {
        let v = match (u8_point(sp[0]), u8_point(sp[1]), u8_point(sp[2])) {
            (Some(a), Some(b), Some(c)) => [rows[0][a], rows[1][b], rows[2][c]],
            _ => l.curves([f64::from(sp[0]), f64::from(sp[1]), f64::from(sp[2])]),
        };
        l.rest(v, &mut out);
        for c in 0..N {
            dp[c] = out[c] as f32;
        }
        if dst_alpha {
            dp[dc - 1] = if src_alpha { sp[sc - 1] } else { 1.0 };
        }
    }
}

/// `f32` pixels through the exact pipeline. Not generic, so the loop is
/// compiled here, on one zero-tailed array per pixel that the chain works on
/// in place; the generic loop is compiled in the caller's crate and pays a
/// call and two 128-byte copies per pixel.
fn run_chain_f32(
    chain: &Chain,
    src: &[f32],
    dst: &mut [f32],
    (sc, dc, in_ch, out_ch): (usize, usize, usize, usize),
    (src_alpha, dst_alpha): (bool, bool),
    (src_lab, dst_lab): (bool, bool),
) {
    for (sp, dp) in src.chunks_exact(sc).zip(dst.chunks_exact_mut(dc)) {
        let mut v = [0.0f64; 16];
        for c in 0..in_ch {
            v[c] = f64::from(sp[c]);
        }
        if src_lab {
            v[0] /= 100.0;
            v[1] = (v[1] + 128.0) / 255.0;
            v[2] = (v[2] + 128.0) / 255.0;
        }
        chain.eval_in_place(&mut v);
        if dst_lab {
            v[0] *= 100.0;
            v[1] = v[1] * 255.0 - 128.0;
            v[2] = v[2] * 255.0 - 128.0;
        }
        for c in 0..out_ch {
            dp[c] = v[c] as f32;
        }
        if dst_alpha {
            dp[dc - 1] = if src_alpha { sp[sc - 1] } else { 1.0 };
        }
    }
}

/// `true` when the first pixel's colour samples are all 8-bit points
/// ([`u8_point`]) and at least 7 in 8 of the first 64 pixels' are: float
/// pixels widened from 8-bit data, which [`run_chain_f32_u8_points`] serves
/// from a table. Other data is turned away by its first sample.
fn mostly_u8_points(src: &[f32], sc: usize, in_ch: usize) -> bool {
    let Some(first) = src.get(..in_ch) else {
        return false;
    };
    if !first.iter().all(|&x| u8_point(x).is_some()) {
        return false;
    }
    let mut seen = 0usize;
    let mut hits = 0usize;
    for px in src.chunks_exact(sc).take(64) {
        for &x in &px[..in_ch] {
            seen += 1;
            hits += usize::from(u8_point(x).is_some());
        }
    }
    seen > 0 && hits * 8 >= seen * 7
}

/// [`run_chain_f32`] for a source that is not Lab, with the chain's first
/// op looked up in `rows` ([`Chain::head_on_u8_points`]) for every sample
/// that is an 8-bit point and evaluated by the chain for any other: the
/// same values either way.
#[inline(never)]
fn run_chain_f32_u8_points(
    chain: &Chain,
    rows: &[[f64; 256]],
    src: &[f32],
    dst: &mut [f32],
    (sc, dc, in_ch, out_ch): (usize, usize, usize, usize),
    (src_alpha, dst_alpha): (bool, bool),
    dst_lab: bool,
) {
    for (sp, dp) in src.chunks_exact(sc).zip(dst.chunks_exact_mut(dc)) {
        let mut v = [0.0f64; 16];
        let mut all = true;
        for (c, row) in rows.iter().enumerate().take(in_ch) {
            match u8_point(sp[c]) {
                Some(k) => v[c] = row[k],
                None => all = false,
            }
        }
        if all {
            chain.eval_tail_in_place(&mut v);
        } else {
            v = [0.0f64; 16];
            for c in 0..in_ch {
                v[c] = f64::from(sp[c]);
            }
            chain.eval_in_place(&mut v);
        }
        if dst_lab {
            v[0] *= 100.0;
            v[1] = v[1] * 255.0 - 128.0;
            v[2] = v[2] * 255.0 - 128.0;
        }
        for c in 0..out_ch {
            dp[c] = v[c] as f32;
        }
        if dst_alpha {
            dp[dc - 1] = if src_alpha { sp[sc - 1] } else { 1.0 };
        }
    }
}

/// Every 8-bit value's `(cell, fraction)` on a grid axis.
type AxisTable = [(usize, f64); 256];

/// The 8-bit device-link grid loop. Not generic, so it is compiled here, with
/// the lookup in line, instead of in the crate of the caller with a call
/// into the library per pixel.
fn run_grid_u8(
    g: &ClutEval,
    src: &[u8],
    dst: &mut [u8],
    ch: (usize, usize, usize, usize),
    alpha: (bool, bool),
) {
    if !fixed_shape(g, ch.2, ch.3) {
        return run_grid(g, src, dst, ch, alpha);
    }
    // The (cell, fraction) of every 8-bit value, by the expressions wider
    // samples use per value, once per call instead of per pixel.
    let points = g.points(0);
    let table: AxisTable = core::array::from_fn(|v| ClutEval::axis(points, (v as u8).to_unit()));
    grid_shaped(g, src, dst, ch, alpha, |_, v| table[usize::from(v)]);
}

/// [`run_grid_u8`] for a small call: the table is the transform's, built
/// by the first such call. In its own frame, so the loop of the big calls
/// keeps its code (sharing a frame with a second instance of the loop cost
/// it 1 to 5 instructions per pixel: fogu8 +0.52..2.92%).
#[inline(never)]
fn run_grid_u8_shared(
    g: &ClutEval,
    shared: &OnceLock<Box<AxisTable>>,
    src: &[u8],
    dst: &mut [u8],
    ch: (usize, usize, usize, usize),
    alpha: (bool, bool),
) {
    let table: &AxisTable = shared.get_or_init(|| {
        let points = g.points(0);
        Box::new(core::array::from_fn(|v| {
            ClutEval::axis(points, (v as u8).to_unit())
        }))
    });
    // Every other shape through `run_grid_u8` itself: a second caller of
    // its helpers (`fixed_shape`, `run_grid`) re-rolled its loops (cgatsu8
    // +0.41%).
    if !grid_shaped(g, src, dst, ch, alpha, |_, v| table[usize::from(v)]) {
        run_grid_u8(g, src, dst, ch, alpha);
    }
}

/// The 16-bit device-link grid loop, likewise.
fn run_grid_u16(
    g: &ClutEval,
    src: &[u16],
    dst: &mut [u16],
    ch: (usize, usize, usize, usize),
    alpha: (bool, bool),
) {
    if !grid_shaped(g, src, dst, ch, alpha, |points, v| {
        ClutEval::axis(points, v.to_unit())
    }) {
        run_grid(g, src, dst, ch, alpha);
    }
}

/// [`run_grid_u16`] with every sample's `(cell, fraction)` read from `t`
/// (the values the per-sample expressions give).
fn run_grid_u16_table(
    g: &ClutEval,
    t: &Axis16,
    src: &[u16],
    dst: &mut [u16],
    ch: (usize, usize, usize, usize),
    alpha: (bool, bool),
) {
    let (Ok(cell), Ok(frac)) = (
        <&[u16; 65536]>::try_from(&t.cell[..]),
        <&[f64; 65536]>::try_from(&t.frac[..]),
    ) else {
        return run_grid_u16(g, src, dst, ch, alpha);
    };
    if !grid_shaped(g, src, dst, ch, alpha, |_, v| {
        let v = usize::from(v);
        (usize::from(cell[v]), frac[v])
    }) {
        run_grid(g, src, dst, ch, alpha);
    }
}

/// `true` when [`grid_shaped`] has a loop for this grid.
fn fixed_shape(g: &ClutEval, in_ch: usize, out_ch: usize) -> bool {
    match (in_ch, out_ch) {
        (3, 3) => g.fixed::<3, 3>().is_some(),
        (3, 4) => g.fixed::<3, 4>().is_some(),
        (4, 3) => g.fixed::<4, 3>().is_some(),
        (4, 4) => g.fixed::<4, 4>().is_some(),
        _ => false,
    }
}

/// Runs the loop for the shape of the grid when there is one with fixed
/// channel counts (RGB, Lab or CMYK in and out); `false`, with nothing
/// done, otherwise. `cell(points, sample)` is the `(cell, fraction)` of a
/// sample on an axis of that many grid points.
#[inline(always)]
fn grid_shaped<T: Sample, C: Fn(usize, T) -> (usize, f64) + Copy>(
    g: &ClutEval,
    src: &[T],
    dst: &mut [T],
    (sc, dc, in_ch, out_ch): (usize, usize, usize, usize),
    alpha: (bool, bool),
    cell: C,
) -> bool {
    macro_rules! shape {
        ($i:literal, $o:literal) => {
            if let Some(f) = g.fixed::<$i, $o>() {
                grid_fixed::<T, $i, $o, C>(&f, src, dst, (sc, dc), alpha, cell);
                return true;
            }
        };
    }
    match (in_ch, out_ch) {
        (3, 3) => shape!(3, 3),
        (3, 4) => shape!(3, 4),
        (4, 3) => shape!(4, 3),
        (4, 4) => shape!(4, 4),
        _ => {}
    }
    false
}

/// The grid loop for `IN` colour channels in and `OUT` out: the same
/// lookups and conversions as [`run_grid`], with every per-channel index a
/// constant and the shape of the CLUT settled before the loop.
fn grid_fixed<T: Sample, const IN: usize, const OUT: usize, C: Fn(usize, T) -> (usize, f64)>(
    f: &FixedClut<'_, IN, OUT>,
    src: &[T],
    dst: &mut [T],
    (sc, dc): (usize, usize),
    (src_alpha, dst_alpha): (bool, bool),
    cell: C,
) {
    let mut out = [0.0f64; 16];
    for (sp, dp) in src.chunks_exact(sc).zip(dst.chunks_exact_mut(dc)) {
        let cells: [(usize, f64); IN] = core::array::from_fn(|c| cell(f.points(c), sp[c]));
        f.at(&cells, &mut out);
        for c in 0..OUT {
            dp[c] = T::from_unit(out[c]);
        }
        if dst_alpha {
            dp[dc - 1] = if src_alpha { sp[sc - 1] } else { T::opaque() };
        }
    }
}

/// The integer device-link grid loop, in its own frame so the inlining
/// decisions of the matrix-shaper monomorphs in `Transform::transform` do
/// not reshape it (sharing the frame cost the grid path +1.7-1.9% Ir).
fn run_grid<T: Sample>(
    g: &ClutEval,
    src: &[T],
    dst: &mut [T],
    (sc, dc, in_ch, out_ch): (usize, usize, usize, usize),
    (src_alpha, dst_alpha): (bool, bool),
) {
    let mut out = [0.0f64; 16];
    if T::KIND == SampleKind::U8 {
        // Every 8-bit value's (cell, fraction), computed by the same
        // expressions `eval` uses, once per call instead of per pixel.
        let points = g.points(0);
        let table: Vec<(usize, f64)> = (0..256u16)
            .map(|v| ClutEval::axis(points, (v as u8).to_unit()))
            .collect();
        let mut axes = [(0usize, 0.0f64); 16];
        for (sp, dp) in src.chunks_exact(sc).zip(dst.chunks_exact_mut(dc)) {
            for c in 0..in_ch {
                axes[c] = table[sp[c].index() & 255];
            }
            g.eval_axes(&axes, &mut out);
            for c in 0..out_ch {
                dp[c] = T::from_unit(out[c]);
            }
            if dst_alpha {
                dp[dc - 1] = if src_alpha { sp[sc - 1] } else { T::opaque() };
            }
        }
        return;
    }
    let mut input = [0.0f64; 16];
    for (sp, dp) in src.chunks_exact(sc).zip(dst.chunks_exact_mut(dc)) {
        for c in 0..in_ch {
            input[c] = sp[c].to_unit();
        }
        g.eval(&input[..in_ch], &mut out);
        for c in 0..out_ch {
            dp[c] = T::from_unit(out[c]);
        }
        if dst_alpha {
            dp[dc - 1] = if src_alpha { sp[sc - 1] } else { T::opaque() };
        }
    }
}

/// Gamut checking against a proofing profile.
///
/// Each source colour is taken to the PCS as it would arrive at `proof`
/// (with `intent`), sent through the proof's PCS-to-device side and back
/// through its relative-colorimetric device-to-PCS side; a colour is out of
/// gamut when the round trip moves it by more than `threshold` CIEDE2000.
/// Device values clipped by the proof's output side are what move it, so
/// this flags both matrix-shaper and LUT gamut boundaries; LUT round-trip
/// error inside the gamut is why the threshold is not zero. The `gamt` tag
/// is not consulted.
pub struct GamutCheck {
    layout: Layout,
    chain: GamutChain,
    threshold: f64,
}

impl core::fmt::Debug for GamutCheck {
    fn fmt(&self, f: &mut core::fmt::Formatter<'_>) -> core::fmt::Result {
        f.debug_struct("GamutCheck")
            .field("layout", &self.layout)
            .field("threshold", &self.threshold)
            .finish_non_exhaustive()
    }
}

impl GamutCheck {
    /// Builds a gamut check of `src` colours against `proof`.
    ///
    /// # Errors
    /// As [`Transform::new`].
    pub fn new(
        src: &Profile,
        layout: Layout,
        proof: &Profile,
        intent: Intent,
        options: Options,
        threshold: f64,
    ) -> Result<Self> {
        let chain = GamutChain::new(src, proof, intent, options.black_point_compensation)?;
        if layout.color_space() != chain.in_space {
            return Err(Error::InvalidTransform(
                "layout does not match the source profile colour space",
            ));
        }
        Ok(Self {
            layout,
            chain,
            threshold,
        })
    }

    /// The CIEDE2000 shift of each pixel through the proof round trip.
    ///
    /// # Errors
    /// [`Error::BufferSize`] when `out` does not hold one value per pixel.
    pub fn delta_e<T: Sample>(&self, src: &[T], out: &mut [f64]) -> Result<()> {
        let sc = self.layout.channels();
        if !src.len().is_multiple_of(sc) || out.len() != src.len() / sc {
            return Err(Error::BufferSize {
                src: src.len(),
                dst: out.len(),
            });
        }
        let lab = self.layout.is_lab() && T::KIND == SampleKind::F32;
        let mut input = [0.0f64; 16];
        for (sp, o) in src.chunks_exact(sc).zip(out.iter_mut()) {
            for c in 0..self.layout.color_channels() {
                input[c] = sp[c].to_unit();
            }
            if lab {
                input[0] /= 100.0;
                input[1] = (input[1] + 128.0) / 255.0;
                input[2] = (input[2] + 128.0) / 255.0;
            }
            let (a, b) = self.chain.eval(&input);
            *o = delta_e_2000(a, b);
        }
        Ok(())
    }

    /// Marks each pixel `true` when it is out of the proof's gamut.
    ///
    /// # Errors
    /// [`Error::BufferSize`] when `out` does not hold one flag per pixel.
    pub fn check<T: Sample>(&self, src: &[T], out: &mut [bool]) -> Result<()> {
        let mut d = vec![0.0; out.len()];
        self.delta_e(src, &mut d)?;
        for (o, v) in out.iter_mut().zip(d) {
            *o = v > self.threshold;
        }
        Ok(())
    }
}

#[cfg(test)]
mod tests {
    use super::*;

    /// The fixed-shape grid loops against the general one, on a synthetic
    /// device-link grid: every shape, both integer widths, with and without
    /// alpha on either side.
    #[test]
    fn fixed_grid_loops_match_the_general_loop() {
        fn noise(n: usize, seed: u64) -> Vec<u16> {
            let mut s = seed;
            (0..n)
                .map(|i| {
                    s = s
                        .wrapping_mul(6_364_136_223_846_793_005)
                        .wrapping_add(1_442_695_040_888_963_407);
                    // Grid points and both ends among the noise.
                    match i % 7 {
                        0 => 0,
                        1 => 65535,
                        2 => 0x8080,
                        _ => (s >> 40) as u16,
                    }
                })
                .collect()
        }
        for (in_ch, out_ch) in [(3usize, 3usize), (3, 4), (4, 3), (4, 4), (1, 3), (3, 1)] {
            let g = 5usize;
            let total = g.pow(in_ch as u32) * out_ch;
            let data: Vec<f32> = (0..total)
                .map(|i| ((i * 7919 + 13) % 1009) as f32 / 1008.0)
                .collect();
            let clut = ClutEval::from_f32(vec![g; in_ch], out_ch, data);
            assert_eq!(fixed_shape(&clut, in_ch, out_ch), in_ch > 1 && out_ch > 1);
            for (src_alpha, dst_alpha) in
                [(false, false), (true, false), (false, true), (true, true)]
            {
                let (sc, dc) = (
                    in_ch + usize::from(src_alpha),
                    out_ch + usize::from(dst_alpha),
                );
                let ch = (sc, dc, in_ch, out_ch);
                let alpha = (src_alpha, dst_alpha);
                let src16 = noise(sc * 400, 7 + sc as u64);
                let src8: Vec<u8> = src16.iter().map(|v| (v >> 8) as u8).collect();
                let (mut a8, mut b8) = (vec![1u8; dc * 400], vec![2u8; dc * 400]);
                run_grid_u8(&clut, &src8, &mut a8, ch, alpha);
                run_grid(&clut, &src8, &mut b8, ch, alpha);
                assert_eq!(a8, b8, "u8 {in_ch}->{out_ch} {alpha:?}");
                let shared = OnceLock::new();
                for _ in 0..2 {
                    run_grid_u8_shared(&clut, &shared, &src8, &mut a8, ch, alpha);
                    assert_eq!(a8, b8, "u8 shared {in_ch}->{out_ch} {alpha:?}");
                }
                let (mut a16, mut b16) = (vec![1u16; dc * 400], vec![2u16; dc * 400]);
                run_grid_u16(&clut, &src16, &mut a16, ch, alpha);
                run_grid(&clut, &src16, &mut b16, ch, alpha);
                assert_eq!(a16, b16, "u16 {in_ch}->{out_ch} {alpha:?}");
            }
        }
    }

    /// The `f32` loops against the per-colour reference: [`Chain::eval`] on
    /// a zero-tailed array, with the Lab scaling and the alpha rule of the
    /// documented layouts.
    /// A Lab-PCS LUT output profile of `n` device channels (`space`) built
    /// for the tests: `B2A0..2` of 3 to `n` and `A2B0..2` of `n` to 3, as
    /// `lut16` (or `lut8`) tags with nonlinear `in_len`-entry input tables,
    /// 9-point CLUTs of scrambled smooth values and `out_len`-entry output
    /// tables, at `version`.
    pub(crate) fn test_lut_profile(
        space: crate::ColorSpace,
        n: usize,
        version: u32,
        (in_len, out_len): (usize, usize),
        sixteen: bool,
    ) -> Profile {
        use crate::lut::{Clut, LegacyLut, Lut};
        use crate::profile::ProfileClass;
        use crate::tag::Tag;
        let q = |v: f64| (v.clamp(0.0, 1.0) * 65535.0).round() as u16;
        let q8 = |v: f64| {
            if sixteen {
                q(v)
            } else {
                u16::from((v.clamp(0.0, 1.0) * 255.0).round() as u8) * 257
            }
        };
        let table = |len: usize, k: usize| -> Vec<u16> {
            (0..len)
                .map(|i| {
                    let x = i as f64 / (len - 1) as f64;
                    q8(x.powf(0.7 + 0.2 * k as f64))
                })
                .collect()
        };
        let clut = |ins: usize, outs: usize, lab: bool| -> Clut {
            let g = 9usize;
            let total = g.pow(ins as u32);
            let mut data = Vec::with_capacity(total * outs);
            for p in 0..total {
                let mut sum = 0.0;
                let mut r = p;
                for _ in 0..ins {
                    sum += (r % g) as f64 / (g - 1) as f64;
                    r /= g;
                }
                for o in 0..outs {
                    let wobble = ((p * 7919 + o * 104_729) % 97) as f64 / 970.0;
                    let v = if lab && o == 0 {
                        1.0 - sum / ins as f64 * 0.9
                    } else if lab {
                        0.5 + (wobble - 0.05) * 2.0
                    } else {
                        (sum / ins as f64 * (0.6 + 0.1 * o as f64) + wobble).fract()
                    };
                    data.push(q8(v));
                }
            }
            Clut {
                grid: vec![g as u8; ins],
                outputs: outs,
                data,
                precision: if sixteen { 2 } else { 1 },
            }
        };
        let lut = |ins: usize, outs: usize, lab: bool| -> Lut {
            let l = LegacyLut {
                matrix: [1.0, 0.0, 0.0, 0.0, 1.0, 0.0, 0.0, 0.0, 1.0],
                input_tables: (0..ins)
                    .map(|k| table(if sixteen { in_len } else { 256 }, k))
                    .collect(),
                clut: clut(ins, outs, lab),
                output_tables: (0..outs)
                    .map(|k| table(if sixteen { out_len } else { 256 }, k + 1))
                    .collect(),
            };
            if sixteen {
                Lut::Lut16(l)
            } else {
                Lut::Lut8(l)
            }
        };
        let mut p = Profile::blank(ProfileClass::Output, space, crate::ColorSpace::Lab);
        p.header.version = version;
        p.set_tag(*b"wtpt", Tag::Xyz(vec![crate::color::D50]));
        for (b2a, a2b) in [
            (*b"B2A0", *b"A2B0"),
            (*b"B2A1", *b"A2B1"),
            (*b"B2A2", *b"A2B2"),
        ] {
            p.set_tag(b2a, Tag::Lut(lut(3, n, false)));
            p.set_tag(a2b, Tag::Lut(lut(n, 3, true)));
        }
        p
    }

    #[test]
    fn lut_out_loop_matches_chain_eval_bit_for_bit() {
        // sRGB into Lab-PCS LUT outputs (CMYK and RGB, v2 and v4 so with
        // and without black-point compensation's second affine map, lut16
        // and lut8, short and long tables): every float the loop meets,
        // including NaN, infinities, -0.0 and out-of-range values, against
        // the interpreter.
        use crate::ColorSpace;
        let srgb = Profile::new_srgb();
        let vals = [
            -0.5f32,
            -0.0,
            0.0,
            1e-9,
            0.001,
            0.04,
            0.2,
            0.5,
            0.75,
            0.999,
            1.0,
            1.5,
            f32::INFINITY,
            f32::NAN,
        ];
        let mut ran = [0usize; 2];
        let mut diagonals = 0;
        for (space, layout, n) in [
            (ColorSpace::Cmyk, Layout::Cmyk, 4),
            (ColorSpace::Rgb, Layout::Rgb, 3),
        ] {
            for (version, tables, sixteen) in [
                (0x0210_0000, (256, 256), true),
                (0x0430_0000, (4096, 2), true),
                (0x0210_0000, (256, 256), false),
            ] {
                let dst = test_lut_profile(space, n, version, tables, sixteen);
                for intent in [Intent::Perceptual, Intent::RelativeColorimetric] {
                    for al in [Layout::Rgb, Layout::Rgba] {
                        for bpc in [false, true] {
                            let o = Options {
                                black_point_compensation: bpc,
                            };
                            let t = Transform::new(&srgb, al, &dst, layout, intent, o).unwrap();
                            let (shaped, diagonal) = if n == 4 {
                                let l = t.chain.as_lut_out::<4>();
                                (l.is_some(), l.is_some_and(|l| l.has_diagonal_map()))
                            } else {
                                let l = t.chain.as_lut_out::<3>();
                                (l.is_some(), l.is_some_and(|l| l.has_diagonal_map()))
                            };
                            diagonals += usize::from(diagonal);
                            assert!(shaped, "{space:?} {version:x} {intent:?}");
                            ran[n - 3] += 1;
                            let (sc, dc) = (al.channels(), layout.channels());
                            let mut src = Vec::new();
                            let m = vals.len();
                            for i in 0..m.pow(3) {
                                src.extend([vals[i % m], vals[i / m % m], vals[i / m / m % m]]);
                                if sc == 4 {
                                    src.push(0.25);
                                }
                            }
                            let mut out_px = vec![-7.0f32; src.len() / sc * dc];
                            t.transform(&src, &mut out_px).unwrap();
                            for (sp, dp) in src.chunks_exact(sc).zip(out_px.chunks_exact(dc)) {
                                let mut input = [0.0f64; 16];
                                for c in 0..3 {
                                    input[c] = f64::from(sp[c]);
                                }
                                let mut out = [0.0f64; 16];
                                t.chain.eval(&input, &mut out);
                                for c in 0..n {
                                    assert_eq!(
                                        dp[c].to_bits(),
                                        (out[c] as f32).to_bits(),
                                        "{sp:?}"
                                    );
                                }
                            }
                        }
                    }
                }
            }
        }
        assert!(
            ran[0] > 0 && ran[1] > 0 && diagonals > 0,
            "{ran:?} {diagonals}"
        );
    }

    #[test]
    fn u8_point_recognises_exactly_the_8_bit_samples() {
        for k in 0..256usize {
            let x = k as f32 / 255.0;
            assert_eq!(u8_point(x), Some(k));
            for y in [
                f32::from_bits(x.to_bits() + 1),
                f32::from_bits(x.to_bits().wrapping_sub(1)),
            ] {
                assert_eq!(u8_point(y), None, "{k}");
            }
        }
        for x in [
            -0.0f32,
            f32::NAN,
            -1.0,
            1.5,
            255.0,
            f32::INFINITY,
            -1e30,
            1e30,
            0.5,
        ] {
            assert_eq!(u8_point(x), None, "{x}");
        }
    }

    #[test]
    fn u8_point_input_matches_chain_eval_bit_for_bit() {
        // Float pixels widened from 8-bit values take the first op from a
        // table (`run_chain_f32_u8_points`); the odd sample that is not an
        // 8-bit point (one in nine here, under the 1-in-8 the detection
        // allows) sends its pixel through the chain whole.
        let srgb = Profile::new_srgb();
        let lab = Profile::new_lab_identity();
        let cmyk =
            Profile::new_from_slice(include_bytes!("../tests/data/CGATS001Compat-v2-micro.icc"))
                .unwrap();
        let v4 =
            Profile::new_from_slice(include_bytes!("../tests/data/sRGB_v4_ICC_preference.icc"))
                .unwrap();
        let p3 = Profile::new_display_p3();
        let g22 = Profile::new_rgb(
            crate::color::D65_XY,
            [
                crate::color::Xy::new(0.64, 0.33),
                crate::color::Xy::new(0.3, 0.6),
                crate::color::Xy::new(0.15, 0.06),
            ],
            crate::Curve::gamma(2.2),
        )
        .unwrap();
        let lut4 = test_lut_profile(crate::ColorSpace::Cmyk, 4, 0x0430_0000, (4096, 2), true);
        let lut3 = test_lut_profile(crate::ColorSpace::Rgb, 3, 0x0210_0000, (256, 256), false);
        let odd = [0.3f32, -0.25, f32::NAN, 1.25, -0.0, 0.5];
        let (mut ran, mut tabled) = (0, 0);
        for (a, al, b, bl) in [
            (&srgb, Layout::Rgb, &cmyk, Layout::Cmyk),
            (&srgb, Layout::Rgba, &cmyk, Layout::Cmyk),
            (&cmyk, Layout::Cmyk, &srgb, Layout::Rgba),
            (&srgb, Layout::Rgb, &lab, Layout::Lab),
            (&srgb, Layout::Rgb, &v4, Layout::Rgb),
            (&v4, Layout::Rgb, &srgb, Layout::Rgb),
            (&srgb, Layout::Rgb, &lut4, Layout::Cmyk),
            (&srgb, Layout::Rgba, &lut4, Layout::Cmyka),
            (&srgb, Layout::Rgb, &lut3, Layout::Rgb),
            (&srgb, Layout::Rgb, &p3, Layout::Rgb),
            (&srgb, Layout::Rgba, &g22, Layout::Rgba),
            (&g22, Layout::Rgb, &srgb, Layout::Rgb),
        ] {
            for intent in [Intent::Perceptual, Intent::RelativeColorimetric] {
                // (The micro test profiles lack some intents' tables.)
                let Ok(t) = Transform::new(a, al, b, bl, intent, Options::default()) else {
                    continue;
                };
                ran += 1;
                let (sc, dc) = (al.channels(), bl.channels());
                let (ic, oc) = (al.color_channels(), bl.color_channels());
                let mut src = Vec::new();
                for i in 0..2048usize {
                    for c in 0..sc {
                        let j = i * sc + c;
                        src.push(if j % 9 == 4 {
                            odd[j / 9 % odd.len()]
                        } else {
                            ((i * 37 + c * 101) % 256) as f32 / 255.0
                        });
                    }
                }
                assert!(mostly_u8_points(&src, sc, ic));
                let mut dst = vec![-7.0f32; src.len() / sc * dc];
                t.transform(&src, &mut dst).unwrap();
                tabled += usize::from(t.u8_head.get().is_some_and(Option::is_some));
                for (sp, dp) in src.chunks_exact(sc).zip(dst.chunks_exact(dc)) {
                    let mut input = [0.0f64; 16];
                    for c in 0..ic {
                        input[c] = f64::from(sp[c]);
                    }
                    let mut out = [0.0f64; 16];
                    t.chain.eval(&input, &mut out);
                    if bl.is_lab() {
                        out[0] *= 100.0;
                        out[1] = out[1] * 255.0 - 128.0;
                        out[2] = out[2] * 255.0 - 128.0;
                    }
                    for c in 0..oc {
                        assert_eq!(
                            dp[c].to_bits(),
                            (out[c] as f32).to_bits(),
                            "{al:?} {bl:?} {sp:?}"
                        );
                    }
                    if bl.has_alpha() {
                        let want = if al.has_alpha() { sp[sc - 1] } else { 1.0 };
                        assert_eq!(dp[dc - 1].to_bits(), want.to_bits());
                    }
                }
            }
        }
        assert!(ran >= 20 && tabled >= 18, "{ran} {tabled}");
    }

    #[test]
    fn f32_loops_match_chain_eval_bit_for_bit() {
        let srgb = Profile::new_srgb();
        let p3 = Profile::new_display_p3();
        let lab = Profile::new_lab_identity();
        let gray = Profile::new_gray(crate::color::D65_XY, crate::Curve::gamma(2.2));
        let vals = [
            -0.5f32,
            0.0,
            0.001,
            0.2,
            0.5,
            0.999,
            1.0,
            1.5,
            37.0,
            -90.0,
            f32::NAN,
        ];
        for (a, al, b, bl) in [
            (&srgb, Layout::Rgb, &p3, Layout::Rgba),
            (&srgb, Layout::Rgba, &p3, Layout::Rgb),
            (&srgb, Layout::Rgba, &lab, Layout::Laba),
            (&lab, Layout::Lab, &srgb, Layout::Rgba),
            (&gray, Layout::GrayAlpha, &srgb, Layout::Rgb),
            (&srgb, Layout::Rgb, &gray, Layout::GrayAlpha),
        ] {
            let intent = Intent::RelativeColorimetric;
            let t = Transform::new(a, al, b, bl, intent, Options::default()).unwrap();
            let (sc, dc) = (al.channels(), bl.channels());
            let (ic, oc) = (al.color_channels(), bl.color_channels());
            let mut src = Vec::new();
            for i in 0..vals.len() * vals.len() {
                for c in 0..sc {
                    src.push(vals[(i / vals.len().pow((c % 2) as u32) + 3 * c) % vals.len()]);
                }
            }
            let mut dst = vec![-7.0f32; src.len() / sc * dc];
            t.transform(&src, &mut dst).unwrap();
            for (sp, dp) in src.chunks_exact(sc).zip(dst.chunks_exact(dc)) {
                let mut input = [0.0f64; 16];
                for c in 0..ic {
                    input[c] = f64::from(sp[c]);
                }
                if al.is_lab() {
                    input[0] /= 100.0;
                    input[1] = (input[1] + 128.0) / 255.0;
                    input[2] = (input[2] + 128.0) / 255.0;
                }
                let mut out = [0.0f64; 16];
                t.chain.eval(&input, &mut out);
                if bl.is_lab() {
                    out[0] *= 100.0;
                    out[1] = out[1] * 255.0 - 128.0;
                    out[2] = out[2] * 255.0 - 128.0;
                }
                for c in 0..oc {
                    assert_eq!(dp[c].to_bits(), (out[c] as f32).to_bits(), "{al:?} {bl:?}");
                }
                if bl.has_alpha() {
                    let want = if al.has_alpha() { sp[sc - 1] } else { 1.0 };
                    assert_eq!(dp[dc - 1].to_bits(), want.to_bits());
                }
            }
        }
    }

    #[test]
    fn from_unit_matches_the_clamped_form() {
        use crate::curve::clamp01;
        // The form released in 0.1.1, clamp first.
        let old8 = |v: f64| (clamp01(v) * 255.0 + 0.5) as u8;
        let old16 = |v: f64| (clamp01(v) * 65535.0 + 0.5) as u16;
        let mut vs = vec![
            f64::NAN,
            f64::INFINITY,
            f64::NEG_INFINITY,
            -0.0,
            0.0,
            1.0,
            f64::MIN_POSITIVE,
            -f64::MIN_POSITIVE,
            1.0 - f64::EPSILON,
            1.0 + f64::EPSILON,
            -1.0,
            -1.0e300,
            1.0e300,
            0.5 / 255.0,
            254.5 / 255.0,
            65534.5 / 65535.0,
        ];
        // A dense sweep past both ends and every rounding boundary of both
        // widths with its two neighbours.
        vs.extend((-2000..=12_000).map(|i| f64::from(i) / 10_000.0));
        for k in 0..=65535u32 {
            let b = (f64::from(k) + 0.5) / 65535.0;
            vs.extend([
                b,
                f64::from_bits(b.to_bits() - 1),
                f64::from_bits(b.to_bits() + 1),
            ]);
        }
        for k in 0..=255u32 {
            let b = (f64::from(k) + 0.5) / 255.0;
            vs.extend([
                b,
                f64::from_bits(b.to_bits() - 1),
                f64::from_bits(b.to_bits() + 1),
            ]);
        }
        for v in vs {
            assert_eq!(<u8 as Sample>::from_unit(v), old8(v), "u8 at {v:e}");
            assert_eq!(<u16 as Sample>::from_unit(v), old16(v), "u16 at {v:e}");
        }
    }

    /// The grey 8-bit output table against the engine's loop: every byte, with
    /// and without alpha on either side, one and three output channels, and
    /// the small-call path that does not build the table.
    #[test]
    fn axis16_table_matches_the_per_sample_grid_loop() {
        // A 16-bit call big enough to build the table, every 16-bit value on
        // every channel, through 4-input (17-point) and 3-input (33-point)
        // grids, against the per-sample loop.
        use crate::ColorSpace;
        let srgb = Profile::new_srgb();
        let cmyk = test_lut_profile(ColorSpace::Cmyk, 4, 0x0210_0000, (256, 256), true);
        for (a, al, b, bl) in [
            (&cmyk, Layout::Cmyk, &srgb, Layout::Rgb),
            (&cmyk, Layout::Cmyka, &srgb, Layout::Rgba),
            (&srgb, Layout::Rgb, &cmyk, Layout::Cmyk),
        ] {
            let t = Transform::new(a, al, b, bl, Intent::Perceptual, Options::default()).unwrap();
            let Engine::Grid(g) = &t.engine else {
                panic!("grid")
            };
            let (sc, dc) = (al.channels(), bl.channels());
            let px = 65536 + 3;
            let src: Vec<u16> = (0..px * sc)
                .map(|i| ((i / sc) as u32 * (1 + 2 * (i % sc) as u32) % 65536) as u16)
                .collect();
            let mut got = vec![0u16; px * dc];
            t.transform(&src, &mut got).unwrap();
            assert!(t.axis16.get().is_some());
            let mut want = vec![1u16; px * dc];
            let ch = (sc, dc, al.color_channels(), bl.color_channels());
            run_grid_u16(g, &src, &mut want, ch, (al.has_alpha(), bl.has_alpha()));
            assert_eq!(got, want, "{al:?} {bl:?}");
            // A small call after the table exists uses it too.
            let mut got2 = vec![0u16; 64 * dc];
            t.transform(&src[..64 * sc], &mut got2).unwrap();
            assert_eq!(got2, want[..64 * dc]);
            // Small calls build it once they add up to enough pixels.
            let t = Transform::new(a, al, b, bl, Intent::Perceptual, Options::default()).unwrap();
            for (k, (s, w)) in src
                .chunks(64 * sc)
                .zip(want.chunks(64 * dc))
                .take(300)
                .enumerate()
            {
                let mut got3 = vec![0u16; w.len()];
                t.transform(s, &mut got3).unwrap();
                assert_eq!(got3, w);
                assert_eq!(t.axis16.get().is_some(), (k + 1) * 64 >= AXIS16_MIN_PIXELS);
            }
        }
    }

    #[test]
    fn grey8_table_matches_the_shaper_loop() {
        let grey = Profile::new_gray(crate::color::D65_XY, crate::curve::Curve::gamma(1.8));
        let srgb = Profile::new_srgb();
        for (src_layout, dst_layout) in [
            (Layout::Gray, Layout::Rgb),
            (Layout::GrayAlpha, Layout::Rgba),
            (Layout::Gray, Layout::Rgba),
            (Layout::GrayAlpha, Layout::Rgb),
            (Layout::Gray, Layout::Gray),
            (Layout::GrayAlpha, Layout::GrayAlpha),
        ] {
            let (from, to) = if dst_layout.color_channels() == 3 {
                (&grey, &srgb)
            } else {
                (&grey, &grey)
            };
            let t = Transform::new(
                from,
                src_layout,
                to,
                dst_layout,
                Intent::Perceptual,
                Options::default(),
            )
            .unwrap();
            let Engine::Shaper(e) = &t.engine else {
                panic!("matrix-shaper expected");
            };
            let (sc, dc) = (src_layout.channels(), dst_layout.channels());
            let k = if dst_layout.color_channels() == 3 {
                3
            } else {
                1
            };
            let src: Vec<u8> = (0..300 * sc).map(|i| (i * 37 % 256) as u8).collect();
            let (sa, da) = (src_layout.has_alpha(), dst_layout.has_alpha());
            let mut want = vec![7u8; 300 * dc];
            if k == 3 {
                e.run_loop::<u8, 1, 3>(&e.in8, &src, &mut want, sc, dc, sa, da);
            } else {
                e.run_loop::<u8, 1, 1>(&e.in8, &src, &mut want, sc, dc, sa, da);
            }
            // Fewer than 256 pixels and no table yet: declined.
            let mut got = vec![7u8; 255 * dc];
            assert!(!e.run_grey8(&src[..255 * sc], &mut got, sc, dc, sa, da, k));
            assert!(e.grey8.get().is_none());
            let mut got = vec![7u8; 300 * dc];
            assert!(e.run_grey8(&src, &mut got, sc, dc, sa, da, k));
            assert_eq!(got, want, "{src_layout:?} -> {dst_layout:?}");
            // Built: small calls take it too, and the public path agrees.
            let mut got = vec![7u8; 3 * dc];
            assert!(e.run_grey8(&src[..3 * sc], &mut got, sc, dc, sa, da, k));
            assert_eq!(got, want[..3 * dc]);
            let mut got = vec![7u8; 300 * dc];
            t.transform(&src, &mut got).unwrap();
            assert_eq!(got, want);
        }
    }

    /// Every RGB matrix-shaper pair the kernels serve: built-ins, Adobe RGB
    /// (pure gamma), linear-light Rec. 2020, and the sampled-curve `ProPhoto`
    /// test profile, in both directions, with both intents and BPC.
    fn shaper_pairs() -> Vec<(String, Transform, Transform)> {
        use crate::color::{Xy, D65_XY};
        use crate::curve::Curve;
        let srgb = Profile::new_srgb();
        let p3 = Profile::new_display_p3();
        let rec2020 = Profile::new_rec2020();
        let prim2020 = [
            Xy::new(0.708, 0.292),
            Xy::new(0.170, 0.797),
            Xy::new(0.131, 0.046),
        ];
        let lin2020 = Profile::new_rgb(D65_XY, prim2020, Curve::gamma(1.0)).unwrap();
        let adobe_prim = [
            Xy::new(0.64, 0.33),
            Xy::new(0.21, 0.71),
            Xy::new(0.15, 0.06),
        ];
        let adobe = Profile::new_rgb(D65_XY, adobe_prim, Curve::gamma(563.0 / 256.0)).unwrap();
        let pro =
            Profile::new_from_slice(include_bytes!("../tests/data/ProPhoto-v2-micro.icc")).unwrap();
        let named = [
            ("srgb", &srgb),
            ("p3", &p3),
            ("rec2020", &rec2020),
            ("lin2020", &lin2020),
            ("adobe", &adobe),
            ("prophoto", &pro),
        ];
        let mut out = Vec::new();
        for (a, pa) in named {
            for (b, pb) in named {
                if a == b && a != "srgb" {
                    continue;
                }
                for (intent, bpc) in [
                    (Intent::RelativeColorimetric, false),
                    (Intent::Perceptual, true),
                ] {
                    let opts = Options {
                        black_point_compensation: bpc,
                    };
                    let mk = |sl, dl| Transform::new(pa, sl, pb, dl, intent, opts).unwrap();
                    let (rgb, rgba) =
                        (mk(Layout::Rgb, Layout::Rgb), mk(Layout::Rgba, Layout::Rgba));
                    assert!(rgb.is_matrix_shaper(), "{a} -> {b}");
                    out.push((format!("{a}->{b} {intent:?} bpc={bpc}"), rgb, rgba));
                }
            }
        }
        out
    }

    /// The scalar oracle of the matrix-shaper path: `run_loop` alone.
    fn shaper_oracle<T: Sample>(t: &Transform, src: &[T], dst: &mut [T]) {
        let Engine::Shaper(e) = &t.engine else {
            panic!("matrix-shaper expected");
        };
        let tables = if T::KIND == SampleKind::U8 {
            &e.in8[..]
        } else {
            e.in16()
        };
        let (sc, dc) = (t.src_layout.channels(), t.dst_layout.channels());
        let (sa, da) = (t.src_layout.has_alpha(), t.dst_layout.has_alpha());
        e.run_loop::<T, 3, 3>(tables, src, dst, sc, dc, sa, da);
    }

    fn xorshift(s: &mut u64) -> u64 {
        *s ^= *s << 13;
        *s ^= *s >> 7;
        *s ^= *s << 17;
        *s
    }

    /// 8-bit sample pixels: every combination of edge values, then
    /// pseudo-random ones (an odd count, so the kernels' tails run too).
    fn sample_u8(n: usize, ch: usize) -> Vec<u8> {
        let edges = [0u8, 1, 2, 127, 128, 253, 254, 255];
        let mut v = Vec::new();
        for &r in &edges {
            for &g in &edges {
                for &b in &edges {
                    v.extend_from_slice(&[r, g, b, r ^ g ^ b][..ch]);
                }
            }
        }
        let mut s = 0x9E37_79B9_7F4A_7C15u64;
        while v.len() < n * ch {
            let x = xorshift(&mut s);
            v.extend_from_slice(&x.to_le_bytes()[..ch]);
        }
        v.truncate(n * ch);
        v
    }

    fn sample_u16(n: usize, ch: usize) -> Vec<u16> {
        let edges = [0u16, 1, 2, 255, 256, 32767, 32768, 65533, 65534, 65535];
        let mut v = Vec::new();
        for &r in &edges {
            for &g in &edges {
                for &b in &edges {
                    v.extend_from_slice(&[r, g, b, r ^ g ^ b][..ch]);
                }
            }
        }
        let mut s = 0xD1B5_4A32_D192_ED03u64;
        while v.len() < n * ch {
            let x = xorshift(&mut s);
            for k in 0..ch {
                v.push((x >> (16 * k)) as u16);
            }
        }
        v.truncate(n * ch);
        v
    }

    /// The same chain with other layouts.
    fn relayout(t: &Transform, sl: Layout, dl: Layout) -> Transform {
        Transform::from_chain((*t.chain).clone(), sl, dl).unwrap()
    }

    /// The public 8- and 16-bit matrix-shaper path (AVX2 kernel where the
    /// CPU has it) against its scalar oracle, bit for bit: every pair, RGB
    /// and RGBA on both sides, edges plus pseudo-random pixels.
    #[test]
    fn shaper_kernels_match_the_scalar_loop() {
        let n = 20_003;
        for (name, rgb, _) in shaper_pairs() {
            for (sl, dl) in [
                (Layout::Rgb, Layout::Rgb),
                (Layout::Rgb, Layout::Rgba),
                (Layout::Rgba, Layout::Rgb),
                (Layout::Rgba, Layout::Rgba),
            ] {
                let t = relayout(&rgb, sl, dl);
                let src = sample_u8(n, sl.channels());
                let mut want = vec![0u8; n * dl.channels()];
                let mut got = vec![1u8; n * dl.channels()];
                shaper_oracle(&t, &src, &mut want);
                t.transform(&src, &mut got).unwrap();
                assert!(got == want, "u8 {name} {sl:?}->{dl:?}");
                let src = sample_u16(n, sl.channels());
                let mut want = vec![0u16; n * dl.channels()];
                let mut got = vec![1u16; n * dl.channels()];
                shaper_oracle(&t, &src, &mut want);
                t.transform(&src, &mut got).unwrap();
                assert!(got == want, "u16 {name} {sl:?}->{dl:?}");
            }
        }
    }

    /// 16-bit edge sweeps: each channel through all 65536 values with the
    /// other two at 0, 32768 and 65535, every pair, against the oracle.
    #[test]
    fn shaper_u16_kernel_matches_on_channel_sweeps() {
        let mut src = Vec::with_capacity(9 * 65536 * 3);
        for c in 0..3 {
            for other in [0u16, 32768, 65535] {
                for v in 0..=u16::MAX {
                    let mut px = [other; 3];
                    px[c] = v;
                    src.extend_from_slice(&px);
                }
            }
        }
        let mut want = vec![0u16; src.len()];
        let mut got = vec![1u16; src.len()];
        for (name, rgb, _) in shaper_pairs() {
            shaper_oracle(&rgb, &src, &mut want);
            rgb.transform(&src, &mut got).unwrap();
            assert!(got == want, "u16 sweep {name}");
        }
    }

    /// Every one of the 16,777,216 8-bit RGB colours, every pair, against the
    /// oracle (`cargo test --release -- --ignored shaper_u8_exhaustive`).
    #[test]
    #[ignore = "exhaustive: 2^24 colours per pair"]
    fn shaper_u8_exhaustive() {
        let src: Vec<u8> = (0..1u32 << 24)
            .flat_map(|v| [(v >> 16) as u8, (v >> 8) as u8, v as u8])
            .collect();
        let mut want = vec![0u8; src.len()];
        let mut got = vec![1u8; src.len()];
        let mut pairs = 0;
        for (name, rgb, _) in shaper_pairs() {
            shaper_oracle(&rgb, &src, &mut want);
            rgb.transform(&src, &mut got).unwrap();
            if let Some(i) = got.iter().zip(&want).position(|(a, b)| a != b) {
                let px = i / 3;
                panic!(
                    "u8 exhaustive {name}: pixel {px:06x} kernel {:?} oracle {:?}",
                    &got[3 * px..3 * px + 3],
                    &want[3 * px..3 * px + 3]
                );
            }
            pairs += 1;
        }
        eprintln!("shaper_u8_exhaustive: {pairs} pairs x 2^24 colours identical");
    }

    /// Reachability: on an AVX2 CPU the public 8- and 16-bit paths run the
    /// kernel on every whole group of 8 pixels (the census counts them).
    #[test]
    fn shaper_kernels_are_reached() {
        if !simd::avx2() {
            eprintln!("no AVX2: the scalar loop serves every pixel");
            return;
        }
        let (_, t, _) = shaper_pairs().swap_remove(0);
        let census = || simd::CENSUS.load(Ordering::Relaxed);
        let src = sample_u8(1001, 3);
        let mut dst = vec![0u8; src.len()];
        let before = census();
        t.transform(&src, &mut dst).unwrap();
        assert!(census() - before >= 1000, "u8 kernel not reached");
        let src = sample_u16(1001, 3);
        let mut dst = vec![0u16; src.len()];
        let before = census();
        t.transform(&src, &mut dst).unwrap();
        assert!(census() - before >= 1000, "u16 kernel not reached");
    }

    /// [`U8Codes`] against the function it files, `from_unit(lookup_out(v))`,
    /// for every output table of every pair: around every step (the 64 bit
    /// patterns either side), on a dense stride of bit patterns over 0..=1,
    /// and at the specials (NaN, infinities, negatives, past 1). Also: every
    /// pair's tables qualify (so the kernel takes the threshold path).
    #[test]
    fn u8_codes_match_the_output_tables() {
        let mut tables = 0;
        for (name, rgb, _) in shaper_pairs() {
            let Engine::Shaper(e) = &rgb.engine else {
                panic!("matrix-shaper expected");
            };
            for (k, out) in e.out.iter().enumerate() {
                let c = U8Codes::new(out).unwrap_or_else(|| panic!("{name}: table {k} declined"));
                tables += 1;
                let check = |v: f32| {
                    assert_eq!(
                        c.lookup(v),
                        U8Codes::code(out, v),
                        "{name} table {k} v {v:e} ({:08x})",
                        v.to_bits()
                    );
                };
                for &v in &[
                    f32::NAN,
                    -f32::NAN,
                    f32::INFINITY,
                    f32::NEG_INFINITY,
                    -0.0,
                    0.0,
                ] {
                    check(v);
                }
                for v in [-1.0f32, -1e-30, 1.0, 1.000_000_1, 2.0, f32::MAX] {
                    check(v);
                }
                for bits in (0..=0x3F80_0000u32).step_by(997) {
                    check(f32::from_bits(bits));
                }
                // Around each step of the table (found from the entries).
                for (j, &en) in c.table.iter().enumerate() {
                    let low = en & 0x7F_FFFF;
                    if low >= 1 << c.shift {
                        continue;
                    }
                    let step = ((j as u32) << c.shift) + low + c.base as u32;
                    for b in step.saturating_sub(64)..=(step + 64).min(0x3F80_0000) {
                        check(f32::from_bits(b));
                    }
                }
            }
        }
        assert!(tables > 0);
    }

    /// `f32` test pixels for the fast paths: a 33-point lattice in each
    /// channel (`k / 32`), the 8-bit points, pseudo-random values in 0..=1,
    /// and out-of-range ones (negative, past 1, -0.0, NaN, infinities).
    fn sample_f32(n: usize, ch: usize, out_of_range: bool) -> Vec<f32> {
        let mut v = Vec::new();
        let lattice: Vec<f32> = (0..=32).map(|k| k as f32 / 32.0).collect();
        for &a in &lattice {
            for &b in &lattice {
                for &c in lattice.iter().step_by(4) {
                    v.extend_from_slice(&[a, b, c, 1.0 - a][..ch]);
                }
            }
        }
        let mut s = 0xA076_1D64_78BD_642Fu64;
        let specials = [
            -0.0f32,
            -1e-6,
            -0.5,
            1.0 + 1e-6,
            1.5,
            f32::NAN,
            f32::INFINITY,
            f32::NEG_INFINITY,
        ];
        while v.len() < n * ch {
            let x = xorshift(&mut s);
            for k in 0..ch {
                let r = (x >> (16 * k)) as u16;
                let val = match r % 16 {
                    0 if out_of_range => specials[usize::from(r >> 4) % specials.len()],
                    1 => f32::from(r as u8) / 255.0,
                    _ => f32::from(r) / 65535.0,
                };
                v.push(val);
            }
        }
        v.truncate(n * ch);
        v
    }

    /// The fast matrix-shaper path's scalar twin for `t`.
    fn fast_oracle(t: &Transform, src: &[f32], dst: &mut [f32]) {
        let Engine::Shaper(e) = &t.engine else {
            panic!("matrix-shaper expected");
        };
        let f = e
            .fast
            .get_or_init(|| FastShaper::new(&e.link.in_curves, &e.link.out_curves))
            .as_ref()
            .expect("fast tables");
        let view = f.view(e.m, e.off);
        let sc_dc = (t.src_layout.channels(), t.dst_layout.channels());
        let alpha = (t.src_layout.has_alpha(), t.dst_layout.has_alpha());
        let mut near = Vec::new();
        view.run(src, dst, sc_dc, alpha, (0, &mut near));
        let chain = t.chain.as_shaper().expect("shaper chain");
        for k in near {
            let sp = &src[k * sc_dc.0..];
            let out = chain.eval([f64::from(sp[0]), f64::from(sp[1]), f64::from(sp[2])]);
            for (c, v) in out.iter().enumerate() {
                dst[k * sc_dc.1 + c] = *v as f32;
            }
        }
    }

    /// Which pairs have fast tables (and which keep the exact path).
    fn fast_shaper_pairs() -> Vec<(String, Transform)> {
        let mut out = Vec::new();
        for (name, rgb, _) in shaper_pairs() {
            let t = rgb.with_precision(Precision::Fast);
            let Engine::Shaper(e) = &t.engine else {
                unreachable!()
            };
            let ok = e
                .fast
                .get_or_init(|| FastShaper::new(&e.link.in_curves, &e.link.out_curves))
                .is_some();
            // Pure power-law output curves (Adobe RGB 2.2, ProPhoto 1.8)
            // are infinitely steep at 0, and the ProPhoto test profile's
            // coarse sampled curves have kinks a 4096-entry table misses by
            // more than 1e-6: those keep the exact path. Every pair of the
            // parametric and identity profiles must qualify.
            let family = |p: &str| ["srgb", "p3", "rec2020", "lin2020"].contains(&p);
            let (a, b) = name.split_once(' ').unwrap().0.split_once("->").unwrap();
            if family(a) && family(b) {
                assert!(ok, "{name}: no fast tables");
            }
            if !ok {
                eprintln!("fast shaper {name}: exact path (tables declined)");
            }
            if ok {
                out.push((name, t));
            }
        }
        out
    }

    /// The fast `f32` matrix-shaper kernel against its scalar twin, bit for
    /// bit (determinism with and without SIMD): every qualifying pair, every
    /// RGB/RGBA layout, in- and out-of-range values.
    #[test]
    fn fast_shaper_kernel_matches_its_scalar_twin() {
        let n = 20_003;
        for (name, t) in fast_shaper_pairs() {
            for (sl, dl) in [
                (Layout::Rgb, Layout::Rgb),
                (Layout::Rgb, Layout::Rgba),
                (Layout::Rgba, Layout::Rgb),
                (Layout::Rgba, Layout::Rgba),
            ] {
                let t = relayout(&t, sl, dl).with_precision(Precision::Fast);
                let src = sample_f32(n, sl.channels(), true);
                let mut want = vec![0f32; n * dl.channels()];
                let mut got = vec![1f32; n * dl.channels()];
                fast_oracle(&t, &src, &mut want);
                t.transform(&src, &mut got).unwrap();
                let same = got
                    .iter()
                    .zip(&want)
                    .all(|(a, b)| a.to_bits() == b.to_bits());
                assert!(same, "fast f32 {name} {sl:?}->{dl:?}");
            }
        }
    }

    /// The fast matrix-shaper error budget: within 1e-5 of the exact path
    /// per channel for every qualifying pair, on the lattice, the 8-bit
    /// points, pseudo-random values and out-of-range ones (the exact path
    /// clamps; so does the fast one, with no new clipping); NaN in gives
    /// what the exact path gives. Prints the worst error per pair.
    #[test]
    fn fast_shaper_error_budget() {
        let n = 300_007;
        let src = sample_f32(n, 3, true);
        let mut worst_all = 0f32;
        for (name, fast) in fast_shaper_pairs() {
            let exact = relayout(&fast, Layout::Rgb, Layout::Rgb);
            let mut want = vec![0f32; src.len()];
            let mut got = vec![0f32; src.len()];
            exact.transform(&src, &mut want).unwrap();
            fast.transform(&src, &mut got).unwrap();
            let mut worst = (0f32, 0usize);
            for (i, (a, b)) in got.iter().zip(&want).enumerate() {
                assert!((0.0..=1.0).contains(a), "{name}: fast {a} out of range");
                let d = (a - b).abs();
                if d > worst.0 {
                    worst = (d, i);
                }
            }
            let px = worst.1 / 3;
            assert!(
                worst.0 <= 1e-5,
                "{name}: |fast - exact| {:e} at {:?}",
                worst.0,
                &src[3 * px..3 * px + 3]
            );
            worst_all = worst_all.max(worst.0);
            eprintln!("fast shaper {name}: max |fast - exact| {:.3e}", worst.0);
        }
        eprintln!("fast shaper: worst over all pairs {worst_all:.3e}");
    }

    /// Reachability: the public fast `f32` path runs the AVX2 kernel.
    #[test]
    fn fast_shaper_kernel_is_reached() {
        if !simd::avx2() {
            eprintln!("no AVX2: the scalar twin serves every pixel");
            return;
        }
        let (_, t) = fast_shaper_pairs().swap_remove(0);
        let src = sample_f32(1001, 3, false);
        let mut dst = vec![0f32; src.len()];
        let before = simd::CENSUS.load(Ordering::Relaxed);
        t.transform(&src, &mut dst).unwrap();
        assert!(simd::CENSUS.load(Ordering::Relaxed) - before >= 1000);
    }

    /// The fast matrix-shaper error budget on dense sweeps: all 2^24 colours
    /// whose channels are 8-bit points (`k / 255`, the shape of float data
    /// decoded from 8-bit images) and a 257^3 lattice (`k / 256`), every
    /// qualifying pair (`cargo test --release -- --ignored fast_shaper_dense`).
    #[test]
    #[ignore = "dense: 2^24 + 257^3 colours per pair"]
    fn fast_shaper_dense() {
        let points: Vec<f32> = (0..1u32 << 24)
            .flat_map(|v| [v >> 16, (v >> 8) & 255, v & 255].map(|k| k as f32 / 255.0))
            .collect();
        let lattice: Vec<f32> = (0..257u32 * 257 * 257)
            .flat_map(|v| [v / (257 * 257), (v / 257) % 257, v % 257].map(|k| k as f32 / 256.0))
            .collect();
        let mut worst_all = 0f32;
        for (name, fast) in fast_shaper_pairs() {
            if name.contains("Perceptual") {
                continue; // matrix-shaper intents share one matrix
            }
            let exact = relayout(&fast, Layout::Rgb, Layout::Rgb);
            for (set, src) in [("8-bit points", &points), ("257^3 lattice", &lattice)] {
                let mut want = vec![0f32; src.len()];
                let mut got = vec![0f32; src.len()];
                exact.transform(src, &mut want).unwrap();
                fast.transform(src, &mut got).unwrap();
                let (worst, at) = got
                    .iter()
                    .zip(&want)
                    .map(|(a, b)| (a - b).abs())
                    .enumerate()
                    .fold((0f32, 0), |m, (i, d)| if d > m.0 { (d, i) } else { m });
                eprintln!(
                    "fast shaper dense {name} {set}: max |fast - exact| {worst:.3e} at {:?} -> fast {} exact {}",
                    &src[at / 3 * 3..at / 3 * 3 + 3],
                    got[at],
                    want[at]
                );
                assert!(worst <= 1e-5, "{name} {set}: {worst:e}");
                worst_all = worst_all.max(worst);
            }
        }
        eprintln!("fast shaper dense: worst {worst_all:.3e}");
    }
}
