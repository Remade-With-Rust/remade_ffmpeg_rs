//! Pure-Rust codecs for the small image formats, and header-only probing.
//!
//! | format | decode | encode |
//! |---|---|---|
//! | BMP (1/2/4/8/16/24/32-bit, RLE4/RLE8, `BI_BITFIELDS`, core/V2-V5 headers) | ✓, plus headerless DIBs ([`bmp::decode_dib`]: `CF_DIB` / `CF_DIBV5`) | ✓, plus [`bmp::encode_dib`] |
//! | TGA (true-colour, grey, colour-mapped, RLE, all four origins) | ✓ | ✓ |
//! | Radiance HDR (RGBE, new- and old-style RLE, all eight orientations) | ✓ | ✓ |
//! | ICO / CUR (BMP and PNG entries, AND masks, hotspots) | ✓, plus every entry ([`ico::decode_entries`]) | ✓ |
//! | PNM P1-P6 and PAM (P7), 8 and 16-bit | ✓ | ✓ |
//! | QOI | ✓ | ✓ |
//!
//! [`probe`] reads the format and dimensions from the first bytes of all of
//! the above plus PNG, JPEG, GIF, WebP, TIFF and AVIF.
//!
//! Decoded pixels match the `image` crate 0.25 byte for byte, including its
//! choice of pixel layout for each file (so a 32-bit `BI_RGB` BMP decodes to
//! [`PixelFormat::Rgb8`], as there), which lets callers swap one for the other.
//! 16-bit and float samples are stored in native byte order, as `image`'s
//! `as_bytes()` gives them.
//!
//! Every decoder works on a byte slice and bounds-checks every read: damaged
//! or hostile input returns an [`Error`], never a panic, and the decoded size
//! is checked before anything is allocated.
//!
//! ```
//! let img = rusty_image::Image::new(2, 1, rusty_image::PixelFormat::Rgb8, vec![255, 0, 0, 0, 0, 255]).unwrap();
//! let bmp = rusty_image::encode(&img, rusty_image::Format::Bmp).unwrap();
//! assert_eq!(rusty_image::probe(&bmp), Some((rusty_image::Format::Bmp, 2, 1)));
//! assert_eq!(rusty_image::decode(&bmp).unwrap(), img);
//! ```

#![forbid(unsafe_code)]
#![warn(missing_docs)]

pub mod bmp;
pub mod hdr;
pub mod ico;
pub mod pnm;
mod probe;
pub mod qoi;
mod read;
mod swizzle;
pub mod tga;

pub use probe::probe;

use std::fmt;

/// The largest decoded image accepted, in bytes (1 GiB). A header that claims
/// more is refused before anything is allocated.
pub const MAX_DECODED_BYTES: usize = 1 << 30;

/// A file format.
#[derive(Clone, Copy, Debug, PartialEq, Eq, Hash)]
pub enum Format {
    /// Windows bitmap.
    Bmp,
    /// Truevision TGA.
    Tga,
    /// Radiance RGBE.
    Hdr,
    /// Windows icon.
    Ico,
    /// Windows cursor (an ICO with hotspots).
    Cur,
    /// Netpbm: PBM, PGM, PPM and PAM.
    Pnm,
    /// Quite OK Image format.
    Qoi,
    /// PNG (probe only).
    Png,
    /// JPEG (probe only).
    Jpeg,
    /// GIF (probe only).
    Gif,
    /// WebP (probe only).
    WebP,
    /// TIFF (probe only).
    Tiff,
    /// AVIF (probe only).
    Avif,
}

/// The layout of [`Image::pixels`]. Multi-byte samples are in native byte
/// order.
#[derive(Clone, Copy, Debug, PartialEq, Eq, Hash)]
pub enum PixelFormat {
    /// 8-bit grey.
    L8,
    /// 8-bit grey and alpha.
    La8,
    /// 8-bit RGB.
    Rgb8,
    /// 8-bit RGBA (straight alpha).
    Rgba8,
    /// 16-bit grey.
    L16,
    /// 16-bit grey and alpha.
    La16,
    /// 16-bit RGB.
    Rgb16,
    /// 16-bit RGBA.
    Rgba16,
    /// 32-bit float RGB.
    Rgb32F,
    /// 32-bit float RGBA.
    Rgba32F,
}

