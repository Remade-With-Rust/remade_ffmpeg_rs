//! Header attributes: the typed standard ones, and everything else as raw
//! bytes (kept, and written back unchanged).

use crate::error::{Error, Result};

/// An integer rectangle, inclusive on both ends (`box2i`).
#[derive(Debug, Clone, Copy, PartialEq, Eq, Hash)]
pub struct Box2i {
    pub x_min: i32,
    pub y_min: i32,
    pub x_max: i32,
    pub y_max: i32,
}

impl Box2i {
    /// A `width` x `height` box with its top-left corner at the origin.
    #[must_use]
    pub fn from_size(width: u32, height: u32) -> Self {
        Self {
            x_min: 0,
            y_min: 0,
            x_max: width as i32 - 1,
            y_max: height as i32 - 1,
        }
    }
    /// Width in pixels (`x_max - x_min + 1`); zero or negative when empty.
    #[must_use]
    pub fn width(&self) -> i64 {
        i64::from(self.x_max) - i64::from(self.x_min) + 1
    }
    /// Height in pixels.
    #[must_use]
    pub fn height(&self) -> i64 {
        i64::from(self.y_max) - i64::from(self.y_min) + 1
    }
}

/// Channel sample type.
#[derive(Debug, Clone, Copy, PartialEq, Eq, Hash)]
pub enum SampleType {
    /// 32-bit unsigned integer.
    U32,
    /// 16-bit float (half).
    F16,
    /// 32-bit float.
    F32,
}

impl SampleType {
    /// Bytes per sample.
    #[must_use]
    pub fn bytes(self) -> usize {
        match self {
            Self::F16 => 2,
            Self::U32 | Self::F32 => 4,
        }
    }
    pub(crate) fn code(self) -> i32 {
        match self {
            Self::U32 => 0,
            Self::F16 => 1,
            Self::F32 => 2,
        }
    }
    pub(crate) fn from_code(c: i32) -> Result<Self> {
        match c {
            0 => Ok(Self::U32),
            1 => Ok(Self::F16),
            2 => Ok(Self::F32),
            _ => Err(Error::invalid(format!("pixel type {c}"))),
        }
    }
}

/// One entry of a channel list (`chlist`).
#[derive(Debug, Clone, PartialEq, Eq, Hash)]
pub struct ChannelDesc {
    /// Full name, layer prefix included (`diffuse.R`).
    pub name: String,
    pub sample_type: SampleType,
    /// The `pLinear` hint (perceptually linear; used by B44 and DWA).
    pub p_linear: bool,
    pub x_sampling: i32,
    pub y_sampling: i32,
}

impl ChannelDesc {
    /// A full-resolution channel.
    ///
    /// `p_linear` gets the same default the `exr` crate gives
    /// (`AnyChannel::new`): false for channels named exactly `R`, `G`, `B`,
    /// `L`, `Y`, `X` or `Z` (any case), true for everything else (`A`,
    /// `diffuse.R`, `Z.depth` ...). That keeps files written through this
    /// crate byte-identical to the ones `exr` wrote. The flag only changes
    /// what the lossy B44/DWA codecs do; set it explicitly when it matters.
    #[must_use]
    pub fn new(name: impl Into<String>, sample_type: SampleType) -> Self {
        let name = name.into();
        let p_linear = !["R", "G", "B", "L", "Y", "X", "Z"]
            .iter()
            .any(|n| name.eq_ignore_ascii_case(n));
        Self {
            name,
            sample_type,
            p_linear,
            x_sampling: 1,
            y_sampling: 1,
        }
    }
}

/// Compression method.
#[derive(Debug, Clone, Copy, PartialEq, Eq, Hash)]
pub enum Compression {
    None,
    Rle,
    /// zlib, one scanline per block.
    Zips,
    /// zlib, sixteen scanlines per block.
    Zip,
    Piz,
    Pxr24,
    B44,
    B44a,
    Dwaa,
    Dwab,
}

