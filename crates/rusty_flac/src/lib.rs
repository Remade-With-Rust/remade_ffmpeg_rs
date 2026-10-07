#![cfg_attr(not(feature = "std"), no_std)]
//! # rusty_flac — pure-Rust FLAC codec
//!
//! A self-contained FLAC **encoder + decoder**: no FFI, no required dependencies.
//!
//! - [`Encoder`]: feed planar or interleaved `i32` samples (8/16/24-bit, up to
//!   8 channels), get a complete spec-valid FLAC stream with STREAMINFO MD5.
//! - [`Decoder`]: streaming per-frame decode of a FLAC stream to planar `i32`,
//!   with CRC verification; [`decode`] is the whole-stream convenience.
//!
//! Lossless is the invariant: `decode(encode(x)) == x` exactly, and every
//! stream interoperates with libFLAC/ffmpeg in both directions.
//!
//! ## `no_std`
//!
//! The crate is `no_std` + `alloc` with `default-features = false` plus the
//! `libm` feature (pure-Rust math for the window design, LPC quantisation and
//! the Rice estimate). The `std` feature adds per-thread scratch reuse,
//! runtime AVX2 detection and the `RUSTY_FLAC_TIMING` stage print; the
//! output bytes do not depend on it — only on whether `libm` is in use.

extern crate alloc;
#[cfg(test)]
extern crate std;

#[cfg(all(not(feature = "std"), not(feature = "libm")))]
compile_error!(
    "rusty_flac without `std` needs the `libm` feature for its floating-point math: \
     `rusty_flac = { version = \"0.1\", default-features = false, features = [\"libm\"] }`"
);

mod bitio;
mod crc;
mod decode;
mod encode;
mod math;
mod md5;
mod window_table;

pub use decode::{decode, DecodeError, Decoder, StreamInfo};
pub use encode::{EncodeError, EncodeStats, Encoder};
