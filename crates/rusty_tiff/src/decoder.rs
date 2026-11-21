//! The TIFF decoder.

use std::collections::HashSet;
use std::io::{IoSliceMut, Read, Seek};

use crate::ccitt;
use crate::error::{TiffError, TiffResult};
use crate::ifd::{self, ByteOrder, Ifd, Stream};
use crate::predictor;
use crate::tags::{compression, photometric, predictor as pred, sample_format, Tag};
use crate::value::Value;
use crate::{deflate, lzw, packbits};

/// Resource limits. Every allocation the decoder makes from file data is
/// checked against one of these first (or is bounded by the file length).
#[derive(Clone, Debug, PartialEq, Eq)]
#[non_exhaustive]
pub struct Limits {
    /// Maximum size in bytes of a decoded image buffer.
    pub decoding_buffer_size: usize,
    /// Maximum size in bytes of one tag value.
    pub ifd_value_size: usize,
    /// Maximum size in bytes of one compressed or decompressed chunk.
    pub intermediate_buffer_size: usize,
    /// How much of a damaged image may be zero-filled. A strip or tile whose
    /// data ends early keeps what it holds and the rest reads as zero, but
    /// only while the zero-filled bytes stay within this many times the bytes
    /// really decoded, plus 64 KiB (so small images always recover). Past
    /// that the image is refused: a damaged size field (say `SamplesPerPixel`
    /// = 12,800) must not turn a 53 KB file into hundreds of megabytes of
    /// zeros. The same bound is applied before anything is allocated, from
    /// the bytes the chunks hold and the most their compression can expand.
    /// `0` is strict: any short chunk is an error. Default 64 (a file cut
    /// down to about 1.5% of its image still decodes).
    pub max_fill_ratio: u64,
}

impl Default for Limits {
    /// 256 MiB image, 16 MiB tag value, 128 MiB chunk, zero-fill up to 64
    /// times the decoded data.
    fn default() -> Self {
        Self {
            decoding_buffer_size: 256 << 20,
            ifd_value_size: 16 << 20,
            intermediate_buffer_size: 128 << 20,
            max_fill_ratio: 64,
        }
    }
}

impl Limits {
    /// No limits beyond the file length and address space.
    #[must_use]
    pub fn unlimited() -> Self {
        Self {
            decoding_buffer_size: usize::MAX,
            ifd_value_size: usize::MAX,
            intermediate_buffer_size: usize::MAX,
            max_fill_ratio: u64::MAX,
        }
    }
}

/// Layout of a [`DecodingResult`]: channels and bits per sample.
#[derive(Clone, Copy, Debug, PartialEq, Eq, Hash)]
#[non_exhaustive]
pub enum ColorType {
    /// Grey, one sample.
    Gray(u8),
    /// Grey plus alpha (or one extra sample).
    GrayA(u8),
    /// RGB.
    RGB(u8),
    /// RGB plus alpha (or one extra sample).
    RGBA(u8),
    /// Palette indices (only from [`Decoder::read_image_raw`]).
    Palette(u8),
    /// CMYK.
    CMYK(u8),
    /// CMYK plus alpha.
    CMYKA(u8),
    /// Uncompressed YCbCr without subsampling, samples as stored.
    YCbCr(u8),
    /// Anything else (CIELab, DNG CFA/LinearRaw, many extra samples...):
    /// samples as stored.
    Multiband {
        /// Bits per sample.
        bit_depth: u8,
        /// Samples per pixel.
        num_samples: u16,
    },
}

impl ColorType {
    /// Bits per sample.
    #[must_use]
    pub fn bit_depth(self) -> u8 {
        match self {
            Self::Gray(b)
            | Self::GrayA(b)
            | Self::RGB(b)
            | Self::RGBA(b)
            | Self::Palette(b)
            | Self::CMYK(b)
            | Self::CMYKA(b)
            | Self::YCbCr(b) => b,
            Self::Multiband { bit_depth, .. } => bit_depth,
        }
    }

    /// Samples per pixel.
    #[must_use]
    pub fn num_samples(self) -> u16 {
        match self {
            Self::Gray(_) | Self::Palette(_) => 1,
            Self::GrayA(_) => 2,
            Self::RGB(_) | Self::YCbCr(_) => 3,
            Self::RGBA(_) | Self::CMYK(_) => 4,
            Self::CMYKA(_) => 5,
            Self::Multiband { num_samples, .. } => num_samples,
        }
    }
}

/// Decoded pixels, interleaved (chunky) whatever the file's planar
/// configuration, rows top to bottom, samples in native byte order.
///
/// Sub-byte depths (1, 2, 4 bits) come back as `U8` with each row packed
/// most-significant-bit first and padded to a whole byte, exactly as
/// image-tiff returns them.
///
/// 16-bit floats come back as `F16` holding the raw IEEE binary16 bit
/// patterns (no `half` dependency); convert with [`f16_to_f32`].
#[derive(Clone, Debug, PartialEq)]
#[non_exhaustive]
pub enum DecodingResult {
    /// 1-8 bit unsigned samples.
    U8(Vec<u8>),
    /// 16-bit unsigned samples.
    U16(Vec<u16>),
    /// 32-bit unsigned samples.
    U32(Vec<u32>),
    /// 64-bit unsigned samples.
    U64(Vec<u64>),
    /// 8-bit signed samples.
    I8(Vec<i8>),
    /// 16-bit signed samples.
    I16(Vec<i16>),
    /// 32-bit signed samples.
    I32(Vec<i32>),
    /// 64-bit signed samples.
    I64(Vec<i64>),
    /// 16-bit IEEE floats as raw bit patterns.
    F16(Vec<u16>),
    /// 32-bit IEEE floats.
    F32(Vec<f32>),
    /// 64-bit IEEE floats.
    F64(Vec<f64>),
}

impl DecodingResult {
    /// Number of elements.
    #[must_use]
    pub fn len(&self) -> usize {
        match self {
            Self::U8(v) => v.len(),
            Self::U16(v) | Self::F16(v) => v.len(),
            Self::U32(v) => v.len(),
            Self::U64(v) => v.len(),
            Self::I8(v) => v.len(),
            Self::I16(v) => v.len(),
            Self::I32(v) => v.len(),
            Self::I64(v) => v.len(),
            Self::F32(v) => v.len(),
            Self::F64(v) => v.len(),
        }
    }

    /// True when there are no elements.
    #[must_use]
    pub fn is_empty(&self) -> bool {
        self.len() == 0
    }

    /// The samples as native-endian bytes (a copy). To avoid the copy of
    /// 8-bit data, use [`Self::into_ne_bytes`].
    #[must_use]
    pub fn to_ne_bytes(&self) -> Vec<u8> {
        // `flat_map(..).collect()` is fine here: a sized buffer with
        // fixed-width stores measured the same (16-bit decode, within 300 Ir).
        fn f<T: Copy, const N: usize>(v: &[T], g: impl Fn(T) -> [u8; N]) -> Vec<u8> {
            v.iter().flat_map(|&x| g(x)).collect()
        }
        match self {
            Self::U8(v) => v.clone(),
            Self::U16(v) | Self::F16(v) => f(v, u16::to_ne_bytes),
            Self::U32(v) => f(v, u32::to_ne_bytes),
            Self::U64(v) => f(v, u64::to_ne_bytes),
            Self::I8(v) => f(v, i8::to_ne_bytes),
            Self::I16(v) => f(v, i16::to_ne_bytes),
            Self::I32(v) => f(v, i32::to_ne_bytes),
            Self::I64(v) => f(v, i64::to_ne_bytes),
            Self::F32(v) => f(v, f32::to_ne_bytes),
            Self::F64(v) => f(v, f64::to_ne_bytes),
        }
    }

    /// The samples as native-endian bytes, consuming the result: 8-bit
    /// samples (`U8`, and `I8` reinterpreted in place) are moved out with no
    /// copy; wider samples are converted once, as [`Self::to_ne_bytes`] does.
    /// Prefer this when handing pixels on as bytes.
    #[must_use]
    pub fn into_ne_bytes(self) -> Vec<u8> {
        match self {
            Self::U8(v) => v,
            Self::I8(v) => v.into_iter().map(|x| x as u8).collect(),
            other => other.to_ne_bytes(),
        }
    }
}

/// Convert an IEEE binary16 bit pattern to `f32` (exact; NaN payloads kept).
#[must_use]
pub fn f16_to_f32(h: u16) -> f32 {
    let sign = u32::from(h >> 15) << 31;
    let exp = u32::from((h >> 10) & 0x1F);
    let man = u32::from(h & 0x3FF);
    let bits = match (exp, man) {
        (0, 0) => sign,
        (0, m) => {
            // Subnormal: normalise.
            let shift = m.leading_zeros() - 21;
            let m = (m << shift) & 0x3FF;
            sign | ((113 - shift) << 23) | (m << 13)
        }
        (31, m) => sign | 0x7F80_0000 | (m << 13),
        (e, m) => sign | ((e + 112) << 23) | (m << 13),
    };
    f32::from_bits(bits)
}

/// Sample representation of an image.
#[derive(Clone, Copy, Debug, PartialEq, Eq)]
pub enum SampleKind {
    /// Unsigned integer (also `SampleFormat` 4, "void").
    Uint,
    /// Signed integer.
    Int,
    /// IEEE float.
    Float,
}

/// Everything the decoder parsed about the current image.
#[derive(Clone, Debug, PartialEq, Eq)]
#[non_exhaustive]
pub struct ImageInfo {
    /// Width in pixels.
    pub width: u32,
    /// Height in pixels.
    pub height: u32,
    /// Bits per sample (all samples share it).
    pub bits_per_sample: u16,
    /// Samples per pixel, extra samples included.
    pub samples_per_pixel: u16,
    /// Sample representation.
    pub sample_kind: SampleKind,
    /// `PhotometricInterpretation` value (see [`crate::tags::photometric`]).
    pub photometric: u16,
    /// `Compression` value (see [`crate::tags::compression`]).
    pub compression: u16,
    /// `Predictor` value.
    pub predictor: u16,
    /// Planar configuration 2 (separate planes).
    pub planar: bool,
    /// Tiled rather than stripped.
    pub tiled: bool,
    /// Tile size, or (width, rows per strip) for strips.
    pub chunk_dimensions: (u32, u32),
    /// `ExtraSamples` values.
    pub extra_samples: Vec<u16>,
    /// `FillOrder` (2 = least significant bit first).
    pub fill_order: u16,
}

#[derive(Clone, Debug)]
struct Image {
    info: ImageInfo,
    colormap: Option<Vec<u16>>,
    offsets: Vec<u64>,
    counts: Vec<u64>,
    jpeg_tables: Option<Vec<u8>>,
    t4_options: u32,
    ycbcr_subsampling: (u16, u16),
}

