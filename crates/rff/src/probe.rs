//! Media inspection — the engine side of `ffprobe`.
//!
//! [`probe`] opens an input, identifies its container, reads the stream
//! headers, and returns a structured [`MediaInfo`]. The CLI's `ffprobe` and the
//! server's `POST /v1/probe` both call straight into this.

use std::io::Read;
use std::path::Path;

use rff_core::{CodecId, Error, MediaType, Rational, Result};

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
    let path_str = path
        .to_str()
        .ok_or_else(|| Error::Option("input path is not valid UTF-8".into()))?;
    let (format_name, reader) = open_source(engine, path_str, None)?;
    let mut demuxer = engine.formats.open_demuxer(&format_name, reader)?;

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

/// Decide which container a path should be read as: an explicit `-f` override
/// wins; otherwise sniff the file's leading bytes (content beats filename);
/// otherwise fall back to the extension. Shared by [`probe`] and the transcode
/// input path so both detect formats the same way.
pub(crate) fn detect_input_format(
    engine: &Engine,
    path: &Path,
    forced: Option<&str>,
) -> Result<String> {
    if let Some(name) = forced {
        return Ok(name.to_string());
    }
    // Content sniff (best effort — an unreadable head just falls through).
    if let Ok(head) = read_head(path, 4096) {
        if let Some(format) = engine.formats.probe(&head) {
            return Ok(format.name.to_string());
        }
    }
    // Filename fallback.
    let ext = path
        .extension()
        .and_then(|e| e.to_str())
        .unwrap_or_default();
    engine
        .formats
        .by_extension(ext)
        .map(|f| f.name.to_string())
        .ok_or_else(|| Error::DemuxerNotFound(path.display().to_string()))
}

/// Read up to `max` leading bytes of a file for content sniffing.
fn read_head(path: &Path, max: usize) -> std::io::Result<Vec<u8>> {
    let file = std::fs::File::open(path)?;
    let mut buf = Vec::new();
    file.take(max as u64).read_to_end(&mut buf)?;
    Ok(buf)
}

/// Open a path-or-URL as a streaming reader and decide its container format.
///
/// Local files keep the read-twice (sniff then open) path. For `http(s)://`
/// URLs the stream is opened once, its head peeked for content sniffing, then
/// chained back in front so the demuxer sees the whole stream from byte 0.
/// Shared by [`probe`] (ffprobe) and the transcode input path (ffmpeg).
pub(crate) fn open_source(
    engine: &Engine,
    path: &str,
    forced: Option<&str>,
) -> Result<(String, Box<dyn Read + Send>)> {
    if !rff_io::is_url(path) {
        let format = detect_input_format(engine, Path::new(path), forced)?;
        return Ok((format, Box::new(std::fs::File::open(path)?)));
    }

    let mut reader = rff_io::open(path)?;
    let mut head = vec![0u8; 4096];
    let n = read_some(&mut reader, &mut head)?;
    head.truncate(n);
    let format = match forced {
        Some(f) => f.to_string(),
        None => engine
            .formats
            .probe(&head)
            .map(|f| f.name.to_string())
            .or_else(|| url_extension_format(engine, path))
            .ok_or_else(|| Error::DemuxerNotFound(path.to_string()))?,
    };
    let chained: Box<dyn Read + Send> = Box::new(std::io::Cursor::new(head).chain(reader));
    Ok((format, chained))
}

/// Read up to `buf.len()` bytes, tolerating short reads (network streams arrive
/// in pieces); stops at EOF.
fn read_some(reader: &mut dyn Read, buf: &mut [u8]) -> Result<usize> {
    let mut filled = 0;
    while filled < buf.len() {
        match reader.read(&mut buf[filled..]) {
            Ok(0) => break,
            Ok(n) => filled += n,
            Err(ref e) if e.kind() == std::io::ErrorKind::Interrupted => continue,
            Err(e) => return Err(e.into()),
        }
    }
    Ok(filled)
}

/// Guess a container from a URL's extension (after the last `.`, ignoring any
/// `?query`/`#fragment`).
fn url_extension_format(engine: &Engine, url: &str) -> Option<String> {
    let stem = url.split(['?', '#']).next().unwrap_or(url);
    let ext = stem.rsplit('.').next().unwrap_or("");
    engine.formats.by_extension(ext).map(|f| f.name.to_string())
}
