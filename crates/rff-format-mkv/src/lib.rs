//! Matroska / WebM demuxer and muxer.
//!
//! Matroska is an [EBML](https://www.matroska.org/) (binary XML) container.
//! WebM is its restricted profile (VP8/VP9/AV1 video + Opus/Vorbis audio). The
//! demuxer streams the EBML element tree from the (unseekable) [`Input`]:
//! `read_header` reads the Info and Tracks bodies, streams past everything
//! else (SeekHead, Tags, Void…) and stops at the first Cluster, so opening a
//! file costs memory proportional to its headers. `read_packet` then walks the
//! Clusters incrementally — known- or unknown-size — turning each
//! (Simple)Block into a [`Packet`]; peak memory is one block, not the file.
//! The muxers (in [`mux`]) write the inverse: SeekHead + Info + Tracks +
//! Clusters + Cues -- streamed on a seekable output, buffered otherwise.

mod mux;

use std::io::{self, BufReader, Read};

use rff_core::{CodecId, Error, MediaType, Packet, Rational, Result, SampleFormat};
use rff_format::avc::{avcc_to_annexb, parse_avcc, AvcConfig};
use rff_format::{Demuxer, Format, FormatRegistry, Input, MuxCaps, Stream};

pub use mux::{MkvMuxer, MkvStreamingMuxer};

/// Register the Matroska and WebM formats (demux + mux). WebM is a separate
/// registry entry so `out.webm` gets the restricted doctype and codec checks;
/// both share one demuxer.
pub fn register(registry: &mut FormatRegistry) {
    registry.register(Format {
        name: "matroska",
        long_name: "Matroska (EBML)",
        extensions: &["mkv", "mka", "mks"],
        demuxer: Some(|input| Box::new(MkvDemuxer::new(input))),
        muxer: Some(|out| Box::new(MkvMuxer::new(out, false))),
        muxer_path: None,
        probe: Some(probe_mkv),
        mux_caps: MuxCaps::container(&[
            CodecId::Vp9,
            CodecId::Avif,
            CodecId::H264,
            CodecId::Opus,
            CodecId::Vorbis,
            CodecId::Aac,
            CodecId::Flac,
            CodecId::Mp3,
            CodecId::Pcm,
            CodecId::Subrip,
            CodecId::WebVtt,
        ]),
    });
    registry.register(Format {
        name: "webm",
        long_name: "WebM (restricted Matroska)",
        extensions: &["webm"],
        demuxer: Some(|input| Box::new(MkvDemuxer::new(input))),
        muxer: Some(|out| Box::new(MkvMuxer::new(out, true))),
        muxer_path: None,
        probe: None, // content-probing is matroska's job; webm is chosen by name
        mux_caps: MuxCaps::container(&[
            CodecId::Vp9,
            CodecId::Avif,
            CodecId::Opus,
            CodecId::Vorbis,
            CodecId::WebVtt,
        ]),
    });
    // Seekable outputs (local files) stream; the plain factories above keep
    // serving pipes and sockets with the in-memory layout.
    registry.register_seekable_muxer("matroska", |out| {
        Box::new(MkvStreamingMuxer::new(out, false))
    });
    registry.register_seekable_muxer("webm", |out| Box::new(MkvStreamingMuxer::new(out, true)));
}

/// The EBML magic (`\x1A\x45\xDF\xA3`) starts every Matroska/WebM file.
pub fn probe_mkv(bytes: &[u8]) -> i32 {
    if bytes.starts_with(&[0x1A, 0x45, 0xDF, 0xA3]) {
        90
    } else {
        0
    }
}

// ---- EBML element IDs (with their length-marker bits intact) --------------
const ID_EBML_HEADER: u32 = 0x1A45_DFA3;
const ID_SEGMENT: u32 = 0x1853_8067;
const ID_SEEK_HEAD: u32 = 0x114D_9B74;
const ID_INFO: u32 = 0x1549_A966;
const ID_TIMESTAMP_SCALE: u32 = 0x2AD7B1;
const ID_DURATION: u32 = 0x4489;
const ID_TRACKS: u32 = 0x1654_AE6B;
const ID_TRACK_ENTRY: u32 = 0xAE;
const ID_TRACK_NUMBER: u32 = 0xD7;
const ID_TRACK_TYPE: u32 = 0x83;
const ID_CODEC_ID: u32 = 0x86;
const ID_CODEC_PRIVATE: u32 = 0x63A2;
const ID_VIDEO: u32 = 0xE0;
const ID_PIXEL_WIDTH: u32 = 0xB0;
const ID_PIXEL_HEIGHT: u32 = 0xBA;
const ID_AUDIO: u32 = 0xE1;
const ID_SAMPLING_FREQUENCY: u32 = 0xB5;
const ID_CHANNELS: u32 = 0x9F;
const ID_BIT_DEPTH: u32 = 0x6264;
const ID_CLUSTER: u32 = 0x1F43_B675;
const ID_TIMESTAMP: u32 = 0xE7;
const ID_SIMPLE_BLOCK: u32 = 0xA3;
const ID_BLOCK_GROUP: u32 = 0xA0;
const ID_BLOCK: u32 = 0xA1;
const ID_BLOCK_DURATION: u32 = 0x9B;
const ID_CUES: u32 = 0x1C53_BB6B;
const ID_ATTACHMENTS: u32 = 0x1941_A469;
const ID_CHAPTERS: u32 = 0x1043_A770;
const ID_TAGS: u32 = 0x1254_C367;

/// Segment children ("level 1") and the level-0 IDs. Meeting one of these
/// while inside a Cluster means that Cluster is over — the only way an
/// unknown-size (live-written) Cluster ends.
fn is_level1(id: u32) -> bool {
    matches!(
        id,
        ID_EBML_HEADER
            | ID_SEGMENT
            | ID_SEEK_HEAD
            | ID_INFO
            | ID_TRACKS
            | ID_CLUSTER
            | ID_CUES
            | ID_ATTACHMENTS
            | ID_CHAPTERS
            | ID_TAGS
    )
}

/// Read-ahead buffer over the (unseekable) input.
const READ_BUF: usize = 64 * 1024;
/// Info / Tracks bodies are read whole; a declared size past this is corrupt
/// (real ones are a few KiB) and the element is skipped instead.
const MAX_HEADER_ELEMENT: u64 = 64 << 20;
/// A (Simple)Block or BlockGroup declaring more than this is skipped rather
/// than buffered — far above any real frame, it bounds garbage input.
const MAX_BLOCK: u64 = 1 << 30;

/// A cursor over an in-memory element body that reads EBML primitives.
struct Ebml<'a> {
    d: &'a [u8],
    pos: usize,
}

impl<'a> Ebml<'a> {
    fn new(d: &'a [u8]) -> Ebml<'a> {
        Ebml { d, pos: 0 }
    }

    fn at(d: &'a [u8], pos: usize) -> Ebml<'a> {
        Ebml { d, pos }
    }

    /// Read an element ID (1-4 bytes), keeping the length-marker bits.
    fn read_id(&mut self) -> Option<u32> {
        let first = *self.d.get(self.pos)?;
        let len = first.leading_zeros() as usize + 1;
        if len > 4 || self.pos + len > self.d.len() {
            return None;
        }
        let mut id = 0u32;
        for i in 0..len {
            id = (id << 8) | self.d[self.pos + i] as u32;
        }
        self.pos += len;
        Some(id)
    }

    /// Read an EBML size (vint with the marker stripped). `None` element value
    /// means an "unknown size" marker (all value bits set).
    fn read_size(&mut self) -> Option<(u64, bool)> {
        let first = *self.d.get(self.pos)?;
        let len = first.leading_zeros() as usize + 1;
        if len > 8 || self.pos + len > self.d.len() {
            return None;
        }
        let mask = (1u64 << (8 - len)) - 1;
        let mut val = (first as u64) & mask;
        let mut all_ones = (first as u64) & mask == mask;
        for i in 1..len {
            let b = self.d[self.pos + i];
            val = (val << 8) | b as u64;
            all_ones = all_ones && b == 0xFF;
        }
        self.pos += len;
        Some((val, all_ones && len > 0))
    }

    /// Big-endian unsigned of `len` bytes (missing bytes read as zero). Only
    /// the last 8 bytes can survive the shifts, so only those are visited —
    /// a corrupt multi-gigabyte `len` costs nothing.
    fn read_uint(&mut self, len: usize) -> u64 {
        let mut v = 0u64;
        for i in len.saturating_sub(8)..len {
            let b = self.pos.checked_add(i).and_then(|p| self.d.get(p));
            v = (v << 8) | *b.unwrap_or(&0) as u64;
        }
        self.pos = self.pos.saturating_add(len);
        v
    }

    fn read_float(&mut self, len: usize) -> f64 {
        let bytes = |n: usize| {
            self.pos
                .checked_add(n)
                .and_then(|end| self.d.get(self.pos..end))
        };
        let v = match len {
            4 => bytes(4)
                .map(|b| f32::from_be_bytes([b[0], b[1], b[2], b[3]]) as f64)
                .unwrap_or(0.0),
            8 => bytes(8)
                .map(|b| f64::from_be_bytes(b.try_into().unwrap()))
                .unwrap_or(0.0),
            _ => 0.0,
        };
        self.pos = self.pos.saturating_add(len);
        v
    }

    fn read_bytes(&mut self, len: usize) -> &'a [u8] {
        let start = self.pos.min(self.d.len());
        let end = self.pos.saturating_add(len).min(self.d.len());
        let s = &self.d[start..end];
        self.pos = end;
        s
    }

    fn skip(&mut self, len: usize) {
        self.pos = self.pos.saturating_add(len);
    }
}

/// An element header read off the stream: `start` is the absolute offset of
/// its body.
#[derive(Clone, Copy)]
struct ElemHeader {
    id: u32,
    size: u64,
    unknown: bool,
    start: u64,
}

/// The input, buffered, with the absolute position tracked (it can't seek).
struct Source {
    r: BufReader<Input>,
    pos: u64,
}

impl Source {
    fn byte(&mut self) -> io::Result<Option<u8>> {
        let mut b = [0u8];
        loop {
            match self.r.read(&mut b) {
                Ok(0) => return Ok(None),
                Ok(_) => {
                    self.pos += 1;
                    return Ok(Some(b[0]));
                }
                Err(e) if e.kind() == io::ErrorKind::Interrupted => continue,
                Err(e) => return Err(e),
            }
        }
    }

