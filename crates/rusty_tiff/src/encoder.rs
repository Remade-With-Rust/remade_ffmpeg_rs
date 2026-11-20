//! The TIFF encoder: little-endian classic TIFF or BigTIFF, strips or tiles,
//! chunky or planar, any number of pages.

use std::collections::BTreeMap;
use std::io::{Seek, SeekFrom, Write};
use std::marker::PhantomData;

use crate::decoder::{get_bits, row_bytes, set_bits};
use crate::error::{TiffError, TiffResult};
use crate::tags::{compression, extra_samples, photometric, predictor as pred, sample_format, Tag};
use crate::value::Value;
use crate::{ccitt, deflate, lzw, packbits, predictor};

/// Compression for written chunks.
#[derive(Clone, Copy, Debug, PartialEq, Eq, Hash)]
#[non_exhaustive]
pub enum Compression {
    /// Uncompressed.
    None,
    /// TIFF 6 LZW.
    Lzw,
    /// Adobe Deflate (code 8) at a zlib level 0-9.
    Deflate(u8),
    /// PackBits (rows coded separately).
    PackBits,
    /// CCITT Group 3 1-D (code 3), 1-bit greyscale only.
    Fax3,
    /// CCITT Group 4 (code 4), 1-bit greyscale only.
    Fax4,
}

impl Compression {
    /// Deflate at the zlib default level 6.
    #[must_use]
    pub const fn deflate() -> Self {
        Self::Deflate(6)
    }

    fn code(self) -> u16 {
        match self {
            Self::None => compression::NONE,
            Self::Lzw => compression::LZW,
            Self::Deflate(_) => compression::DEFLATE,
            Self::PackBits => compression::PACKBITS,
            Self::Fax3 => compression::CCITT_FAX3,
            Self::Fax4 => compression::CCITT_FAX4,
        }
    }
}

/// Predictor for written chunks.
#[derive(Clone, Copy, Debug, PartialEq, Eq, Hash)]
pub enum Predictor {
    /// None.
    None,
    /// Horizontal differencing (integer samples of 8 bits or more).
    Horizontal,
    /// Floating-point predictor (float samples).
    FloatingPoint,
}

/// A sample type the encoder accepts.
pub trait TiffSample: Copy + private::Sealed {
    /// Size in bytes.
    const BYTES: usize;
    /// Append the native-endian bytes.
    fn put_ne(self, out: &mut Vec<u8>);
    /// The samples as native-endian bytes: borrowed for byte samples,
    /// converted in one sized pass otherwise.
    #[doc(hidden)]
    fn ne_bytes(src: &[Self]) -> std::borrow::Cow<'_, [u8]> {
        let mut out = Vec::with_capacity(src.len() * Self::BYTES);
        for &s in src {
            s.put_ne(&mut out);
        }
        std::borrow::Cow::Owned(out)
    }
}

mod private {
    pub trait Sealed {}
}

macro_rules! sample_impl {
    ($($t:ty),*) => {$(
        impl private::Sealed for $t {}
        impl TiffSample for $t {
            const BYTES: usize = std::mem::size_of::<$t>();
            #[inline]
            fn put_ne(self, out: &mut Vec<u8>) {
                out.extend_from_slice(&self.to_ne_bytes());
            }
        }
    )*};
}
sample_impl!(i8, u16, i16, u32, i32, u64, i64, f32, f64);

impl private::Sealed for u8 {}
impl TiffSample for u8 {
    const BYTES: usize = 1;
    #[inline]
    fn put_ne(self, out: &mut Vec<u8>) {
        out.push(self);
    }
    #[inline]
    fn ne_bytes(src: &[Self]) -> std::borrow::Cow<'_, [u8]> {
        std::borrow::Cow::Borrowed(src)
    }
}

/// A pixel layout the encoder writes. See [`colortype`] for the provided ones.
pub trait ColorType {
    /// Element type of the input slice.
    type Inner: TiffSample;
    /// Bits per sample.
    const BITS_PER_SAMPLE: u16;
    /// Samples per pixel.
    const SAMPLES: u16;
    /// `PhotometricInterpretation` written.
    const PHOTOMETRIC: u16;
    /// `SampleFormat` written.
    const SAMPLE_FORMAT: u16;
    /// `ExtraSamples` written.
    const EXTRA_SAMPLES: &'static [u16];
}