impl Compression {
    /// The on-disk code.
    #[must_use]
    pub fn code(self) -> u8 {
        match self {
            Self::None => 0,
            Self::Rle => 1,
            Self::Zips => 2,
            Self::Zip => 3,
            Self::Piz => 4,
            Self::Pxr24 => 5,
            Self::B44 => 6,
            Self::B44a => 7,
            Self::Dwaa => 8,
            Self::Dwab => 9,
        }
    }
    pub(crate) fn from_code(c: u8) -> Result<Self> {
        Ok(match c {
            0 => Self::None,
            1 => Self::Rle,
            2 => Self::Zips,
            3 => Self::Zip,
            4 => Self::Piz,
            5 => Self::Pxr24,
            6 => Self::B44,
            7 => Self::B44a,
            8 => Self::Dwaa,
            9 => Self::Dwab,
            10 | 11 => return Err(Error::unsupported(format!("HTJ2K compression ({c})"))),
            _ => return Err(Error::unsupported(format!("compression code {c}"))),
        })
    }
    /// Scanlines per block for scanline images.
    #[must_use]
    pub fn lines_per_block(self) -> u32 {
        match self {
            Self::None | Self::Rle | Self::Zips => 1,
            Self::Zip | Self::Pxr24 => 16,
            Self::Piz | Self::B44 | Self::B44a | Self::Dwaa => 32,
            Self::Dwab => 256,
        }
    }
    /// True when decoding returns exactly the samples that were written,
    /// for every sample type.
    #[must_use]
    pub fn is_lossless(self) -> bool {
        matches!(
            self,
            Self::None | Self::Rle | Self::Zips | Self::Zip | Self::Piz
        )
    }
}

/// Order in which scanline blocks (or tiles) are stored.
#[derive(Debug, Clone, Copy, PartialEq, Eq, Hash)]
pub enum LineOrder {
    Increasing,
    Decreasing,
    /// Tiles only: any order (`RANDOM_Y`, "unspecified" in some APIs).
    Random,
}

impl LineOrder {
    pub(crate) fn code(self) -> u8 {
        match self {
            Self::Increasing => 0,
            Self::Decreasing => 1,
            Self::Random => 2,
        }
    }
}

/// Resolution levels of a tiled part.
#[derive(Debug, Clone, Copy, PartialEq, Eq, Hash)]
pub enum LevelMode {
    One,
    Mipmap,
    Ripmap,
}

/// How level sizes round when halving.
#[derive(Debug, Clone, Copy, PartialEq, Eq, Hash)]
pub enum RoundingMode {
    Down,
    Up,
}

/// `tiledesc`.
#[derive(Debug, Clone, Copy, PartialEq, Eq, Hash)]
pub struct TileDesc {
    pub x_size: u32,
    pub y_size: u32,
    pub level_mode: LevelMode,
    pub rounding: RoundingMode,
}

/// CIE xy chromaticities of the primaries and white point.
#[derive(Debug, Clone, Copy, PartialEq)]
pub struct Chromaticities {
    pub red: [f32; 2],
    pub green: [f32; 2],
    pub blue: [f32; 2],
    pub white: [f32; 2],
}

impl Chromaticities {
    /// Rec. 709 primaries, D65 white (the OpenEXR default).
    pub const REC709: Self = Self {
        red: [0.6400, 0.3300],
        green: [0.3000, 0.6000],
        blue: [0.1500, 0.0600],
        white: [0.3127, 0.3290],
    };
}

/// A preview image: 8-bit RGBA, non-linear.
#[derive(Debug, Clone, PartialEq, Eq)]
pub struct Preview {
    pub width: u32,
    pub height: u32,
    /// `width * height * 4` bytes, row-major RGBA.
    pub rgba: Vec<u8>,
}

