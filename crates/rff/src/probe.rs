//! Media inspection — the engine side of `ffprobe`.
//!
//! [`probe`] opens an input, identifies its container, reads the stream
//! headers, and returns a structured [`MediaInfo`]. The CLI's `ffprobe` and the
//! server's `POST /v1/probe` both call straight into this.

use std::path::Path;

use rff_core::{CodecId, MediaType, Rational, Result};

use crate::Engine;

/// Everything we learned about an input.
#[derive(Debug, Clone)]
pub struct MediaInfo {
    /// Short name of the detected container format (e.g. `avi`).
    pub format_name: String,
    /// One entry per elementary stream.
    pub streams: Vec<StreamInfo>,
}

/// Summary of one elementary stream.
#[derive(Debug, Clone)]
pub struct StreamInfo {
    pub index: usize,
    pub media_type: MediaType,
    pub codec_id: CodecId,
    pub time_base: Rational,
    pub width: u32,
    pub height: u32,
    pub sample_rate: u32,
    pub channels: u16,
}

/// Probe a media file at `path`.
///
/// The container is currently chosen by file extension; content-sniffing
/// (reading magic bytes) is a planned upgrade. Reading the header itself relies
/// on the corresponding demuxer, which is still scaffolded — so today this
/// resolves the format and then surfaces the demuxer's
/// [`Unimplemented`](rff_core::Error::Unimplemented) state. The plumbing is
/// real; only the per-format parsing is pending.
pub fn probe(engine: &Engine, path: impl AsRef<Path>) -> Result<MediaInfo> {
    let path = path.as_ref();
    let ext = path
        .extension()
        .and_then(|e| e.to_str())
        .unwrap_or_default();

    let format = engine
        .formats
        .by_extension(ext)
        .ok_or_else(|| rff_core::Error::DemuxerNotFound(path.display().to_string()))?;
    let format_name = format.name.to_string();

    let file = std::fs::File::open(path)?;
    let mut demuxer = engine
        .formats
        .open_demuxer(&format_name, Box::new(file))?;

    let streams = demuxer.read_header()?;
    let streams = streams
        .into_iter()
        .map(|s| StreamInfo {
            index: s.index,
            media_type: s.media_type,
            codec_id: s.codec_id,
            time_base: s.time_base,
            width: s.width,
            height: s.height,
            sample_rate: s.sample_rate,
            channels: s.channels,
        })
        .collect();

    Ok(MediaInfo {
        format_name,
        streams,
    })
}
