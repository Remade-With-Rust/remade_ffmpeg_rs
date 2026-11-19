//! Error type shared by the decoder, the encoder and the codecs.

use std::fmt;
use std::io;

use crate::tags::Tag;

/// Everything that can go wrong reading or writing a TIFF.
///
/// Malformed input always surfaces as [`TiffError::Format`] (or
/// [`TiffError::Io`] for an unexpected end of file); it never panics.
#[derive(Debug)]
#[non_exhaustive]
pub enum TiffError {
    /// An I/O error from the underlying reader or writer.
    Io(io::Error),
    /// The file is malformed: a bad header, an IFD loop, an offset past the
    /// end of the file, corrupt compressed data, inconsistent tags...
    Format(String),
    /// The file is well formed but uses a feature this crate does not decode
    /// (or the encoder was asked for a combination it does not write).
    Unsupported(String),
    /// Decoding would exceed a configured [`crate::Limits`] value.
    Limits(String),
    /// A required tag is absent.
    TagNotFound(Tag),
    /// The API was used incorrectly (wrong buffer length, missing colormap...).
    Usage(String),
}

/// `Result` alias used throughout the crate.
pub type TiffResult<T> = Result<T, TiffError>;

impl TiffError {
    pub(crate) fn format(msg: impl Into<String>) -> Self {
        Self::Format(msg.into())
    }
    pub(crate) fn unsupported(msg: impl Into<String>) -> Self {
        Self::Unsupported(msg.into())
    }
    pub(crate) fn limits(msg: impl Into<String>) -> Self {
        Self::Limits(msg.into())
    }
    pub(crate) fn usage(msg: impl Into<String>) -> Self {
        Self::Usage(msg.into())
    }
}

impl fmt::Display for TiffError {
    fn fmt(&self, f: &mut fmt::Formatter<'_>) -> fmt::Result {
        match self {
            Self::Io(e) => write!(f, "TIFF I/O error: {e}"),
            Self::Format(m) => write!(f, "malformed TIFF: {m}"),
            Self::Unsupported(m) => write!(f, "unsupported TIFF feature: {m}"),
            Self::Limits(m) => write!(f, "TIFF limit exceeded: {m}"),
            Self::TagNotFound(t) => write!(f, "required TIFF tag {t:?} not found"),
            Self::Usage(m) => write!(f, "TIFF API misuse: {m}"),
        }
    }
}

impl std::error::Error for TiffError {
    fn source(&self) -> Option<&(dyn std::error::Error + 'static)> {
        match self {
            Self::Io(e) => Some(e),
            _ => None,
        }
    }
}

impl From<io::Error> for TiffError {
    fn from(e: io::Error) -> Self {
        Self::Io(e)
    }
}

impl From<crate::ccitt::CcittError> for TiffError {
    fn from(e: crate::ccitt::CcittError) -> Self {
        Self::Format(format!("CCITT: {e}"))
    }
}