/// SMPTE time code.
#[derive(Debug, Clone, Copy, PartialEq, Eq)]
pub struct TimeCode {
    pub time_and_flags: u32,
    pub user_data: u32,
}

/// Film key code.
#[derive(Debug, Clone, Copy, PartialEq, Eq)]
pub struct KeyCode {
    pub film_mfc_code: i32,
    pub film_type: i32,
    pub prefix: i32,
    pub count: i32,
    pub perf_offset: i32,
    pub perfs_per_frame: i32,
    pub perfs_per_count: i32,
}

/// An attribute value. Unknown types keep their bytes verbatim.
#[derive(Debug, Clone, PartialEq)]
pub enum AttributeValue {
    Box2i(Box2i),
    Box2f([f32; 4]),
    Channels(Vec<ChannelDesc>),
    Chromaticities(Chromaticities),
    Compression(Compression),
    Double(f64),
    EnvMap(u8),
    Float(f32),
    FloatVector(Vec<f32>),
    Int(i32),
    KeyCode(KeyCode),
    LineOrder(LineOrder),
    M33f([f32; 9]),
    M33d([f64; 9]),
    M44f([f32; 16]),
    M44d([f64; 16]),
    Preview(Preview),
    /// `rational`: numerator, denominator.
    Rational(i32, u32),
    /// `string`: raw bytes (usually UTF-8).
    Text(Vec<u8>),
    TextVector(Vec<Vec<u8>>),
    TileDesc(TileDesc),
    TimeCode(TimeCode),
    V2i([i32; 2]),
    V2f([f32; 2]),
    V2d([f64; 2]),
    V3i([i32; 3]),
    V3f([f32; 3]),
    V3d([f64; 3]),
    DeepImageState(u8),
    /// Any other type: its type name and raw value bytes.
    Custom {
        type_name: String,
        data: Vec<u8>,
    },
}

impl AttributeValue {
    /// The type name as stored in the file.
    #[must_use]
    pub fn type_name(&self) -> &str {
        match self {
            Self::Box2i(_) => "box2i",
            Self::Box2f(_) => "box2f",
            Self::Channels(_) => "chlist",
            Self::Chromaticities(_) => "chromaticities",
            Self::Compression(_) => "compression",
            Self::Double(_) => "double",
            Self::EnvMap(_) => "envmap",
            Self::Float(_) => "float",
            Self::FloatVector(_) => "floatvector",
            Self::Int(_) => "int",
            Self::KeyCode(_) => "keycode",
            Self::LineOrder(_) => "lineOrder",
            Self::M33f(_) => "m33f",
            Self::M33d(_) => "m33d",
            Self::M44f(_) => "m44f",
            Self::M44d(_) => "m44d",
            Self::Preview(_) => "preview",
            Self::Rational(..) => "rational",
            Self::Text(_) => "string",
            Self::TextVector(_) => "stringvector",
            Self::TileDesc(_) => "tiledesc",
            Self::TimeCode(_) => "timecode",
            Self::V2i(_) => "v2i",
            Self::V2f(_) => "v2f",
            Self::V2d(_) => "v2d",
            Self::V3i(_) => "v3i",
            Self::V3f(_) => "v3f",
            Self::V3d(_) => "v3d",
            Self::DeepImageState(_) => "deepImageState",
            Self::Custom { type_name, .. } => type_name,
        }
    }

    /// The string value, if this is a `string` attribute holding UTF-8.
    #[must_use]
    pub fn as_text(&self) -> Option<&str> {
        match self {
            Self::Text(b) => std::str::from_utf8(b).ok(),
            _ => None,
        }
    }
}

/// A named attribute.
#[derive(Debug, Clone, PartialEq)]
pub struct Attribute {
    pub name: String,
    pub value: AttributeValue,
}

/// One part's header: its attributes in file order.
#[derive(Debug, Clone, PartialEq, Default)]
pub struct Header {
    pub attributes: Vec<Attribute>,
}

