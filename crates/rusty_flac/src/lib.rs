//! # rusty_flac — pure-Rust FLAC codec
//!
//! A self-contained FLAC **encoder + decoder**: no FFI, no dependencies.
//!
//! - [`Encoder`]: feed planar or interleaved `i32` samples (8/16/24-bit, up to
//!   8 channels), get a complete spec-valid FLAC stream with STREAMINFO MD5.
//! - [`Decoder`]: streaming per-frame decode of a FLAC stream to planar `i32`,
//!   with CRC verification; [`decode`] is the whole-stream convenience.
//!
//! Lossless is the invariant: `decode(encode(x)) == x` exactly, and every
//! stream interoperates with libFLAC/ffmpeg in both directions.

mod bitio;
mod crc;
mod decode;
mod encode;
mod md5;

pub use decode::{decode, DecodeError, Decoder, StreamInfo};
pub use encode::{EncodeError, EncodeStats, Encoder};
