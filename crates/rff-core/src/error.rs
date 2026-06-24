//! Unified error type for the whole workspace.

use crate::media::CodecId;

/// The workspace-wide result alias. Every fallible API returns this.
pub type Result<T> = std::result::Result<T, Error>;

/// Errors produced anywhere in the pipeline.
///
/// The `Again` and `Eof` variants intentionally mirror FFmpeg's `EAGAIN` /
/// `AVERROR_EOF` control-flow signals from the send/receive codec API: they are
/// not "failures" so much as "ask again later" / "stream is finished".
#[derive(Debug, thiserror::Error)]
#[non_exhaustive]
pub enum Error {
    /// A code path that is scaffolded but not yet implemented. Carries a short
    /// static label so logs point straight at the missing piece.
    #[error("not yet implemented: {0}")]
    Unimplemented(&'static str),

    /// End of stream — a demuxer or decoder has no more data to give.
    #[error("end of stream")]
    Eof,

    /// More input is required before output can be produced (codec drain/fill).
    #[error("more input required")]
    Again,

    /// The input bytes were malformed for the expected format/codec.
    #[error("invalid data: {0}")]
    InvalidData(String),

    /// A requested capability exists in concept but isn't supported here.
    #[error("unsupported: {0}")]
    Unsupported(String),

    /// No registered decoder could handle this codec.
    #[error("no decoder found for codec {0}")]
    DecoderNotFound(CodecId),

    /// No registered encoder could handle this codec.
    #[error("no encoder found for codec {0}")]
    EncoderNotFound(CodecId),

    /// No registered muxer matched the requested container name/extension.
    #[error("no muxer found for format `{0}`")]
    MuxerNotFound(String),

    /// No registered demuxer could probe/handle the input.
    #[error("no demuxer found for input `{0}`")]
    DemuxerNotFound(String),

    /// A passed option was not recognised or had an invalid value.
    #[error("invalid option: {0}")]
    Option(String),

    /// Underlying I/O failure.
    #[error("i/o error: {0}")]
    Io(#[from] std::io::Error),
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
