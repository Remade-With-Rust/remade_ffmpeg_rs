//! `rff-core` — shared primitives for Remade FFmpeg (Rust).
//!
//! This is the equivalent of FFmpeg's `libavutil`: the small, dependency-free
//! vocabulary that every other crate speaks. Codecs, formats, the engine, the
//! CLI and the server all build on the types defined here.
//!
//! Nothing in this crate performs encoding or decoding — it only defines the
//! data that flows between those stages (`Packet`s of compressed bytes,
//! `Frame`s of raw samples) and the way errors are reported (`Error`).

pub mod dict;
pub mod error;
pub mod frame;
pub mod media;
pub mod packet;
pub mod rational;

pub use dict::Dictionary;
pub use error::{Error, Result};
pub use frame::{AudioFrame, Frame, VideoFrame};
pub use media::{CodecId, MediaType, PixelFormat, SampleFormat};
pub use packet::{Packet, PacketFlags};
pub use rational::Rational;
