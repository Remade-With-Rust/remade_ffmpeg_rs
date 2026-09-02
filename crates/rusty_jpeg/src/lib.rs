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
//! let bytes = std::fs::read("in.jpg").unwrap();
//! let mut decoder = rusty_jpeg::decode::Decoder::new(&bytes[..]);
//! let pixels = decoder.decode()?;
//! let info = decoder.info().unwrap();
//! # Ok(()) }
//! ```
//!
//! The decoder reads any [`decode::Source`]: with `std` that is every
//! `std::io::Read`; without it, a `&[u8]`.
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
//!
//! # Without `std`
//!
//! `--no-default-features` makes the crate `no_std` + `alloc`. The encoder
//! writes into a `Vec<u8>`, a `&mut [u8]` or an [`encode::SliceWriter`] (a
//! caller-owned buffer that reports how much it holds); the decoder reads a
//! `&[u8]`, and [`decode::Decoder::scale`] decodes a sensor-sized picture at
//! 1/2, 1/4 or 1/8 for a fraction of the work. No libm is needed: every float
//! on the coding path was replaced by exact integer arithmetic, so a host and
//! a chip produce the same bytes. Environment knobs (`RUSTY_JPEG_*`) read as
//! their defaults; a chip's configuration is a field, not a variable.

#![cfg_attr(not(feature = "std"), no_std)]

extern crate alloc;
#[cfg(test)]
extern crate std;

// ---------------------------------------------------------------------------
// `no_std` shims. The only `std` the coding path itself used was the
// environment: the trellis and NEON knobs. Without `std` there is no
// environment, so a knob reads as unset and the code takes its default —
// which is what a chip runs. Defined before the modules so the macro is in
// textual scope.
// ---------------------------------------------------------------------------

/// Read an environment knob (`RUSTY_JPEG_*`). `None` without `std`.
#[cfg(feature = "std")]
#[doc(hidden)]
pub fn knob(name: &str) -> Option<alloc::string::String> {
    std::env::var(name).ok()
}
/// Read an environment knob (`RUSTY_JPEG_*`). `None` without `std`.
#[cfg(not(feature = "std"))]
#[doc(hidden)]
pub fn knob(_name: &str) -> Option<alloc::string::String> {
    None
}

/// A knob evaluated once and cached (`OnceLock` under `std`); without `std`
/// `knob` is always `None`, so the expression folds to its default.
#[doc(hidden)]
#[macro_export]
macro_rules! cached_knob {
    ($ty:ty, $init:expr) => {{
        #[cfg(feature = "std")]
        {
            static V: ::std::sync::OnceLock<$ty> = ::std::sync::OnceLock::new();
            *V.get_or_init(|| $init)
        }
        #[cfg(not(feature = "std"))]
        {
            $init
        }
    }};
}

pub mod decode;
pub mod encode;
pub mod prof;

pub use decode::{ColorTransform, Decoder, ImageInfo, PixelFormat};
pub use encode::{
    ColorType, Encoder, EncodingError, JpegColorType, QuantizationTableType, SamplingFactor,
};