impl PixelFormat {
    /// Channels per pixel.
    pub fn channels(self) -> usize {
        match self {
            PixelFormat::L8 | PixelFormat::L16 => 1,
            PixelFormat::La8 | PixelFormat::La16 => 2,
            PixelFormat::Rgb8 | PixelFormat::Rgb16 | PixelFormat::Rgb32F => 3,
            PixelFormat::Rgba8 | PixelFormat::Rgba16 | PixelFormat::Rgba32F => 4,
        }
    }

    /// Bytes per sample.
    pub fn bytes_per_sample(self) -> usize {
        match self {
            PixelFormat::L8 | PixelFormat::La8 | PixelFormat::Rgb8 | PixelFormat::Rgba8 => 1,
            PixelFormat::L16 | PixelFormat::La16 | PixelFormat::Rgb16 | PixelFormat::Rgba16 => 2,
            PixelFormat::Rgb32F | PixelFormat::Rgba32F => 4,
        }
    }

    /// Bytes per pixel.
    pub fn bytes_per_pixel(self) -> usize {
        self.channels() * self.bytes_per_sample()
    }

    /// Whether the last channel is alpha.
    pub fn has_alpha(self) -> bool {
        matches!(
            self,
            PixelFormat::La8
                | PixelFormat::Rgba8
                | PixelFormat::La16
                | PixelFormat::Rgba16
                | PixelFormat::Rgba32F
        )
    }
}

/// A decoded image: rows top to bottom, pixels left to right, no padding.
#[derive(Clone, Debug, PartialEq)]
pub struct Image {
    /// Width in pixels.
    pub width: u32,
    /// Height in pixels.
    pub height: u32,
    /// Pixel layout.
    pub format: PixelFormat,
    /// `width * height * format.bytes_per_pixel()` bytes.
    pub pixels: Vec<u8>,
}

impl Image {
    /// An image from its parts; the buffer must be exactly the right size.
    pub fn new(width: u32, height: u32, format: PixelFormat, pixels: Vec<u8>) -> Result<Image> {
        let want = byte_len(width, height, format)?;
        if pixels.len() != want {
            return Err(Error::invalid(format!(
                "pixel buffer is {} bytes, {width}x{height} {format:?} needs {want}",
                pixels.len()
            )));
        }
        Ok(Image {
            width,
            height,
            format,
            pixels,
        })
    }

    /// A zero-filled image, size-checked against [`MAX_DECODED_BYTES`].
    pub(crate) fn zeroed(width: u32, height: u32, format: PixelFormat) -> Result<Image> {
        let len = byte_len(width, height, format)?;
        Ok(Image {
            width,
            height,
            format,
            pixels: vec![0; len],
        })
    }

    /// The samples of a 16-bit image.
    pub fn as_u16(&self) -> Option<Vec<u16>> {
        (self.format.bytes_per_sample() == 2).then(|| {
            self.pixels
                .chunks_exact(2)
                .map(|c| u16::from_ne_bytes([c[0], c[1]]))
                .collect()
        })
    }

    /// The samples of a float image.
    pub fn as_f32(&self) -> Option<Vec<f32>> {
        (self.format.bytes_per_sample() == 4).then(|| {
            self.pixels
                .chunks_exact(4)
                .map(|c| f32::from_ne_bytes([c[0], c[1], c[2], c[3]]))
                .collect()
        })
    }
}

