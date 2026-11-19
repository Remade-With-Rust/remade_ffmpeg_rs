//! Pure-Rust TIFF and BigTIFF decoding and encoding.
//!
//! - **Decode**: classic TIFF and BigTIFF, both byte orders; IFD chains with
//!   loop detection; SubIFDs (DNG) and EXIF/GPS IFDs; strips and tiles;
//!   planar configuration 1 and 2; 1/2/4/8/16/32/64-bit unsigned, 8-64-bit
//!   signed, 16/32/64-bit float samples; compression none, LZW (including
//!   the pre-TIFF-6 "old-style" bit order), Deflate (8 and 32946),
//!   PackBits, CCITT modified Huffman / Group 3 (1-D and 2-D) / Group 4, and
//!   JPEG (compression 7 with shared `JPEGTables`, via `rusty_jpeg`);
//!   predictors 2 and 3; palette expansion and min-is-white inversion.
//! - **Tags**: every tag readable as a typed [`Value`] or raw bytes; helpers
//!   for ICC (34675), XMP (700), the EXIF IFD (34665) and SubIFDs (330).
//! - **Encode**: [`TiffEncoder`] writes classic or BigTIFF, strips or tiles,
//!   chunky or planar, none/LZW/Deflate/PackBits (and CCITT G3/G4 for 1-bit),
//!   predictors 2 and 3, ICC and arbitrary tags, any number of pages.
//! - [`ccitt`]: the fax codec as a standalone module (also for PDF
//!   `CCITTFaxDecode`).
//!
//! Malformed input returns an error; it does not panic, loop forever or
//! allocate beyond [`Limits`] (or the file's own length for metadata).
//!
//! ```
//! use rusty_tiff::encoder::{colortype, Compression, TiffEncoder};
//! use rusty_tiff::{ColorType, Decoder, DecodingResult};
//!
//! let mut file = std::io::Cursor::new(Vec::new());
//! let pixels: Vec<u16> = (0..64 * 32 * 3).map(|i| i as u16).collect();
//! TiffEncoder::new(&mut file)?
//!     .with_compression(Compression::deflate())
//!     .write_image::<colortype::RGB16>(64, 32, &pixels)?;
//!
//! file.set_position(0);
//! let mut dec = Decoder::new(file)?;
//! assert_eq!(dec.dimensions()?, (64, 32));
//! assert_eq!(dec.colortype()?, ColorType::RGB(16));
//! assert_eq!(dec.read_image()?, DecodingResult::U16(pixels));
//! # Ok::<_, rusty_tiff::TiffError>(())
//! ```
#![forbid(unsafe_code)]
#![warn(missing_docs)]

pub mod ccitt;
mod decoder;
mod deflate;
pub mod encoder;
mod error;
mod ifd;
#[cfg(feature = "jpeg")]
mod jpeg;
mod lzw;
mod packbits;
mod predictor;
pub mod tags;
mod value;

pub use decoder::{f16_to_f32, ColorType, Decoder, DecodingResult, ImageInfo, Limits, SampleKind};
pub use encoder::TiffEncoder;
pub use error::{TiffError, TiffResult};
pub use ifd::{ByteOrder, Entry, Ifd};
pub use tags::{Tag, Type};
pub use value::Value;
