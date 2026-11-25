//! Pure-Rust JBIG2 (ITU-T T.88 | ISO/IEC 14492) decoder.
//!
//! - **Regions**: generic (arithmetic templates 0 to 3 with adaptive pixels,
//!   the 12-AT-pixel extended template, typical prediction; MMR), generic
//!   refinement (both templates, adaptive pixels, typical prediction), text
//!   (arithmetic and Huffman, all corners, transposition, refinement) and
//!   halftone (arithmetic and MMR, skipping, rotated grids).
//! - **Dictionaries**: symbol (arithmetic, Huffman, refinement/aggregate
//!   coding, retained contexts, export flags), pattern, and custom Huffman
//!   tables.
//! - **Pages**: all combination operators, default pixel, intermediate
//!   regions, stripes, unknown height, regions of unknown length.
//! - **Organisations**: embedded segments with separate globals (PDF
//!   `JBIG2Decode`), and sequential and random-access files.
//!
//! Not implemented: the colour extensions added to T.88 later
//! ([`Error::Unsupported`]).
//!
//! # Polarity
//!
//! A 1 bit is **black**, as T.88 has it. PDF's `JBIG2Decode` filter produces
//! the inverse; use [`Bitmap1::into_pdf_samples`].
//!
//! # Untrusted input
//!
//! Malformed input returns an [`Error`]. Nothing panics, and neither memory
//! nor time exceeds what [`Limits`] allows, whatever sizes the data declares.
//!
//! ```
//! // A 16x1 page with one generic region (MMR): 8 white, 8 black pixels.
//! let page = [
//!     0, 0, 0, 0, 48, 0, 1, 0, 0, 0, 19, // page information, page 1
//!     0, 0, 0, 16, 0, 0, 0, 1, 0, 0, 0, 0, 0, 0, 0, 0, 0, 0, 0, //
//!     0, 0, 0, 1, 38, 0, 1, 0, 0, 0, 20, // immediate generic region
//!     0, 0, 0, 16, 0, 0, 0, 1, 0, 0, 0, 0, 0, 0, 0, 0, 0, 1, //
//!     0b0011_0011, 0b0001_0100, // horizontal mode, white 8, black 8
//! ];
//! let bitmap = rusty_jbig2::decode_jbig2(&page, None)?;
//! assert_eq!((bitmap.width, bitmap.height), (16, 1));
//! assert_eq!(bitmap.data, [0x00, 0xFF]); // 1 = black
//! assert_eq!(bitmap.into_pdf_samples(), [0xFF, 0x00]); // 0 = black
//! # Ok::<(), rusty_jbig2::Error>(())
//! ```
#![deny(unsafe_code)]
#![warn(missing_docs)]

mod arith;
mod bitmap;
mod budget;
mod error;
mod generic;
mod halftone;
mod huffman;
mod kernels;
mod mmr;
mod page;
mod refinement;
mod segment;
mod symbol;
mod text;

pub use bitmap::Bitmap1;
pub use error::{Error, Result};

use segment::{kind, Segment};

/// Bounds on what one decode call may use. They are checked against declared
/// sizes before anything is allocated.
#[derive(Clone, Debug, PartialEq, Eq)]
#[non_exhaustive]
pub struct Limits {
    /// Pixels in any one bitmap, its rows rounded up to whole bytes: the page,
    /// a region, a symbol, a halftone grid. Default 2^28 (32 MiB packed).
    pub max_pixels: u64,
    /// Symbols visible to one segment, or patterns in one dictionary.
    /// Default 2^20.
    pub max_symbols: u32,
    /// Total work of one call, roughly one unit per pixel decoded or byte
    /// drawn. It bounds memory too: decoded pixels are packed, so all bitmaps
    /// of a call together take at most a byte per 8 units plus a small
    /// constant each, which is charged for. Default 2^29, a few seconds.
    pub max_work: u64,
}

impl Default for Limits {
    fn default() -> Self {
        Self {
            max_pixels: 1 << 28,
            max_symbols: 1 << 20,
            max_work: 1 << 29,
        }
    }
}

impl Limits {
    /// Set [`Limits::max_pixels`].
    #[must_use]
    pub fn with_max_pixels(mut self, v: u64) -> Self {
        self.max_pixels = v;
        self
    }

    /// Set [`Limits::max_symbols`].
    #[must_use]
    pub fn with_max_symbols(mut self, v: u32) -> Self {
        self.max_symbols = v;
        self
    }