    /// The next element header (ID + size vint). `None` at end of input, on
    /// a truncated header, or on an invalid vint — all of which end a walk.
    fn header(&mut self) -> io::Result<Option<ElemHeader>> {
        let Some(first) = self.byte()? else {
            return Ok(None);
        };
        let len = first.leading_zeros() as usize + 1;
        if len > 4 {
            return Ok(None);
        }
        let mut id = first as u32;
        for _ in 1..len {
            let Some(b) = self.byte()? else {
                return Ok(None);
            };
            id = (id << 8) | b as u32;
        }
        let Some(first) = self.byte()? else {
            return Ok(None);
        };
        let len = first.leading_zeros() as usize + 1;
        if len > 8 {
            return Ok(None);
        }
        let mask = (1u64 << (8 - len)) - 1;
        let mut size = first as u64 & mask;
        let mut unknown = size == mask;
        for _ in 1..len {
            let Some(b) = self.byte()? else {
                return Ok(None);
            };
            size = (size << 8) | b as u64;
            unknown = unknown && b == 0xFF;
        }
        Ok(Some(ElemHeader {
            id,
            size,
            unknown,
            start: self.pos,
        }))
    }

    /// Discard up to `n` bytes (fewer at end of input) without buffering them.
    fn skip(&mut self, n: u64) -> io::Result<()> {
        let copied = io::copy(&mut (&mut self.r).take(n), &mut io::sink())?;
        self.pos += copied;
        Ok(())
    }

    /// Read up to `n` bytes (fewer at end of input — a truncated final block
    /// comes back partial). Allocation tracks bytes actually read, never `n`.
    fn body(&mut self, n: u64) -> io::Result<Vec<u8>> {
        let mut v = Vec::new();
        (&mut self.r).take(n).read_to_end(&mut v)?;
        self.pos += v.len() as u64;
        Ok(v)
    }
}

/// The Cluster being walked: its end offset (`None` = unknown size, ended by
/// the next level-1 element) and its base timestamp.
struct Cluster {
    end: Option<u64>,
    ts: i64,
}

struct MkvDemuxer {
    src: Source,
    streams: Vec<Stream>,
    /// Maps a Matroska track number to our 0-based stream index.
    track_map: Vec<(u64, usize)>,
    /// Indexed by stream index; `Some` for H.264 tracks whose CodecPrivate is
    /// an `avcC` — their blocks are AVCC and get normalised to Annex-B.
    avc: Vec<Option<AvcConfig>>,
    /// Indexed by stream index; ASS/SSA subtitle tracks, whose block payloads
    /// carry `ReadOrder,Layer,Style,…,Text` and get reduced to plain text.
    ass: Vec<bool>,
    timestamp_scale: u64,
    /// Segment duration in `timestamp_scale` ticks (same unit as `time_base`).
    duration_ticks: Option<i64>,
    /// Absolute end of the Segment body; `None` = unknown size (runs to EOF).
    seg_end: Option<u64>,
    cluster: Option<Cluster>,
    /// One-element lookahead: a header already read that the next step of
    /// the walk must handle (the first Cluster after `read_header`, or the
    /// level-1 element that ended an unknown-size Cluster).
    pending: Option<ElemHeader>,
    /// The walk is over (end of Segment, end of input, or corrupt framing).
    done: bool,
    parsed: bool,
}

impl MkvDemuxer {
    fn new(input: Input) -> MkvDemuxer {
        MkvDemuxer {
            src: Source {
                r: BufReader::with_capacity(READ_BUF, input),
                pos: 0,
            },
            streams: Vec::new(),
            track_map: Vec::new(),
            avc: Vec::new(),
            ass: Vec::new(),
            timestamp_scale: 1_000_000, // default: 1 ms
            duration_ticks: None,
            seg_end: None,
            cluster: None,
            pending: None,
            done: false,
            parsed: false,
        }
    }

    /// `size` clamped to what is left of the innermost enclosing element with
    /// a known end (the Cluster being walked, else the Segment).
    fn bounded(&self, h: &ElemHeader) -> u64 {
        let end = self.cluster.as_ref().and_then(|c| c.end).or(self.seg_end);
        match end {
            Some(end) => h.size.min(end.saturating_sub(h.start)),
            None => h.size,
        }
    }

    /// Skip the EBML header, enter the Segment and parse everything up to the
    /// first Cluster (Info, Tracks); other elements are streamed past.
    fn parse_headers(&mut self) -> Result<()> {
        let no_tracks = || Error::invalid("mkv: no tracks found");
        if self.done {
            return Err(no_tracks());
        }
        loop {
            let Some(h) = self.src.header()? else {
                self.done = true;
                return Err(no_tracks());
            };
            if h.id == ID_SEGMENT {
                self.seg_end = (!h.unknown).then(|| h.start.saturating_add(h.size));
                break;
            }
            if h.unknown {
                self.done = true;
                return Err(no_tracks());
            }
            self.src.skip(h.size)?;
        }
        loop {
            if self.seg_end.is_some_and(|end| self.src.pos >= end) {
                self.done = true;
                break;
            }
            let Some(h) = self.src.header()? else {
                self.done = true;
                break;
            };
            match h.id {
                ID_CLUSTER => {
                    self.pending = Some(h);
                    break;
                }
                // An unknown-size element (only legal for Segment/Cluster):
                // descend into it, so its children are met one by one.
                _ if h.unknown => {}
                ID_INFO | ID_TRACKS => {
                    let len = self.bounded(&h);
                    if len > MAX_HEADER_ELEMENT {
                        self.src.skip(len)?;
                        continue;
                    }
                    let body = self.src.body(len)?;
                    if h.id == ID_INFO {
                        self.parse_info(&body);
                    } else {
                        self.parse_tracks(&body);
                    }
                }
                // Children of an unknown-size Info/Tracks descended into.
                ID_TRACK_ENTRY | ID_TIMESTAMP_SCALE | ID_DURATION => {
                    let len = self.bounded(&h);
                    if len > MAX_HEADER_ELEMENT {
                        self.src.skip(len)?;
                        continue;
                    }
                    let body = self.src.body(len)?;
                    if h.id == ID_TRACK_ENTRY {
                        self.parse_track_entry(&body);
                    } else {
                        self.info_element(h.id, &mut Ebml::new(&body), h.size as usize);
                    }
                }
                _ => {
                    let len = self.bounded(&h);
                    self.src.skip(len)?;
                }
            }
        }
        if self.streams.is_empty() {
            self.done = true;
            return Err(no_tracks());
        }
        for s in &mut self.streams {
            s.duration = self.duration_ticks;
            // Per-stream frame counts would need the whole file read (ffprobe
            // reports N/A for Matroska too), so `nb_frames` stays `None`.
        }
        Ok(())
    }

