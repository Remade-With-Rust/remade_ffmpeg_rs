use std::fmt;

/// Why a stream could not be decoded.
#[derive(Clone, Copy, Debug, PartialEq, Eq)]
#[non_exhaustive]
pub enum Error {
    /// The data ended before the structure being read did.
    Truncated,
    /// The data breaks a rule of ITU-T T.88; the text names it.
    Invalid(&'static str),
    /// A construct this decoder does not implement (colour extensions,
    /// reserved segment types, unknown extensions marked necessary).
    Unsupported(&'static str),
    /// One of the [`Limits`](crate::Limits) would be exceeded.
    LimitExceeded(&'static str),
}

/// The crate's result type.
pub type Result<T> = std::result::Result<T, Error>;

impl fmt::Display for Error {
    fn fmt(&self, f: &mut fmt::Formatter<'_>) -> fmt::Result {
        match self {
            Error::Truncated => f.write_str("JBIG2 data is truncated"),
            Error::Invalid(m) => write!(f, "invalid JBIG2 data: {m}"),
            Error::Unsupported(m) => write!(f, "unsupported JBIG2 feature: {m}"),
            Error::LimitExceeded(m) => write!(f, "JBIG2 limit exceeded: {m}"),
        }
    }
}

impl std::error::Error for Error {}
