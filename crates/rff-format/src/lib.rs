//! `rff-format` — the container (muxer/demuxer) abstraction layer (FFmpeg's
//! `libavformat` core, minus the formats themselves).
//!
//! A container is the file wrapper (AVI, MP4, Matroska, ...) that interleaves
//! one or more elementary streams. This crate defines:
//! * [`Stream`] — the description of one elementary stream inside a container,
//! * [`Demuxer`] — reads a container, exposing streams and [`Packet`]s,
//! * [`Muxer`] — writes streams + packets back into a container,
//! * [`FormatRegistry`] — what individual format crates register into.
//!
//! Concrete formats live in their own crates (`rff-format-avi`, ...).

use std::collections::HashMap;
use std::io::{Read, Write};

use rff_core::{CodecId, Error, MediaType, Packet, Rational, Result};

/// Description of one elementary stream within a container.
#[derive(Debug, Clone)]
pub struct Stream {
    /// Position of this stream within its container.
    pub index: usize,
    pub codec_id: CodecId,
    pub media_type: MediaType,
    /// Unit of all timestamps on packets of this stream.
    pub time_base: Rational,
    // --- Video parameters (zero/ignored for non-video) ---
    pub width: u32,
    pub height: u32,
    // --- Audio parameters (zero/ignored for non-audio) ---
    pub sample_rate: u32,
    pub channels: u16,
}

impl Stream {
    /// Create a stream with sane defaults for the given codec.
    pub fn new(index: usize, codec_id: CodecId) -> Stream {
        Stream {
            index,
            codec_id,
            media_type: codec_id.media_type(),
            time_base: Rational::new(1, 1000),
            width: 0,
            height: 0,
            sample_rate: 0,
            channels: 0,
        }
    }
}

/// A byte source a demuxer reads from. (`Box<dyn Read + Send>` keeps the trait
/// object-safe and lets inputs be files, network streams, or in-memory buffers.)
pub type Input = Box<dyn Read + Send>;
/// A byte sink a muxer writes to.
pub type Output = Box<dyn Write + Send>;

/// Reads a container format: parses the header into [`Stream`]s, then yields
/// [`Packet`]s until the input is exhausted.
pub trait Demuxer: Send {
    /// Parse the container header and return its streams.
    fn read_header(&mut self) -> Result<Vec<Stream>>;

    /// Read the next packet. Returns [`Error::Eof`] at end of input.
    fn read_packet(&mut self) -> Result<Packet>;
}

/// Writes a container format: declare streams, write packets, finalize.
pub trait Muxer: Send {
    /// Write the container header for the given set of streams.
    fn write_header(&mut self, streams: &[Stream]) -> Result<()>;

    /// Write one packet (its `stream_index` selects the target stream).
    fn write_packet(&mut self, packet: &Packet) -> Result<()>;

    /// Finalize the container (indexes, trailers, fixups).
    fn write_trailer(&mut self) -> Result<()>;
}

/// Factory opening a demuxer over an input byte source.
pub type DemuxerFactory = fn(Input) -> Box<dyn Demuxer>;
/// Factory opening a muxer over an output byte sink.
pub type MuxerFactory = fn(Output) -> Box<dyn Muxer>;

/// Static description of a container format and its read/write support.
pub struct Format {
    /// Short name (`avi`, `mp4`, ...), as used by `ffmpeg -f <name>`.
    pub name: &'static str,
    pub long_name: &'static str,
    /// File extensions that imply this format (without the dot).
    pub extensions: &'static [&'static str],
    /// Present if this format can be demuxed (read).
    pub demuxer: Option<DemuxerFactory>,
    /// Present if this format can be muxed (written).
    pub muxer: Option<MuxerFactory>,
}

impl Format {
    pub fn can_demux(&self) -> bool {
        self.demuxer.is_some()
    }
    pub fn can_mux(&self) -> bool {
        self.muxer.is_some()
    }
}

/// Holds every container format known to a running engine.
#[derive(Default)]
pub struct FormatRegistry {
    by_name: HashMap<&'static str, Format>,
}

impl FormatRegistry {
    pub fn new() -> FormatRegistry {
        FormatRegistry::default()
    }

    pub fn register(&mut self, format: Format) {
        self.by_name.insert(format.name, format);
    }

    pub fn by_name(&self, name: &str) -> Option<&Format> {
        self.by_name.get(name)
    }

    /// Find a format whose extension list contains `ext` (case-insensitive,
    /// no leading dot). This is the cheap "guess by filename" path; a real
    /// content-sniffing probe comes later.
    pub fn by_extension(&self, ext: &str) -> Option<&Format> {
        let ext = ext.trim_start_matches('.').to_ascii_lowercase();
        self.by_name
            .values()
            .find(|f| f.extensions.iter().any(|e| e.eq_ignore_ascii_case(&ext)))
    }

    /// Open a demuxer for the format named `name`.
    pub fn open_demuxer(&self, name: &str, input: Input) -> Result<Box<dyn Demuxer>> {
        self.by_name
            .get(name)
            .and_then(|f| f.demuxer)
            .map(|factory| factory(input))
            .ok_or_else(|| Error::DemuxerNotFound(name.to_string()))
    }

    /// Open a muxer for the format named `name`.
    pub fn open_muxer(&self, name: &str, output: Output) -> Result<Box<dyn Muxer>> {
        self.by_name
            .get(name)
            .and_then(|f| f.muxer)
            .map(|factory| factory(output))
            .ok_or_else(|| Error::MuxerNotFound(name.to_string()))
    }

    pub fn iter(&self) -> impl Iterator<Item = &Format> {
        self.by_name.values()
    }

    pub fn len(&self) -> usize {
        self.by_name.len()
    }

    pub fn is_empty(&self) -> bool {
        self.by_name.is_empty()
    }
}
