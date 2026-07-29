//! The crate's error type — dependency-free, hand-rolled `Display`.
//!
//! The `Again` and `Eof` variants intentionally mirror FFmpeg's `EAGAIN` /
//! `AVERROR_EOF` control-flow convention: they are not failures but signals
//! driving the push/pull codec loop — `Again` means "feed more input before
//! asking for output again", `Eof` means "the stream is finished and fully
//! drained". Everything else is a real error.

/// Crate-wide result alias.
pub type Result<T> = std::result::Result<T, Error>;

/// Errors (and control-flow signals) produced by the VP9 decoder and encoder.
#[non_exhaustive]
#[derive(Debug, Clone, PartialEq, Eq)]
pub enum Error {
    /// A code path that is scaffolded but not yet implemented. Carries a short
    /// static description of the missing piece.
    Unimplemented(&'static str),

    /// End of stream: the input is finished and all output has been drained.
    /// Control flow, not a failure (FFmpeg's `AVERROR_EOF`).
    Eof,

    /// More input is required before output can be produced. Control flow, not
    /// a failure (FFmpeg's `EAGAIN`).
    Again,

    /// The bitstream is malformed or internally inconsistent.
    InvalidData(String),

    /// The input is valid but uses a feature this implementation does not
    /// support.
    Unsupported(String),
}

impl Error {
    /// Convenience constructor for `InvalidData` from anything string-like.
    pub fn invalid(msg: impl Into<String>) -> Self {
        Error::InvalidData(msg.into())
    }

    /// Convenience constructor for `Unsupported`.
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