impl Image {
    fn planes(&self) -> u32 {
        if self.info.planar {
            u32::from(self.info.samples_per_pixel)
        } else {
            1
        }
    }
    fn spp_chunk(&self) -> u16 {
        if self.info.planar {
            1
        } else {
            self.info.samples_per_pixel
        }
    }
    fn across_down(&self) -> (u32, u32) {
        let (cw, ch) = self.info.chunk_dimensions;
        (self.info.width.div_ceil(cw), self.info.height.div_ceil(ch))
    }
}

/// Bytes per row of `w` pixels with `spp` samples of `bits` bits.
pub(crate) fn row_bytes(w: u32, spp: u16, bits: u16) -> TiffResult<usize> {
    let b = u64::from(w)
        .checked_mul(u64::from(spp))
        .and_then(|x| x.checked_mul(u64::from(bits)))
        .ok_or_else(|| TiffError::limits("row size overflow"))?;
    usize::try_from(b.div_ceil(8)).map_err(|_| TiffError::limits("row size overflow"))
}

/// The TIFF decoder: navigates IFDs, exposes tags and decodes images.
///
/// ```no_run
/// use rusty_tiff::{Decoder, DecodingResult};
/// let file = std::fs::File::open("scan.tif")?;
/// let mut dec = Decoder::new(std::io::BufReader::new(file))?;
/// loop {
///     let (w, h) = dec.dimensions()?;
///     let pixels = dec.read_image()?;
///     println!("{w}x{h} {:?} ({} samples)", dec.colortype()?, pixels.len());
///     if !dec.more_images() {
///         break;
///     }
///     dec.next_image()?;
/// }
/// # Ok::<_, rusty_tiff::TiffError>(())
/// ```
pub struct Decoder<R> {
    s: Stream<R>,
    bigtiff: bool,
    limits: Limits,
    first_ifd: u64,
    ifd: Ifd,
    chain: HashSet<u64>,
    image: Option<Image>,
}

impl<R: Read + Seek> Decoder<R> {
    /// Read the header and the first IFD.
    pub fn new(r: R) -> TiffResult<Self> {
        let mut s = Stream::new(r)?;
        let (bigtiff, first_ifd) = ifd::read_header(&mut s)?;
        let first = ifd::read_ifd(&mut s, bigtiff, first_ifd)?;
        let mut chain = HashSet::new();
        chain.insert(first_ifd);
        Ok(Self {
            s,
            bigtiff,
            limits: Limits::default(),
            first_ifd,
            ifd: first,
            chain,
            image: None,
        })
    }

    /// Replace the resource limits.
    #[must_use]
    pub fn with_limits(mut self, limits: Limits) -> Self {
        self.limits = limits;
        self
    }

    /// Byte order of the file.
    #[must_use]
    pub fn byte_order(&self) -> ByteOrder {
        if self.s.big_endian {
            ByteOrder::BigEndian
        } else {
            ByteOrder::LittleEndian
        }
    }

    /// Whether the file is BigTIFF.
    #[must_use]
    pub fn is_bigtiff(&self) -> bool {
        self.bigtiff
    }

    /// The current IFD's raw entries.
    #[must_use]
    pub fn current_ifd(&self) -> &Ifd {
        &self.ifd
    }

    // ---- navigation -------------------------------------------------------

    /// Whether the current IFD links to another one.
    #[must_use]
    pub fn more_images(&self) -> bool {
        self.ifd.next != 0
    }

    /// Advance to the next IFD in the current chain. A chain that revisits an
    /// IFD (self-loop, A->B->A...) is an error, never an endless walk.
    pub fn next_image(&mut self) -> TiffResult<()> {
        let next = self.ifd.next;
        if next == 0 {
            return Err(TiffError::usage("no more images"));
        }
        if !self.chain.insert(next) {
            return Err(TiffError::format(format!("IFD loop at offset {next}")));
        }
        self.ifd = ifd::read_ifd(&mut self.s, self.bigtiff, next)?;
        self.image = None;
        Ok(())
    }

    /// Go to image `index` of the main IFD chain (0 = first).
    pub fn seek_to_image(&mut self, index: usize) -> TiffResult<()> {
        self.chain.clear();
        self.chain.insert(self.first_ifd);
        self.ifd = ifd::read_ifd(&mut self.s, self.bigtiff, self.first_ifd)?;
        self.image = None;
        for _ in 0..index {
            self.next_image()?;
        }
        Ok(())
    }

    /// Make the IFD at `offset` current (e.g. a SubIFD from [`Self::sub_ifds`]),
    /// starting a new chain for [`Self::next_image`].
    pub fn seek_to_ifd(&mut self, offset: u64) -> TiffResult<()> {
        self.ifd = ifd::read_ifd(&mut self.s, self.bigtiff, offset)?;
        self.chain.clear();
        self.chain.insert(offset);
        self.image = None;
        Ok(())
    }

    /// Read the IFD at `offset` without making it current.
    pub fn read_ifd(&mut self, offset: u64) -> TiffResult<Ifd> {
        ifd::read_ifd(&mut self.s, self.bigtiff, offset)
    }

    // ---- tags --------------------------------------------------------------

    /// A tag of the current IFD, or `None` if absent.
    ///
    /// An entry whose field-type code TIFF does not define reads as absent:
    /// TIFF 6.0 section 2 has readers skip fields of an unexpected type, so a
    /// damaged type code on an optional tag no longer fails the file (a
    /// required tag hit this way still fails, as missing).
    pub fn find_tag(&mut self, tag: Tag) -> TiffResult<Option<Value>> {
        match known(self.ifd.entries.get(&tag.to_u16())) {
            None => Ok(None),
            Some(e) => {
                let e = e.clone();
                ifd::read_value(&mut self.s, self.bigtiff, &e, self.limits.ifd_value_size).map(Some)
            }
        }
    }

    /// A tag of the current IFD; absent is [`TiffError::TagNotFound`].
    pub fn get_tag(&mut self, tag: Tag) -> TiffResult<Value> {
        self.find_tag(tag)?.ok_or(TiffError::TagNotFound(tag))
    }

    /// A tag of another IFD (from [`Self::read_ifd`], [`Self::exif_ifd`]...).
    /// Undefined field types read as absent, as in [`Self::find_tag`].
    pub fn find_tag_in(&mut self, ifd: &Ifd, tag: Tag) -> TiffResult<Option<Value>> {
        match known(ifd.entries.get(&tag.to_u16())) {
            None => Ok(None),
            Some(e) => {
                ifd::read_value(&mut self.s, self.bigtiff, e, self.limits.ifd_value_size).map(Some)
            }
        }
    }

    /// The raw value bytes of a tag of the current IFD, in file byte order.
    /// Undefined field types read as absent, as in [`Self::find_tag`].
    pub fn find_tag_bytes(&mut self, tag: Tag) -> TiffResult<Option<Vec<u8>>> {
        match known(self.ifd.entries.get(&tag.to_u16())) {
            None => Ok(None),
            Some(e) => {
                let e = e.clone();
                ifd::read_raw(&mut self.s, self.bigtiff, &e, self.limits.ifd_value_size).map(Some)
            }
        }
    }

    /// A single unsigned tag value as `u32`.
    pub fn get_tag_u32(&mut self, tag: Tag) -> TiffResult<u32> {
        self.get_tag(tag)?
            .to_u32()
            .ok_or_else(|| TiffError::format(format!("{tag:?} is not a single u32")))
    }

    /// A single unsigned tag value as `u64`.
    pub fn get_tag_u64(&mut self, tag: Tag) -> TiffResult<u64> {
        self.get_tag(tag)?
            .to_u64()
            .ok_or_else(|| TiffError::format(format!("{tag:?} is not a single unsigned value")))
    }

    /// An unsigned tag as `Vec<u16>`.
    pub fn get_tag_u16_vec(&mut self, tag: Tag) -> TiffResult<Vec<u16>> {
        self.get_tag(tag)?
            .to_u16_vec()
            .ok_or_else(|| TiffError::format(format!("{tag:?} is not a u16 array")))
    }

    /// An unsigned tag as `Vec<u32>`.
    pub fn get_tag_u32_vec(&mut self, tag: Tag) -> TiffResult<Vec<u32>> {
        self.get_tag(tag)?
            .to_u32_vec()
            .ok_or_else(|| TiffError::format(format!("{tag:?} is not a u32 array")))
    }

    /// An unsigned tag as `Vec<u64>`.
    pub fn get_tag_u64_vec(&mut self, tag: Tag) -> TiffResult<Vec<u64>> {
        self.get_tag(tag)?
            .to_u64_vec()
            .ok_or_else(|| TiffError::format(format!("{tag:?} is not an unsigned array")))
    }

    /// A numeric tag as `Vec<f64>` (rationals divided out).
    pub fn get_tag_f64_vec(&mut self, tag: Tag) -> TiffResult<Vec<f64>> {
        self.get_tag(tag)?
            .to_f64_vec()
            .ok_or_else(|| TiffError::format(format!("{tag:?} is not numeric")))
    }

    /// An `ASCII` tag.
    pub fn get_tag_ascii_string(&mut self, tag: Tag) -> TiffResult<String> {
        match self.get_tag(tag)? {
            Value::Ascii(s) => Ok(s),
            _ => Err(TiffError::format(format!("{tag:?} is not ASCII"))),
        }
    }

    /// All tags of the current IFD with their values. Entries whose value
    /// cannot be read (unknown type, offset past EOF, over the limit) are
    /// skipped.
    pub fn tags(&mut self) -> Vec<(Tag, Value)> {
        let entries: Vec<_> = self
            .ifd
            .entries
            .iter()
            .map(|(&k, e)| (k, e.clone()))
            .collect();
        entries
            .into_iter()
            .filter_map(|(k, e)| {
                ifd::read_value(&mut self.s, self.bigtiff, &e, self.limits.ifd_value_size)
                    .ok()
                    .map(|v| (Tag::from_u16(k), v))
            })
            .collect()
    }

    /// The embedded ICC profile (tag 34675).
    pub fn icc_profile(&mut self) -> TiffResult<Option<Vec<u8>>> {
        self.find_tag_bytes(Tag::IccProfile)
    }

    /// The XMP packet (tag 700).
    pub fn xmp(&mut self) -> TiffResult<Option<Vec<u8>>> {
        self.find_tag_bytes(Tag::Xmp)
    }

    fn pointer_ifd(&mut self, tag: Tag) -> TiffResult<Option<Ifd>> {
        let Some(v) = self.find_tag(tag)? else {
            return Ok(None);
        };
        let off = v
            .to_u64_vec()
            .and_then(|v| v.first().copied())
            .ok_or_else(|| TiffError::format(format!("{tag:?} is not an IFD pointer")))?;
        self.read_ifd(off).map(Some)
    }

