//! Pure-Rust JPEG 2000 (ISO/IEC 15444-1) decoder: raw codestreams and JP2/JPX
//! files, as found on their own and in PDF `JPXDecode` streams.
//!
//! ```no_run
//! let file = std::fs::read("image.jp2")?;
//! let image = rusty_jpx::decode_jpx(&file)?;
//! println!("{}x{}", image.width, image.height);
//! for c in &image.components {
//!     println!("{} bits, {:?}, {} samples", c.bits, c.role, c.data.len());
//! }
//! # Ok::<_, Box<dyn std::error::Error>>(())
//! ```
//!
//! Samples come back as they were coded, component by component: no colour
//! conversion ([`Jpx::color`] and [`Jpx::icc`] say what the values mean) and
//! no chroma upsampling unless asked for ([`Jpx::upsampled`]).
//!
//! Malformed input returns an error; it does not panic, and it does not
//! allocate or loop beyond [`Limits`].
//!
//! The API is safe. `unsafe` code is confined to one module, the sample
//! kernels (AVX2, SSE2 and NEON, with a scalar twin that the tests hold them
//! to).
#![deny(unsafe_code)]
#![warn(missing_docs)]

mod codestream;
mod decode;
mod dwt;
mod error;
mod jp2;
mod kernels;
mod mq;
mod t1;
mod tagtree;
mod tile;

pub use error::{JpxError, JpxResult};

/// Bounds on what a file may ask for.
#[derive(Debug, Clone, PartialEq, Eq)]
#[non_exhaustive]
pub struct Limits {
    /// Output samples, over all components (after `reduce`); each takes four
    /// bytes in the result and up to four more while decoding. Default
    /// 2^28.
    pub max_samples: u64,
    /// Components in the codestream. Default 1024.
    pub max_components: u32,
    /// Resolutions, precincts and code-blocks, over all tiles. Default 2^22.
    pub max_structures: u64,
}

impl Default for Limits {
    fn default() -> Self {
        Limits {
            max_samples: 1 << 28,
            max_components: 1024,
            max_structures: 1 << 22,
        }
    }
}

/// How to decode.
#[derive(Debug, Clone, PartialEq, Eq)]
#[non_exhaustive]
pub struct DecodeOptions {
    /// Resolutions to discard: each halves width and height. Must not exceed
    /// any tile-component's decomposition levels ([`Info::max_reduce`]).
    pub reduce: u8,
    /// Return what could be decoded of incomplete or damaged packet data
    /// (with [`Jpx::truncated`] set) instead of an error. Tiles that are
    /// missing altogether are zero.
    pub allow_truncated: bool,
    /// Expand a palette (`pclr`/`cmap` boxes) into its channels. Default on.
    pub apply_palette: bool,
    /// Bounds on what the file may ask for.
    pub limits: Limits,
}

impl Default for DecodeOptions {
    fn default() -> Self {
        DecodeOptions {
            reduce: 0,
            allow_truncated: false,
            apply_palette: true,
            limits: Limits::default(),
        }
    }
}

impl DecodeOptions {
    /// Default options.
    #[must_use]
    pub fn new() -> Self {
        Self::default()
    }

    /// Set [`DecodeOptions::reduce`].
    #[must_use]
    pub fn with_reduce(mut self, reduce: u8) -> Self {
        self.reduce = reduce;
        self
    }

    /// Set [`DecodeOptions::allow_truncated`].
    #[must_use]
    pub fn with_allow_truncated(mut self, allow: bool) -> Self {
        self.allow_truncated = allow;
        self
    }

    /// Set [`DecodeOptions::limits`].
    #[must_use]
    pub fn with_limits(mut self, limits: Limits) -> Self {
        self.limits = limits;
        self
    }

    /// Set [`DecodeOptions::apply_palette`].
    #[must_use]
    pub fn with_apply_palette(mut self, apply: bool) -> Self {
        self.apply_palette = apply;
        self
    }
}

/// Colour specification of the file (the first `colr` box that is understood).
#[derive(Debug, Clone, Copy, PartialEq, Eq)]
#[non_exhaustive]
pub enum ColorSpace {
    /// None given: a raw codestream, or no `colr` box.
    Unspecified,
    /// Bi-level (enumerated 0 or 15).
    Bilevel,
    /// Luminance (17).
    Grey,
    /// sRGB (16).
    Srgb,
    /// sYCC (18): full-range YCbCr of sRGB. Samples are returned as YCC.
    Sycc,
    /// e-sYCC (24).
    ESycc,
    /// e-sRGB (20).
    ESrgb,
    /// ROMM-RGB (21).
    RommRgb,
    /// CMY (11).
    Cmy,
    /// CMYK (12).
    Cmyk,
    /// YCCK (13).
    Ycck,
    /// CIE L*a*b* (14).
    CieLab,
    /// An ICC profile, in [`Jpx::icc`].
    Icc,
    /// Any other enumerated value.
    Other(u32),
}

