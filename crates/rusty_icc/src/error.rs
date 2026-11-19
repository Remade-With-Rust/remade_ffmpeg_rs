//! Error type.

use core::fmt;

/// Everything that can go wrong parsing a profile or building a transform.
#[derive(Debug, Clone, PartialEq, Eq)]
#[non_exhaustive]
pub enum Error {
    /// The bytes are not a well-formed ICC profile. The message names the
    /// structure that failed its bounds or consistency check.
    Malformed(&'static str),
    /// The profile is well formed but uses something this crate does not
    /// implement (a colour space, tag type or profile class).
    Unsupported(&'static str),
    /// A transform was requested between incompatible profiles or layouts.
    InvalidTransform(&'static str),
    /// A pixel buffer length does not match the transform's layouts.
    BufferSize {
        /// Samples in the source buffer.
        src: usize,
        /// Samples in the destination buffer.
        dst: usize,
    },
}

impl fmt::Display for Error {
    fn fmt(&self, f: &mut fmt::Formatter<'_>) -> fmt::Result {
        match self {
            Self::Malformed(m) => write!(f, "malformed ICC profile: {m}"),
            Self::Unsupported(m) => write!(f, "unsupported ICC feature: {m}"),
            Self::InvalidTransform(m) => write!(f, "invalid transform: {m}"),
            Self::BufferSize { src, dst } => {
                write!(
                    f,
                    "buffer sizes do not match layouts (src {src}, dst {dst})"
                )
            }
        }
    }
}

impl std::error::Error for Error {}

/// `Result` with this crate's [`Error`].
pub type Result<T> = core::result::Result<T, Error>;
