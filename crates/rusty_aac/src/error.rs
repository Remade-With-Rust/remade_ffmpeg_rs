//! The crate's dependency-free error type.
//!
//! [`Error::Again`] and [`Error::Eof`] follow EAGAIN-style pull semantics:
//! they are not failures. `Again` means "more input is required before output
//! can be produced" (feed more data, then pull again); `Eof` means "the stream
//! is finished and fully drained" (stop pulling). Everything else is a real
//! error.

/// Crate-wide result alias.
pub type Result<T> = std::result::Result<T, Error>;

/// All the ways AAC decoding/encoding can fail — or, for [`Error::Again`] /
/// [`Error::Eof`], signal normal stream flow control.
#[derive(Debug)]
#[non_exhaustive]
pub enum Error {
    /// A code path that is scaffolded but not yet implemented. Carries a short
    /// static label so logs point straight at the missing piece.
    Unimplemented(&'static str),

    /// End of stream — the codec has no more data to give.
    Eof,

    /// More input is required before output can be produced (codec drain/fill).
    Again,

    /// The input bytes were malformed for AAC-LC.
    InvalidData(String),

    /// A requested capability exists in concept but isn't supported here.
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