    /// The EXIF IFD (tag 34665). Read its values with [`Self::find_tag_in`].
    pub fn exif_ifd(&mut self) -> TiffResult<Option<Ifd>> {
        self.pointer_ifd(Tag::ExifIfd)
    }

    /// The GPS IFD (tag 34853).
    pub fn gps_ifd(&mut self) -> TiffResult<Option<Ifd>> {
        self.pointer_ifd(Tag::GpsIfd)
    }

    /// Offsets of the current IFD's SubIFDs (tag 330; DNG keeps its raw
    /// images there). Decode one with [`Self::seek_to_ifd`].
    pub fn sub_ifds(&mut self) -> TiffResult<Vec<u64>> {
        match self.find_tag(Tag::SubIfds)? {
            None => Ok(Vec::new()),
            Some(v) => v
                .to_u64_vec()
                .ok_or_else(|| TiffError::format("SubIFDs is not an offset array")),
        }
    }

    // ---- image -------------------------------------------------------------

    fn opt_u64(&mut self, tag: Tag) -> TiffResult<Option<u64>> {
        match self.find_tag(tag)? {
            None => Ok(None),
            Some(v) => v
                .to_u64_vec()
                .and_then(|v| v.first().copied())
                .map(Some)
                .ok_or_else(|| TiffError::format(format!("{tag:?} has a non-integer type"))),
        }
    }

    fn opt_u16_vec(&mut self, tag: Tag) -> TiffResult<Option<Vec<u16>>> {
        match self.find_tag(tag)? {
            None => Ok(None),
            Some(v) => v
                .to_u16_vec()
                .map(Some)
                .ok_or_else(|| TiffError::format(format!("{tag:?} has bad values"))),
        }
    }

    fn opt_u64_vec(&mut self, tag: Tag) -> TiffResult<Option<Vec<u64>>> {
        match self.find_tag(tag)? {
            None => Ok(None),
            Some(v) => v
                .to_u64_vec()
                .map(Some)
                .ok_or_else(|| TiffError::format(format!("{tag:?} has bad values"))),
        }
    }

    fn small_u16(&mut self, tag: Tag, default: u16) -> TiffResult<u16> {
        match self.opt_u64(tag)? {
            None => Ok(default),
            Some(v) => u16::try_from(v).map_err(|_| TiffError::format(format!("{tag:?} = {v}"))),
        }
    }

    fn parse_image(&mut self) -> TiffResult<Image> {
        let width = self
            .opt_u64(Tag::ImageWidth)?
            .ok_or(TiffError::TagNotFound(Tag::ImageWidth))?;
        let height = self
            .opt_u64(Tag::ImageLength)?
            .ok_or(TiffError::TagNotFound(Tag::ImageLength))?;
        let (Ok(width), Ok(height)) = (u32::try_from(width), u32::try_from(height)) else {
            return Err(TiffError::format("dimensions exceed u32"));
        };
        if width == 0 || height == 0 {
            return Err(TiffError::format("zero image dimension"));
        }
        let samples = self.small_u16(Tag::SamplesPerPixel, 1)?;
        if samples == 0 {
            return Err(TiffError::format("SamplesPerPixel = 0"));
        }
        let bits_v = self
            .opt_u16_vec(Tag::BitsPerSample)?
            .unwrap_or_else(|| vec![1]);
        let bits = *bits_v
            .first()
            .ok_or_else(|| TiffError::format("empty BitsPerSample"))?;
        if bits_v.iter().take(usize::from(samples)).any(|&b| b != bits) {
            return Err(TiffError::unsupported("differing BitsPerSample per sample"));
        }
        let fmt_v = self
            .opt_u16_vec(Tag::SampleFormat)?
            .unwrap_or_else(|| vec![sample_format::UINT]);
        let fmt = *fmt_v
            .first()
            .ok_or_else(|| TiffError::format("empty SampleFormat"))?;
        if fmt_v.iter().take(usize::from(samples)).any(|&f| f != fmt) {
            return Err(TiffError::unsupported("differing SampleFormat per sample"));
        }
        let kind = match fmt {
            sample_format::UINT | sample_format::VOID => SampleKind::Uint,
            sample_format::INT => SampleKind::Int,
            sample_format::IEEEFP => SampleKind::Float,
            f => return Err(TiffError::unsupported(format!("SampleFormat {f}"))),
        };
        let depth_ok = match kind {
            SampleKind::Uint => matches!(bits, 1 | 2 | 4 | 8 | 16 | 32 | 64),
            SampleKind::Int => matches!(bits, 8 | 16 | 32 | 64),
            SampleKind::Float => matches!(bits, 16 | 32 | 64),
        };
        if !depth_ok {
            return Err(TiffError::unsupported(format!(
                "{bits}-bit {kind:?} samples"
            )));
        }
        let comp = self.small_u16(Tag::Compression, compression::NONE)?;
        let is_ccitt = matches!(
            comp,
            compression::CCITT_RLE | compression::CCITT_FAX3 | compression::CCITT_FAX4
        );
        let default_photo = if is_ccitt {
            photometric::WHITE_IS_ZERO
        } else if samples >= 3 {
            photometric::RGB
        } else {
            photometric::BLACK_IS_ZERO
        };
        let photo = self.small_u16(Tag::PhotometricInterpretation, default_photo)?;
        let planar_cfg = self.small_u16(Tag::PlanarConfiguration, 1)?;
        let planar = match planar_cfg {
            1 => false,
            2 => samples > 1,
            p => return Err(TiffError::format(format!("PlanarConfiguration {p}"))),
        };
        let predictor = self.small_u16(Tag::Predictor, pred::NONE)?;
        match predictor {
            pred::NONE => {}
            pred::HORIZONTAL => {
                if bits < 8 || kind == SampleKind::Float {
                    return Err(TiffError::unsupported(format!(
                        "horizontal predictor on {bits}-bit {kind:?}"
                    )));
                }
            }
            pred::FLOATING_POINT => {
                if kind != SampleKind::Float {
                    return Err(TiffError::unsupported(
                        "floating-point predictor on integers",
                    ));
                }
            }
            p => return Err(TiffError::unsupported(format!("Predictor {p}"))),
        }
        let fill_order = self.small_u16(Tag::FillOrder, 1)?;
        let extra_samples = self.opt_u16_vec(Tag::ExtraSamples)?.unwrap_or_default();
        let colormap = if photo == photometric::PALETTE {
            if samples != 1 || bits > 16 || kind != SampleKind::Uint {
                return Err(TiffError::unsupported("palette image layout"));
            }
            let map = self
                .opt_u16_vec(Tag::ColorMap)?
                .ok_or(TiffError::TagNotFound(Tag::ColorMap))?;
            if map.len() < 3usize << bits {
                return Err(TiffError::format("ColorMap too short"));
            }
            Some(map)
        } else {
            None
        };
        let tiled = self.ifd.contains(Tag::TileWidth) || self.ifd.contains(Tag::TileOffsets);
        let (cw, ch, off_tag, cnt_tag) = if tiled {
            let tw = self.opt_u64(Tag::TileWidth)?.unwrap_or(0);
            let th = self.opt_u64(Tag::TileLength)?.unwrap_or(0);
            let (Ok(tw), Ok(th)) = (u32::try_from(tw), u32::try_from(th)) else {
                return Err(TiffError::format("tile dimensions exceed u32"));
            };
            if tw == 0 || th == 0 {
                return Err(TiffError::format("zero or missing tile dimensions"));
            }
            (tw, th, Tag::TileOffsets, Tag::TileByteCounts)
        } else {
            let rps = self.opt_u64(Tag::RowsPerStrip)?.unwrap_or(u64::MAX);
            let rps = if rps == 0 {
                height
            } else {
                rps.min(u64::from(height)) as u32
            };
            (width, rps, Tag::StripOffsets, Tag::StripByteCounts)
        };
        let planes = if planar { u64::from(samples) } else { 1 };
        let expected = u64::from(width.div_ceil(cw)) * u64::from(height.div_ceil(ch)) * planes;
        let offsets = self
            .opt_u64_vec(off_tag)?
            .ok_or(TiffError::TagNotFound(off_tag))?;
        let counts = match self.opt_u64_vec(cnt_tag)? {
            Some(c) => c,
            None if comp == compression::NONE && expected == 1 => {
                let spp = if planar { 1 } else { samples };
                vec![row_bytes(cw, spp, bits)? as u64 * u64::from(ch)]
            }
            None => return Err(TiffError::TagNotFound(cnt_tag)),
        };
        if (offsets.len() as u64) < expected || (counts.len() as u64) < expected {
            return Err(TiffError::format(format!(
                "{} offsets / {} byte counts for {expected} chunks",
                offsets.len(),
                counts.len()
            )));
        }
        let jpeg_tables = if comp == compression::JPEG {
            self.find_tag_bytes(Tag::JpegTables)?
        } else {
            None
        };
        let t4_options = self.opt_u64(Tag::T4Options)?.unwrap_or(0) as u32;
        let ycbcr_subsampling = match self.opt_u16_vec(Tag::YCbCrSubSampling)? {
            Some(v) if v.len() >= 2 => (v[0], v[1]),
            _ => (2, 2),
        };
        Ok(Image {
            info: ImageInfo {
                width,
                height,
                bits_per_sample: bits,
                samples_per_pixel: samples,
                sample_kind: kind,
                photometric: photo,
                compression: comp,
                predictor,
                planar,
                tiled,
                chunk_dimensions: (cw, ch),
                extra_samples,
                fill_order,
            },
            colormap,
            offsets,
            counts,
            jpeg_tables,
            t4_options,
            ycbcr_subsampling,
        })
    }

    fn img(&self) -> TiffResult<&Image> {
        self.image
            .as_ref()
            .ok_or_else(|| TiffError::format("image not parsed"))
    }

    fn ensure_image(&mut self) -> TiffResult<()> {
        if self.image.is_none() {
            self.image = Some(self.parse_image()?);
        }
        Ok(())
    }

    /// Everything parsed about the current image.
    pub fn info(&mut self) -> TiffResult<ImageInfo> {
        self.ensure_image()?;
        Ok(self.img()?.info.clone())
    }

    /// The palette (`ColorMap`, 3 x 2^bits entries: all reds, greens, blues).
    pub fn colormap(&mut self) -> TiffResult<Option<Vec<u16>>> {
        self.ensure_image()?;
        Ok(self.image.as_ref().and_then(|i| i.colormap.clone()))
    }

    /// `(width, height)` of the current image.
    pub fn dimensions(&mut self) -> TiffResult<(u32, u32)> {
        let i = self.info()?;
        Ok((i.width, i.height))
    }