impl Header {
    /// The attribute called `name`, if present.
    #[must_use]
    pub fn get(&self, name: &str) -> Option<&AttributeValue> {
        self.attributes
            .iter()
            .find(|a| a.name == name)
            .map(|a| &a.value)
    }

    /// Insert or replace an attribute (replacing keeps its position).
    pub fn set(&mut self, name: impl Into<String>, value: AttributeValue) {
        let name = name.into();
        if let Some(a) = self.attributes.iter_mut().find(|a| a.name == name) {
            a.value = value;
        } else {
            self.attributes.push(Attribute { name, value });
        }
    }

    /// Remove an attribute, returning its value.
    pub fn remove(&mut self, name: &str) -> Option<AttributeValue> {
        let i = self.attributes.iter().position(|a| a.name == name)?;
        Some(self.attributes.remove(i).value)
    }

    /// The channel list.
    #[must_use]
    pub fn channels(&self) -> &[ChannelDesc] {
        match self.get("channels") {
            Some(AttributeValue::Channels(c)) => c,
            _ => &[],
        }
    }

    /// The compression method (None when absent).
    #[must_use]
    pub fn compression(&self) -> Compression {
        match self.get("compression") {
            Some(AttributeValue::Compression(c)) => *c,
            _ => Compression::None,
        }
    }

    /// The data window.
    #[must_use]
    pub fn data_window(&self) -> Option<Box2i> {
        match self.get("dataWindow") {
            Some(AttributeValue::Box2i(b)) => Some(*b),
            _ => None,
        }
    }

    /// The display window.
    #[must_use]
    pub fn display_window(&self) -> Option<Box2i> {
        match self.get("displayWindow") {
            Some(AttributeValue::Box2i(b)) => Some(*b),
            _ => None,
        }
    }

    /// Line order (Increasing when absent).
    #[must_use]
    pub fn line_order(&self) -> LineOrder {
        match self.get("lineOrder") {
            Some(AttributeValue::LineOrder(l)) => *l,
            _ => LineOrder::Increasing,
        }
    }

    /// Tile description, for tiled parts.
    #[must_use]
    pub fn tiles(&self) -> Option<TileDesc> {
        match self.get("tiles") {
            Some(AttributeValue::TileDesc(t)) => Some(*t),
            _ => None,
        }
    }

    /// The part name (`name` attribute).
    #[must_use]
    pub fn name(&self) -> Option<&str> {
        self.get("name").and_then(AttributeValue::as_text)
    }

    /// The part type (`type` attribute: `scanlineimage`, `tiledimage`,
    /// `deepscanline`, `deeptile`).
    #[must_use]
    pub fn part_type(&self) -> Option<&str> {
        self.get("type").and_then(AttributeValue::as_text)
    }

    /// The `chromaticities` attribute.
    #[must_use]
    pub fn chromaticities(&self) -> Option<Chromaticities> {
        match self.get("chromaticities") {
            Some(AttributeValue::Chromaticities(c)) => Some(*c),
            _ => None,
        }
    }

    /// The preview image.
    #[must_use]
    pub fn preview(&self) -> Option<&Preview> {
        match self.get("preview") {
            Some(AttributeValue::Preview(p)) => Some(p),
            _ => None,
        }
    }

    /// The `dwaCompressionLevel` attribute.
    #[must_use]
    pub fn dwa_compression_level(&self) -> Option<f32> {
        match self.get("dwaCompressionLevel") {
            Some(AttributeValue::Float(f)) => Some(*f),
            _ => None,
        }
    }

    /// True for deep parts.
    #[must_use]
    pub fn is_deep(&self) -> bool {
        matches!(self.part_type(), Some("deepscanline" | "deeptile"))
    }
}

// ---------------------------------------------------------------------------
// Parsing.

/// A byte cursor that never reads past its slice.
pub(crate) struct Cursor<'a> {
    pub data: &'a [u8],
    pub pos: usize,
}