impl ColorSpace {
    fn from_enum(v: u32) -> Self {
        match v {
            0 | 15 => Self::Bilevel,
            11 => Self::Cmy,
            12 => Self::Cmyk,
            13 => Self::Ycck,
            14 => Self::CieLab,
            16 => Self::Srgb,
            17 => Self::Grey,
            18 => Self::Sycc,
            20 => Self::ESrgb,
            21 => Self::RommRgb,
            24 => Self::ESycc,
            v => Self::Other(v),
        }
    }
}

/// What a channel is for (`cdef` box).
#[derive(Debug, Clone, Copy, PartialEq, Eq)]
#[non_exhaustive]
pub enum ChannelRole {
    /// A colour channel. Without a `cdef` box every channel is one.
    Color,
    /// Opacity (PDF `SMaskInData` 1).
    Opacity,
    /// Opacity the colour channels are premultiplied with (`SMaskInData` 2).
    PremultipliedOpacity,
    /// Something else.
    Unspecified,
}

/// One decoded component, or palette channel.
#[derive(Debug, Clone, PartialEq, Eq)]
#[non_exhaustive]
pub struct Component {
    /// Bits per sample.
    pub bits: u8,
    /// Whether samples are signed.
    pub signed: bool,
    /// What the channel is for.
    pub role: ChannelRole,
    /// Width in samples; smaller than the image's when subsampled.
    pub width: u32,
    /// Height in samples.
    pub height: u32,
    /// Horizontal subsampling.
    pub dx: u32,
    /// Vertical subsampling.
    pub dy: u32,
    /// Samples, row by row.
    pub data: Vec<i32>,
    /// First sample position on the component's own grid.
    x0: u32,
    y0: u32,
}

/// A decoded image.
#[derive(Debug, Clone, PartialEq, Eq)]
#[non_exhaustive]
pub struct Jpx {
    /// Image width (after `reduce`).
    pub width: u32,
    /// Image height.
    pub height: u32,
    /// Channels, colour first (in `cdef` association order), then the rest.
    pub components: Vec<Component>,
    /// Colour specification.
    pub color: ColorSpace,
    /// ICC profile, when the file has one.
    pub icc: Option<Vec<u8>>,
    /// Whether a channel holds opacity: see [`Jpx::alpha`].
    pub has_alpha: bool,
    /// Packet data was incomplete or damaged (only with
    /// [`DecodeOptions::allow_truncated`]).
    pub truncated: bool,
    /// Image origin on the (reduced) reference grid.
    x0: u32,
    y0: u32,
}

impl Jpx {
    /// The opacity channel, straight or premultiplied: PDF's soft mask when
    /// `SMaskInData` is not 0.
    #[must_use]
    pub fn alpha(&self) -> Option<&Component> {
        self.components.iter().find(|c| {
            matches!(
                c.role,
                ChannelRole::Opacity | ChannelRole::PremultipliedOpacity
            )
        })
    }

    /// The colour channels.
    pub fn colors(&self) -> impl Iterator<Item = &Component> {
        self.components
            .iter()
            .filter(|c| c.role == ChannelRole::Color)
    }

    /// Samples of component `i` at full image size, repeating subsampled
    /// ones. `None` when there is no such component or it has no samples.
    #[must_use]
    pub fn upsampled(&self, i: usize) -> Option<std::borrow::Cow<'_, [i32]>> {
        let c = self.components.get(i)?;
        if (c.width, c.height) == (self.width, self.height) {
            return Some(std::borrow::Cow::Borrowed(&c.data));
        }
        if c.data.is_empty() {
            return None;
        }
        let index = |v: u32, origin: u32, step: u32, first: u32, n: u32| {
            (((u64::from(origin) + u64::from(v)) / u64::from(step))
                .saturating_sub(first.into())
                .min(u64::from(n) - 1)) as usize
        };
        let columns: Vec<usize> = (0..self.width)
            .map(|x| index(x, self.x0, c.dx, c.x0, c.width))
            .collect();
        let mut out = Vec::new();
        out.try_reserve_exact(self.width as usize * self.height as usize)
            .ok()?;
        for y in 0..self.height {
            let row = index(y, self.y0, c.dy, c.y0, c.height) * c.width as usize;
            let row = &c.data[row..row + c.width as usize];
            out.extend(columns.iter().map(|&x| row[x]));
        }
        Some(std::borrow::Cow::Owned(out))
    }
}

/// Header-level description of a component.
#[derive(Debug, Clone, Copy, PartialEq, Eq)]
#[non_exhaustive]
pub struct ComponentInfo {
    /// Bits per sample (1 to 38).
    pub bits: u8,
    /// Whether samples are signed.
    pub signed: bool,
    /// Horizontal subsampling.
    pub dx: u32,
    /// Vertical subsampling.
    pub dy: u32,
}