    /// Layout of what [`Self::read_image`] returns. Palette images report
    /// `RGB(16)` (they are expanded through the colormap); JPEG-compressed
    /// YCbCr reports `RGB(8)` (the JPEG decoder converts).
    pub fn colortype(&mut self) -> TiffResult<ColorType> {
        let i = self.info()?;
        Ok(colortype_of(&i, true))
    }

    /// Layout of what [`Self::read_image_raw`] returns (palette indices are
    /// `Palette(bits)`).
    pub fn raw_colortype(&mut self) -> TiffResult<ColorType> {
        let i = self.info()?;
        Ok(colortype_of(&i, false))
    }

    /// Decode the current image, interpreted: palette expanded to `U16` RGB
    /// from the colormap; `WhiteIsZero` grey inverted to black-is-zero
    /// (integers bitwise, 32/64-bit floats as `1.0 - v`, as image-tiff does).
    /// Everything else is returned as stored.
    pub fn read_image(&mut self) -> TiffResult<DecodingResult> {
        self.ensure_image()?;
        let Some(img) = self.image.as_ref() else {
            return Err(TiffError::format("image not parsed"));
        };
        let raw = decode_image(&mut self.s, img, &self.limits, true)?;
        let img = self.img()?;
        let info = &img.info;
        match info.photometric {
            photometric::WHITE_IS_ZERO => invert_white_is_zero(raw, info),
            photometric::PALETTE => {
                let map = img.colormap.as_deref().unwrap_or(&[]);
                expand_palette(&raw, info, map)
            }
            photometric::YCBCR
                if info.compression != compression::JPEG && img.ycbcr_subsampling != (1, 1) =>
            {
                Err(TiffError::unsupported("subsampled uncompressed YCbCr"))
            }
            _ => Ok(raw),
        }
    }

    /// Decode the current image with no photometric interpretation: samples
    /// exactly as stored after decompression and prediction (palette indices,
    /// min-is-white values, DNG CFA data, JPEG-compressed YCbCr left as
    /// YCbCr, which is what image-tiff returns).
    pub fn read_image_raw(&mut self) -> TiffResult<DecodingResult> {
        self.ensure_image()?;
        let Some(img) = self.image.as_ref() else {
            return Err(TiffError::format("image not parsed"));
        };
        decode_image(&mut self.s, img, &self.limits, false)
    }
}

fn colortype_of(i: &ImageInfo, interpreted: bool) -> ColorType {
    let b = i.bits_per_sample as u8;
    let n = i.samples_per_pixel;
    let multi = ColorType::Multiband {
        bit_depth: b,
        num_samples: n,
    };
    match i.photometric {
        photometric::WHITE_IS_ZERO | photometric::BLACK_IS_ZERO => match n {
            1 => ColorType::Gray(b),
            2 => ColorType::GrayA(b),
            _ => multi,
        },
        photometric::RGB => match n {
            3 => ColorType::RGB(b),
            4 => ColorType::RGBA(b),
            _ => multi,
        },
        photometric::PALETTE => {
            if interpreted {
                ColorType::RGB(16)
            } else {
                ColorType::Palette(b)
            }
        }
        photometric::SEPARATED => match n {
            4 => ColorType::CMYK(b),
            5 => ColorType::CMYKA(b),
            _ => multi,
        },
        photometric::YCBCR if n == 3 => {
            if i.compression == compression::JPEG && interpreted {
                ColorType::RGB(b)
            } else {
                ColorType::YCbCr(b)
            }
        }
        _ => multi,
    }
}

// ---------------------------------------------------------------------------
// Image assembly.

/// Typed output buffer.
enum Out {
    Packed(Vec<u8>),
    U8(Vec<u8>),
    U16(Vec<u16>),
    U32(Vec<u32>),
    U64(Vec<u64>),
}

trait Sample: Copy {
    const N: usize;
    fn from_ne(b: &[u8]) -> Self;
}
macro_rules! sample {
    ($t:ty, $n:expr) => {
        impl Sample for $t {
            const N: usize = $n;
            #[inline]
            fn from_ne(b: &[u8]) -> Self {
                <$t>::from_ne_bytes(b[..$n].try_into().unwrap_or([0; $n]))
            }
        }
    };
}
sample!(u8, 1);
sample!(u16, 2);
sample!(u32, 4);
sample!(u64, 8);

struct Geom {
    width: usize,
    spp: usize,
    planar: bool,
    plane: usize,
    x0: usize,
    y0: usize,
    cols: usize,
    rows: usize,
    chunk_row_bytes: usize,
    bits: usize,
}

fn copy_typed<T: Sample>(out: &mut [T], chunk: &[u8], g: &Geom) {
    for r in 0..g.rows {
        let src = &chunk[r * g.chunk_row_bytes..];
        let y = g.y0 + r;
        if g.planar {
            // One sample of every pixel of the row. The pixel is a
            // fixed-size chunk for 2 to 4 samples, so the write is one store
            // at a constant stride; indexing `out` per sample (a multiply
            // and a bounds check each) was 10 Ir a sample.
            let base = (y * g.width + g.x0) * g.spp;
            let row = &mut out[base..base + g.cols * g.spp];
            match g.spp {
                2 => scatter::<T, 2>(row, src, g.plane),
                3 => scatter::<T, 3>(row, src, g.plane),
                4 => scatter::<T, 4>(row, src, g.plane),
                _ => {
                    for (px, s) in row.chunks_exact_mut(g.spp).zip(src.chunks_exact(T::N)) {
                        px[g.plane] = T::from_ne(s);
                    }
                }
            }
        } else {
            let base = (y * g.width + g.x0) * g.spp;
            let n = g.cols * g.spp;
            for (d, s) in out[base..base + n].iter_mut().zip(src.chunks_exact(T::N)) {
                *d = T::from_ne(s);
            }
        }
    }
}

/// Write the samples in `src` to sample `plane` of each `SPP`-sample pixel
/// of `row`.
fn scatter<T: Sample, const SPP: usize>(row: &mut [T], src: &[u8], plane: usize) {
    assert!(plane < SPP);
    for (px, s) in row.chunks_exact_mut(SPP).zip(src.chunks_exact(T::N)) {
        let px: &mut [T; SPP] = px.try_into().unwrap();
        px[plane] = T::from_ne(s);
    }
}

#[inline]
pub(crate) fn get_bits(src: &[u8], bit: usize, n: usize) -> u8 {
    let mut v = 0u8;
    for k in 0..n {
        let p = bit + k;
        v = (v << 1) | ((src[p >> 3] >> (7 - (p & 7))) & 1);
    }
    v
}

#[inline]
pub(crate) fn set_bits(dst: &mut [u8], bit: usize, n: usize, v: u8) {
    for k in 0..n {
        let p = bit + k;
        let b = (v >> (n - 1 - k)) & 1;
        let m = 0x80u8 >> (p & 7);
        if b == 1 {
            dst[p >> 3] |= m;
        } else {
            dst[p >> 3] &= !m;
        }
    }
}

fn copy_packed(out: &mut [u8], chunk: &[u8], g: &Geom, out_row_bytes: usize) {
    for r in 0..g.rows {
        let src = &chunk[r * g.chunk_row_bytes..][..g.chunk_row_bytes];
        let y = g.y0 + r;
        let dst = &mut out[y * out_row_bytes..][..out_row_bytes];
        if !g.planar && g.x0 == 0 && g.chunk_row_bytes == out_row_bytes {
            // Strip rows: byte copy, padding bits as stored.
            dst.copy_from_slice(src);
            continue;
        }
        let bit0 = g.x0 * g.spp * g.bits;
        if !g.planar && bit0.is_multiple_of(8) {
            // A tile that starts on a byte of the output row (every tile of
            // a TIFF with the required multiple-of-16 tile width): whole
            // bytes are copied, and a last partial byte is merged under a
            // mask so the bits right of the tile stay as they were. Moving
            // one bit at a time was 386 Ir per output byte.
            let nbits = g.cols * g.spp * g.bits;
            let (full, rem) = (nbits / 8, nbits % 8);
            let d = &mut dst[bit0 / 8..];
            d[..full].copy_from_slice(&src[..full]);
            if rem != 0 {
                let m = !(0xFFu8 >> rem);
                d[full] = (d[full] & !m) | (src[full] & m);
            }
            continue;
        }
        if g.planar {
            for i in 0..g.cols {
                let v = get_bits(src, i * g.bits, g.bits);
                set_bits(dst, ((g.x0 + i) * g.spp + g.plane) * g.bits, g.bits, v);
            }
        } else {
            let n = g.cols * g.spp;
            for i in 0..n {
                let v = get_bits(src, i * g.bits, g.bits);
                set_bits(dst, (g.x0 * g.spp + i) * g.bits, g.bits, v);
            }
        }
    }
}

/// Zero-fill always allowed, whatever the ratio (unless it is 0).
const FILL_FLOOR: u64 = 64 << 10;

/// The state of one image decode. The limits, plus the zero-fill
/// accounting: how many bytes were really decoded, how many were zero-filled
/// because a chunk ended early, and the most the chunks not yet decoded can
/// still produce. And the inflater the Deflate chunks share (reset for each).
struct Budget<'a> {
    limits: &'a Limits,
    inflate: Option<flate2::Decompress>,
    /// Most bytes one stored byte can decode to with this compression.
    per_byte: u64,
    potential: u64,
    real: u64,
    filled: u64,
}

impl Budget<'_> {
    /// Zero-fill allowed beside `real` decoded bytes.
    fn allowed(&self, real: u64) -> u64 {
        match self.limits.max_fill_ratio {
            0 => 0,
            r => real.saturating_mul(r).saturating_add(FILL_FLOOR),
        }
    }

    /// One chunk is done: `stored` file bytes gave `real` of its `expected`
    /// bytes. Errors as soon as the image can no longer end within budget,
    /// even if every remaining chunk decodes to the most it could.
    fn chunk(&mut self, stored: u64, real: usize, expected: usize) -> TiffResult<()> {
        let real = real.min(expected);
        self.potential = self
            .potential
            .saturating_sub(stored.saturating_mul(self.per_byte));
        self.real += real as u64;
        self.filled += (expected - real) as u64;
        if self.filled > self.allowed(self.real.saturating_add(self.potential)) {
            return Err(TiffError::format(format!(
                "image data is mostly missing: {} bytes decoded, {} would be zero-filled",
                self.real, self.filled
            )));
        }
        Ok(())
    }
}