impl<'a> Cursor<'a> {
    pub fn new(data: &'a [u8]) -> Self {
        Self { data, pos: 0 }
    }
    pub fn remaining(&self) -> usize {
        self.data.len() - self.pos
    }
    pub fn bytes(&mut self, n: usize) -> Result<&'a [u8]> {
        if n > self.remaining() {
            return Err(Error::invalid("unexpected end of data"));
        }
        let s = &self.data[self.pos..self.pos + n];
        self.pos += n;
        Ok(s)
    }
    pub fn u8(&mut self) -> Result<u8> {
        Ok(self.bytes(1)?[0])
    }
    pub fn u32(&mut self) -> Result<u32> {
        let b = self.bytes(4)?;
        Ok(u32::from_le_bytes([b[0], b[1], b[2], b[3]]))
    }
    pub fn i32(&mut self) -> Result<i32> {
        Ok(self.u32()? as i32)
    }
    pub fn u64(&mut self) -> Result<u64> {
        let b = self.bytes(8)?;
        let mut a = [0u8; 8];
        a.copy_from_slice(b);
        Ok(u64::from_le_bytes(a))
    }
    pub fn f32(&mut self) -> Result<f32> {
        Ok(f32::from_bits(self.u32()?))
    }
    pub fn f64(&mut self) -> Result<f64> {
        Ok(f64::from_bits(self.u64()?))
    }
    /// A NUL-terminated string of at most `max` bytes (excluding the NUL).
    pub fn cstr(&mut self, max: usize) -> Result<&'a [u8]> {
        let rest = &self.data[self.pos..];
        let lim = rest.len().min(max + 1);
        let Some(end) = rest[..lim].iter().position(|&b| b == 0) else {
            return Err(Error::invalid("unterminated or over-long name"));
        };
        let s = &rest[..end];
        self.pos += end + 1;
        Ok(s)
    }
}

fn utf8(b: &[u8]) -> String {
    String::from_utf8_lossy(b).into_owned()
}

fn need(data: &[u8], n: usize, ty: &str) -> Result<()> {
    if data.len() == n {
        Ok(())
    } else {
        Err(Error::invalid(format!(
            "{ty} attribute of {} bytes",
            data.len()
        )))
    }
}

fn f32s<const N: usize>(data: &[u8], ty: &str) -> Result<[f32; N]> {
    need(data, N * 4, ty)?;
    let mut c = Cursor::new(data);
    let mut out = [0f32; N];
    for v in &mut out {
        *v = c.f32()?;
    }
    Ok(out)
}

fn f64s<const N: usize>(data: &[u8], ty: &str) -> Result<[f64; N]> {
    need(data, N * 8, ty)?;
    let mut c = Cursor::new(data);
    let mut out = [0f64; N];
    for v in &mut out {
        *v = c.f64()?;
    }
    Ok(out)
}

fn i32s<const N: usize>(data: &[u8], ty: &str) -> Result<[i32; N]> {
    need(data, N * 4, ty)?;
    let mut c = Cursor::new(data);
    let mut out = [0i32; N];
    for v in &mut out {
        *v = c.i32()?;
    }
    Ok(out)
}

fn parse_channels(data: &[u8], max_name: usize, max_channels: usize) -> Result<Vec<ChannelDesc>> {
    let mut c = Cursor::new(data);
    let mut out = Vec::new();
    loop {
        let name = c.cstr(max_name)?;
        if name.is_empty() {
            break;
        }
        if out.len() >= max_channels {
            return Err(Error::limit(format!("more than {max_channels} channels")));
        }
        let ty = SampleType::from_code(c.i32()?)?;
        let p_linear = c.u8()? != 0;
        c.bytes(3)?;
        let x_sampling = c.i32()?;
        let y_sampling = c.i32()?;
        if x_sampling < 1 || y_sampling < 1 {
            return Err(Error::invalid(format!(
                "channel sampling {x_sampling}x{y_sampling}"
            )));
        }
        out.push(ChannelDesc {
            name: utf8(name),
            sample_type: ty,
            p_linear,
            x_sampling,
            y_sampling,
        });
    }
    Ok(out)
}

