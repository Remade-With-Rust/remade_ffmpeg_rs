//! Error type.

use std::fmt;

/// Everything that can go wrong reading or writing an EXR file.
#[derive(Debug, Clone, PartialEq, Eq)]
pub enum Error {
    /// The bytes are not a well-formed EXR file (truncated, corrupt,
    /// inconsistent header, a chunk that does not decompress to its size).
    Invalid(String),
    /// A well-formed file uses a feature this crate does not implement
    /// (deep data, an unknown compression, a future version).
    Unsupported(String),
    /// Decoding would exceed a [`crate::Limits`] bound.
    LimitExceeded(String),
    /// The caller asked for something impossible (writing an image whose
    /// channel sizes do not match its window, an out-of-range part index).
    Usage(String),
}

impl Error {
    pub(crate) fn invalid(msg: impl Into<String>) -> Self {
        Self::Invalid(msg.into())
    }
    pub(crate) fn unsupported(msg: impl Into<String>) -> Self {
        Self::Unsupported(msg.into())
    }
    pub(crate) fn limit(msg: impl Into<String>) -> Self {
        Self::LimitExceeded(msg.into())
    }
    pub(crate) fn usage(msg: impl Into<String>) -> Self {
        Self::Usage(msg.into())
    }
}

impl fmt::Display for Error {
    fn fmt(&self, f: &mut fmt::Formatter<'_>) -> fmt::Result {
        match self {
            Self::Invalid(m) => write!(f, "invalid EXR data: {m}"),
            Self::Unsupported(m) => write!(f, "unsupported EXR feature: {m}"),
            Self::LimitExceeded(m) => write!(f, "EXR decode limit exceeded: {m}"),
            Self::Usage(m) => write!(f, "invalid EXR request: {m}"),
        }
    }
}

impl std::error::Error for Error {}

/// Result alias used throughout the crate.
pub type Result<T> = std::result::Result<T, Error>;

/// Bounds on what a decode may allocate or do. Every size read from a file
/// is checked against these before anything is allocated.
#[derive(Debug, Clone, Copy, PartialEq, Eq)]
pub struct Limits {
    /// Maximum data-window area (pixels) of one part or level.
    pub max_pixels: u64,
    /// Maximum number of channels in one part.
    pub max_channels: usize,
    /// Maximum decoded bytes for one whole part (all channels).
    pub max_image_bytes: u64,
    /// Maximum decompressed bytes of one chunk (one scanline block or tile).
    pub max_chunk_bytes: u64,
    /// Maximum size in bytes of one header attribute value.
    pub max_attribute_bytes: u64,
    /// Maximum number of parts in a multi-part file.
    pub max_parts: usize,
    /// Maximum number of chunks (offset-table entries) in one part.
    pub max_chunks: u64,
}

impl Default for Limits {
    /// 2^28 pixels (16384 x 16384), 1024 channels, 4 GiB per part, 512 MiB
    /// per chunk, 64 MiB per attribute (a 4K preview is 32 MiB), 1024 parts,
    /// 2^24 chunks per part.
    fn default() -> Self {
        Self {
            max_pixels: 1 << 28,
            max_channels: 1024,
            max_image_bytes: 4 << 30,
            max_chunk_bytes: 512 << 20,
            max_attribute_bytes: 64 << 20,
            max_parts: 1024,
            max_chunks: 1 << 24,
        }
    }
}

impl Limits {
    /// No practical bounds (for trusted input).
    #[must_use]
    pub fn unlimited() -> Self {
        Self {
            max_pixels: u64::MAX,
            max_channels: usize::MAX,
            max_image_bytes: u64::MAX,
            max_chunk_bytes: u64::MAX,
            max_attribute_bytes: u64::MAX,
            max_parts: usize::MAX,
            max_chunks: u64::MAX,
        }
    }
}