/// The most bytes one stored byte can decode to: 1 uncompressed; 64 for
/// PackBits (two bytes repeat 128); about 1362 for TIFF LZW (all-equal data
/// between two clear codes); 1032 for Deflate; one row per bit for CCITT (a
/// row can be a single code); a 2-bit 8x8 block for baseline JPEG, with
/// margin.
fn max_expansion(comp: u16, chunk_row_bytes: usize) -> u64 {
    match comp {
        compression::NONE => 1,
        compression::PACKBITS => 64,
        compression::LZW => 1400,
        compression::DEFLATE | compression::OLD_DEFLATE => 1040,
        compression::CCITT_RLE | compression::CCITT_FAX3 | compression::CCITT_FAX4 => {
            (chunk_row_bytes as u64).saturating_mul(8)
        }
        compression::JPEG => 1024,
        // Anything else is refused when its first chunk is decoded.
        _ => u64::MAX,
    }
}

fn decode_image<R: Read + Seek>(
    s: &mut Stream<R>,
    img: &Image,
    limits: &Limits,
    interpret: bool,
) -> TiffResult<DecodingResult> {
    let info = &img.info;
    let bits = info.bits_per_sample;
    let spp = info.samples_per_pixel;
    let is_jpeg = info.compression == compression::JPEG;
    if is_jpeg && bits != 8 {
        return Err(TiffError::unsupported(
            "JPEG-in-TIFF with BitsPerSample != 8",
        ));
    }
    let out_row_bytes = row_bytes(info.width, spp, bits)?;
    let total_bytes = out_row_bytes
        .checked_mul(info.height as usize)
        .ok_or_else(|| TiffError::limits("image size overflow"))?;
    if total_bytes > limits.decoding_buffer_size {
        return Err(TiffError::limits(format!(
            "image needs {total_bytes} bytes, limit {}",
            limits.decoding_buffer_size
        )));
    }
    // Before allocating: the image must be coverable by what its chunks
    // hold, allowing the zero-fill budget for a file that was cut short.
    let stored = img
        .offsets
        .iter()
        .zip(&img.counts)
        .fold(0u64, |a, (&off, &n)| {
            a.saturating_add(n.min(s.len.saturating_sub(off)))
        });
    let (cw0, _) = info.chunk_dimensions;
    let per_byte = max_expansion(info.compression, row_bytes(cw0, img.spp_chunk(), bits)?);
    let mut budget = Budget {
        limits,
        inflate: None,
        per_byte,
        potential: stored.saturating_mul(per_byte),
        real: 0,
        filled: 0,
    };
    let limits = &mut budget;
    if total_bytes as u64
        > limits
            .potential
            .saturating_add(limits.allowed(limits.potential))
    {
        return Err(TiffError::format(format!(
            "image declares {total_bytes} bytes but its chunks hold {stored}: too little to decode"
        )));
    }
    let samples = total_bytes / (usize::from(bits.max(8)) / 8);
    if !info.tiled && img.planes() == 1 {
        return decode_strips(s, img, limits, interpret, total_bytes, samples);
    }
    if info.tiled && img.planes() == 1 && (bits == 8 || bits == 16) {
        return decode_tile_bands(s, img, limits, interpret, total_bytes);
    }
    let mut out = match bits {
        1 | 2 | 4 => Out::Packed(zeroed(total_bytes)?),
        8 => Out::U8(zeroed(samples)?),
        16 => Out::U16(zeroed(samples)?),
        32 => Out::U32(zeroed(samples)?),
        _ => Out::U64(zeroed(samples)?),
    };
    let (across, down) = img.across_down();
    let (cw, ch) = info.chunk_dimensions;
    let spp_chunk = img.spp_chunk();
    // One chunk buffer, reused for every tile and plane.
    let mut chunk = Vec::new();
    for plane in 0..img.planes() {
        for cy in 0..down {
            for cx in 0..across {
                let idx = (plane * across * down + cy * across + cx) as usize;
                let y0 = cy * ch;
                let x0 = cx * cw;
                let rows_in_chunk = if info.tiled {
                    ch
                } else {
                    ch.min(info.height - y0)
                };
                let chunk_row_bytes = row_bytes(cw, spp_chunk, bits)?;
                chunk.clear();
                decode_chunk_append(
                    s,
                    img,
                    idx,
                    (cw, rows_in_chunk, chunk_row_bytes),
                    limits,
                    interpret,
                    &mut chunk,
                )?;
                let g = Geom {
                    width: info.width as usize,
                    spp: usize::from(spp),
                    planar: info.planar,
                    plane: plane as usize,
                    x0: x0 as usize,
                    y0: y0 as usize,
                    cols: cw.min(info.width - x0) as usize,
                    rows: rows_in_chunk.min(info.height - y0) as usize,
                    chunk_row_bytes,
                    bits: usize::from(bits),
                };
                match &mut out {
                    Out::Packed(o) => copy_packed(o, &chunk, &g, out_row_bytes),
                    Out::U8(o) => copy_typed(o, &chunk, &g),
                    Out::U16(o) => copy_typed(o, &chunk, &g),
                    Out::U32(o) => copy_typed(o, &chunk, &g),
                    Out::U64(o) => copy_typed(o, &chunk, &g),
                }
            }
        }
    }
    Ok(typed_result(out, info.sample_kind, bits))
}

/// Strip images with one plane: strips are full-width rows in order, so
/// each one is decoded straight onto the end of the output (bytes, for 1-8
/// bit samples), or into one reused strip buffer appended as typed samples.
/// Nothing is zero-filled and nothing is copied twice.
fn decode_strips<R: Read + Seek>(
    s: &mut Stream<R>,
    img: &Image,
    limits: &mut Budget<'_>,
    interpret: bool,
    total_bytes: usize,
    samples: usize,
) -> TiffResult<DecodingResult> {
    let info = &img.info;
    let bits = info.bits_per_sample;
    let (cw, ch) = info.chunk_dimensions;
    let row = row_bytes(cw, img.spp_chunk(), bits)?;
    let (_, down) = img.across_down();
    let rows_of = |cy: u32| ch.min(info.height - cy * ch);
    let out = if bits <= 8 {
        let mut out: Vec<u8> = Vec::new();
        out.try_reserve_exact(total_bytes)
            .map_err(|_| TiffError::limits(format!("cannot allocate {total_bytes} bytes")))?;
        for cy in 0..down {
            decode_chunk_append(
                s,
                img,
                cy as usize,
                (cw, rows_of(cy), row),
                limits,
                interpret,
                &mut out,
            )?;
        }
        if bits == 8 {
            Out::U8(out)
        } else {
            Out::Packed(out)
        }
    } else {
        let mut chunk = Vec::new();
        macro_rules! typed {
            ($t:ty, $v:ident) => {{
                let mut out: Vec<$t> = Vec::new();
                out.try_reserve_exact(samples)
                    .map_err(|_| TiffError::limits(format!("cannot allocate {samples} samples")))?;
                for cy in 0..down {
                    chunk.clear();
                    decode_chunk_append(
                        s,
                        img,
                        cy as usize,
                        (cw, rows_of(cy), row),
                        limits,
                        interpret,
                        &mut chunk,
                    )?;
                    // `chunks_exact` fixes the length, so this converts with
                    // no fallible path and vectorises. (A resize + memcpy
                    // through a bytemuck byte view measured worse: +8.8M Ir
                    // and 0.19 -> 0.21 s wall on 16-bit uncompressed.)
                    out.extend(
                        chunk
                            .chunks_exact(<$t>::N)
                            .map(|c| <$t>::from_ne_bytes(c.try_into().unwrap())),
                    );
                }
                Out::$v(out)
            }};
        }
        match bits {
            16 => typed!(u16, U16),
            32 => typed!(u32, U32),
            _ => typed!(u64, U64),
        }
    };
    Ok(typed_result(out, info.sample_kind, bits))
}

/// Tiled images with one plane of 8- or 16-bit samples: one row of tiles (a band)
/// is decoded into one buffer, then the band's image rows are appended to
/// the output, each tile's part cut to the image width.
///
/// The output is never zero-filled. The general path allocates the image
/// zeroed and overwrites it tile by tile, which sends every byte to memory
/// twice; on the clock that made an uncompressed tiled decode from memory
/// 0.86-0.89x the speed of the `tiff` crate, where a stripped one is 1.5x.
/// With the band it is 0.96-1.04x: 2-11% faster than before on
/// uncompressed and PackBits tiles (median of 400-600 interleaved rounds;
/// LZW and Deflate are bound by the codec and do not move).
///
/// At 16 bits the rows are appended as `u16` slices viewed straight from
/// the band (one copy, no per-sample conversion). A first 16-bit band that
/// converted sample by sample measured no gain on the clock (median +1.6%,
/// 261 of 600 pairs) and more instructions than the general path; as one
/// copy it removes the zero-fill and the conversion pass (-0.6% of the
/// whole uncompressed 16-bit run's instructions, about -2.8% of the decode,
/// and -0.1..-0.3% on compressed 16-bit tiles).
///
/// Memory: beside the image, one band. A tile is cut to the rows the image
/// uses as soon as it is decoded, so the band holds at most those rows of
/// every tile plus one whole tile, however much taller than the image the
/// tiles are.
fn decode_tile_bands<R: Read + Seek>(
    s: &mut Stream<R>,
    img: &Image,
    limits: &mut Budget<'_>,
    interpret: bool,
    total_bytes: usize,
) -> TiffResult<DecodingResult> {
    let info = &img.info;
    let bits = info.bits_per_sample;
    if bits == 16
        && row_bytes(info.chunk_dimensions.0, img.spp_chunk(), 16)? >= DIRECT_TILE_ROW
        && info.compression == compression::NONE
        && !matches!(info.predictor, pred::HORIZONTAL | pred::FLOATING_POINT)
        && info.fill_order != 2
    {
        let out = tiles_direct16(s, img, limits, total_bytes)?;
        return Ok(typed_result(Out::U16(out), info.sample_kind, 16));
    }
    if bits == 16 {
        let mut out: Vec<u16> = Vec::new();
        out.try_reserve_exact(total_bytes / 2)
            .map_err(|_| TiffError::limits(format!("cannot allocate {total_bytes} bytes")))?;
        tile_band_rows(s, img, limits, interpret, |r| {
            // The band rows start on even offsets of a heap buffer, so they
            // are u16-aligned and append as one copy; the per-sample
            // conversion is kept for any that are not.
            match bytemuck::try_cast_slice::<u8, u16>(r) {
                Ok(v) => out.extend_from_slice(v),
                Err(_) => out.extend(
                    r.chunks_exact(2)
                        .map(|c| u16::from_ne_bytes(c.try_into().unwrap())),
                ),
            }
        })?;
        return Ok(typed_result(Out::U16(out), info.sample_kind, 16));
    }
    let mut out: Vec<u8> = Vec::new();
    out.try_reserve_exact(total_bytes)
        .map_err(|_| TiffError::limits(format!("cannot allocate {total_bytes} bytes")))?;
    tile_band_rows(s, img, limits, interpret, |r| out.extend_from_slice(r))?;
    Ok(typed_result(Out::U8(out), info.sample_kind, 8))
}