pub(crate) fn parse_value(
    ty: &[u8],
    data: &[u8],
    max_name: usize,
    max_channels: usize,
) -> Result<AttributeValue> {
    use AttributeValue as V;
    Ok(match ty {
        b"box2i" => {
            let [a, b, c, d] = i32s::<4>(data, "box2i")?;
            V::Box2i(Box2i {
                x_min: a,
                y_min: b,
                x_max: c,
                y_max: d,
            })
        }
        b"box2f" => V::Box2f(f32s::<4>(data, "box2f")?),
        b"chlist" => V::Channels(parse_channels(data, max_name, max_channels)?),
        b"chromaticities" => {
            let v = f32s::<8>(data, "chromaticities")?;
            V::Chromaticities(Chromaticities {
                red: [v[0], v[1]],
                green: [v[2], v[3]],
                blue: [v[4], v[5]],
                white: [v[6], v[7]],
            })
        }
        b"compression" => {
            need(data, 1, "compression")?;
            V::Compression(Compression::from_code(data[0])?)
        }
        b"double" => V::Double(f64s::<1>(data, "double")?[0]),
        b"envmap" => {
            need(data, 1, "envmap")?;
            V::EnvMap(data[0])
        }
        b"float" => V::Float(f32s::<1>(data, "float")?[0]),
        b"floatvector" => {
            if data.len() % 4 != 0 {
                return Err(Error::invalid("floatvector size"));
            }
            V::FloatVector(
                data.chunks_exact(4)
                    .map(|b| f32::from_le_bytes([b[0], b[1], b[2], b[3]]))
                    .collect(),
            )
        }
        b"int" => V::Int(i32s::<1>(data, "int")?[0]),
        b"keycode" => {
            let v = i32s::<7>(data, "keycode")?;
            V::KeyCode(KeyCode {
                film_mfc_code: v[0],
                film_type: v[1],
                prefix: v[2],
                count: v[3],
                perf_offset: v[4],
                perfs_per_frame: v[5],
                perfs_per_count: v[6],
            })
        }
        b"lineOrder" => {
            need(data, 1, "lineOrder")?;
            V::LineOrder(match data[0] {
                0 => LineOrder::Increasing,
                1 => LineOrder::Decreasing,
                2 => LineOrder::Random,
                x => return Err(Error::invalid(format!("line order {x}"))),
            })
        }
        b"m33f" => V::M33f(f32s::<9>(data, "m33f")?),
        b"m33d" => V::M33d(f64s::<9>(data, "m33d")?),
        b"m44f" => V::M44f(f32s::<16>(data, "m44f")?),
        b"m44d" => V::M44d(f64s::<16>(data, "m44d")?),
        b"preview" => {
            let mut c = Cursor::new(data);
            let w = c.u32()?;
            let h = c.u32()?;
            let n = u64::from(w) * u64::from(h) * 4;
            if n != c.remaining() as u64 {
                return Err(Error::invalid("preview size"));
            }
            V::Preview(Preview {
                width: w,
                height: h,
                rgba: c.bytes(n as usize)?.to_vec(),
            })
        }
        b"rational" => {
            need(data, 8, "rational")?;
            let mut c = Cursor::new(data);
            V::Rational(c.i32()?, c.u32()?)
        }
        b"string" => V::Text(data.to_vec()),
        b"stringvector" => {
            let mut c = Cursor::new(data);
            let mut v = Vec::new();
            while c.remaining() > 0 {
                let n = c.i32()?;
                if n < 0 {
                    return Err(Error::invalid("stringvector entry length"));
                }
                v.push(c.bytes(n as usize)?.to_vec());
            }
            V::TextVector(v)
        }
        b"tiledesc" => {
            need(data, 9, "tiledesc")?;
            let mut c = Cursor::new(data);
            let x = c.u32()?;
            let y = c.u32()?;
            let mode = c.u8()?;
            let level_mode = match mode & 0xf {
                0 => LevelMode::One,
                1 => LevelMode::Mipmap,
                2 => LevelMode::Ripmap,
                m => return Err(Error::invalid(format!("tile level mode {m}"))),
            };
            let rounding = match mode >> 4 {
                0 => RoundingMode::Down,
                1 => RoundingMode::Up,
                r => return Err(Error::invalid(format!("tile rounding mode {r}"))),
            };
            V::TileDesc(TileDesc {
                x_size: x,
                y_size: y,
                level_mode,
                rounding,
            })
        }
        b"timecode" => {
            need(data, 8, "timecode")?;
            let mut c = Cursor::new(data);
            V::TimeCode(TimeCode {
                time_and_flags: c.u32()?,
                user_data: c.u32()?,
            })
        }
        b"v2i" => V::V2i(i32s::<2>(data, "v2i")?),
        b"v2f" => V::V2f(f32s::<2>(data, "v2f")?),
        b"v2d" => V::V2d(f64s::<2>(data, "v2d")?),
        b"v3i" => V::V3i(i32s::<3>(data, "v3i")?),
        b"v3f" => V::V3f(f32s::<3>(data, "v3f")?),
        b"v3d" => V::V3d(f64s::<3>(data, "v3d")?),
        b"deepImageState" => {
            need(data, 1, "deepImageState")?;
            V::DeepImageState(data[0])
        }
        _ => V::Custom {
            type_name: utf8(ty),
            data: data.to_vec(),
        },
    })
}