    /// Walk Cluster children until a block yields a packet. `None` = done.
    fn next_packet(&mut self) -> Result<Option<Packet>> {
        loop {
            if self.done {
                return Ok(None);
            }
            let h = match self.pending.take() {
                Some(h) => h,
                None => {
                    let pos = self.src.pos;
                    if let Some(Cluster { end: Some(end), .. }) = self.cluster {
                        if pos >= end {
                            self.cluster = None;
                            continue;
                        }
                    }
                    if self.seg_end.is_some_and(|end| pos >= end) {
                        self.done = true;
                        continue;
                    }
                    match self.src.header()? {
                        Some(h) => h,
                        None => {
                            // Corrupt framing inside a known-size Cluster:
                            // resync at its end. Anywhere else (or at EOF)
                            // the walk is over.
                            match self.cluster.take().and_then(|c| c.end) {
                                Some(end) if end > self.src.pos => {
                                    self.src.skip(end - self.src.pos)?;
                                }
                                _ => self.done = true,
                            }
                            continue;
                        }
                    }
                }
            };

            // Segment level: enter Clusters, stream past everything else.
            let Some(ts) = self.cluster.as_ref().map(|c| c.ts) else {
                if h.id == ID_CLUSTER {
                    let end = (!h.unknown).then(|| h.start.saturating_add(self.bounded(&h)));
                    self.cluster = Some(Cluster { end, ts: 0 });
                } else if !h.unknown {
                    let len = self.bounded(&h);
                    self.src.skip(len)?;
                }
                continue;
            };

            // Inside a Cluster.
            if is_level1(h.id) {
                self.cluster = None;
                self.pending = Some(h);
                continue;
            }
            if h.unknown {
                continue; // descend into an unknown-size child
            }
            match h.id {
                ID_TIMESTAMP => {
                    let body = self.src.body(h.size.min(8))?;
                    self.src.skip(h.size.saturating_sub(8))?;
                    let v = Ebml::new(&body).read_uint(h.size.min(8) as usize) as i64;
                    if let Some(c) = self.cluster.as_mut() {
                        c.ts = v;
                    }
                }
                ID_SIMPLE_BLOCK if h.size <= MAX_BLOCK => {
                    let block = self.src.body(h.size)?;
                    if let Some(p) = self.parse_block(&block, ts, 0) {
                        return Ok(Some(p));
                    }
                }
                ID_BLOCK_GROUP if h.size <= MAX_BLOCK => {
                    let group = self.src.body(h.size)?;
                    if let Some(p) = self.parse_block_group(&group, ts) {
                        return Ok(Some(p));
                    }
                }
                _ => {
                    let len = self.bounded(&h);
                    self.src.skip(len)?;
                }
            }
        }
    }

    fn parse_info(&mut self, data: &[u8]) {
        let mut e = Ebml::new(data);
        while e.pos < data.len() {
            let Some(id) = e.read_id() else { break };
            let Some((size, _)) = e.read_size() else {
                break;
            };
            self.info_element(id, &mut e, size as usize);
        }
    }

    /// One Info child whose body starts at `e.pos`.
    fn info_element(&mut self, id: u32, e: &mut Ebml, len: usize) {
        match id {
            ID_TIMESTAMP_SCALE => self.timestamp_scale = e.read_uint(len),
            // Duration is a float in `timestamp_scale` ticks — our time_base.
            ID_DURATION => {
                let d = e.read_float(len);
                if d > 0.0 {
                    self.duration_ticks = Some(d as i64);
                }
            }
            _ => e.skip(len),
        }
    }

    fn parse_tracks(&mut self, data: &[u8]) {
        let mut e = Ebml::new(data);
        while e.pos < data.len() {
            let Some(id) = e.read_id() else { break };
            let Some((size, _)) = e.read_size() else {
                break;
            };
            let entry_end = e.pos.saturating_add(size as usize).min(data.len());
            if id == ID_TRACK_ENTRY {
                self.parse_track_entry(&data[e.pos..entry_end]);
            }
            e.pos = entry_end;
        }
    }

    fn parse_track_entry(&mut self, data: &[u8]) {
        let mut number = 0u64;
        let mut codec = String::new();
        let mut codec_private: Vec<u8> = Vec::new();
        let mut track_type = 0u64;
        let (mut width, mut height) = (0u32, 0u32);
        let (mut rate, mut channels) = (0u32, 0u16);
        let mut bit_depth = 0u64;

        let mut e = Ebml::new(data);
        while e.pos < data.len() {
            let Some(id) = e.read_id() else { break };
            let Some((size, _)) = e.read_size() else {
                break;
            };
            let len = size as usize;
            match id {
                ID_TRACK_NUMBER => number = e.read_uint(len),
                ID_TRACK_TYPE => track_type = e.read_uint(len),
                ID_CODEC_ID => codec = String::from_utf8_lossy(e.read_bytes(len)).into_owned(),
                ID_CODEC_PRIVATE => codec_private = e.read_bytes(len).to_vec(),
                ID_VIDEO => {
                    let mut v = Ebml::at(data, e.pos);
                    let ve = e.pos.saturating_add(len);
                    while v.pos < ve {
                        let Some(vid) = v.read_id() else { break };
                        let Some((vsz, _)) = v.read_size() else { break };
                        match vid {
                            ID_PIXEL_WIDTH => width = v.read_uint(vsz as usize) as u32,
                            ID_PIXEL_HEIGHT => height = v.read_uint(vsz as usize) as u32,
                            _ => v.skip(vsz as usize),
                        }
                    }
                    e.skip(len);
                }
                ID_AUDIO => {
                    let mut a = Ebml::at(data, e.pos);
                    let ae = e.pos.saturating_add(len);
                    while a.pos < ae {
                        let Some(aid) = a.read_id() else { break };
                        let Some((asz, _)) = a.read_size() else { break };
                        match aid {
                            ID_SAMPLING_FREQUENCY => rate = a.read_float(asz as usize) as u32,
                            ID_CHANNELS => channels = a.read_uint(asz as usize) as u16,
                            ID_BIT_DEPTH => bit_depth = a.read_uint(asz as usize),
                            _ => a.skip(asz as usize),
                        }
                    }
                    e.skip(len);
                }
                _ => e.skip(len),
            }
        }

        let codec_id = map_codec(&codec);
        let is_ass = matches!(codec.as_str(), "S_TEXT/ASS" | "S_TEXT/SSA");
        self.ass.push(is_ass);
        let index = self.streams.len();
        let mut s = Stream::new(index, codec_id);
        s.media_type = match track_type {
            1 => MediaType::Video,
            2 => MediaType::Audio,
            17 => MediaType::Subtitle,
            _ => MediaType::Data,
        };
        s.width = width;
        s.height = height;
        s.sample_rate = rate;
        s.channels = channels;
        // PCM: the codec string + bit depth decide the sample layout.
        if codec_id == CodecId::Pcm {
            s.sample_format = match (codec.as_str(), bit_depth) {
                ("A_PCM/FLOAT/IEEE", _) => Some(SampleFormat::F32),
                (_, 0 | 16) => Some(SampleFormat::S16),
                _ => None, // 24-bit int etc. — no rff layout yet
            };
        }
        // H.264 and HEVC are stored length-prefixed (CodecPrivate = `avcC` /
        // `hvcC`). Normalise to rff's Annex-B packet contract — same as
        // rff-format-mp4 — so extradata stays empty and the parameter sets
        // ride in the keyframe packets.
        let avc = match codec_id {
            CodecId::H264 => parse_avcc(&codec_private),
            CodecId::Hevc => rff_format::hvc::parse_hvcc(&codec_private),
            _ => None,
        };
        s.extradata = if avc.is_some() {
            Vec::new()
        } else if codec_id == CodecId::Vorbis {
            // Matroska stores the three Vorbis setup headers Xiph-laced; rff's
            // decoder contract wants them u32-LE length-prefixed (the Ogg
            // demuxer's packing). Convert, or the decoder can't configure.
            xiph_to_packed(&codec_private).unwrap_or(codec_private)
        } else {
            codec_private
        };
        self.avc.push(avc);
        // Matroska timestamps are in `timestamp_scale` ns; expose ms time base.
        s.time_base = Rational::new(1, (1_000_000_000 / self.timestamp_scale.max(1)) as i32);
        self.track_map.push((number, index));
        self.streams.push(s);
    }

    /// A BlockGroup body. BlockDuration may come before or after the Block:
    /// collect both, then emit (subtitle cues need their duration).
    fn parse_block_group(&self, group: &[u8], cluster_ts: i64) -> Option<Packet> {
        let mut g = Ebml::new(group);
        let mut block: Option<&[u8]> = None;
        let mut duration = 0i64;
        while g.pos < group.len() {
            let Some(gid) = g.read_id() else { break };
            let Some((gsz, _)) = g.read_size() else { break };
            match gid {
                ID_BLOCK => block = Some(g.read_bytes(gsz as usize)),
                ID_BLOCK_DURATION => duration = g.read_uint(gsz as usize) as i64,
                _ => g.skip(gsz as usize),
            }
        }
        self.parse_block(block?, cluster_ts, duration)
    }

