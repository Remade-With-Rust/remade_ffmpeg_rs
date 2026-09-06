//! The crate's error type — dependency-free, hand-rolled `Display`.
//!
//! `Again` / `Eof` mirror FFmpeg's `EAGAIN` / `AVERROR_EOF` control-flow
//! convention (the same contract as `rusty_vp9`): they drive the push/pull
//! loop and are not failures.

/// Crate-wide result alias.
pub type Result<T> = std::result::Result<T, Error>;

/// Errors (and control-flow signals) produced by the decoder.
#[non_exhaustive]
#[derive(Debug, Clone, PartialEq, Eq)]
pub enum Error {
    /// A code path that is scaffolded but not yet implemented.
    Unimplemented(&'static str),
    /// End of stream: all output has been drained.
    Eof,
    /// More input is required before output can be produced.
    Again,
    /// The bitstream is malformed or internally inconsistent.
    InvalidData(String),
    /// The input is valid but uses a feature outside this decoder's scope
    /// (RExt / SCC / multi-layer profiles are parsed and refused by name).
    Unsupported(String),
}

impl Error {
    pub fn invalid(msg: impl Into<String>) -> Self {
        Error::InvalidData(msg.into())
    }
    pub fn unsupported(msg: impl Into<String>) -> Self {
        Error::Unsupported(msg.into())
    }
}

impl std::fmt::Display for Error {
    fn fmt(&self, f: &mut std::fmt::Formatter<'_>) -> std::fmt::Result {
        match self {
            Error::Unimplemented(what) => write!(f, "not yet implemented: {what}"),
            Error::Eof => write!(f, "end of stream"),
            Error::Again => write!(f, "more input required"),
            Error::InvalidData(msg) => write!(f, "invalid data: {msg}"),
            Error::Unsupported(msg) => write!(f, "unsupported: {msg}"),
        }
    }
}

impl std::error::Error for Error {}

impl From<crate::bits::OutOfData> for Error {
    fn from(_: crate::bits::OutOfData) -> Self {
        Error::InvalidData("bit reader ran out of data".into())
    }
}