/// Parse one header (up to and including its terminating NUL).
pub(crate) fn parse_header(c: &mut Cursor<'_>, limits: &crate::Limits) -> Result<Header> {
    // Like OpenEXR's default (non-strict) reader, accept names up to 255
    // bytes whether or not the long-names flag is set: writers (the `exr`
    // crate among them) put long channel names in files without the flag.
    let max_name = 255;
    let mut h = Header::default();
    loop {
        let name = c.cstr(max_name)?;
        if name.is_empty() {
            return Ok(h);
        }
        let ty = c.cstr(max_name)?;
        let size = c.i32()?;
        if size < 0 {
            return Err(Error::invalid("negative attribute size"));
        }
        if size as u64 > limits.max_attribute_bytes {
            return Err(Error::limit(format!("attribute of {size} bytes")));
        }
        let data = c.bytes(size as usize)?;
        let value = parse_value(ty, data, max_name, limits.max_channels)?;
        let name = utf8(name);
        if h.attributes.iter().any(|a| a.name == name) {
            return Err(Error::invalid(format!("duplicate attribute {name}")));
        }
        h.attributes.push(Attribute { name, value });
    }
}

// ---------------------------------------------------------------------------
// Writing.

fn put_i32(out: &mut Vec<u8>, v: i32) {
    out.extend_from_slice(&v.to_le_bytes());
}
fn put_f32(out: &mut Vec<u8>, v: f32) {
    out.extend_from_slice(&v.to_le_bytes());
}
fn put_f64(out: &mut Vec<u8>, v: f64) {
    out.extend_from_slice(&v.to_le_bytes());
}