/// The provided colour types. Sub-byte types (`Gray1`, `Gray2`, `Gray4`,
/// `Palette4`) take packed rows, MSB first, each row padded to a byte.
/// `*16Float` types take IEEE binary16 bit patterns as `u16`.
pub mod colortype {
    use super::{extra_samples, photometric, sample_format, ColorType};

    macro_rules! ct {
        ($(#[$m:meta])* $name:ident, $inner:ty, $bits:expr, $spp:expr, $photo:expr, $fmt:expr, $extra:expr) => {
            $(#[$m])*
            #[derive(Clone, Copy, Debug)]
            pub struct $name;
            impl ColorType for $name {
                type Inner = $inner;
                const BITS_PER_SAMPLE: u16 = $bits;
                const SAMPLES: u16 = $spp;
                const PHOTOMETRIC: u16 = $photo;
                const SAMPLE_FORMAT: u16 = $fmt;
                const EXTRA_SAMPLES: &'static [u16] = $extra;
            }
        };
    }
    const U: u16 = sample_format::UINT;
    const I: u16 = sample_format::INT;
    const F: u16 = sample_format::IEEEFP;
    const G: u16 = photometric::BLACK_IS_ZERO;
    const C: u16 = photometric::RGB;
    const K: u16 = photometric::SEPARATED;
    const P: u16 = photometric::PALETTE;
    const N: &[u16] = &[];
    const A: &[u16] = &[extra_samples::UNASSOCIATED_ALPHA];

    ct!(/** 1-bit grey. */ Gray1, u8, 1, 1, G, U, N);
    ct!(/** 2-bit grey. */ Gray2, u8, 2, 1, G, U, N);
    ct!(/** 4-bit grey. */ Gray4, u8, 4, 1, G, U, N);
    ct!(/** 8-bit grey. */ Gray8, u8, 8, 1, G, U, N);
    ct!(/** 16-bit grey. */ Gray16, u16, 16, 1, G, U, N);
    ct!(/** 32-bit grey. */ Gray32, u32, 32, 1, G, U, N);
    ct!(/** 64-bit grey. */ Gray64, u64, 64, 1, G, U, N);
    ct!(/** Signed 8-bit grey. */ GrayI8, i8, 8, 1, G, I, N);
    ct!(/** Signed 16-bit grey. */ GrayI16, i16, 16, 1, G, I, N);
    ct!(/** Signed 32-bit grey. */ GrayI32, i32, 32, 1, G, I, N);
    ct!(/** Signed 64-bit grey. */ GrayI64, i64, 64, 1, G, I, N);
    ct!(/** binary16 grey (bit patterns). */ Gray16Float, u16, 16, 1, G, F, N);
    ct!(/** f32 grey. */ Gray32Float, f32, 32, 1, G, F, N);
    ct!(/** f64 grey. */ Gray64Float, f64, 64, 1, G, F, N);
    ct!(/** 8-bit grey + alpha. */ GrayA8, u8, 8, 2, G, U, A);
    ct!(/** 16-bit grey + alpha. */ GrayA16, u16, 16, 2, G, U, A);
    ct!(/** 8-bit RGB. */ RGB8, u8, 8, 3, C, U, N);
    ct!(/** 16-bit RGB. */ RGB16, u16, 16, 3, C, U, N);
    ct!(/** 32-bit RGB. */ RGB32, u32, 32, 3, C, U, N);
    ct!(/** binary16 RGB (bit patterns). */ RGB16Float, u16, 16, 3, C, F, N);
    ct!(/** f32 RGB. */ RGB32Float, f32, 32, 3, C, F, N);
    ct!(/** f64 RGB. */ RGB64Float, f64, 64, 3, C, F, N);
    ct!(/** 8-bit RGBA (unassociated alpha). */ RGBA8, u8, 8, 4, C, U, A);
    ct!(/** 16-bit RGBA (unassociated alpha). */ RGBA16, u16, 16, 4, C, U, A);
    ct!(/** f32 RGBA (unassociated alpha). */ RGBA32Float, f32, 32, 4, C, F, A);
    ct!(/** 8-bit CMYK. */ CMYK8, u8, 8, 4, K, U, N);
    ct!(/** 16-bit CMYK. */ CMYK16, u16, 16, 4, K, U, N);
    ct!(/** 8-bit CMYK + alpha. */ CMYKA8, u8, 8, 5, K, U, A);
    ct!(/** 16-bit CMYK + alpha. */ CMYKA16, u16, 16, 5, K, U, A);
    ct!(/** 4-bit palette indices (set a colormap). */ Palette4, u8, 4, 1, P, U, N);
    ct!(/** 8-bit palette indices (set a colormap). */ Palette8, u8, 8, 1, P, U, N);
}

/// Writes a TIFF: create, then add pages with [`Self::new_image`] or
/// [`Self::write_image`].
///
/// ```
/// use rusty_tiff::encoder::{colortype, Compression, TiffEncoder};
/// let mut file = std::io::Cursor::new(Vec::new());
/// let mut enc = TiffEncoder::new(&mut file)?.with_compression(Compression::Lzw);
/// let pixels = vec![128u8; 16 * 8 * 3];
/// let mut img = enc.new_image::<colortype::RGB8>(16, 8)?;
/// img.icc_profile(vec![0; 132]);
/// img.write_data(&pixels)?;
/// enc.write_image::<colortype::Gray16>(4, 4, &[1000u16; 16])?; // page 2
/// # Ok::<_, rusty_tiff::TiffError>(())
/// ```
pub struct TiffEncoder<W: Write + Seek> {
    w: W,
    bigtiff: bool,
    next_ptr_pos: u64,
    compression: Compression,
    predictor: Predictor,
}

impl<W: Write + Seek> TiffEncoder<W> {
    /// Start a classic (32-bit offset) little-endian TIFF.
    pub fn new(mut w: W) -> TiffResult<Self> {
        w.write_all(b"II\x2A\x00\x00\x00\x00\x00")?;
        Ok(Self {
            w,
            bigtiff: false,
            next_ptr_pos: 4,
            compression: Compression::None,
            predictor: Predictor::None,
        })
    }

    /// Start a little-endian BigTIFF (64-bit offsets).
    pub fn new_big(mut w: W) -> TiffResult<Self> {
        w.write_all(b"II\x2B\x00\x08\x00\x00\x00")?;
        w.write_all(&[0u8; 8])?;
        Ok(Self {
            w,
            bigtiff: true,
            next_ptr_pos: 8,
            compression: Compression::None,
            predictor: Predictor::None,
        })
    }

    /// Default compression for later images.
    #[must_use]
    pub fn with_compression(mut self, c: Compression) -> Self {
        self.compression = c;
        self
    }

    /// Default predictor for later images.
    #[must_use]
    pub fn with_predictor(mut self, p: Predictor) -> Self {
        self.predictor = p;
        self
    }

    /// Begin an image (a page). Configure it, then call
    /// [`ImageEncoder::write_data`].
    pub fn new_image<C: ColorType>(
        &mut self,
        width: u32,
        height: u32,
    ) -> TiffResult<ImageEncoder<'_, W, C>> {
        self.image_encoder(width, height, false)
    }

    /// Begin an image whose IFD is NOT linked into the page chain: a SubIFD
    /// (DNG raw data, reduced resolutions). [`ImageEncoder::write_data`]
    /// returns its offset; list it in a parent's [`Tag::SubIfds`] with
    /// `Value::Ifd` (classic) or `Value::Ifd8` (BigTIFF).
    pub fn new_subimage<C: ColorType>(
        &mut self,
        width: u32,
        height: u32,
    ) -> TiffResult<ImageEncoder<'_, W, C>> {
        self.image_encoder(width, height, true)
    }