/// The byte size of a `width` x `height` image, refusing sizes past
/// [`MAX_DECODED_BYTES`].
pub(crate) fn byte_len(width: u32, height: u32, format: PixelFormat) -> Result<usize> {
    (width as usize)
        .checked_mul(height as usize)
        .and_then(|n| n.checked_mul(format.bytes_per_pixel()))
        .filter(|&n| n <= MAX_DECODED_BYTES)
        .ok_or_else(|| {
            Error::Unsupported(format!(
                "{width}x{height} {format:?} is larger than {MAX_DECODED_BYTES} bytes"
            ))
        })
}

/// Errors.
#[derive(Clone, Debug, PartialEq, Eq)]
pub enum Error {
    /// The data ended before the image did.
    Truncated,
    /// The data is not a valid file of its format.
    Invalid(String),
    /// A valid file using something this crate does not implement.
    Unsupported(String),
    /// The format could not be recognised.
    UnknownFormat,
}

impl Error {
    pub(crate) fn invalid(msg: impl Into<String>) -> Error {
        Error::Invalid(msg.into())
    }
    pub(crate) fn unsupported(msg: impl Into<String>) -> Error {
        Error::Unsupported(msg.into())
    }
}

impl fmt::Display for Error {
    fn fmt(&self, f: &mut fmt::Formatter<'_>) -> fmt::Result {
        match self {
            Error::Truncated => f.write_str("truncated image data"),
            Error::Invalid(m) => write!(f, "invalid image: {m}"),
            Error::Unsupported(m) => write!(f, "unsupported image: {m}"),
            Error::UnknownFormat => f.write_str("unrecognised image format"),
        }
    }
}

impl std::error::Error for Error {}

/// This crate's result type.
pub type Result<T> = std::result::Result<T, Error>;

/// Decode an image, recognising its format from the data.
pub fn decode(data: &[u8]) -> Result<Image> {
    let format = probe::sniff(data).ok_or(Error::UnknownFormat)?;
    decode_as(data, format)
}

/// Decode an image of a known format (TGA has no signature, so a TGA that
/// [`decode`] does not recognise can be read this way).
pub fn decode_as(data: &[u8], format: Format) -> Result<Image> {
    match format {
        Format::Bmp => bmp::decode(data),
        Format::Tga => tga::decode(data),
        Format::Hdr => hdr::decode(data),
        Format::Ico | Format::Cur => ico::decode(data),
        Format::Pnm => pnm::decode(data),
        Format::Qoi => qoi::decode(data),
        other => Err(Error::unsupported(format!(
            "{other:?} decoding lives in its own crate (rusty_png, rusty_jpeg, rusty_gif, rusty_webp, rusty_tiff)"
        ))),
    }
}

/// Encode an image.
///
/// Each format takes the pixel layouts it can store without loss and refuses
/// the rest with [`Error::Unsupported`] rather than converting silently:
///
/// | format | layouts |
/// |---|---|
/// | BMP | `L8` (8-bit grey palette), `Rgb8` (24-bit), `Rgba8` (32-bit `BI_BITFIELDS` with alpha) |
/// | TGA | `L8`, `La8`, `Rgb8`, `Rgba8` (RLE) |
/// | HDR | `Rgb32F` (RGBE is lossy by design) |
/// | ICO / CUR | `Rgba8`, `Rgb8` (one entry; [`ico::encode`] writes several) |
/// | PNM | every 8- and 16-bit layout (PGM, PPM or PAM) |
/// | QOI | `Rgb8`, `Rgba8` |
pub fn encode(image: &Image, format: Format) -> Result<Vec<u8>> {
    match format {
        Format::Bmp => bmp::encode(image),
        Format::Tga => tga::encode(image),
        Format::Hdr => hdr::encode(image),
        Format::Ico => ico::encode(std::slice::from_ref(image)),
        Format::Cur => ico::encode_cur(&[(image.clone(), 0, 0)]),
        Format::Pnm => pnm::encode(image),
        Format::Qoi => qoi::encode(image),
        other => Err(Error::unsupported(format!(
            "{other:?} encoding lives in its own crate"
        ))),
    }
}