/// Tile rows at least this long (bytes) are read straight into the output
/// ([`tiles_direct16`]); shorter ones go through the band. Measured by
/// process ABBA on a sweep of square uncompressed tiles (band path = 1.0,
/// paired median of 20, i7-14650HX): gray16 2048 wide, tile rows of 32 / 64
/// / 128 / 256 / 512 / 1024 B: 1.379 / 1.318 / 1.436 / 1.135 / 0.891 /
/// 0.783; RGB16 1024 wide, 96 / 192 / 384 / 768 / 1536 / 3072 B: 1.205 /
/// 1.149 / 0.928 / 0.741 / 0.668 / 0.654. Short rows written straight into
/// the image are many small strided writes, which lose to the band's
/// sequential appends; from about 400 B the copy the band saves wins.
const DIRECT_TILE_ROW: usize = 512;

/// Uncompressed tiles of 16-bit samples with no predictor: each tile is read
/// from the file straight into its rows of the output, one vectored read
/// per tile ([`Stream::read_at_scatter`]: one buffer per row, the padding
/// columns of a tile wider than the image into a scratch row), so every
/// sample is copied once. The band path copied each tile into the band and
/// then each row into the output; `tiff` 0.11 reads row by row into a
/// zeroed output, a read call per row. Each band of rows is zeroed just
/// before its tiles are read into it (a vectored read needs initialised
/// buffers, and a chunk shorter than its rows relies on it: the rest reads
/// as zero, as everywhere else), and a big-endian file is swapped in place.
///
/// Only the copy changes, so the work-removing part shows in instructions
/// only when they price a copied byte the same however it is copied: with
/// glibc's `rep movsb` disabled (valgrind counts each of its bytes), the
/// whole 1024x1024 RGB16 decode with 256x256 tiles drops 274,324 of
/// 26.90M instructions (one of the two copies of 6 MB); on the clock it is
/// 0.68x the time (process ABBA, 24 of 24 pairs), and from 0.69x the speed
/// of the `tiff` crate to 1.09x.
fn tiles_direct16<R: Read + Seek>(
    s: &mut Stream<R>,
    img: &Image,
    limits: &mut Budget<'_>,
    total_bytes: usize,
) -> TiffResult<Vec<u16>> {
    let info = &img.info;
    let (cw, ch) = info.chunk_dimensions;
    let (across, down) = img.across_down();
    let tile_row = row_bytes(cw, img.spp_chunk(), 16)?;
    let px = usize::from(info.samples_per_pixel) * 2;
    let out_row = info.width as usize * px;
    let swap = s.big_endian != cfg!(target_endian = "big");
    let mut out: Vec<u16> = Vec::new();
    out.try_reserve_exact(total_bytes / 2)
        .map_err(|_| TiffError::limits(format!("cannot allocate {total_bytes} bytes")))?;
    let mut pad: Vec<u8> = Vec::new();
    let mut scratch: Vec<u8> = Vec::new();
    for cy in 0..down {
        let y0 = (cy * ch) as usize;
        let rows = ch.min(info.height - cy * ch) as usize;
        // Zero one band of rows just before its tiles overwrite it (while
        // it is still in cache), not the whole image up front.
        grow_zeroed(&mut out, rows * out_row / 2);
        for cx in 0..across {
            let idx = (cy * across + cx) as usize;
            let (offset, n, expected) = chunk_extent(s, img, idx, ch, tile_row, limits.limits)?;
            // As in `decode_chunk_append`: a short chunk keeps what it has.
            let have = (n as usize).min(expected);
            let x0 = (cx * cw) as usize * px;
            let seg = cw.min(info.width - cx * cw) as usize * px;
            let skip = tile_row - seg;
            if skip > 0 && pad.len() < skip * rows {
                pad.resize(skip * rows, 0);
            }
            // The image's part of each of the tile's rows, with the padding
            // between them; nothing past the last row's part is read.
            let want = (rows - 1) * tile_row + seg;
            let mut left = have.min(want);
            let mut bufs = Vec::with_capacity(2 * rows);
            let mut pads = pad.chunks_mut(skip.max(1));
            let mut out_rows = out_bytes(&mut out)[y0 * out_row..]
                .chunks_mut(out_row)
                .take(rows);
            if skip == 0 && left == want {
                // Whole rows, back to back in the file: one buffer per row.
                bufs.extend(
                    out_rows
                        .by_ref()
                        .map(|row| IoSliceMut::new(&mut row[x0..x0 + seg])),
                );
                left = 0;
            }
            for row in out_rows {
                if left == 0 {
                    break;
                }
                let k = seg.min(left);
                bufs.push(IoSliceMut::new(&mut row[x0..x0 + k]));
                left -= k;
                if skip > 0 && left > 0 {
                    let k = skip.min(left);
                    if let Some(p) = pads.next() {
                        bufs.push(IoSliceMut::new(&mut p[..k]));
                    }
                    left -= k;
                }
            }
            s.read_at_scatter(offset, &mut bufs, &mut scratch)?;
            drop(bufs);
            limits.chunk(n, have, expected)?;
            if swap {
                for row in out[y0 * out_row / 2..].chunks_mut(out_row / 2).take(rows) {
                    for v in &mut row[x0 / 2..x0 / 2 + seg / 2] {
                        *v = v.swap_bytes();
                    }
                }
            }
        }
    }
    Ok(out)
}

/// Append `n` zero samples. On its own so that `resize` inlines here with
/// the zero as a constant and becomes a memset: inlined into the tile loop
/// it stayed a call with the value as an argument and wrote the zeros one
/// sample at a time (+0.3 instructions a sample; `extend(repeat_n(0, n))`
/// did the same).
#[inline(never)]
fn grow_zeroed(v: &mut Vec<u16>, n: usize) {
    v.resize(v.len() + n, 0);
}

fn out_bytes(v: &mut [u16]) -> &mut [u8] {
    bytemuck::cast_slice_mut(v)
}

/// The band loop of [`decode_tile_bands`]: hands each image row's part of
/// each tile, in image order, to `emit`.
fn tile_band_rows<R: Read + Seek>(
    s: &mut Stream<R>,
    img: &Image,
    limits: &mut Budget<'_>,
    interpret: bool,
    mut emit: impl FnMut(&[u8]),
) -> TiffResult<()> {
    let info = &img.info;
    let (cw, ch) = info.chunk_dimensions;
    let (across, down) = img.across_down();
    let bits = info.bits_per_sample;
    let tile_row = row_bytes(cw, img.spp_chunk(), bits)?;
    let px = usize::from(info.samples_per_pixel) * usize::from(bits / 8);
    let too_big = || TiffError::limits("cannot allocate a band of tiles");
    let mut band: Vec<u8> = Vec::new();
    for cy in 0..down {
        let rows = ch.min(info.height - cy * ch) as usize;
        // Bytes of each tile the image uses (its first `rows` rows).
        let keep = rows * tile_row;
        // Room for every tile's kept rows, and for the whole of the tile
        // being decoded, so the band never reallocates inside the loop.
        let need = tile_row
            .checked_mul(ch as usize)
            .and_then(|t| (across as usize - 1).checked_mul(keep)?.checked_add(t))
            .ok_or_else(too_big)?;
        band.clear();
        band.try_reserve_exact(need).map_err(|_| too_big())?;
        for cx in 0..across {
            let at = band.len();
            decode_chunk_append(
                s,
                img,
                (cy * across + cx) as usize,
                (cw, ch, tile_row),
                limits,
                interpret,
                &mut band,
            )?;
            band.truncate(at + keep);
        }
        for r in 0..rows {
            for cx in 0..across {
                let cols = cw.min(info.width - cx * cw) as usize;
                let at = cx as usize * keep + r * tile_row;
                emit(&band[at..at + cols * px]);
            }
        }
    }
    Ok(())
}

fn typed_result(out: Out, kind: SampleKind, bits: u16) -> DecodingResult {
    match (out, kind) {
        (Out::Packed(v) | Out::U8(v), SampleKind::Uint | SampleKind::Float) => {
            DecodingResult::U8(v)
        }
        (Out::Packed(v) | Out::U8(v), SampleKind::Int) => {
            DecodingResult::I8(v.into_iter().map(|x| x as i8).collect())
        }
        (Out::U16(v), SampleKind::Uint) => DecodingResult::U16(v),
        (Out::U16(v), SampleKind::Int) => {
            DecodingResult::I16(v.into_iter().map(|x| x as i16).collect())
        }
        (Out::U16(v), SampleKind::Float) => DecodingResult::F16(v),
        (Out::U32(v), SampleKind::Uint) => DecodingResult::U32(v),
        (Out::U32(v), SampleKind::Int) => {
            DecodingResult::I32(v.into_iter().map(|x| x as i32).collect())
        }
        (Out::U32(v), SampleKind::Float) => {
            DecodingResult::F32(v.into_iter().map(f32::from_bits).collect())
        }
        (Out::U64(v), SampleKind::Uint) => DecodingResult::U64(v),
        (Out::U64(v), SampleKind::Int) => {
            DecodingResult::I64(v.into_iter().map(|x| x as i64).collect())
        }
        (Out::U64(v), SampleKind::Float) => {
            debug_assert_eq!(bits, 64);
            DecodingResult::F64(v.into_iter().map(f64::from_bits).collect())
        }
    }
}

/// A zeroed vector, with allocation failure reported as an error rather
/// than an abort (so even `Limits::unlimited()` cannot crash on a lying size).
pub(crate) fn zeroed<T: Clone + Default>(n: usize) -> TiffResult<Vec<T>> {
    let mut v = Vec::new();
    v.try_reserve_exact(n)
        .map_err(|_| TiffError::limits(format!("cannot allocate {n} elements")))?;
    v.resize(n, T::default());
    Ok(v)
}

fn reverse_bits(buf: &mut [u8]) {
    for b in buf {
        *b = b.reverse_bits();
    }
}

/// The checks every chunk starts with. Returns the chunk's file offset, its
/// stored bytes (the byte count clamped to the end of the file, as libtiff
/// does; a stream that then decodes short is the codec's business) and the
/// bytes it must decode to.
fn chunk_extent<R>(
    s: &Stream<R>,
    img: &Image,
    idx: usize,
    rows: u32,
    chunk_row_bytes: usize,
    limits: &Limits,
) -> TiffResult<(u64, u64, usize)> {
    let expected = chunk_row_bytes
        .checked_mul(rows as usize)
        .ok_or_else(|| TiffError::limits("chunk size overflow"))?;
    if expected > limits.intermediate_buffer_size {
        return Err(TiffError::limits("chunk exceeds intermediate_buffer_size"));
    }
    let offset = img.offsets[idx];
    let count = img.counts[idx];
    if offset > s.len {
        return Err(TiffError::format(format!(
            "chunk {idx} offset {offset} past end of file"
        )));
    }
    let n = count.min(s.len - offset);
    if n > limits.intermediate_buffer_size as u64 {
        return Err(TiffError::limits(
            "compressed chunk exceeds intermediate_buffer_size",
        ));
    }
    Ok((offset, n, expected))
}