pub(crate) fn encode_value(v: &AttributeValue) -> Vec<u8> {
    use AttributeValue as V;
    let mut o = Vec::new();
    match v {
        V::Box2i(b) => {
            for x in [b.x_min, b.y_min, b.x_max, b.y_max] {
                put_i32(&mut o, x);
            }
        }
        V::Box2f(a) => a.iter().for_each(|&x| put_f32(&mut o, x)),
        V::Channels(chs) => {
            for ch in chs {
                o.extend_from_slice(ch.name.as_bytes());
                o.push(0);
                put_i32(&mut o, ch.sample_type.code());
                o.push(u8::from(ch.p_linear));
                o.extend_from_slice(&[0, 0, 0]);
                put_i32(&mut o, ch.x_sampling);
                put_i32(&mut o, ch.y_sampling);
            }
            o.push(0);
        }
        V::Chromaticities(c) => {
            for x in [c.red, c.green, c.blue, c.white].iter().flatten() {
                put_f32(&mut o, *x);
            }
        }
        V::Compression(c) => o.push(c.code()),
        V::Double(d) => put_f64(&mut o, *d),
        V::EnvMap(e) | V::DeepImageState(e) => o.push(*e),
        V::Float(f) => put_f32(&mut o, *f),
        V::FloatVector(v) => v.iter().for_each(|&x| put_f32(&mut o, x)),
        V::Int(i) => put_i32(&mut o, *i),
        V::KeyCode(k) => {
            for x in [
                k.film_mfc_code,
                k.film_type,
                k.prefix,
                k.count,
                k.perf_offset,
                k.perfs_per_frame,
                k.perfs_per_count,
            ] {
                put_i32(&mut o, x);
            }
        }
        V::LineOrder(l) => o.push(l.code()),
        V::M33f(a) => a.iter().for_each(|&x| put_f32(&mut o, x)),
        V::M33d(a) => a.iter().for_each(|&x| put_f64(&mut o, x)),
        V::M44f(a) => a.iter().for_each(|&x| put_f32(&mut o, x)),
        V::M44d(a) => a.iter().for_each(|&x| put_f64(&mut o, x)),
        V::Preview(p) => {
            o.extend_from_slice(&p.width.to_le_bytes());
            o.extend_from_slice(&p.height.to_le_bytes());
            o.extend_from_slice(&p.rgba);
        }
        V::Rational(n, d) => {
            put_i32(&mut o, *n);
            o.extend_from_slice(&d.to_le_bytes());
        }
        V::Text(t) => o.extend_from_slice(t),
        V::TextVector(v) => {
            for s in v {
                put_i32(&mut o, s.len() as i32);
                o.extend_from_slice(s);
            }
        }
        V::TileDesc(t) => {
            o.extend_from_slice(&t.x_size.to_le_bytes());
            o.extend_from_slice(&t.y_size.to_le_bytes());
            let lm = match t.level_mode {
                LevelMode::One => 0u8,
                LevelMode::Mipmap => 1,
                LevelMode::Ripmap => 2,
            };
            let rm = match t.rounding {
                RoundingMode::Down => 0u8,
                RoundingMode::Up => 1,
            };
            o.push(lm | (rm << 4));
        }
        V::TimeCode(t) => {
            o.extend_from_slice(&t.time_and_flags.to_le_bytes());
            o.extend_from_slice(&t.user_data.to_le_bytes());
        }
        V::V2i(a) => a.iter().for_each(|&x| put_i32(&mut o, x)),
        V::V2f(a) => a.iter().for_each(|&x| put_f32(&mut o, x)),
        V::V2d(a) => a.iter().for_each(|&x| put_f64(&mut o, x)),
        V::V3i(a) => a.iter().for_each(|&x| put_i32(&mut o, x)),
        V::V3f(a) => a.iter().for_each(|&x| put_f32(&mut o, x)),
        V::V3d(a) => a.iter().for_each(|&x| put_f64(&mut o, x)),
        V::Custom { data, .. } => o.extend_from_slice(data),
    }
    o
}

pub(crate) fn write_attribute(out: &mut Vec<u8>, name: &str, value: &AttributeValue) {
    out.extend_from_slice(name.as_bytes());
    out.push(0);
    out.extend_from_slice(value.type_name().as_bytes());
    out.push(0);
    let data = encode_value(value);
    put_i32(out, data.len() as i32);
    out.extend_from_slice(&data);
}