    /// Write a detached IFD holding only `tags` (an EXIF or GPS IFD) and
    /// return its offset, for a parent's [`Tag::ExifIfd`] / [`Tag::GpsIfd`].
    pub fn write_directory(&mut self, tags: &[(Tag, Value)]) -> TiffResult<u64> {
        let map: BTreeMap<u16, Value> = tags.iter().map(|(t, v)| (t.to_u16(), v.clone())).collect();
        self.write_ifd(&map, true)
    }

    fn image_encoder<C: ColorType>(
        &mut self,
        width: u32,
        height: u32,
        detached: bool,
    ) -> TiffResult<ImageEncoder<'_, W, C>> {
        if width == 0 || height == 0 {
            return Err(TiffError::usage("zero image dimension"));
        }
        let (compression, predictor) = (self.compression, self.predictor);
        Ok(ImageEncoder {
            enc: self,
            width,
            height,
            compression,
            predictor,
            rows_per_strip: None,
            tiles: None,
            planar: false,
            photometric: C::PHOTOMETRIC,
            tags: BTreeMap::new(),
            detached,
            _c: PhantomData,
        })
    }

    /// Write a whole image with the default settings.
    pub fn write_image<C: ColorType>(
        &mut self,
        width: u32,
        height: u32,
        data: &[C::Inner],
    ) -> TiffResult<u64> {
        self.new_image::<C>(width, height)?.write_data(data)
    }

    /// The underlying writer.
    pub fn into_inner(self) -> W {
        self.w
    }

    fn pos(&mut self) -> TiffResult<u64> {
        Ok(self.w.stream_position()?)
    }

    fn align2(&mut self) -> TiffResult<()> {
        if self.pos()? % 2 == 1 {
            self.w.write_all(&[0])?;
        }
        Ok(())
    }

    fn check_offset(&self, off: u64) -> TiffResult<()> {
        if !self.bigtiff && off > u64::from(u32::MAX) {
            return Err(TiffError::usage(
                "classic TIFF passed 4 GiB; use TiffEncoder::new_big",
            ));
        }
        Ok(())
    }

    /// Write an IFD (entries sorted by tag), link it into the page chain
    /// unless `detached`, and return its offset.
    fn write_ifd(&mut self, entries: &BTreeMap<u16, Value>, detached: bool) -> TiffResult<u64> {
        let inline = if self.bigtiff { 8 } else { 4 };
        let mut fields = Vec::with_capacity(entries.len());
        for (&tag, v) in entries {
            let bytes = v.to_le_bytes();
            let field = if bytes.len() <= inline {
                let mut f = [0u8; 8];
                f[..bytes.len()].copy_from_slice(&bytes);
                f
            } else {
                self.align2()?;
                let off = self.pos()?;
                self.check_offset(off)?;
                self.w.write_all(&bytes)?;
                let mut f = [0u8; 8];
                if self.bigtiff {
                    f.copy_from_slice(&off.to_le_bytes());
                } else {
                    f[..4].copy_from_slice(&(off as u32).to_le_bytes());
                }
                f
            };
            fields.push((tag, v.field_type() as u16, v.count() as u64, field));
        }
        self.align2()?;
        let ifd_off = self.pos()?;
        self.check_offset(ifd_off)?;
        let mut buf = Vec::new();
        if self.bigtiff {
            buf.extend_from_slice(&(fields.len() as u64).to_le_bytes());
        } else {
            buf.extend_from_slice(&(fields.len() as u16).to_le_bytes());
        }
        for (tag, ty, count, field) in &fields {
            buf.extend_from_slice(&tag.to_le_bytes());
            buf.extend_from_slice(&ty.to_le_bytes());
            if self.bigtiff {
                buf.extend_from_slice(&count.to_le_bytes());
                buf.extend_from_slice(field);
            } else {
                buf.extend_from_slice(&(*count as u32).to_le_bytes());
                buf.extend_from_slice(&field[..4]);
            }
        }
        let next_field = ifd_off + buf.len() as u64;
        buf.extend_from_slice(&[0u8; 8][..inline]);
        self.w.write_all(&buf)?;
        if detached {
            return Ok(ifd_off);
        }
        // Link from the previous next-pointer (or the header).
        self.w.seek(SeekFrom::Start(self.next_ptr_pos))?;
        if self.bigtiff {
            self.w.write_all(&ifd_off.to_le_bytes())?;
        } else {
            self.w.write_all(&(ifd_off as u32).to_le_bytes())?;
        }
        self.w.seek(SeekFrom::End(0))?;
        self.next_ptr_pos = next_field;
        Ok(ifd_off)
    }
}