/// Decompress and un-predict one chunk, appending exactly
/// `chunk_row_bytes * rows` native-endian bytes to `dst`. Codecs write
/// straight into `dst`'s spare capacity (no zero-fill), and uncompressed data
/// is read from the file directly into place.
fn decode_chunk_append<R: Read + Seek>(
    s: &mut Stream<R>,
    img: &Image,
    idx: usize,
    (cw, rows, chunk_row_bytes): (u32, u32, usize),
    budget: &mut Budget<'_>,
    interpret: bool,
    dst: &mut Vec<u8>,
) -> TiffResult<()> {
    let limits = budget.limits;
    let info = &img.info;
    let (offset, n, expected) = chunk_extent(s, img, idx, rows, chunk_row_bytes, limits)?;
    let comp = info.compression;
    // FillOrder 2: reverse the bits of every compressed byte, as libtiff
    // does for every codec except JPEG (verified against libtiff 4.7 on
    // LZW, Deflate, PackBits, none and CCITT).
    let reverse =
        info.fill_order == 2 && !matches!(comp, compression::JPEG | compression::OLD_JPEG);
    let start = dst.len();
    if comp == compression::NONE {
        // A chunk shorter than its rows (a damaged byte count, or a file cut
        // off) keeps the bytes it has and the rest reads as zero, as the
        // `tiff` crate does, rather than losing the whole image.
        let have = (n as usize).min(expected);
        s.read_at_into(offset, have, dst)?;
        dst.resize(start + expected, 0);
        budget.chunk(n, have, expected)?;
        if reverse {
            reverse_bits(&mut dst[start..]);
        }
    } else {
        let mut raw = s.read_at(offset, n)?;
        if reverse {
            reverse_bits(&mut raw);
        }
        let real = match comp {
            compression::LZW => lzw::decode_append(&raw, dst, expected)?,
            compression::DEFLATE | compression::OLD_DEFLATE => {
                deflate::decode_append(&raw, dst, expected, &mut budget.inflate)?
            }
            compression::PACKBITS => packbits::decode_append(&raw, dst, expected)?,
            compression::CCITT_RLE | compression::CCITT_FAX3 | compression::CCITT_FAX4 => {
                if info.bits_per_sample != 1 || info.samples_per_pixel != 1 {
                    return Err(TiffError::format(
                        "CCITT compression on a non-bilevel image",
                    ));
                }
                let coding = match comp {
                    compression::CCITT_RLE => ccitt::Coding::ModifiedHuffman,
                    compression::CCITT_FAX3 if img.t4_options & 1 != 0 => ccitt::Coding::Group3_2D,
                    compression::CCITT_FAX3 => ccitt::Coding::Group3_1D,
                    _ => ccitt::Coding::Group4,
                };
                let mut o = ccitt::Options::new(coding, cw).with_rows(rows);
                o.max_output_bytes = limits.intermediate_buffer_size;
                ccitt::decode_append(&raw, &o, dst)?;
                dst.truncate(start + expected);
                return budget.chunk(n, expected, expected);
            }
            compression::JPEG => {
                decode_jpeg_chunk(
                    img,
                    &raw,
                    (cw, rows, chunk_row_bytes),
                    dst,
                    limits,
                    interpret,
                )?;
                // Rows the JPEG does not hold read as zero.
                dst.resize(start + expected, 0);
                return budget.chunk(n, expected, expected);
            }
            compression::OLD_JPEG => {
                return Err(TiffError::unsupported("old-style JPEG (compression 6)"))
            }
            c => return Err(TiffError::unsupported(format!("compression {c}"))),
        };
        budget.chunk(n, real, expected)?;
    }
    let buf = &mut dst[start..];
    let bytes = usize::from(info.bits_per_sample) / 8;
    let spp = usize::from(img.spp_chunk());
    match info.predictor {
        pred::FLOATING_POINT => predictor::undo_float(buf, chunk_row_bytes, spp, bytes),
        p => {
            if s.big_endian != cfg!(target_endian = "big") {
                predictor::swap_bytes(buf, bytes);
            }
            if p == pred::HORIZONTAL {
                predictor::undo_horizontal(buf, chunk_row_bytes, spp, bytes);
            }
        }
    }
    Ok(())
}

/// Decode one JPEG strip or tile and append its rows to `dst`, each cut or
/// zero-padded to `chunk_row_bytes`. The rows are appended (the caller
/// reserved nothing and zero-fills only rows the JPEG lacks): zero-filling
/// the chunk first and copying over it wrote every byte twice.
#[cfg(feature = "jpeg")]
fn decode_jpeg_chunk(
    img: &Image,
    raw: &[u8],
    (cw, rows, chunk_row_bytes): (u32, u32, usize),
    dst: &mut Vec<u8>,
    limits: &Limits,
    convert_ycbcr: bool,
) -> TiffResult<()> {
    let j = crate::jpeg::decode(
        img.jpeg_tables.as_deref(),
        raw,
        img.info.photometric == photometric::YCBCR && convert_ycbcr,
        limits.intermediate_buffer_size,
    )?;
    let spp = img.spp_chunk();
    if j.components != spp {
        return Err(TiffError::format(format!(
            "JPEG chunk has {} components, image {spp}",
            j.components
        )));
    }
    if j.width < cw.min(img.info.width) {
        return Err(TiffError::format("JPEG chunk narrower than the chunk"));
    }
    let src_row = j.width as usize * usize::from(spp);
    let copy = src_row.min(chunk_row_bytes);
    let have = rows.min(j.height) as usize;
    dst.try_reserve_exact(chunk_row_bytes * rows as usize)
        .map_err(|_| TiffError::limits("cannot allocate a JPEG chunk"))?;
    for r in 0..have {
        dst.extend_from_slice(&j.data[r * src_row..][..copy]);
        if copy < chunk_row_bytes {
            dst.resize(dst.len() + chunk_row_bytes - copy, 0);
        }
    }
    Ok(())
}

#[cfg(not(feature = "jpeg"))]
fn decode_jpeg_chunk(
    _img: &Image,
    _raw: &[u8],
    _geom: (u32, u32, usize),
    _dst: &mut Vec<u8>,
    _limits: &Limits,
    _convert_ycbcr: bool,
) -> TiffResult<()> {
    Err(TiffError::unsupported(
        "JPEG-in-TIFF (build with the `jpeg` feature)",
    ))
}

// Invert only the grey channel (sample 0 of each pixel). Plain grey takes
// one straight pass, which vectorises; with extra samples the pass steps a
// pixel at a time. Testing `i % n == 0` on every sample was a division per
// sample: 11.7 Ir a sample on 8-bit grey.
fn each<T>(v: &mut [T], n: usize, f: impl Fn(&mut T)) {
    if n == 1 {
        v.iter_mut().for_each(f);
    } else {
        v.iter_mut().step_by(n).for_each(f);
    }
}

fn invert_white_is_zero(raw: DecodingResult, info: &ImageInfo) -> TiffResult<DecodingResult> {
    let n = usize::from(info.samples_per_pixel);
    if n > 1 && info.bits_per_sample < 8 {
        return Err(TiffError::unsupported(
            "min-is-white with extra samples below 8 bits",
        ));
    }
    let mut r = raw;
    match &mut r {
        DecodingResult::U8(v) => {
            if info.bits_per_sample < 8 {
                for x in v.iter_mut() {
                    *x = !*x;
                }
            } else {
                each(v, n, |x| *x = !*x);
            }
        }
        DecodingResult::U16(v) => each(v, n, |x| *x = !*x),
        DecodingResult::U32(v) => each(v, n, |x| *x = !*x),
        DecodingResult::U64(v) => each(v, n, |x| *x = !*x),
        DecodingResult::I8(v) => each(v, n, |x| *x = !*x),
        DecodingResult::I16(v) => each(v, n, |x| *x = !*x),
        DecodingResult::I32(v) => each(v, n, |x| *x = !*x),
        DecodingResult::I64(v) => each(v, n, |x| *x = !*x),
        DecodingResult::F32(v) => each(v, n, |x| *x = 1.0 - *x),
        DecodingResult::F64(v) => each(v, n, |x| *x = 1.0 - *x),
        DecodingResult::F16(_) => return Err(TiffError::unsupported("min-is-white 16-bit float")),
    }
    Ok(r)
}

fn expand_palette(
    raw: &DecodingResult,
    info: &ImageInfo,
    map: &[u16],
) -> TiffResult<DecodingResult> {
    let bits = usize::from(info.bits_per_sample);
    let entries = 1usize << bits;
    let (w, h) = (info.width as usize, info.height as usize);
    if map.len() < 3 * entries {
        return Err(TiffError::format("ColorMap too short"));
    }
    let mut out = Vec::with_capacity(w * h * 3);
    let push = |out: &mut Vec<u16>, idx: usize| {
        out.push(map[idx]);
        out.push(map[entries + idx]);
        out.push(map[2 * entries + idx]);
    };
    match raw {
        DecodingResult::U8(v) if bits < 8 => {
            // Packed indices, most significant first: every stored byte
            // holds `8 / bits` of them, so a byte is split with constant
            // shifts instead of one `get_bits` call (a loop over the bits)
            // per pixel: 111 Ir a pixel at 4 bits.
            let table = rgb_table(map, entries);
            let stride = (w * bits).div_ceil(8);
            for row in v.chunks_exact(stride).take(h) {
                match bits {
                    4 => expand_packed_row::<4>(row, w, &table, &mut out),
                    2 => expand_packed_row::<2>(row, w, &table, &mut out),
                    _ => expand_packed_row::<1>(row, w, &table, &mut out),
                }
            }
        }
        DecodingResult::U8(v) => {
            // One `[r, g, b]` entry per index, read with one fixed-size
            // index (a `u8` cannot leave a 256-entry table) and appended
            // as one sized extend. Three pushes per pixel, each with its
            // own capacity check and a bounds-checked read of one of the
            // colormap's three planes, cost 61 Ir a pixel.
            let table = rgb_table(map, entries);
            out.extend(v.iter().flat_map(|&i| table[usize::from(i)]));
        }
        DecodingResult::U16(v) => v.iter().for_each(|&i| push(&mut out, usize::from(i))),
        _ => return Err(TiffError::unsupported("palette sample type")),
    }
    Ok(DecodingResult::U16(out))
}