    /// Parse a (Simple)Block body: track vint, int16 relative timestamp, flags,
    /// then the frame payload (no-lacing only for now).
    fn parse_block(&self, block: &[u8], cluster_ts: i64, duration: i64) -> Option<Packet> {
        let mut b = Ebml::new(block);
        let (track_num, _) = b.read_size()?;
        if b.pos + 3 > block.len() {
            return None;
        }
        let rel = i16::from_be_bytes([block[b.pos], block[b.pos + 1]]) as i64;
        b.pos += 2;
        let flags = block[b.pos];
        b.pos += 1;
        let lacing = (flags >> 1) & 0x03;
        let keyframe = flags & 0x80 != 0;
        let &(_, index) = self.track_map.iter().find(|(n, _)| *n == track_num)?;
        if lacing != 0 {
            return None; // laced blocks unsupported for now (rare for video/Opus)
        }
        let raw = &block[b.pos..];
        // H.264: AVCC → Annex-B, prepending SPS/PPS on keyframes so the
        // decoder is self-contained (mirrors the MP4 demuxer).
        let data = match self.avc.get(index).and_then(|a| a.as_ref()) {
            Some(cfg) => {
                let mut out = Vec::with_capacity(cfg.headers_annexb.len() + raw.len() + 16);
                if keyframe {
                    out.extend_from_slice(&cfg.headers_annexb);
                }
                avcc_to_annexb(raw, cfg.nal_len, &mut out);
                out
            }
            // ASS blocks: `ReadOrder,Layer,Style,Name,ML,MR,MV,Effect,Text` —
            // reduce to the plain text our subtitle contract carries.
            None if self.ass.get(index).copied().unwrap_or(false) => {
                rff_subtitle::ass_dialogue_text(&String::from_utf8_lossy(raw), 8).into_bytes()
            }
            None => raw.to_vec(),
        };
        let mut packet = Packet::from_data(index, data);
        packet.pts = Some(cluster_ts.wrapping_add(rel));
        packet.duration = duration.max(0);
        packet.flags.keyframe = keyframe;
        Some(packet)
    }
}

/// Convert a Xiph-laced Vorbis CodecPrivate (count-1, lace sizes, packets) to
/// rff's u32-LE length-prefixed `extradata` packing.
fn xiph_to_packed(private: &[u8]) -> Option<Vec<u8>> {
    let count = *private.first()? as usize + 1;
    let mut sizes = Vec::with_capacity(count);
    let mut i = 1;
    for _ in 0..count - 1 {
        let mut len = 0usize;
        loop {
            let b = *private.get(i)? as usize;
            i += 1;
            len += b;
            if b != 255 {
                break;
            }
        }
        sizes.push(len);
    }
    let head: usize = sizes.iter().sum();
    sizes.push(private.len().checked_sub(i + head)?); // last packet: remainder
    let mut out = Vec::new();
    for len in sizes {
        let packet = private.get(i..i + len)?;
        out.extend_from_slice(&(len as u32).to_le_bytes());
        out.extend_from_slice(packet);
        i += len;
    }
    Some(out)
}

impl Demuxer for MkvDemuxer {
    fn read_header(&mut self) -> Result<Vec<Stream>> {
        if !self.parsed {
            self.parse_headers()?;
            self.parsed = true;
        }
        Ok(self.streams.clone())
    }