/// What the headers say, without decoding.
#[derive(Debug, Clone, PartialEq, Eq)]
#[non_exhaustive]
pub struct Info {
    /// Image width.
    pub width: u32,
    /// Image height.
    pub height: u32,
    /// Codestream components (a palette is not expanded).
    pub components: Vec<ComponentInfo>,
    /// Colour specification.
    pub color: ColorSpace,
    /// ICC profile, when the file has one.
    pub icc: Option<Vec<u8>>,
    /// Whether a `cdef` box declares an opacity channel.
    pub has_alpha: bool,
    /// Whether the file has a palette.
    pub has_palette: bool,
    /// Tiles.
    pub tiles: u32,
    /// Quality layers.
    pub layers: u32,
    /// Largest [`DecodeOptions::reduce`] the main header allows (a tile's own
    /// header may lower it).
    pub max_reduce: u8,
    /// Whether every component uses the reversible wavelet.
    pub reversible: bool,
}

/// Read the headers only.
pub fn info(data: &[u8]) -> JpxResult<Info> {
    let container = jp2::parse(data)?;
    let (siz, params) = codestream::parse_main(container.codestream, &Limits::default())?;
    let mut max_reduce = 32;
    let mut reversible = true;
    for c in 0..siz.comps.len() {
        let coding = params.coding(c)?;
        max_reduce = max_reduce.min(coding.levels);
        reversible &= coding.reversible;
    }
    Ok(Info {
        width: siz.x1 - siz.x0,
        height: siz.y1 - siz.y0,
        components: siz
            .comps
            .iter()
            .map(|c| ComponentInfo {
                bits: c.bits,
                signed: c.signed,
                dx: c.dx,
                dy: c.dy,
            })
            .collect(),
        color: first_color(&container),
        icc: container.icc.map(<[u8]>::to_vec),
        has_alpha: container.cdef.iter().any(|d| matches!(d.1, 1 | 2)),
        has_palette: container.palette.is_some(),
        tiles: siz.tiles_x * siz.tiles_y,
        layers: params.layers,
        max_reduce,
        reversible,
    })
}

fn first_color(c: &jp2::Container) -> ColorSpace {
    c.colors.first().copied().unwrap_or(ColorSpace::Unspecified)
}

/// Decode a JPEG 2000 image (raw codestream, JP2 or JPX) with the default
/// options.
pub fn decode_jpx(data: &[u8]) -> JpxResult<Jpx> {
    decode_jpx_with(data, &DecodeOptions::default())
}

/// Decode a JPEG 2000 image.
pub fn decode_jpx_with(data: &[u8], options: &DecodeOptions) -> JpxResult<Jpx> {
    let container = jp2::parse(data)?;
    let decoded = decode::decode(container.codestream, options)?;
    let siz = &decoded.siz;
    let reduce = u32::from(options.reduce);
    let mut components = Vec::with_capacity(siz.comps.len());
    for (c, (k, data)) in siz.comps.iter().zip(decoded.samples).enumerate() {
        let r = siz.comp_rect(c, reduce);
        components.push(Component {
            bits: k.bits,
            signed: k.signed,
            role: ChannelRole::Color,
            width: r[2].saturating_sub(r[0]),
            height: r[3].saturating_sub(r[1]),
            dx: k.dx,
            dy: k.dy,
            data,
            x0: r[0],
            y0: r[1],
        });
    }
    if options.apply_palette {
        if let Some(p) = &container.palette {
            // A palette without a mapping maps every column from component 0.
            let default: Vec<_> = (0..p.columns.len()).map(|i| (0, 1, i as u8)).collect();
            let cmap = if container.cmap.is_empty() {
                &default
            } else {
                &container.cmap
            };
            components = jp2::apply_palette(components, p, cmap, options.limits.max_samples)?;
        }
    }
    apply_cdef(&mut components, &container.cdef);
    let has_alpha = components.iter().any(|c| {
        matches!(
            c.role,
            ChannelRole::Opacity | ChannelRole::PremultipliedOpacity
        )
    });
    let grid = [siz.x0, siz.y0, siz.x1, siz.y1].map(|v| codestream::ceil_shift(v, reduce));
    Ok(Jpx {
        width: grid[2].saturating_sub(grid[0]),
        height: grid[3].saturating_sub(grid[1]),
        components,
        color: first_color(&container),
        icc: container.icc.map(<[u8]>::to_vec),
        has_alpha,
        truncated: decoded.truncated,
        x0: grid[0],
        y0: grid[1],
    })
}

/// Set channel roles and put colour channels first, in association order.
fn apply_cdef(components: &mut Vec<Component>, cdef: &[(u16, u16, u16)]) {
    if cdef.is_empty() {
        return;
    }
    let mut order = vec![u32::MAX; components.len()];
    for &(channel, kind, association) in cdef {
        let Some(c) = components.get_mut(usize::from(channel)) else {
            continue;
        };
        c.role = match kind {
            0 => ChannelRole::Color,
            1 => ChannelRole::Opacity,
            2 => ChannelRole::PremultipliedOpacity,
            _ => ChannelRole::Unspecified,
        };
        if kind == 0 {
            order[usize::from(channel)] = u32::from(association);
        }
    }
    let mut tagged: Vec<(u32, Component)> = order.into_iter().zip(components.drain(..)).collect();
    tagged.sort_by_key(|t| t.0);
    components.extend(tagged.into_iter().map(|t| t.1));
}
