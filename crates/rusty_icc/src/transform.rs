//! Colour transforms between profiles.

use std::sync::{Arc, OnceLock};

use crate::color::delta_e_2000;
use crate::curve::{clamp01, InvCurve};
use crate::error::{Error, Result};
use crate::lut::ClutEval;
use crate::pipeline::{build_chain, Chain, GamutChain, ShaperLink};
use crate::profile::{ColorSpace, Profile};

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
}

impl Sample for u8 {
    const KIND: SampleKind = SampleKind::U8;
    const TABLE_LEN: usize = 256;
    fn to_unit(self) -> f64 {
        f64::from(self) / 255.0
    }
    fn from_unit(v: f64) -> Self {
        (clamp01(v) * 255.0 + 0.5) as Self
    }
    fn opaque() -> Self {
        Self::MAX
    }
    fn index(self) -> usize {
        usize::from(self)
    }
}

impl Sample for u16 {
    const KIND: SampleKind = SampleKind::U16;
    const TABLE_LEN: usize = 65536;
    fn to_unit(self) -> f64 {
        f64::from(self) / 65535.0
    }
    fn from_unit(v: f64) -> Self {
        (clamp01(v) * 65535.0 + 0.5) as Self
    }
    fn opaque() -> Self {
        Self::MAX
    }
    fn index(self) -> usize {
        usize::from(self)
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
}

/// Entries of the inverse-curve output tables, indexed by `sqrt(linear)`
/// so the steep start of a gamma inverse is sampled densely.
const OUT_TABLE: usize = 16384;

/// Matrix-shaper engine for integer samples. Tables are flat, one fixed-size
/// row per channel (`TABLE_LEN` input entries, `OUT_TABLE` output entries),
/// so the hot loop slices a row once and its indexing is provably in bounds.
struct ShaperEngine {
    link: ShaperLink,
    m: [[f32; 3]; 3],
    off: [f32; 3],
    in8: Box<[f32]>,
    in16: OnceLock<Box<[f32]>>,
    out: Box<[[f32; OUT_TABLE]]>,
}

impl ShaperEngine {
    fn table(link: &ShaperLink, n: usize) -> Box<[f32]> {
        link.in_curves
            .iter()
            .flat_map(|c| (0..n).map(move |i| c.eval(i as f64 / (n - 1) as f64) as f32))
            .collect()
    }

    fn new(link: ShaperLink) -> Self {
        let in8 = Self::table(&link, 256);
        let mut out = vec![[0.0f32; OUT_TABLE]; link.out_curves.len()].into_boxed_slice();
        for (row, c) in out.iter_mut().zip(&link.out_curves) {
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
        Self {
            link,
            m,
            off,
            in8,
            in16: OnceLock::new(),
            out,
        }
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

    /// The integer hot loop, monomorphised on the colour channel counts so
    /// every per-channel index is a constant.
    #[allow(clippy::too_many_arguments)]
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
    Shaper(ShaperEngine),
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
            Engine::Shaper(ShaperEngine::new(link))
        } else {
            let g = grid_points(chain.in_ch);
            let total = (0..chain.in_ch).fold(1usize, |a, _| a * g);
            let mut data = Vec::with_capacity(total * chain.out_ch);
            let mut input = [0.0f64; 16];
            let mut out = [0.0f64; 16];
            for idx in 0..total {
                let mut rem = idx;
                for c in (0..chain.in_ch).rev() {
                    input[c] = (rem % g) as f64 / (g - 1) as f64;
                    rem /= g;
                }
                chain.eval(&input, &mut out);
                data.extend(out[..chain.out_ch].iter().map(|&v| v as f32));
            }
            Engine::Grid(ClutEval::from_f32(vec![g; chain.in_ch], chain.out_ch, data))
        };
        Ok(Self {
            src_layout,
            dst_layout,
            chain: Arc::new(chain),
            engine,
        })
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
        if src.len() % sc != 0 || dst.len() != src.len() / sc * dc {
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
                run_grid(g, src, dst, (sc, dc, in_ch, out_ch), (src_alpha, dst_alpha));
            }
            _ => {
                let src_lab = self.src_layout.is_lab();
                let dst_lab = self.dst_layout.is_lab();
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
        if src.len() % sc != 0 || out.len() != src.len() / sc {
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