    fn read_packet(&mut self) -> Result<Packet> {
        if !self.parsed {
            self.read_header()?;
        }
        self.next_packet()?.ok_or(Error::Eof)
    }
}

fn map_codec(codec: &str) -> CodecId {
    match codec {
        "V_AV1" => CodecId::Avif, // our AV1 (rav1d) decoder
        "V_VP9" => CodecId::Vp9,
        "V_MPEG4/ISO/AVC" => CodecId::H264,
        "V_MPEGH/ISO/HEVC" => CodecId::Hevc,
        "A_OPUS" => CodecId::Opus,
        "A_VORBIS" => CodecId::Vorbis,
        "A_AAC" => CodecId::Aac,
        "A_FLAC" => CodecId::Flac,
        "A_MPEG/L3" => CodecId::Mp3,
        "A_PCM/INT/LIT" | "A_PCM/FLOAT/IEEE" => CodecId::Pcm,
        // ASS/SSA tracks are reduced to plain text at block level, so they
        // surface as SubRip-contract cues.
        "S_TEXT/UTF8" | "S_TEXT/ASS" | "S_TEXT/SSA" => CodecId::Subrip,
        "S_TEXT/WEBVTT" => CodecId::WebVtt,
        _ => CodecId::None,
    }
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn reads_element_ids_keeping_marker() {
        let mut e = Ebml::new(&[0x1A, 0x45, 0xDF, 0xA3]);
        assert_eq!(e.read_id(), Some(0x1A45_DFA3)); // EBML, 4-byte id
        let mut e = Ebml::new(&[0x83]);
        assert_eq!(e.read_id(), Some(0x83)); // TrackType, 1-byte id
        let mut e = Ebml::new(&[0x63, 0xA2]);
        assert_eq!(e.read_id(), Some(0x63A2)); // CodecPrivate, 2-byte id
    }

    #[test]
    fn reads_sizes_stripping_marker() {
        // 1-byte size 0x82 → value 2.
        let mut e = Ebml::new(&[0x82]);
        assert_eq!(e.read_size(), Some((2, false)));
        // 2-byte size 0x40 0x07 → value 7.
        let mut e = Ebml::new(&[0x40, 0x07]);
        assert_eq!(e.read_size(), Some((7, false)));
        // All-ones 0xFF → unknown size.
        let mut e = Ebml::new(&[0xFF]);
        assert_eq!(e.read_size(), Some((0x7F, true)));
    }

    #[test]
    fn probe_detects_ebml_magic() {
        assert_eq!(probe_mkv(&[0x1A, 0x45, 0xDF, 0xA3, 0x00]), 90);
        assert_eq!(probe_mkv(&[0x00, 0x00]), 0);
    }

    // ---- mux → demux round trips ------------------------------------------

    use std::io::Cursor;
    use std::sync::{Arc, Mutex};

    #[derive(Clone)]
    struct SharedBuf(Arc<Mutex<Vec<u8>>>);
    impl std::io::Write for SharedBuf {
        fn write(&mut self, buf: &[u8]) -> std::io::Result<usize> {
            self.0.lock().unwrap().extend_from_slice(buf);
            Ok(buf.len())
        }
        fn flush(&mut self) -> std::io::Result<()> {
            Ok(())
        }
    }

    fn mux_streams(
        webm: bool,
        streams: &[Stream],
        packets: &[Packet],
    ) -> std::result::Result<Vec<u8>, Error> {
        use rff_format::Muxer;
        let sink = SharedBuf(Arc::new(Mutex::new(Vec::new())));
        let mut mux = MkvMuxer::new(Box::new(sink.clone()), webm);
        mux.write_header(streams)?;
        for p in packets {
            mux.write_packet(p)?;
        }
        mux.write_trailer()?;
        let file = sink.0.lock().unwrap().clone();
        Ok(file)
    }

    fn pkt(stream: usize, pts: i64, key: bool, data: &[u8]) -> Packet {
        let mut p = Packet::from_data(stream, data.to_vec());
        p.pts = Some(pts);
        p.flags.keyframe = key;
        p
    }

    #[test]
    fn mux_demux_roundtrips_vp9_opus() {
        let mut v = Stream::new(0, CodecId::Vp9);
        v.width = 320;
        v.height = 240;
        let mut a = Stream::new(1, CodecId::Opus);
        a.sample_rate = 48_000;
        a.channels = 2;
        a.time_base = Rational::new(1, 48_000); // audio pts in samples

        let packets = [
            pkt(0, 0, true, &[0x11; 100]),
            pkt(1, 0, true, &[0x22; 40]),
            pkt(1, 960, true, &[0x33; 41]),
            pkt(0, 33, false, &[0x44; 50]),
        ];
        let file = mux_streams(false, &[v, a], &packets).unwrap();
        assert_eq!(probe_mkv(&file), 90);

        let mut dem = MkvDemuxer::new(Box::new(Cursor::new(file)));
        let streams = dem.read_header().unwrap();
        assert_eq!(streams.len(), 2);
        assert_eq!(streams[0].codec_id, CodecId::Vp9);
        assert_eq!((streams[0].width, streams[0].height), (320, 240));
        assert_eq!(streams[1].codec_id, CodecId::Opus);
        assert_eq!(streams[1].sample_rate, 48_000);
        assert_eq!(streams[1].channels, 2);
        assert!(streams[1].extradata.starts_with(b"OpusHead"));

        // Blocks come back in timestamp order with data + keyframes intact.
        let p1 = dem.read_packet().unwrap();
        assert_eq!(p1.data, vec![0x11; 100]);
        assert!(p1.flags.keyframe);
        let p2 = dem.read_packet().unwrap();
        assert_eq!(p2.data, vec![0x22; 40]);
        let p3 = dem.read_packet().unwrap();
        assert_eq!(p3.data, vec![0x33; 41]);
        assert_eq!(p3.pts, Some(20)); // 960 samples @48 kHz = 20 ms
        let p4 = dem.read_packet().unwrap();
        assert_eq!(p4.data, vec![0x44; 50]);
        assert_eq!(p4.pts, Some(33));
        assert!(!p4.flags.keyframe);
    }

    #[test]
    fn mux_demux_roundtrips_h264_as_annexb() {
        let mut v = Stream::new(0, CodecId::H264);
        v.width = 64;
        v.height = 64;
        // Annex-B keyframe: SPS + PPS + IDR slice.
        let mut sample = Vec::new();
        for nal in [
            &[0x67u8, 0xAA, 0xBB][..],
            &[0x68, 0xCC][..],
            &[0x65, 1, 2, 3][..],
        ] {
            sample.extend_from_slice(&[0, 0, 0, 1]);
            sample.extend_from_slice(nal);
        }
        let file = mux_streams(false, &[v], &[pkt(0, 0, true, &sample)]).unwrap();

        let mut dem = MkvDemuxer::new(Box::new(Cursor::new(file)));
        let streams = dem.read_header().unwrap();
        assert_eq!(streams[0].codec_id, CodecId::H264);
        assert!(streams[0].extradata.is_empty()); // normalised to Annex-B
        let p = dem.read_packet().unwrap();
        // Keyframe comes back with SPS/PPS prepended, then the slice.
        assert_eq!(p.data, sample);
    }

    #[test]
    fn webm_refuses_non_webm_codecs() {
        let s = Stream::new(0, CodecId::H264);
        let err = mux_streams(true, &[s], &[]).unwrap_err();
        assert!(err.to_string().contains("webm"), "got: {err}");
    }

    #[test]
    fn mux_demux_roundtrips_subtitles_with_duration() {
        let mut v = Stream::new(0, CodecId::Vp9);
        v.width = 16;
        v.height = 16;
        let s = Stream::new(1, CodecId::Subrip);
        let mut cue = pkt(1, 500, true, b"Hello, Matroska!");
        cue.duration = 1200;
        let file = mux_streams(false, &[v, s], &[pkt(0, 0, true, &[0x11; 10]), cue]).unwrap();

        let mut dem = MkvDemuxer::new(Box::new(Cursor::new(file)));
        let streams = dem.read_header().unwrap();
        assert_eq!(streams[1].codec_id, CodecId::Subrip);
        assert_eq!(streams[1].media_type, MediaType::Subtitle);
        let _video = dem.read_packet().unwrap();
        let p = dem.read_packet().unwrap();
        assert_eq!(p.data, b"Hello, Matroska!");
        assert_eq!(p.pts, Some(500));
        assert_eq!(p.duration, 1200);
    }

    #[test]
    fn vorbis_xiph_private_unpacks_to_rff_packing() {
        // 3 headers, first 300 bytes (lacing needs 255-continuation).
        let h0 = vec![1u8; 300];
        let h1 = vec![2u8; 10];
        let h2 = vec![3u8; 5];
        let mut private = vec![2u8, 255, 45, 10];
        private.extend_from_slice(&h0);
        private.extend_from_slice(&h1);
        private.extend_from_slice(&h2);
        let packed = xiph_to_packed(&private).unwrap();
        assert_eq!(&packed[0..4], &300u32.to_le_bytes());
        assert_eq!(packed.len(), 3 * 4 + 300 + 10 + 5);
        assert_eq!(&packed[4 + 300..4 + 300 + 4], &10u32.to_le_bytes());
    }

    // ---- golden fixtures: streaming must reproduce the buffered demuxer ----

    /// Deterministic byte source for fixture payloads.
    struct Lcg(u64);
    impl Lcg {
        fn next(&mut self) -> u64 {
            self.0 = self
                .0
                .wrapping_mul(6_364_136_223_846_793_005)
                .wrapping_add(1_442_695_040_888_963_407);
            self.0 >> 33
        }
        /// `n` bytes, never zero (so H.264 NALs carry no start-code emulation).
        fn bytes(&mut self, n: usize) -> Vec<u8> {
            (0..n).map(|_| (self.next() as u8) | 0x10).collect()
        }
    }

    fn fnv(data: &[u8]) -> u64 {
        let mut h = 0xcbf2_9ce4_8422_2325u64;
        for &b in data {
            h = (h ^ b as u64).wrapping_mul(0x0100_0000_01b3);
        }
        h
    }

    /// Everything observable about a demux (minus `nb_frames`), as text.
    fn fingerprint(file: Vec<u8>) -> String {
        use std::fmt::Write;
        let mut out = String::new();
        let mut dem = MkvDemuxer::new(Box::new(Cursor::new(file)));
        let streams = match dem.read_header() {
            Ok(s) => s,
            Err(e) => return format!("ERR {e}\n"),
        };
        for s in &streams {
            writeln!(
                out,
                "S{} {:?} {:?} {}x{} {}Hz {}ch {:?} xd={}:{:x} tb={}/{} dur={:?}",
                s.index,
                s.codec_id,
                s.media_type,
                s.width,
                s.height,
                s.sample_rate,
                s.channels,
                s.sample_format,
                s.extradata.len(),
                fnv(&s.extradata),
                s.time_base.num,
                s.time_base.den,
                s.duration
            )
            .unwrap();
        }
        for _ in 0..1_000_000 {
            match dem.read_packet() {
                Ok(p) => writeln!(
                    out,
                    "P{} pts={:?} dts={:?} d={} k={} tb={}/{} {}:{:x}",
                    p.stream_index,
                    p.pts,
                    p.dts,
                    p.duration,
                    p.flags.keyframe,
                    p.time_base.num,
                    p.time_base.den,
                    p.data.len(),
                    fnv(&p.data)
                )
                .unwrap(),
                Err(Error::Eof) => break,
                Err(e) => {
                    writeln!(out, "PERR {e}").unwrap();
                    break;
                }
            }
        }
        out
    }

    fn fx_vp9_opus() -> Vec<u8> {
        let mut r = Lcg(1);
        let mut v = Stream::new(0, CodecId::Vp9);
        v.width = 320;
        v.height = 240;
        let mut a = Stream::new(1, CodecId::Opus);
        a.sample_rate = 48_000;
        a.channels = 2;
        a.time_base = Rational::new(1, 48_000);
        let mut packets = Vec::new();
        for i in 0..90i64 {
            let n = 50 + (r.next() % 5000) as usize;
            packets.push(pkt(0, i * 33, i % 30 == 0, &r.bytes(n)));
        }
        for i in 0..150i64 {
            let n = 40 + (r.next() % 160) as usize;
            let mut p = pkt(1, i * 960, true, &r.bytes(n));
            p.duration = 960;
            packets.push(p);
        }
        mux_streams(false, &[v, a], &packets).unwrap()
    }

    fn fx_h264() -> Vec<u8> {
        let mut r = Lcg(2);
        let mut v = Stream::new(0, CodecId::H264);
        v.width = 64;
        v.height = 48;
        let mut packets = Vec::new();
        for i in 0..60i64 {
            let key = i % 25 == 0;
            let mut sample = Vec::new();
            let mut nals: Vec<Vec<u8>> = Vec::new();
            if key {
                nals.push([&[0x67u8, 0x42, 0x00, 0x1E][..], &r.bytes(6)].concat());
                nals.push([&[0x68u8][..], &r.bytes(3)].concat());
                nals.push([&[0x65u8][..], &r.bytes(900)].concat());
            } else {
                nals.push([&[0x06u8][..], &r.bytes(5)].concat()); // SEI
                let n = 20 + (r.next() % 400) as usize;
                nals.push([&[0x41u8][..], &r.bytes(n)].concat());
            }
            for nal in nals {
                sample.extend_from_slice(&[0, 0, 0, 1]);
                sample.extend_from_slice(&nal);
            }
            packets.push(pkt(0, i * 40, key, &sample));
        }
        mux_streams(false, &[v], &packets).unwrap()
    }

    fn fx_vorbis() -> Vec<u8> {
        let mut r = Lcg(3);
        let mut a = Stream::new(0, CodecId::Vorbis);
        a.sample_rate = 44_100;
        a.channels = 2;
        a.time_base = Rational::new(1, 44_100);
        for h in [r.bytes(30), r.bytes(300), r.bytes(1000)] {
            a.extradata
                .extend_from_slice(&(h.len() as u32).to_le_bytes());
            a.extradata.extend_from_slice(&h);
        }
        let packets: Vec<Packet> = (0..100i64)
            .map(|i| {
                let n = 10 + (r.next() % 300) as usize;
                pkt(0, i * 1024, true, &r.bytes(n))
            })
            .collect();
        mux_streams(true, &[a], &packets).unwrap()
    }

    fn fx_subs_pcm() -> Vec<u8> {
        let mut r = Lcg(4);
        let mut v = Stream::new(0, CodecId::Vp9);
        v.width = 16;
        v.height = 16;
        let s = Stream::new(1, CodecId::Subrip);
        let mut a = Stream::new(2, CodecId::Pcm);
        a.sample_rate = 8_000;
        a.channels = 1;
        a.sample_format = Some(SampleFormat::S16);
        a.time_base = Rational::new(1, 8_000);
        let mut f = Stream::new(3, CodecId::Pcm);
        f.sample_rate = 8_000;
        f.channels = 2;
        f.sample_format = Some(SampleFormat::F32);
        let w = Stream::new(4, CodecId::WebVtt);
        let mut packets = Vec::new();
        for i in 0..40i64 {
            packets.push(pkt(0, i * 250, i % 8 == 0, &r.bytes(30)));
            packets.push(pkt(2, i * 2000, true, &r.bytes(500)));
            packets.push(pkt(3, i * 250, true, &r.bytes(200)));
        }
        for i in 0..6i64 {
            let mut cue = pkt(
                1,
                700 + i * 1500,
                true,
                format!("cue number {i}").as_bytes(),
            );
            cue.duration = 1000 + i * 10;
            packets.push(cue);
            let mut vtt = pkt(4, 900 + i * 1500, true, format!("vtt {i}").as_bytes());
            vtt.duration = 400;
            packets.push(vtt);
        }
        mux_streams(false, &[v, s, a, f, w], &packets).unwrap()
    }

    // Tiny EBML writer for hand-built fixtures (the muxer never emits ASS,
    // unknown sizes, or foreign level-1 elements).
    fn put_id(out: &mut Vec<u8>, id: u32) {
        let n = 4 - (id.leading_zeros() / 8) as usize;
        for i in (0..n).rev() {
            out.push((id >> (8 * i)) as u8);
        }
    }
    fn elem(out: &mut Vec<u8>, id: u32, body: &[u8]) {
        put_id(out, id);
        out.push(0x01); // 8-byte size vint: marker + 7 value bytes
        out.extend_from_slice(&(body.len() as u64).to_be_bytes()[1..]);
        out.extend_from_slice(body);
    }
    /// An unknown-size element header (all-ones 8-byte size).
    fn open_unknown(out: &mut Vec<u8>, id: u32) {
        put_id(out, id);
        out.extend_from_slice(&[0x01, 0xFF, 0xFF, 0xFF, 0xFF, 0xFF, 0xFF, 0xFF]);
    }
    fn uint(out: &mut Vec<u8>, id: u32, v: u64) {
        elem(out, id, &v.to_be_bytes());
    }
    fn block(track: u8, rel: i16, flags: u8, payload: &[u8]) -> Vec<u8> {
        let mut b = vec![0x80 | track];
        b.extend_from_slice(&rel.to_be_bytes());
        b.push(flags);
        b.extend_from_slice(payload);
        b
    }
    fn ebml_header() -> Vec<u8> {
        let mut h = Vec::new();
        elem(&mut h, 0x4282, b"matroska");
        let mut out = Vec::new();
        elem(&mut out, 0x1A45_DFA3, &h);
        out
    }
    fn track_entry(number: u64, ttype: u64, codec: &str, private: &[u8], extra: &[u8]) -> Vec<u8> {
        let mut te = Vec::new();
        uint(&mut te, ID_TRACK_NUMBER, number);
        uint(&mut te, ID_TRACK_TYPE, ttype);
        elem(&mut te, ID_CODEC_ID, codec.as_bytes());
        if !private.is_empty() {
            elem(&mut te, ID_CODEC_PRIVATE, private);
        }
        te.extend_from_slice(extra);
        let mut out = Vec::new();
        elem(&mut out, ID_TRACK_ENTRY, &te);
        out
    }

    /// ASS track + foreign level-1 elements + BlockGroups in both child
    /// orders + blocks the demuxer must drop (laced, unknown track).
    fn fx_ass() -> Vec<u8> {
        let mut seg = Vec::new();
        elem(&mut seg, 0x114D_9B74, &[0xEC, 0x81, 0x00]); // SeekHead
        elem(&mut seg, 0xEC, &[0; 10]); // Void
        let mut info = Vec::new();
        uint(&mut info, ID_TIMESTAMP_SCALE, 1_000_000);
        elem(&mut info, ID_DURATION, &5000.0f64.to_be_bytes());
        elem(&mut seg, ID_INFO, &info);
        let mut tracks = track_entry(1, 17, "S_TEXT/ASS", b"[Script Info]\nTitle: x\n", &[]);
        let mut video = Vec::new();
        uint(&mut video, ID_PIXEL_WIDTH, 32);
        uint(&mut video, ID_PIXEL_HEIGHT, 18);
        let mut vel = Vec::new();
        elem(&mut vel, ID_VIDEO, &video);
        tracks.extend(track_entry(2, 1, "V_VP9", &[], &vel));
        elem(&mut seg, ID_TRACKS, &tracks);
        elem(&mut seg, 0x1254_C367, b"tags-ignored"); // Tags
        let mut c1 = Vec::new();
        uint(&mut c1, ID_TIMESTAMP, 0);
        let mut g = Vec::new();
        elem(
            &mut g,
            ID_BLOCK,
            &block(1, 100, 0, b"0,0,Default,,0,0,0,,Hello, world"),
        );
        uint(&mut g, ID_BLOCK_DURATION, 900);
        elem(&mut c1, ID_BLOCK_GROUP, &g);
        elem(&mut c1, ID_SIMPLE_BLOCK, &block(2, 0, 0x80, &[7; 64]));
        elem(&mut c1, ID_SIMPLE_BLOCK, &block(2, 40, 0x00, &[8; 33]));
        elem(&mut seg, ID_CLUSTER, &c1);
        let mut c2 = Vec::new();
        uint(&mut c2, ID_TIMESTAMP, 2000);
        let mut g = Vec::new();
        uint(&mut g, ID_BLOCK_DURATION, 1500);
        elem(
            &mut g,
            ID_BLOCK,
            &block(1, -5, 0, b"1,0,Default,,0,0,0,,Second, line"),
        );
        elem(&mut c2, ID_BLOCK_GROUP, &g);
        elem(&mut c2, 0xEC, &[0; 4]); // Void inside a cluster
        elem(&mut c2, ID_SIMPLE_BLOCK, &block(2, 10, 0x82, &[9; 20])); // laced
        elem(&mut c2, ID_SIMPLE_BLOCK, &block(9, 10, 0x80, &[9; 20])); // no track
        elem(&mut c2, 0xBF, &[1, 2, 3, 4]); // CRC-32
        elem(&mut c2, ID_SIMPLE_BLOCK, &block(2, 80, 0x80, &[10; 50]));
        elem(&mut seg, ID_CLUSTER, &c2);
        elem(&mut seg, ID_CUES, b"cues-ignored");
        let mut file = ebml_header();
        elem(&mut file, ID_SEGMENT, &seg);
        file
    }

    /// Unknown-size Segment, 0.1 ms TimestampScale, H.264 (avcC) + Opus, a
    /// known-size Cluster then a final unknown-size one trailed by Cues/Tags.
    fn fx_unknown_tail() -> Vec<u8> {
        let mut r = Lcg(5);
        let mut file = ebml_header();
        open_unknown(&mut file, ID_SEGMENT);
        let mut info = Vec::new();
        uint(&mut info, ID_TIMESTAMP_SCALE, 100_000);
        elem(&mut file, ID_INFO, &info);
        let sps = [0x67u8, 0x64, 0x00, 0x1F, 0xAC];
        let pps = [0x68u8, 0xEE, 0x3C];
        let avcc = rff_format::avc::build_avcc_record(&sps, &pps);
        let mut tracks = track_entry(1, 1, "V_MPEG4/ISO/AVC", &avcc, &[]);
        let mut audio = Vec::new();
        elem(
            &mut audio,
            ID_SAMPLING_FREQUENCY,
            &48_000.0f64.to_be_bytes(),
        );
        uint(&mut audio, ID_CHANNELS, 2);
        let mut ael = Vec::new();
        elem(&mut ael, ID_AUDIO, &audio);
        tracks.extend(track_entry(2, 2, "A_OPUS", &opus_head_bytes(), &ael));
        elem(&mut file, ID_TRACKS, &tracks);
        let avcc_frame = |r: &mut Lcg, kind: u8, n: usize| {
            let nal = [&[kind][..], &r.bytes(n)].concat();
            let mut f = (nal.len() as u32).to_be_bytes().to_vec();
            f.extend_from_slice(&nal);
            f
        };
        let mut c1 = Vec::new();
        uint(&mut c1, ID_TIMESTAMP, 0);
        let f = avcc_frame(&mut r, 0x65, 300);
        elem(&mut c1, ID_SIMPLE_BLOCK, &block(1, 0, 0x80, &f));
        elem(&mut c1, ID_SIMPLE_BLOCK, &block(2, 0, 0x80, &r.bytes(80)));
        let f = avcc_frame(&mut r, 0x41, 120);
        elem(&mut c1, ID_SIMPLE_BLOCK, &block(1, 400, 0x00, &f));
        elem(&mut file, ID_CLUSTER, &c1);
        open_unknown(&mut file, ID_CLUSTER);
        uint(&mut file, ID_TIMESTAMP, 800);
        let f = avcc_frame(&mut r, 0x65, 200);
        elem(&mut file, ID_SIMPLE_BLOCK, &block(1, 0, 0x80, &f));
        elem(
            &mut file,
            ID_SIMPLE_BLOCK,
            &block(2, 200, 0x80, &r.bytes(90)),
        );
        let f = avcc_frame(&mut r, 0x41, 50);
        elem(&mut file, ID_SIMPLE_BLOCK, &block(1, 400, 0x00, &f));
        elem(&mut file, ID_CUES, b"cues");
        elem(&mut file, 0x1254_C367, b"tags");
        file
    }

    fn opus_head_bytes() -> Vec<u8> {
        let mut v = b"OpusHead".to_vec();
        v.extend_from_slice(&[1, 2, 0, 0]);
        v.extend_from_slice(&48_000u32.to_le_bytes());
        v.extend_from_slice(&[0, 0, 0]);
        v
    }

    /// The named fixture set: full files plus truncations at fixed distances
    /// from the end (stable against muxer header-size changes) or fixed
    /// offsets (hand-built files).
    fn golden_cases() -> Vec<(String, Vec<u8>)> {
        let mut cases = vec![
            ("vp9_opus".to_string(), fx_vp9_opus()),
            ("h264".to_string(), fx_h264()),
            ("vorbis".to_string(), fx_vorbis()),
            ("subs_pcm".to_string(), fx_subs_pcm()),
            ("ass".to_string(), fx_ass()),
            ("unknown_tail".to_string(), fx_unknown_tail()),
        ];
        let h264 = fx_h264();
        let subs = fx_subs_pcm();
        for k in [1usize, 37, 500, 2_000, 9_000] {
            cases.push((format!("h264-{k}"), h264[..h264.len() - k].to_vec()));
            cases.push((format!("subs_pcm-{k}"), subs[..subs.len() - k].to_vec()));
        }
        for name in ["ass", "unknown_tail"] {
            let f = if name == "ass" {
                fx_ass()
            } else {
                fx_unknown_tail()
            };
            for cut in (7..f.len()).step_by(61) {
                cases.push((format!("{name}@{cut}"), f[..cut].to_vec()));
            }
        }
        cases
    }

    /// `(case, packet-line count, fnv of the fingerprint)` recorded from the
    /// pre-streaming (whole-file buffered) demuxer.
    const GOLDENS: &[(&str, usize, u64)] = &[
        ("vp9_opus", 240, 0x96842ed96f2e9d29),
        ("h264", 60, 0xbda95d370f6a1c36),
        ("vorbis", 100, 0x67050340b1055ab7),
        ("subs_pcm", 132, 0x86965869a4a8cce6),
        ("ass", 5, 0x11f42feb0e81cfe4),
        ("unknown_tail", 6, 0xc411396fdf4e6dd7),
        ("h264-1", 60, 0xbda95d370f6a1c36),
        ("subs_pcm-1", 132, 0x86965869a4a8cce6),
        ("h264-37", 60, 0xbda95d370f6a1c36),
        ("subs_pcm-37", 132, 0x86965869a4a8cce6),
        ("h264-500", 58, 0x417368a5665708f2),
        ("subs_pcm-500", 131, 0x11b3aba76bfd6b79),
        ("h264-2000", 53, 0x46d8ad95299d739e),
        ("subs_pcm-2000", 125, 0x7ebec31897b551c3),
        ("h264-9000", 26, 0x82efca5dd86e42d1),
        ("subs_pcm-9000", 96, 0xe54efbb2a507a4f9),
        ("ass@7", 0, 0xb88f458891534654),
        ("ass@68", 0, 0xb88f458891534654),
        ("ass@129", 0, 0xb88f458891534654),
        ("ass@190", 0, 0x9db95c26a859bb7a),
        ("ass@251", 0, 0x4ebbedf20cdfd5d0),
        ("ass@312", 0, 0x7602000005937fb4),
        ("ass@373", 0, 0x372ea377fd90c6a2),
        ("ass@434", 1, 0x77f3fa8608a82be8),
        ("ass@495", 2, 0xb9002d93649ef0d3),
        ("ass@556", 3, 0x8561e8a38db8e3c7),
        ("ass@617", 3, 0x20df468e3a3d3675),
        ("ass@678", 4, 0x371bc2ff3b87d922),
        ("ass@739", 4, 0xc3afca0a9bc477cf),
        ("ass@800", 5, 0xe9fe7a222f2c3334),
        ("unknown_tail@7", 0, 0xb88f458891534654),
        ("unknown_tail@68", 0, 0xb88f458891534654),
        ("unknown_tail@129", 0, 0x19e5d80fc8c27d2f),
        ("unknown_tail@190", 0, 0xe16a82a06e5f72c7),
        ("unknown_tail@251", 0, 0x794321a15a45a76a),
        ("unknown_tail@312", 0, 0x1a3e72b070089f79),
        ("unknown_tail@373", 1, 0xbe5de860616bcfe8),
        ("unknown_tail@434", 1, 0xbe5de860616bcfe8),
        ("unknown_tail@495", 1, 0xbe5de860616bcfe8),
        ("unknown_tail@556", 1, 0xbe5de860616bcfe8),
        ("unknown_tail@617", 1, 0xbe5de860616bcfe8),
        ("unknown_tail@678", 2, 0x501963effcb0005e),
        ("unknown_tail@739", 2, 0x25d078defe7f1968),
        ("unknown_tail@800", 3, 0x1c434549715436a5),
        ("unknown_tail@861", 3, 0x1c434549715436a5),
        ("unknown_tail@922", 3, 0xb0138b7ec7bd1d1f),
        ("unknown_tail@983", 4, 0x84ce6bf5667e7746),
        ("unknown_tail@1044", 4, 0x84ce6bf5667e7746),
        ("unknown_tail@1105", 4, 0x84ce6bf5667e7746),
        ("unknown_tail@1166", 5, 0x9593526d20d414c5),
        ("unknown_tail@1227", 5, 0x7dcecbffea06f04e),
        ("unknown_tail@1288", 6, 0x72a1fc073c99894c),
    ];

    #[test]
    fn streaming_matches_buffered_goldens() {
        let mut missing = String::new();
        for (name, file) in golden_cases() {
            let fp = fingerprint(file);
            let n = fp.lines().filter(|l| l.starts_with('P')).count();
            let h = fnv(fp.as_bytes());
            match GOLDENS.iter().find(|g| g.0 == name) {
                Some(&(_, gn, gh)) => assert_eq!((n, h), (gn, gh), "{name}:\n{fp}"),
                None => missing.push_str(&format!("    (\"{name}\", {n}, 0x{h:016x}),\n")),
            }
        }
        assert!(missing.is_empty(), "missing goldens:\n{missing}");
    }

    /// Counts the bytes the demuxer pulls from its input.
    struct Counting<R> {
        inner: R,
        read: Arc<std::sync::atomic::AtomicU64>,
    }
    impl<R: Read> Read for Counting<R> {
        fn read(&mut self, buf: &mut [u8]) -> std::io::Result<usize> {
            let n = self.inner.read(buf)?;
            self.read
                .fetch_add(n as u64, std::sync::atomic::Ordering::Relaxed);
            Ok(n)
        }
    }

    #[test]
    fn read_header_reads_only_a_prefix() {
        use std::sync::atomic::Ordering;
        let mut r = Lcg(9);
        let mut v = Stream::new(0, CodecId::Vp9);
        v.width = 640;
        v.height = 360;
        let packets: Vec<Packet> = (0..200i64)
            .map(|i| pkt(0, i * 40, i % 50 == 0, &r.bytes(20_000)))
            .collect();
        let file = mux_streams(false, &[v], &packets).unwrap();
        assert!(file.len() > 4_000_000);
        let read = Arc::new(std::sync::atomic::AtomicU64::new(0));
        let input = Counting {
            inner: Cursor::new(file.clone()),
            read: read.clone(),
        };
        let mut dem = MkvDemuxer::new(Box::new(input));
        let streams = dem.read_header().unwrap();
        assert_eq!((streams[0].width, streams[0].height), (640, 360));
        assert_eq!(streams[0].nb_frames, None); // not knowable without the payload
        let after_header = read.load(Ordering::Relaxed);
        assert!(
            after_header <= READ_BUF as u64,
            "read_header pulled {after_header} of {} bytes",
            file.len()
        );
        // Streaming: each packet costs about one block of read-ahead.
        let p = dem.read_packet().unwrap();
        assert_eq!(p.data, packets[0].data);
        assert!(read.load(Ordering::Relaxed) <= 2 * READ_BUF as u64);
        let mut n = 1;
        while let Ok(p) = dem.read_packet() {
            assert_eq!(p.data, packets[n].data);
            n += 1;
        }
        assert_eq!(n, 200);
        assert_eq!(read.load(Ordering::Relaxed), file.len() as u64);
    }

    /// The H.264 + Opus tracks of [`fx_unknown_tail`], with no Clusters yet.
    fn unknown_segment_prefix() -> Vec<u8> {
        let mut f = fx_unknown_tail();
        let mut cluster = Vec::new();
        put_id(&mut cluster, ID_CLUSTER);
        let at = f
            .windows(4)
            .position(|w| w == &cluster[..])
            .expect("fixture has a cluster");
        f.truncate(at);
        f
    }

    #[test]
    fn unknown_size_clusters_end_at_level1_elements() {
        let mut f = unknown_segment_prefix();
        // Unknown-size cluster, a Void at segment level is impossible to tell
        // from a cluster child, so use Tags (level 1) to end the first one.
        open_unknown(&mut f, ID_CLUSTER);
        uint(&mut f, ID_TIMESTAMP, 100);
        elem(&mut f, ID_SIMPLE_BLOCK, &block(2, 0, 0x80, b"a0"));
        elem(&mut f, ID_SIMPLE_BLOCK, &block(2, 5, 0x80, b"a1"));
        elem(&mut f, ID_TAGS, b"tags between clusters");
        // A second unknown-size cluster ended directly by a third (known).
        open_unknown(&mut f, ID_CLUSTER);
        uint(&mut f, ID_TIMESTAMP, 200);
        elem(&mut f, ID_SIMPLE_BLOCK, &block(2, 1, 0x80, b"b0"));
        let mut c = Vec::new();
        uint(&mut c, ID_TIMESTAMP, 300);
        elem(&mut c, ID_SIMPLE_BLOCK, &block(2, 2, 0x80, b"c0"));
        elem(&mut f, ID_CLUSTER, &c);
        // Unknown-size again, running to EOF (the Segment is unknown-size).
        open_unknown(&mut f, ID_CLUSTER);
        uint(&mut f, ID_TIMESTAMP, 400);
        elem(&mut f, ID_SIMPLE_BLOCK, &block(2, 3, 0x80, b"d0"));

        let mut dem = MkvDemuxer::new(Box::new(Cursor::new(f)));
        let streams = dem.read_header().unwrap();
        assert_eq!(streams.len(), 2);
        assert_eq!(streams[1].time_base, Rational::new(1, 10_000));
        let mut got = Vec::new();
        loop {
            match dem.read_packet() {
                Ok(p) => got.push((p.stream_index, p.pts.unwrap(), p.data)),
                Err(Error::Eof) => break,
                Err(e) => panic!("{e}"),
            }
        }
        let want: Vec<(usize, i64, Vec<u8>)> = [
            (100, "a0"),
            (105, "a1"),
            (201, "b0"),
            (302, "c0"),
            (403, "d0"),
        ]
        .iter()
        .map(|&(pts, d)| (1, pts, d.as_bytes().to_vec()))
        .collect();
        assert_eq!(got, want);
        assert!(matches!(dem.read_packet(), Err(Error::Eof))); // stays at EOF
    }

    #[test]
    fn known_size_segment_ignores_trailing_bytes() {
        let mut file = fx_ass();
        let packets = fingerprint(file.clone());
        // A second Segment (and junk) after the first one's end is not read.
        let mut extra = Vec::new();
        let mut c = Vec::new();
        uint(&mut c, ID_TIMESTAMP, 0);
        elem(&mut c, ID_SIMPLE_BLOCK, &block(2, 0, 0x80, b"after"));
        elem(&mut extra, ID_CLUSTER, &c);
        elem(&mut file, ID_SEGMENT, &extra);
        file.extend_from_slice(&[0xA3, 0x85, 1, 2, 3]);
        assert_eq!(fingerprint(file), packets);
    }

    #[test]
    fn oversized_declarations_do_not_allocate_or_hang() {
        // Tracks declaring 1 GiB (past the header cap) is skipped: no tracks.
        let mut f = ebml_header();
        open_unknown(&mut f, ID_SEGMENT);
        put_id(&mut f, ID_TRACKS);
        f.extend_from_slice(&[0x01, 0, 0, 0, 0x40, 0, 0, 0]);
        f.extend_from_slice(&track_entry(1, 1, "V_VP9", &[], &[]));
        let mut dem = MkvDemuxer::new(Box::new(Cursor::new(f)));
        let err = dem.read_header().unwrap_err();
        assert!(err.to_string().contains("no tracks"), "{err}");
        assert!(dem.read_header().is_err()); // and stays failed

        // A SimpleBlock declaring ~2^55 bytes, or a Timestamp declaring 2^40,
        // over a few real bytes: each is streamed past (no buffer of the
        // declared size) and the walk ends at EOF, quickly.
        for (id, size) in [
            (ID_SIMPLE_BLOCK, [0x01, 0x7F, 0, 0, 0, 0, 0, 0]),
            (ID_TIMESTAMP, [0x01, 0, 0x01, 0, 0, 0, 0, 0]),
        ] {
            let mut f = unknown_segment_prefix();
            open_unknown(&mut f, ID_CLUSTER);
            elem(&mut f, ID_SIMPLE_BLOCK, &block(2, 0, 0x80, b"ok"));
            put_id(&mut f, id);
            f.extend_from_slice(&size);
            f.extend_from_slice(&block(2, 0, 0x80, &[1; 100]));
            elem(&mut f, ID_SIMPLE_BLOCK, &block(2, 1, 0x80, b"unreachable"));
            let fp = fingerprint(f);
            assert_eq!(fp.lines().filter(|l| l.starts_with('P')).count(), 1, "{fp}");
        }
    }

    #[test]
    fn truncated_and_garbage_inputs_terminate_without_panicking() {
        let bounded = |file: Vec<u8>| {
            let len = file.len();
            let fp = fingerprint(file);
            let n = fp.lines().filter(|l| l.starts_with('P')).count();
            assert!(n <= len, "{n} packets from {len} bytes");
        };
        // Every prefix of the hand-built files, a stride through a muxed one.
        for f in [fx_ass(), fx_unknown_tail()] {
            for cut in 0..=f.len() {
                bounded(f[..cut].to_vec());
            }
        }
        let subs = fx_subs_pcm();
        for cut in (0..subs.len()).step_by(97) {
            bounded(subs[..cut].to_vec());
        }
        // Byte flips through valid files.
        let mut r = Lcg(77);
        for base in [fx_ass(), fx_unknown_tail(), fx_h264()] {
            for _ in 0..300 {
                let mut f = base.clone();
                for _ in 0..1 + r.next() % 4 {
                    let at = (r.next() as usize) % f.len();
                    f[at] = r.next() as u8;
                }
                bounded(f);
            }
        }
        // Pure garbage, bare and behind an EBML header + unknown Segment.
        for i in 0..300usize {
            let mut g = r.bytes(i * 7 % 2000);
            for b in g.iter_mut().step_by(3) {
                *b = r.next() as u8; // let zero bytes in too
            }
            bounded(g.clone());
            let mut f = ebml_header();
            open_unknown(&mut f, ID_SEGMENT);
            f.extend_from_slice(&g);
            bounded(f);
            let mut f = unknown_segment_prefix();
            open_unknown(&mut f, ID_CLUSTER);
            f.extend_from_slice(&g);
            bounded(f);
        }
    }
}