/// One image being written. Created by [`TiffEncoder::new_image`].
pub struct ImageEncoder<'a, W: Write + Seek, C: ColorType> {
    enc: &'a mut TiffEncoder<W>,
    width: u32,
    height: u32,
    compression: Compression,
    predictor: Predictor,
    rows_per_strip: Option<u32>,
    tiles: Option<(u32, u32)>,
    planar: bool,
    photometric: u16,
    tags: BTreeMap<u16, Value>,
    detached: bool,
    _c: PhantomData<C>,
}

impl<W: Write + Seek, C: ColorType> ImageEncoder<'_, W, C> {
    /// Compression for this image.
    #[must_use]
    pub fn with_compression(mut self, c: Compression) -> Self {
        self.compression = c;
        self
    }

    /// Predictor for this image.
    #[must_use]
    pub fn with_predictor(mut self, p: Predictor) -> Self {
        self.predictor = p;
        self
    }

    /// Rows per strip (default: about 8 KiB per strip, libtiff's convention,
    /// which keeps a reader's working set small).
    ///
    /// Each strip is compressed on its own, so with Deflate (and to a lesser
    /// degree LZW) small strips cost size: every strip restarts the
    /// compressor's history. On a 1024x1024 RGB8 photo at level 6, 8 KiB
    /// strips came out 38% larger than ~1 MB strips (the `tiff` crate's
    /// default), and LZW about 3% larger. For smaller Deflate files, ask for
    /// bigger strips, e.g. `rows_per_strip((1 << 20) / row_bytes)`; a reader
    /// then needs a buffer of that size per strip.
    pub fn rows_per_strip(&mut self, rows: u32) {
        self.rows_per_strip = Some(rows.max(1));
        self.tiles = None;
    }

    /// Write tiles of `width` x `height` instead of strips (TIFF requires
    /// multiples of 16; the encoder does not enforce it).
    pub fn tiles(&mut self, width: u32, height: u32) {
        self.tiles = Some((width.max(1), height.max(1)));
    }

    /// Planar configuration 2 (each sample in its own plane).
    pub fn planar(&mut self, planar: bool) {
        self.planar = planar;
    }

    /// Override `PhotometricInterpretation` (e.g.
    /// [`photometric::WHITE_IS_ZERO`] for a min-is-white grey image; the
    /// samples are written as given).
    pub fn photometric(&mut self, value: u16) {
        self.photometric = value;
    }

    /// Embed an ICC profile (tag 34675).
    pub fn icc_profile(&mut self, profile: Vec<u8>) {
        self.tags
            .insert(Tag::IccProfile.to_u16(), Value::Undefined(profile));
    }

    /// Set the palette for `Palette*` types: `3 << bits` entries, all reds,
    /// then greens, then blues.
    pub fn colormap(&mut self, map: Vec<u16>) {
        self.tags.insert(Tag::ColorMap.to_u16(), Value::Short(map));
    }

    /// Add (or replace) an arbitrary tag. Tags describing the image layout
    /// (dimensions, offsets, compression...) are written by the encoder and
    /// take precedence.
    pub fn add_tag(&mut self, tag: Tag, value: Value) {
        self.tags.insert(tag.to_u16(), value);
    }

    /// Encode `data` (interleaved samples, rows top to bottom; packed rows
    /// for sub-byte types) and write the image and its IFD. Returns the IFD
    /// offset.
    pub fn write_data(self, data: &[C::Inner]) -> TiffResult<u64> {
        let bits = C::BITS_PER_SAMPLE;
        let spp = C::SAMPLES;
        let (w, h) = (self.width, self.height);
        let full_row = row_bytes(w, spp, bits)?;
        let want = full_row / C::Inner::BYTES.max(1) * h as usize;
        if data.len() != want {
            return Err(TiffError::usage(format!(
                "write_data got {} samples, expected {want}",
                data.len()
            )));
        }
        let comp = self.compression;
        let is_fax = matches!(comp, Compression::Fax3 | Compression::Fax4);
        if is_fax && (bits != 1 || spp != 1) {
            return Err(TiffError::usage("CCITT compression needs 1-bit greyscale"));
        }
        let is_float = C::SAMPLE_FORMAT == sample_format::IEEEFP;
        match self.predictor {
            Predictor::None => {}
            Predictor::Horizontal if bits >= 8 && !is_float && !is_fax => {}
            Predictor::FloatingPoint if is_float && !is_fax => {}
            p => return Err(TiffError::usage(format!("{p:?} predictor not valid here"))),
        }
        if C::PHOTOMETRIC == photometric::PALETTE && !self.tags.contains_key(&320) {
            return Err(TiffError::usage("palette image without a colormap"));
        }
        // Byte samples are used in place; wider ones are converted once.
        let native = C::Inner::ne_bytes(data);
        let planar = self.planar && spp > 1;
        let spp_chunk = if planar { 1 } else { spp };
        let (tiled, cw, ch) = if let Some((tw, th)) = self.tiles {
            (true, tw, th)
        } else {
            let rps = self.rows_per_strip.unwrap_or_else(|| {
                let rb = row_bytes(w, spp_chunk, bits).unwrap_or(1).max(1);
                (8192 / rb).max(1) as u32
            });
            (false, w, rps.min(h))
        };
        let chunk_row = row_bytes(cw, spp_chunk, bits)?;
        let across = w.div_ceil(cw);
        let down = h.div_ceil(ch);
        let planes = if planar { u32::from(spp) } else { 1 };
        let bytes = usize::from(bits) / 8;
        let mut offsets = Vec::new();
        let mut counts = Vec::new();
        // One chunk buffer, one PackBits output and one LZW table for every
        // strip or tile, instead of a fresh allocation (and, for LZW, a
        // fresh 64 KB table fill) per chunk.
        let mut buf: Vec<u8> = Vec::new();
        let mut packed: Vec<u8> = Vec::new();
        let mut lzw_enc: Option<lzw::LzwEncoder> = None;
        let mut zlib_enc: Option<deflate::DeflateEncoder> = None;
        // The predictor (and a big-endian byte swap) rewrite the chunk, so
        // they need it in `buf`; otherwise a full-width strip can be handed
        // to the codec straight from the input.
        let rewrites = !matches!(self.predictor, Predictor::None)
            || (cfg!(target_endian = "big") && bits >= 16);
        for plane in 0..planes {
            for cy in 0..down {
                for cx in 0..across {
                    let y0 = cy * ch;
                    let x0 = cx * cw;
                    let rows = if tiled { ch } else { ch.min(h - y0) };
                    let len = chunk_row * rows as usize;
                    // A whole-width strip of byte-aligned interleaved samples
                    // is one contiguous stretch of the input.
                    let contiguous =
                        !tiled && !planar && bits >= 8 && x0 == 0 && cw == w && y0 + rows <= h;
                    let chunk: &[u8] = if contiguous && !rewrites {
                        &native[y0 as usize * full_row..][..len]
                    } else {
                        buf.clear();
                        if contiguous {
                            buf.extend_from_slice(&native[y0 as usize * full_row..][..len]);
                        } else {
                            // Tiles and planes: padding must read as zero.
                            buf.resize(len, 0);
                            let vrows = rows.min(h - y0) as usize;
                            let vcols = cw.min(w - x0) as usize;
                            for r in 0..vrows {
                                let src = &native[(y0 as usize + r) * full_row..][..full_row];
                                let dst = &mut buf[r * chunk_row..][..chunk_row];
                                if bits < 8 {
                                    let n = if planar {
                                        vcols
                                    } else {
                                        vcols * usize::from(spp)
                                    };
                                    for i in 0..n {
                                        let sbit = if planar {
                                            ((x0 as usize + i) * usize::from(spp) + plane as usize)
                                                * usize::from(bits)
                                        } else {
                                            (x0 as usize * usize::from(spp) + i) * usize::from(bits)
                                        };
                                        let v = get_bits(src, sbit, usize::from(bits));
                                        set_bits(dst, i * usize::from(bits), usize::from(bits), v);
                                    }
                                } else if planar {
                                    for i in 0..vcols {
                                        let s = ((x0 as usize + i) * usize::from(spp)
                                            + plane as usize)
                                            * bytes;
                                        dst[i * bytes..][..bytes]
                                            .copy_from_slice(&src[s..s + bytes]);
                                    }
                                } else {
                                    let s = x0 as usize * usize::from(spp) * bytes;
                                    let n = vcols * usize::from(spp) * bytes;
                                    dst[..n].copy_from_slice(&src[s..s + n]);
                                }
                            }
                        }
                        let sppc = usize::from(spp_chunk);
                        match self.predictor {
                            Predictor::Horizontal => {
                                predictor::apply_horizontal(&mut buf, chunk_row, sppc, bytes);
                                if cfg!(target_endian = "big") {
                                    predictor::swap_bytes(&mut buf, bytes);
                                }
                            }
                            Predictor::FloatingPoint => {
                                predictor::apply_float(&mut buf, chunk_row, sppc, bytes);
                            }
                            Predictor::None => {
                                if cfg!(target_endian = "big") && bits >= 16 {
                                    predictor::swap_bytes(&mut buf, bytes);
                                }
                            }
                        }
                        &buf
                    };
                    let lzw_out;
                    let other_out;
                    let coded: &[u8] = match comp {
                        Compression::None => chunk,
                        Compression::Lzw => {
                            lzw_out = lzw_enc
                                .get_or_insert_with(lzw::LzwEncoder::new)
                                .encode(chunk);
                            &lzw_out
                        }
                        Compression::Deflate(level) => {
                            other_out = zlib_enc
                                .get_or_insert_with(|| {
                                    deflate::DeflateEncoder::new(u32::from(level))
                                })
                                .encode(chunk);
                            &other_out
                        }
                        Compression::PackBits => {
                            packed.clear();
                            packed.reserve(chunk.len() + chunk.len() / 64 + 8);
                            for row in chunk.chunks(chunk_row.max(1)) {
                                packbits::encode_row(row, &mut packed);
                            }
                            &packed
                        }
                        Compression::Fax3 | Compression::Fax4 => {
                            let coding = if comp == Compression::Fax4 {
                                ccitt::Coding::Group4
                            } else {
                                ccitt::Coding::Group3_1D
                            };
                            let o = ccitt::Options::new(coding, cw).with_rows(rows);
                            other_out = ccitt::encode(chunk, &o)?;
                            &other_out
                        }
                    };
                    let off = self.enc.pos()?;
                    self.enc.check_offset(off + coded.len() as u64)?;
                    self.enc.w.write_all(coded)?;
                    offsets.push(off);
                    counts.push(coded.len() as u64);
                }
            }
        }

        let mut e = self.tags;
        let big = self.enc.bigtiff;
        let ptrs = |v: Vec<u64>| {
            if big {
                Value::Long8(v)
            } else {
                Value::Long(v.into_iter().map(|x| x as u32).collect())
            }
        };
        let dim = |x: u32| {
            if u16::try_from(x).is_ok() {
                Value::Short(vec![x as u16])
            } else {
                Value::Long(vec![x])
            }
        };
        e.insert(256, dim(w));
        e.insert(257, dim(h));
        e.insert(258, Value::Short(vec![bits; usize::from(spp)]));
        e.insert(259, Value::Short(vec![comp.code()]));
        e.insert(262, Value::Short(vec![self.photometric]));
        e.insert(277, Value::Short(vec![spp]));
        e.insert(284, Value::Short(vec![if planar { 2 } else { 1 }]));
        if tiled {
            e.insert(322, dim(cw));
            e.insert(323, dim(ch));
            e.insert(324, ptrs(offsets));
            e.insert(325, ptrs(counts));
            for t in [273u16, 278, 279] {
                e.remove(&t);
            }
        } else {
            e.insert(273, ptrs(offsets));
            e.insert(278, dim(ch));
            e.insert(279, ptrs(counts));
            for t in [322u16, 323, 324, 325] {
                e.remove(&t);
            }
        }
        match self.predictor {
            Predictor::None => {
                e.remove(&317);
            }
            Predictor::Horizontal => {
                e.insert(317, Value::Short(vec![pred::HORIZONTAL]));
            }
            Predictor::FloatingPoint => {
                e.insert(317, Value::Short(vec![pred::FLOATING_POINT]));
            }
        }
        if C::EXTRA_SAMPLES.is_empty() {
            e.remove(&338);
        } else {
            e.insert(338, Value::Short(C::EXTRA_SAMPLES.to_vec()));
        }
        if C::SAMPLE_FORMAT == sample_format::UINT {
            e.remove(&339);
        } else {
            e.insert(339, Value::Short(vec![C::SAMPLE_FORMAT; usize::from(spp)]));
        }
        if comp == Compression::Fax3 {
            e.insert(292, Value::Long(vec![0]));
        }
        self.enc.write_ifd(&e, self.detached)
    }
}
