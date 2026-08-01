//! Pure-Rust JPEG / MJPEG decoder and encoder — no C, no FFI.
//!
//! This crate vendors two upstream pure-Rust implementations and carries them
//! forward as one codec (see `NOTICE.md` for full attribution):
//!
//! - [`decode`] — from `jpeg-decoder` 0.3.2 (image-rs), MIT OR Apache-2.0.
//! - [`encode`] — from `jpeg-encoder` 0.7.0, (MIT OR Apache-2.0) AND IJG.
//!
//! They were merged so the two halves can share primitives — quantization
//! tables, zig-zag order, the profiler — and so the encoder can be held to the
//! decoder as a round-trip oracle, which is the standing correctness gate for
//! every change made here.
//!
//! # Decode
//! ```no_run
//! # fn main() -> Result<(), rusty_jpeg::decode::Error> {
//! use std::io::Cursor;
//! let bytes = std::fs::read("in.jpg").unwrap();
//! let mut decoder = rusty_jpeg::decode::Decoder::new(Cursor::new(bytes));
//! let pixels = decoder.decode()?;
//! let info = decoder.info().unwrap();
//! # Ok(()) }
//! ```
//!
//! # Encode
//! ```no_run
//! # fn main() -> Result<(), rusty_jpeg::encode::EncodingError> {
//! use rusty_jpeg::encode::{ColorType, Encoder};
//! let mut out = Vec::new();
//! let encoder = Encoder::new(&mut out, 90);
//! encoder.encode(&[255, 0, 0, 0, 255, 0, 0, 0, 255, 255, 255, 255], 2, 2, ColorType::Rgb)?;
//! # Ok(()) }
//! ```

extern crate alloc;

pub mod decode;
pub mod encode;
pub mod prof;

pub use decode::{ColorTransform, Decoder, ImageInfo, PixelFormat};
pub use encode::{
    ColorType, Encoder, EncodingError, JpegColorType, QuantizationTableType, SamplingFactor,
};