    /// Set [`Limits::max_work`].
    #[must_use]
    pub fn with_max_work(mut self, v: u64) -> Self {
        self.max_work = v;
        self
    }
}

/// Decoder options.
#[derive(Clone, Debug, Default, PartialEq, Eq)]
#[non_exhaustive]
pub struct DecodeOptions {
    /// Resource bounds.
    pub limits: Limits,
    /// Keep going after errors: segments that cannot be decoded are left out,
    /// a truncated stream gives the page as far as it goes, and out-of-range
    /// gray values are clamped. Off by default: any damage is an error.
    /// Exceeding a limit is an error either way.
    pub lenient: bool,
}

impl DecodeOptions {
    /// The default limits, strict.
    #[must_use]
    pub fn new() -> Self {
        Self::default()
    }

    /// Set the limits.
    #[must_use]
    pub fn with_limits(mut self, limits: Limits) -> Self {
        self.limits = limits;
        self
    }

    /// Set [`DecodeOptions::lenient`].
    #[must_use]
    pub fn with_lenient(mut self, lenient: bool) -> Self {
        self.lenient = lenient;
        self
    }
}

/// Decode an embedded JBIG2 page: the data of a PDF `JBIG2Decode` stream and
/// its `JBIG2Globals` stream, if any. Default limits, strict.
///
/// `page` may also be a standalone file, whose first page is decoded.
pub fn decode_jbig2(page: &[u8], globals: Option<&[u8]>) -> Result<Bitmap1> {
    decode_jbig2_with(page, globals, &DecodeOptions::default())
}

/// [`decode_jbig2`] with options.
pub fn decode_jbig2_with(
    page: &[u8],
    globals: Option<&[u8]>,
    opts: &DecodeOptions,
) -> Result<Bitmap1> {
    if page.starts_with(&segment::FILE_ID) {
        return Jbig2File::parse_with(page, opts.clone())?.decode_page(0);
    }
    let (g, ge) = segment::sequential(globals.unwrap_or(&[]), opts.lenient);
    let (p, pe) = segment::sequential(page, opts.lenient);
    page::decode(g.iter().chain(&p), None, ge.or(pe), false, opts)
}

/// A standalone JBIG2 file (sequential or random-access organisation).
///
/// Segments are indexed by [`Jbig2File::parse`]; each page is decoded on
/// demand.
#[derive(Debug)]
pub struct Jbig2File<'a> {
    segments: Vec<Segment<'a>>,
    pages: Vec<u32>,
    error: Option<Error>,
    opts: DecodeOptions,
}

impl<'a> Jbig2File<'a> {
    /// Index a file. Default limits, strict.
    pub fn parse(data: &'a [u8]) -> Result<Self> {
        Self::parse_with(data, DecodeOptions::default())
    }

    /// [`Jbig2File::parse`] with options.
    pub fn parse_with(data: &'a [u8], opts: DecodeOptions) -> Result<Self> {
        let rest = data
            .strip_prefix(&segment::FILE_ID)
            .ok_or(Error::Invalid("not a JBIG2 file"))?;
        let (&flags, rest) = rest.split_first().ok_or(Error::Truncated)?;
        let (rest, count) = if flags & 2 == 0 {
            let (n, rest) = rest.split_first_chunk::<4>().ok_or(Error::Truncated)?;
            (rest, Some(u32::from_be_bytes(*n)))
        } else {
            (rest, None)
        };
        let (segments, error) = if flags & 1 != 0 {
            segment::sequential(rest, opts.lenient)
        } else {
            segment::random_access(rest, opts.lenient)
        };
        if let (Some(e), false) = (error, opts.lenient) {
            return Err(e);
        }
        let pages: Vec<u32> = segments
            .iter()
            .filter(|s| s.kind == kind::PAGE_INFO)
            .map(|s| s.page)
            .collect();
        if !opts.lenient && count.is_some_and(|n| n as usize != pages.len()) {
            return Err(Error::Invalid("page count mismatch"));
        }
        Ok(Self {
            segments,
            pages,
            error,
            opts,
        })
    }

    /// Number of pages.
    #[must_use]
    pub fn page_count(&self) -> usize {
        self.pages.len()
    }

    /// Decode page `index` (from 0).
    pub fn decode_page(&self, index: usize) -> Result<Bitmap1> {
        let number = *self
            .pages
            .get(index)
            .ok_or(self.error.unwrap_or(Error::Invalid("no such page")))?;
        let segments = self.segments.iter();
        page::decode(segments, Some(number), self.error, true, &self.opts)
    }
}