/// Append the colours of one packed row of `w` `BITS`-bit palette indices.
fn expand_packed_row<const BITS: usize>(
    row: &[u8],
    w: usize,
    table: &[[u16; 3]; 256],
    out: &mut Vec<u16>,
) {
    let per = 8 / BITS;
    let mask = (1usize << BITS) - 1;
    let full = (w / per).min(row.len());
    for &b in &row[..full] {
        for j in 0..per {
            let idx = (usize::from(b) >> (8 - BITS * (j + 1))) & mask;
            out.extend_from_slice(&table[idx]);
        }
    }
    // The pixels of a last, partly used byte.
    if let Some(&b) = row.get(full) {
        for j in 0..w % per {
            let idx = (usize::from(b) >> (8 - BITS * (j + 1))) & mask;
            out.extend_from_slice(&table[idx]);
        }
    }
}

/// The colormap's first `entries` (at most 256) colours as `[r, g, b]`
/// triples. `map` holds at least `3 * entries` values: all reds, then all
/// greens, then all blues.
fn rgb_table(map: &[u16], entries: usize) -> [[u16; 3]; 256] {
    let mut table = [[0u16; 3]; 256];
    let (r, gb) = map.split_at(entries);
    let (g, b) = gb.split_at(entries);
    for (((t, &r), &g), &b) in table.iter_mut().zip(r).zip(g).zip(b) {
        *t = [r, g, b];
    }
    table
}

/// The entry, unless its field-type code is one TIFF does not define (such
/// a field is skipped, per TIFF 6.0 section 2).
fn known(e: Option<&ifd::Entry>) -> Option<&ifd::Entry> {
    e.filter(|e| e.field_type().is_some())
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn ne_bytes_both_ways_agree() {
        let cases = [
            DecodingResult::U8(vec![1, 2, 250]),
            DecodingResult::I8(vec![-1, 0, 7]),
            DecodingResult::U16(vec![0x0102, 0xfffe]),
            DecodingResult::F32(vec![1.5, -0.25]),
            DecodingResult::U64(vec![u64::MAX - 3]),
        ];
        for c in cases {
            let want: Vec<u8> = match &c {
                DecodingResult::U8(v) => v.clone(),
                DecodingResult::I8(v) => v.iter().map(|&x| x as u8).collect(),
                DecodingResult::U16(v) => v.iter().flat_map(|x| x.to_ne_bytes()).collect(),
                DecodingResult::F32(v) => v.iter().flat_map(|x| x.to_ne_bytes()).collect(),
                DecodingResult::U64(v) => v.iter().flat_map(|x| x.to_ne_bytes()).collect(),
                _ => unreachable!(),
            };
            assert_eq!(c.to_ne_bytes(), want);
            assert_eq!(c.into_ne_bytes(), want);
        }
    }

    /// The assembly reference: every sample moved one bit at a time.
    fn copy_packed_bits(out: &mut [u8], chunk: &[u8], g: &Geom, out_row_bytes: usize) {
        for r in 0..g.rows {
            let src = &chunk[r * g.chunk_row_bytes..][..g.chunk_row_bytes];
            let dst = &mut out[(g.y0 + r) * out_row_bytes..][..out_row_bytes];
            let n = if g.planar { g.cols } else { g.cols * g.spp };
            for i in 0..n {
                let v = get_bits(src, i * g.bits, g.bits);
                let at = if g.planar {
                    ((g.x0 + i) * g.spp + g.plane) * g.bits
                } else {
                    (g.x0 * g.spp + i) * g.bits
                };
                set_bits(dst, at, g.bits, v);
            }
        }
    }

    #[test]
    fn copy_packed_matches_per_bit_moves() {
        let mut seed = 7u32;
        let mut rnd = move || {
            seed = seed.wrapping_mul(1_103_515_245).wrapping_add(12345);
            (seed >> 16) as usize
        };
        for bits in [1usize, 2, 4] {
            for spp in [1usize, 3] {
                for planar in [false, true] {
                    for (width, cw) in [(40usize, 16usize), (37, 16), (21, 5), (9, 9), (64, 24)] {
                        let (height, ch) = (5usize, 3usize);
                        let out_row = (width * spp * bits).div_ceil(8);
                        let spp_chunk = if planar { 1 } else { spp };
                        let chunk_row = (cw * spp_chunk * bits).div_ceil(8);
                        // Start from set bits, so a bit the copy must leave
                        // alone is seen if it gets cleared.
                        let mut a = vec![0xA5u8; out_row * height];
                        let mut b = a.clone();
                        for plane in 0..if planar { spp } else { 1 } {
                            for y0 in (0..height).step_by(ch) {
                                for x0 in (0..width).step_by(cw) {
                                    let chunk: Vec<u8> =
                                        (0..chunk_row * ch).map(|_| rnd() as u8).collect();
                                    let g = Geom {
                                        width,
                                        spp,
                                        planar,
                                        plane,
                                        x0,
                                        y0,
                                        cols: cw.min(width - x0),
                                        rows: ch.min(height - y0),
                                        chunk_row_bytes: chunk_row,
                                        bits,
                                    };
                                    // The reference never takes the strip
                                    // shortcut (which also copies pad bits).
                                    if !(x0 == 0 && chunk_row == out_row && !planar) {
                                        copy_packed(&mut a, &chunk, &g, out_row);
                                        copy_packed_bits(&mut b, &chunk, &g, out_row);
                                    }
                                }
                            }
                        }
                        assert_eq!(
                            a, b,
                            "bits {bits} spp {spp} planar {planar} w {width} cw {cw}"
                        );
                    }
                }
            }
        }
    }

    #[test]
    fn palette_expansion_matches_the_definition() {
        let mut seed = 5u32;
        let mut rnd = move || {
            seed = seed.wrapping_mul(1_103_515_245).wrapping_add(12345);
            (seed >> 16) as u16
        };
        for bits in [1u16, 2, 4, 8] {
            let entries = 1usize << bits;
            let map: Vec<u16> = (0..3 * entries).map(|_| rnd()).collect();
            for (w, h) in [
                (1u32, 1u32),
                (3, 2),
                (7, 3),
                (8, 2),
                (9, 4),
                (16, 1),
                (37, 5),
            ] {
                let stride = (w as usize * usize::from(bits)).div_ceil(8);
                let raw: Vec<u8> = (0..stride * h as usize).map(|_| rnd() as u8).collect();
                // The definition: pixel by pixel, one bit at a time.
                let mut want = Vec::new();
                for y in 0..h as usize {
                    let row = &raw[y * stride..][..stride];
                    for x in 0..w as usize {
                        let i =
                            usize::from(get_bits(row, x * usize::from(bits), usize::from(bits)));
                        want.extend_from_slice(&[map[i], map[entries + i], map[2 * entries + i]]);
                    }
                }
                let info = ImageInfo {
                    width: w,
                    height: h,
                    bits_per_sample: bits,
                    samples_per_pixel: 1,
                    sample_kind: SampleKind::Uint,
                    photometric: photometric::PALETTE,
                    compression: compression::NONE,
                    predictor: 1,
                    planar: false,
                    tiled: false,
                    chunk_dimensions: (w, h),
                    extra_samples: Vec::new(),
                    fill_order: 1,
                };
                let got = expand_palette(&DecodingResult::U8(raw), &info, &map).unwrap();
                assert_eq!(got, DecodingResult::U16(want), "bits {bits} {w}x{h}");
            }
        }
    }

    #[test]
    fn each_visits_sample_zero_of_every_pixel() {
        for n in 1..=5usize {
            for len in [0usize, 1, 2, 5, 16, 33, 64] {
                let orig: Vec<u8> = (0..len).map(|i| (i * 31 + 7) as u8).collect();
                let mut want = orig.clone();
                for (i, x) in want.iter_mut().enumerate() {
                    if i % n == 0 {
                        *x = !*x;
                    }
                }
                let mut got = orig.clone();
                each(&mut got, n, |x| *x = !*x);
                assert_eq!(got, want, "n {n} len {len}");
            }
        }
    }

    /// Tiles decoded through the band path against the pixels they were
    /// written from: tiles wider and taller than the image, partial last
    /// columns and rows, one-pixel tiles, with and without a codec.
    #[test]
    fn tile_bands_reassemble_the_image() {
        use crate::encoder::{colortype, Compression, TiffEncoder};
        use std::io::Cursor;
        let mut seed = 3u32;
        for (w, h) in [(1u32, 1u32), (5, 3), (16, 16), (37, 21), (64, 9), (100, 70)] {
            let rgb: Vec<u8> = (0..w * h * 3)
                .map(|i| {
                    seed = seed.wrapping_mul(1_103_515_245).wrapping_add(12345);
                    // Runs and noise, so PackBits and LZW both have work.
                    if (i / 24) % 3 == 0 {
                        7
                    } else {
                        (seed >> 16) as u8
                    }
                })
                .collect();
            let grey: Vec<u8> = rgb.iter().step_by(3).copied().collect();
            for (tw, th) in [
                (16u32, 16u32),
                (1, 1),
                (5, 7),
                (48, 16),
                (16, 96),
                (128, 128),
            ] {
                for comp in [Compression::None, Compression::PackBits, Compression::Lzw] {
                    let mut f = Cursor::new(Vec::new());
                    let mut e = TiffEncoder::new(&mut f).unwrap();
                    let mut img = e
                        .new_image::<colortype::RGB8>(w, h)
                        .unwrap()
                        .with_compression(comp);
                    img.tiles(tw, th);
                    img.write_data(&rgb).unwrap();
                    let mut img = e
                        .new_image::<colortype::Gray8>(w, h)
                        .unwrap()
                        .with_compression(comp);
                    img.tiles(tw, th);
                    img.write_data(&grey).unwrap();
                    let file = f.into_inner();
                    let mut d = Decoder::new(Cursor::new(&file)).unwrap();
                    let what = format!("{w}x{h} tiles {tw}x{th} {comp:?}");
                    assert_eq!(
                        d.read_image().unwrap(),
                        DecodingResult::U8(rgb.clone()),
                        "{what}"
                    );
                    d.next_image().unwrap();
                    assert_eq!(
                        d.read_image().unwrap(),
                        DecodingResult::U8(grey.clone()),
                        "{what}"
                    );
                }
            }
        }
    }

    #[test]
    fn f16_conversion() {
        assert_eq!(f16_to_f32(0x3C00), 1.0);
        assert_eq!(f16_to_f32(0xC000), -2.0);
        assert_eq!(f16_to_f32(0x7BFF), 65504.0);
        assert_eq!(f16_to_f32(0x0001), 2f32.powi(-24));
        assert_eq!(f16_to_f32(0x03FF), 1023.0 * 2f32.powi(-24));
        assert!(f16_to_f32(0x7E00).is_nan());
        assert_eq!(f16_to_f32(0x7C00), f32::INFINITY);
        assert_eq!(f16_to_f32(0x8000).to_bits(), 0x8000_0000);
    }
}
