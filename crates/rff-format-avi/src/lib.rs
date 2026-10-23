//! AVI (Audio Video Interleaved) container.
//!
//! AVI is a RIFF file: a tree of `LIST`/chunk records. Layout we read:
//!
//! ```text
//!   RIFF "AVI "
//!     LIST "hdrl"          stream headers
//!       "avih"             main header (frame size, stream count)
//!       LIST "strl"        one per stream
//!         "strh"           stream header (type, codec fourcc, rate)
//!         "strf"           stream format (BITMAPINFOHEADER / WAVEFORMATEX)
//!     LIST "movi"          interleaved data chunks ("##dc", "##wb", ...)
//!     "idx1"               (optional) legacy index — not needed to demux
//! ```
//!
//! The demuxer streams from a plain `Read` (pipes work): opening a file reads
//! only up to `movi`, and each packet is read on demand, so memory is bounded
//! by the header and the largest packet — never the file size.
//!
//! The muxer streams too when its output can seek (a local file): it writes
//! the header with zero totals, each packet's chunk as it arrives, then
//! `idx1`, and seeks back to patch the sizes and frame counts, keeping only
//! the index (16 bytes a packet) in memory. Over a pipe it buffers `movi`
//! until the trailer; both paths write the same bytes. Files are plain AVI
//! 1.0 (no OpenDML), so the 32-bit sizes cannot describe a file past 4 GiB.
//!
//! Note: AVI carries codecs we don't decode yet (e.g. H.264), so a full
//! transcode out of an AVI waits on those codec bodies — but probing and
//! stream-copy remuxing work today.

use std::io::{self, BufReader, Read, Seek, SeekFrom, Write};
use std::ops::Range;

use rff_core::{CodecId, Error, MediaType, Packet, PixelFormat, Rational, Result, SampleFormat};
use rff_format::{
    Demuxer, Format, FormatRegistry, Input, MuxCaps, Muxer, Output, SeekOutput, Stream,
};

/// Register the AVI format into a [`FormatRegistry`].
pub fn register(registry: &mut FormatRegistry) {
    registry.register(Format {
        name: "avi",
        long_name: "AVI (Audio Video Interleaved)",
        extensions: &["avi"],
        demuxer: Some(|input| Box::new(AviDemuxer::new(input))),
        muxer: Some(|output| Box::new(AviMuxer::new(output))),
        muxer_path: None,
        probe: Some(probe_avi),
        mux_caps: MuxCaps::container(&[
            CodecId::H264,
            CodecId::Jpeg,
            CodecId::Avif,
            CodecId::RawVideo,
            CodecId::Pcm,
        ]),
    });
    // Files stream `movi` and patch the header's totals at the end (#28);
    // pipes keep the buffered muxer above.
    registry.register_seekable_muxer("avi", |output| Box::new(AviMuxer::new_seekable(output)));
}

/// Sniff AVI: a RIFF file whose form type is `AVI `.
fn probe_avi(data: &[u8]) -> i32 {
    if data.len() >= 12 && &data[0..4] == b"RIFF" && &data[8..12] == b"AVI " {
        100
    } else {
        0
    }
}

// ===========================================================================
// RIFF parsing helpers
// ===========================================================================

fn rd_u16(buf: &[u8], at: usize) -> u16 {
    buf.get(at..at + 2)
        .map(|b| u16::from_le_bytes([b[0], b[1]]))
        .unwrap_or(0)
}

fn rd_u32(buf: &[u8], at: usize) -> u32 {
    buf.get(at..at + 4)
        .map(|b| u32::from_le_bytes([b[0], b[1], b[2], b[3]]))
        .unwrap_or(0)
}

fn fourcc(buf: &[u8], at: usize) -> [u8; 4] {
    let mut f = [0u8; 4];
    if let Some(s) = buf.get(at..at + 4) {
        f.copy_from_slice(s);
    }
    f
}

/// Parse sibling RIFF chunks in `buf[start..end]` into `(id, data_range)` pairs.
/// Chunk = `[4cc id][u32 LE size][size bytes]`, padded to an even boundary.
fn riff_chunks(buf: &[u8], mut p: usize, end: usize) -> Vec<([u8; 4], Range<usize>)> {
    let mut out = Vec::new();
    while p + 8 <= end {
        let id = fourcc(buf, p);
        let size = rd_u32(buf, p + 4) as usize;
        let data_start = p + 8;
        let data_end = data_start.saturating_add(size);
        if data_end > end {
            break; // truncated chunk
        }
        out.push((id, data_start..data_end));
        // Chunks are word-aligned: an odd size carries one pad byte.
        p = data_end + (size & 1);
    }
    out
}

/// Map a video codec fourcc (from `strf`/`strh`) to a [`CodecId`].
fn map_video_fourcc(mut f: [u8; 4]) -> CodecId {
    f.make_ascii_uppercase();
    match &f {
        b"H264" | b"X264" | b"AVC1" | b"DAVC" => CodecId::H264,
        b"AV01" => CodecId::Avif,
        // Motion JPEG. `MJPG` is what FFmpeg and virtually every camera write;
        // the rest are the long tail of encoder-specific tags for the same
        // baseline-JPEG-per-frame payload.
        b"MJPG" | b"JPEG" | b"MJPA" | b"MJPB" | b"AVRN" | b"DMB1" | b"LJPG" => CodecId::Jpeg,
        _ => CodecId::None,
    }
}

/// Map a WAVE `(format_tag, bits)` to a PCM [`SampleFormat`] (1=int, 3=float).
fn pcm_format(tag: u16, bits: u16) -> Option<SampleFormat> {
    match (tag, bits) {
        (1, 16) => Some(SampleFormat::S16),
        (3, 32) => Some(SampleFormat::F32),
        _ => None,
    }
}

// ===========================================================================
// Demuxer
// ===========================================================================

/// Largest `hdrl` list we buffer. Real headers are a few KiB (OpenDML super
/// indexes push that to tens of KiB); anything near this is corrupt, and the
/// cap keeps a hostile size field from turning into a giant read.
const MAX_HEADER_LIST: u64 = 16 << 20;

/// Streams an AVI from a plain [`Read`] — nothing seeks, so pipes work.
///
/// [`read_header`](Demuxer::read_header) reads up to the `movi` list: the
/// `hdrl` list is buffered (it is small and parsed as a whole) and every other
/// top-level chunk is skipped without being kept.
/// [`read_packet`](Demuxer::read_packet) then reads one `movi` chunk at a
/// time, so peak memory is the largest packet, not the file. Nothing after
/// `movi` (`idx1`, OpenDML `RIFF AVIX` extensions) is read: packets carry no
/// index-derived flags or timestamps.
struct AviDemuxer {
    reader: BufReader<Input>,
    header_read: bool,
    /// Bytes left in the `movi` list body. `None` before the header, when the
    /// file has no `movi`, and once the walk is over.
    movi_left: Option<u64>,
    /// The `rec ` interleave group being walked, if any.
    rec: Option<RecGroup>,
}

/// An open `LIST rec ` group inside `movi`.
struct RecGroup {
    /// Bytes left in the group body.
    left: u64,
    /// The group's declared size is odd: a pad byte follows its body.
    odd: bool,
}

/// Outcome of reading one child chunk of a list.
enum Step {
    Packet(Packet),
    /// A chunk that is not a packet (`ix##`, `JUNK`, a nested list) was skipped.
    Skipped,
    /// The list (or the input) ended.
    End,
}

impl AviDemuxer {
    fn new(input: Input) -> AviDemuxer {
        AviDemuxer {
            reader: BufReader::new(input),
            header_read: false,
            movi_left: None,
            rec: None,
        }
    }

    /// Stop the `movi` walk: every later `read_packet` is `Eof`.
    fn finish(&mut self) {
        self.movi_left = None;
        self.rec = None;
    }

    /// The next packet from `movi`, or `None` once it (or the input) ends.
    ///
    /// Every iteration either consumes at least an 8-byte chunk header or
    /// returns, so a malformed file cannot spin.
    fn next_packet(&mut self) -> io::Result<Option<Packet>> {
        let r = &mut self.reader;
        loop {
            let Some(mut movi_left) = self.movi_left else {
                return Ok(None);
            };

            if let Some(mut rec) = self.rec.take() {
                match read_child(r, &mut rec.left)? {
                    Step::Packet(p) => {
                        self.rec = Some(rec);
                        return Ok(Some(p));
                    }
                    Step::Skipped => self.rec = Some(rec),
                    Step::End => {
                        // Resume `movi` where the group ends, then its pad.
                        if !skip(r, rec.left)? {
                            return Ok(None);
                        }
                        if rec.odd && movi_left > 0 {
                            movi_left -= 1;
                            self.movi_left = Some(movi_left);
                            if !skip(r, 1)? {
                                return Ok(None);
                            }
                        }
                    }
                }
                continue;
            }

            if movi_left < 8 {
                return Ok(None);
            }
            let Some((id, size)) = read_chunk_header(r)? else {
                return Ok(None);
            };
            movi_left -= 8;
            if size > movi_left {
                return Ok(None); // overruns `movi`: the walk ends here
            }
            if &id == b"LIST" && size >= 4 {
                // Interleave group: its children are the real data chunks.
                let mut list_type = [0u8; 4];
                if !read_full(r, &mut list_type)? {
                    return Ok(None);
                }
                movi_left -= size;
                self.movi_left = Some(movi_left);
                if &list_type == b"rec " {
                    self.rec = Some(RecGroup {
                        left: size - 4,
                        odd: size & 1 == 1,
                    });
                    continue;
                }
                // Any other list is not data.
                if !skip(r, size - 4)? || !skip_pad(r, size, &mut movi_left)? {
                    return Ok(None);
                }
                self.movi_left = Some(movi_left);
                continue;
            }
            let step = finish_child(r, id, size, &mut movi_left)?;
            self.movi_left = Some(movi_left);
            match step {
                Step::Packet(p) => return Ok(Some(p)),
                Step::Skipped => {}
                Step::End => return Ok(None),
            }
        }
    }
}

impl Demuxer for AviDemuxer {
    fn read_header(&mut self) -> Result<Vec<Stream>> {
        if self.header_read {
            return Err(Error::invalid("avi demux: header already read"));
        }
        self.header_read = true;
        let r = &mut self.reader;

        let mut riff = [0u8; 12];
        if !read_full(r, &mut riff)? || &riff[0..4] != b"RIFF" || &riff[8..12] != b"AVI " {
            return Err(Error::invalid("avi demux: not a RIFF/AVI file"));
        }

        // Walk the top level up to `movi` (the RIFF size is not trusted),
        // keeping the first `hdrl` body and skipping everything else. A chunk
        // cut short by the end of input ends the walk.
        let mut hdrl: Option<Vec<u8>> = None;
        while let Some((id, size)) = read_chunk_header(r)? {
            if &id == b"LIST" && size >= 4 {
                let mut list_type = [0u8; 4];
                if !read_full(r, &mut list_type)? {
                    break;
                }
                let body = size - 4;
                if &list_type == b"movi" {
                    self.movi_left = Some(body);
                    break;
                }
                if &list_type == b"hdrl" && hdrl.is_none() {
                    if body > MAX_HEADER_LIST {
                        return Err(Error::invalid("avi demux: `hdrl` list too large"));
                    }
                    match read_body(r, body)? {
                        Some(b) => hdrl = Some(b),
                        None => break,
                    }
                } else if !skip(r, body)? {
                    break;
                }
            } else if !skip(r, size)? {
                break;
            }
            if size & 1 == 1 && !skip(r, 1)? {
                break;
            }
        }

        let Some(buf) = hdrl else {
            self.finish();
            return Err(Error::invalid("avi demux: no `hdrl` list before `movi`"));
        };
        let hdrl_children = riff_chunks(&buf, 0, buf.len());

        // Main header gives the frame size shared by the video stream(s).
        let (frame_w, frame_h) = match find(&hdrl_children, b"avih") {
            Some(r) => (rd_u32(&buf, r.start + 32), rd_u32(&buf, r.start + 36)),
            None => (0, 0),
        };

        // One stream per `strl` list.
        let mut streams = Vec::new();
        for (id, r) in &hdrl_children {
            if id != b"LIST" || r.len() < 4 || &buf[r.start..r.start + 4] != b"strl" {
                continue;
            }
            if let Some(stream) =
                parse_strl(&buf, r.start + 4, r.end, streams.len(), frame_w, frame_h)
            {
                streams.push(stream);
            }
        }

        Ok(streams)
    }

    fn read_packet(&mut self) -> Result<Packet> {
        match self.next_packet() {
            Ok(Some(p)) => Ok(p),
            Ok(None) => {
                self.finish();
                Err(Error::Eof)
            }
            Err(e) => {
                self.finish();
                Err(e.into())
            }
        }
    }
}

fn find(chunks: &[([u8; 4], Range<usize>)], id: &[u8; 4]) -> Option<Range<usize>> {
    chunks.iter().find(|(i, _)| i == id).map(|(_, r)| r.clone())
}

/// Build a [`Stream`] from one `strl` list (its `strh` + `strf`).
fn parse_strl(
    buf: &[u8],
    start: usize,
    end: usize,
    index: usize,
    frame_w: u32,
    frame_h: u32,
) -> Option<Stream> {
    let children = riff_chunks(buf, start, end);
    let strh = find(&children, b"strh")?;
    let strf = find(&children, b"strf");

    let fcc_type = fourcc(buf, strh.start); // 'vids' / 'auds'
    let fcc_handler = fourcc(buf, strh.start + 4);
    let scale = rd_u32(buf, strh.start + 20);
    let rate = rd_u32(buf, strh.start + 24);
    let time_base = if rate != 0 {
        Rational::new(scale.max(1) as i32, rate as i32)
    } else {
        Rational::new(1, 1000)
    };

    let mut stream = match &fcc_type {
        b"vids" => {
            // Codec fourcc: prefer biCompression (strf+16), else the handler.
            let codec_fourcc = strf
                .as_ref()
                .map(|r| fourcc(buf, r.start + 16))
                .filter(|f| f != &[0; 4])
                .unwrap_or(fcc_handler);
            let mut s = Stream::new(index, map_video_fourcc(codec_fourcc));
            s.media_type = MediaType::Video;
            s.width = frame_w;
            s.height = frame_h;
            s
        }
        b"auds" => {
            let mut s = Stream::new(index, CodecId::None);
            s.media_type = MediaType::Audio;
            if let Some(r) = &strf {
                s.channels = rd_u16(buf, r.start + 2);
                s.sample_rate = rd_u32(buf, r.start + 4);
                // WAVEFORMATEX: wFormatTag@0, wBitsPerSample@14 → recognize PCM.
                if let Some(fmt) = pcm_format(rd_u16(buf, r.start), rd_u16(buf, r.start + 14)) {
                    s.codec_id = CodecId::Pcm;
                    s.sample_format = Some(fmt);
                }
            }
            s
        }
        _ => return None, // subtitle/data streams: skip for now
    };
    stream.time_base = time_base;
    Some(stream)
}

// ===========================================================================
// Streaming RIFF helpers
// ===========================================================================

/// `read_exact` that reports end of input as `Ok(false)`.
fn read_full<R: Read>(r: &mut R, buf: &mut [u8]) -> io::Result<bool> {
    match r.read_exact(buf) {
        Ok(()) => Ok(true),
        Err(e) if e.kind() == io::ErrorKind::UnexpectedEof => Ok(false),
        Err(e) => Err(e),
    }
}

/// Read a chunk header `[4cc id][u32 LE size]`; `None` at end of input.
fn read_chunk_header<R: Read>(r: &mut R) -> io::Result<Option<([u8; 4], u64)>> {
    let mut h = [0u8; 8];
    if !read_full(r, &mut h)? {
        return Ok(None);
    }
    let id = [h[0], h[1], h[2], h[3]];
    let size = u32::from_le_bytes([h[4], h[5], h[6], h[7]]) as u64;
    Ok(Some((id, size)))
}

/// Discard `n` bytes; `false` if the input ended first.
fn skip<R: Read>(r: &mut R, n: u64) -> io::Result<bool> {
    Ok(io::copy(&mut r.by_ref().take(n), &mut io::sink())? == n)
}

/// Read exactly `n` bytes, allocating only as data actually arrives (a lying
/// size field cannot force a large allocation); `None` if the input ended.
fn read_body<R: Read>(r: &mut R, n: u64) -> io::Result<Option<Vec<u8>>> {
    let mut v = Vec::new();
    r.by_ref().take(n).read_to_end(&mut v)?;
    Ok((v.len() as u64 == n).then_some(v))
}

/// Consume the word-alignment pad byte after an odd-sized chunk — but only if
/// the enclosing list still covers it. Otherwise the parent resumes where the
/// chunk's data ended, exactly as the slice walker [`riff_chunks`] does.
fn skip_pad<R: Read>(r: &mut R, size: u64, left: &mut u64) -> io::Result<bool> {
    if size & 1 == 1 && *left > 0 {
        *left -= 1;
        return skip(r, 1);
    }
    Ok(true)
}

/// Read the next child of a list that has `left` body bytes remaining. Lists
/// nested here (inside a `rec ` group) are skipped like any non-data chunk.
fn read_child<R: Read>(r: &mut R, left: &mut u64) -> io::Result<Step> {
    if *left < 8 {
        return Ok(Step::End);
    }
    let Some((id, size)) = read_chunk_header(r)? else {
        return Ok(Step::End);
    };
    *left -= 8;
    if size > *left {
        return Ok(Step::End); // overruns the list: it ends here
    }
    finish_child(r, id, size, left)
}

/// With a child's header read (and `size <= *left` checked), turn a `##xx`
/// data chunk into a packet — the first two ASCII digits of the id are the
/// stream index — or skip any other chunk (`ix##`, `JUNK`, ...), then consume
/// its pad byte. A chunk cut short by the end of input is dropped (`End`).
fn finish_child<R: Read>(r: &mut R, id: [u8; 4], size: u64, left: &mut u64) -> io::Result<Step> {
    *left -= size;
    let step = if id[0].is_ascii_digit() && id[1].is_ascii_digit() {
        let Some(data) = read_body(r, size)? else {
            return Ok(Step::End);
        };
        let stream_index = ((id[0] - b'0') * 10 + (id[1] - b'0')) as usize;
        Step::Packet(Packet::from_data(stream_index, data))
    } else {
        if !skip(r, size)? {
            return Ok(Step::End);
        }
        Step::Skipped
    };
    // A pad byte missing at end of input just makes the next read end.
    skip_pad(r, size, left)?;
    Ok(step)
}

// ===========================================================================
// RIFF writing helpers
// ===========================================================================

fn put_u16(buf: &mut [u8], at: usize, v: u16) {
    buf[at..at + 2].copy_from_slice(&v.to_le_bytes());
}

fn put_u32(buf: &mut [u8], at: usize, v: u32) {
    buf[at..at + 4].copy_from_slice(&v.to_le_bytes());
}

/// Append a chunk `[4cc id][u32 LE size][data]`, padded to an even boundary.
fn put_chunk(out: &mut Vec<u8>, id: &[u8; 4], data: &[u8]) {
    out.extend_from_slice(id);
    out.extend_from_slice(&(data.len() as u32).to_le_bytes());
    out.extend_from_slice(data);
    if data.len() % 2 == 1 {
        out.push(0);
    }
}

/// Append a `LIST` chunk wrapping `body` under `list_type`.
fn put_list(out: &mut Vec<u8>, list_type: &[u8; 4], body: &[u8]) {
    let total = 4 + body.len();
    out.extend_from_slice(b"LIST");
    out.extend_from_slice(&(total as u32).to_le_bytes());
    out.extend_from_slice(list_type);
    out.extend_from_slice(body);
    if total % 2 == 1 {
        out.push(0);
    }
}

/// Codec id → AVI fourcc for the `strf`/`strh` codec field (inverse of
/// [`map_video_fourcc`]). Unknown codecs get a zero fourcc.
fn codec_to_fourcc(id: CodecId) -> [u8; 4] {
    match id {
        CodecId::H264 => *b"H264",
        CodecId::Avif => *b"AV01",
        // Without this the stream carried a zero fourcc, so FFmpeg fell back to
        // reading our MJPEG AVI as `rawvideo` and rejected every frame
        // ("packet size 1705862 < expected frame_size 6220800").
        CodecId::Jpeg => *b"MJPG",
        _ => [0; 4],
    }
}

/// `##xx` chunk id: two ASCII digits of the stream index plus a 2-byte type.
fn chunk_id(stream_index: usize, suffix: &[u8; 2]) -> [u8; 4] {
    [
        b'0' + (stream_index / 10) as u8,
        b'0' + (stream_index % 10) as u8,
        suffix[0],
        suffix[1],
    ]
}

// ===========================================================================
// Muxer
// ===========================================================================

/// Where the `movi` data chunks go.
enum Sink {
    /// A pipe or socket: the header (written first) carries totals only known
    /// at the end and cannot be patched in, so `movi` is held until the
    /// trailer.
    Buffered { out: Output, movi: Vec<u8> },
    /// A seekable output (#28): the header goes out first with zero totals,
    /// each packet's chunk streams straight through, and the trailer appends
    /// `idx1`, then seeks back to `start` (where the header begins) to rewrite
    /// the header with the real sizes and counts.
    Seekable { out: SeekOutput, start: u64 },
}

/// Writes streams + packets as an AVI file:
/// `RIFF AVI { LIST hdrl, LIST movi { ##dc / ##wb ... }, idx1 }`. The header
/// carries totals (RIFF/`movi` sizes, frame counts) only known once all
/// packets are in; both sinks write the same bytes, but only the seekable one
/// streams -- it keeps just the `idx1` entries (16 bytes a packet) and a
/// count per stream.
struct AviMuxer {
    sink: Sink,
    streams: Vec<Stream>,
    /// Each stream's `strf` body, built (and validated) at `write_header`.
    strfs: Vec<Vec<u8>>,
    /// Packets written per entry of `streams` (matched by `Stream::index`).
    counts: Vec<u64>,
    /// Bytes of data chunks in `movi` so far (headers, payloads, padding).
    movi_len: u64,
    /// The `idx1` entries so far.
    idx1: Vec<u8>,
}

impl AviMuxer {
    fn new(output: Output) -> AviMuxer {
        AviMuxer::with_sink(Sink::Buffered {
            out: output,
            movi: Vec::new(),
        })
    }

    fn new_seekable(output: SeekOutput) -> AviMuxer {
        AviMuxer::with_sink(Sink::Seekable {
            out: output,
            start: 0,
        })
    }

    fn with_sink(sink: Sink) -> AviMuxer {
        AviMuxer {
            sink,
            streams: Vec::new(),
            strfs: Vec::new(),
            counts: Vec::new(),
            movi_len: 0,
            idx1: Vec::new(),
        }
    }

    /// The `##xx` type suffix for a stream's data chunks.
    fn suffix_for(&self, stream_index: usize) -> [u8; 2] {
        match self.streams.iter().find(|s| s.index == stream_index) {
            Some(s) if s.media_type == MediaType::Audio => *b"wb",
            _ => *b"dc", // video (compressed) / default
        }
    }

    /// MainAVIHeader (56 bytes).
    fn avih(&self, micros_per_frame: u32, total_frames: u32, width: u32, height: u32) -> Vec<u8> {
        let mut a = vec![0u8; 56];
        put_u32(&mut a, 0, micros_per_frame);
        put_u32(&mut a, 12, 0x10); // dwFlags = AVIF_HASINDEX
        put_u32(&mut a, 16, total_frames);
        put_u32(&mut a, 24, self.streams.len() as u32);
        put_u32(&mut a, 32, width);
        put_u32(&mut a, 36, height);
        a
    }

    /// Everything before the first data chunk -- `RIFF` size `AVI `, `LIST
    /// hdrl`, and the `LIST movi` header -- for the packets written so far.
    /// Its length depends only on the streams, so the seekable path can
    /// overwrite the zero-totals copy it wrote first in place. The 32-bit
    /// sizes and counts wrap past 4 GiB, as they always have (no OpenDML).
    fn header(&self) -> Vec<u8> {
        let primary_video = self
            .streams
            .iter()
            .position(|s| s.media_type == MediaType::Video);
        let (vw, vh, micros_per_frame, total_frames) = match primary_video {
            Some(i) => {
                let s = &self.streams[i];
                let tb = s.time_base;
                let micros = if tb.den != 0 && tb.num > 0 {
                    (tb.num as i64 * 1_000_000 / tb.den as i64) as u32
                } else {
                    0
                };
                (s.width, s.height, micros, self.counts[i] as u32)
            }
            None => (0, 0, 0, 0),
        };

        // --- hdrl: avih + one strl (strh + strf) per stream ---
        let mut hdrl = Vec::new();
        put_chunk(
            &mut hdrl,
            b"avih",
            &self.avih(micros_per_frame, total_frames, vw, vh),
        );
        for ((s, strf), &count) in self.streams.iter().zip(&self.strfs).zip(&self.counts) {
            let mut strl = Vec::new();
            put_chunk(&mut strl, b"strh", &strh(s, count as u32));
            put_chunk(&mut strl, b"strf", strf);
            put_list(&mut hdrl, b"strl", &strl);
        }

        let mut h = Vec::new();
        h.extend_from_slice(b"RIFF");
        h.extend_from_slice(&[0; 4]); // RIFF size, below
        h.extend_from_slice(b"AVI ");
        put_list(&mut h, b"hdrl", &hdrl);
        // `LIST movi`: the data chunks are each padded to even, so the list
        // needs no pad byte of its own.
        h.extend_from_slice(b"LIST");
        h.extend_from_slice(&((4 + self.movi_len) as u32).to_le_bytes());
        h.extend_from_slice(b"movi");
        // The RIFF body runs to the end of `idx1`.
        let riff = (h.len() - 8) as u64 + self.movi_len + 8 + self.idx1.len() as u64;
        put_u32(&mut h, 4, riff as u32);
        h
    }

    /// The `idx1` chunk header.
    fn idx1_header(&self) -> [u8; 8] {
        let mut h = *b"idx1\0\0\0\0";
        put_u32(&mut h, 4, self.idx1.len() as u32);
        h
    }
}

impl Muxer for AviMuxer {
    fn write_header(&mut self, streams: &[Stream]) -> Result<()> {
        if streams.is_empty() {
            return Err(Error::invalid("avi mux: no streams"));
        }
        self.strfs = streams.iter().map(strf).collect::<Result<_>>()?;
        self.streams = streams.to_vec();
        self.counts = vec![0; streams.len()];
        let header = self.header();
        if let Sink::Seekable { out, start } = &mut self.sink {
            *start = out.stream_position()?;
            out.write_all(&header)?;
        }
        Ok(())
    }

    fn write_packet(&mut self, packet: &Packet) -> Result<()> {
        let stream_index = packet.stream_index;
        let id = chunk_id(stream_index, &self.suffix_for(stream_index));
        let data = &packet.data;
        let len = data.len() as u32;
        let pad: &[u8] = if data.len() % 2 == 1 { &[0] } else { &[] };
        let mut head = [0u8; 8];
        head[..4].copy_from_slice(&id);
        put_u32(&mut head, 4, len);
        match &mut self.sink {
            Sink::Buffered { movi, .. } => {
                movi.extend_from_slice(&head);
                movi.extend_from_slice(data);
                movi.extend_from_slice(pad);
            }
            Sink::Seekable { out, .. } => {
                out.write_all(&head)?;
                out.write_all(data)?;
                out.write_all(pad)?;
            }
        }

        // idx1 offsets are relative to the `movi` 4cc; the first chunk sits
        // 4 bytes past it.
        let offset = (4 + self.movi_len) as u32;
        let flags: u32 = if packet.is_keyframe() { 0x10 } else { 0 }; // AVIIF_KEYFRAME
        self.idx1.extend_from_slice(&id);
        self.idx1.extend_from_slice(&flags.to_le_bytes());
        self.idx1.extend_from_slice(&offset.to_le_bytes());
        self.idx1.extend_from_slice(&len.to_le_bytes());
        self.movi_len += (head.len() + data.len() + pad.len()) as u64;
        for (s, count) in self.streams.iter().zip(&mut self.counts) {
            if s.index == stream_index {
                *count += 1;
            }
        }
        Ok(())
    }

    fn write_trailer(&mut self) -> Result<()> {
        let header = self.header();
        let idx1_header = self.idx1_header();
        match &mut self.sink {
            Sink::Buffered { out, movi } => {
                out.write_all(&header)?;
                out.write_all(movi)?;
                out.write_all(&idx1_header)?;
                out.write_all(&self.idx1)?;
                out.flush()?;
            }
            Sink::Seekable { out, start } => {
                out.write_all(&idx1_header)?;
                out.write_all(&self.idx1)?;
                let end = out.stream_position()?;
                out.seek(SeekFrom::Start(*start))?;
                out.write_all(&header)?;
                out.seek(SeekFrom::Start(end))?;
                out.flush()?;
            }
        }
        Ok(())
    }
}

/// AVIStreamHeader (56 bytes) for one stream.
fn strh(s: &Stream, length: u32) -> Vec<u8> {
    let mut h = vec![0u8; 56];
    match s.media_type {
        MediaType::Audio => {
            h[0..4].copy_from_slice(b"auds");
            put_u32(&mut h, 20, 1); // dwScale
            put_u32(&mut h, 24, s.sample_rate.max(1)); // dwRate
        }
        _ => {
            h[0..4].copy_from_slice(b"vids");
            h[4..8].copy_from_slice(&codec_to_fourcc(s.codec_id)); // fccHandler
            put_u32(&mut h, 20, s.time_base.num.max(1) as u32); // dwScale
            put_u32(&mut h, 24, s.time_base.den.max(1) as u32); // dwRate
            put_u16(&mut h, 52, s.width as u16); // rcFrame.right
            put_u16(&mut h, 54, s.height as u16); // rcFrame.bottom
        }
    }
    put_u32(&mut h, 32, length); // dwLength (frames/samples)
    h
}

/// Stream format: BITMAPINFOHEADER (video) or WAVEFORMATEX (audio).
fn strf(s: &Stream) -> Result<Vec<u8>> {
    match s.media_type {
        MediaType::Audio => {
            let (tag, bits): (u16, u16) = match s.sample_format {
                Some(SampleFormat::F32) => (3, 32),
                _ => (1, 16), // default to s16 PCM
            };
            let block_align = s.channels * (bits / 8);
            let mut w = vec![0u8; 18]; // WAVEFORMATEX
            put_u16(&mut w, 0, tag); // wFormatTag (1=PCM, 3=float)
            put_u16(&mut w, 2, s.channels); // nChannels
            put_u32(&mut w, 4, s.sample_rate); // nSamplesPerSec
            put_u32(&mut w, 8, s.sample_rate * block_align as u32); // nAvgBytesPerSec
            put_u16(&mut w, 12, block_align); // nBlockAlign
            put_u16(&mut w, 14, bits); // wBitsPerSample
            Ok(w)
        }
        _ => {
            // Raw video must describe its actual pixel layout: a zero fourcc
            // means BI_RGB, and readers then interpret planar-YUV packets as
            // packed BGR of a different size — silent corruption. Planar YUV
            // carries its own fourcc and true bits-per-pixel instead.
            let (fourcc, bit_count, frame_bytes): ([u8; 4], u16, u32) = if s.codec_id
                == CodecId::RawVideo
            {
                let (cw, ch) = (s.width.div_ceil(2), s.height.div_ceil(2));
                match s.pixel_format {
                    Some(PixelFormat::Yuv420p) => (*b"I420", 12, s.width * s.height + 2 * cw * ch),
                    Some(PixelFormat::Yuv422p) => {
                        (*b"Y42B", 16, s.width * s.height + 2 * cw * s.height)
                    }
                    Some(PixelFormat::Yuv444p) => (*b"444P", 24, 3 * s.width * s.height),
                    other => {
                        return Err(Error::unsupported(format!(
                            "avi mux: rawvideo pixel format {:?} has no AVI mapping",
                            other
                        )))
                    }
                }
            } else {
                (codec_to_fourcc(s.codec_id), 24, s.width * s.height * 3)
            };
            let mut b = vec![0u8; 40]; // BITMAPINFOHEADER
            put_u32(&mut b, 0, 40); // biSize
            put_u32(&mut b, 4, s.width); // biWidth
            put_u32(&mut b, 8, s.height); // biHeight
            put_u16(&mut b, 12, 1); // biPlanes
            put_u16(&mut b, 14, bit_count); // biBitCount
            b[16..20].copy_from_slice(&fourcc); // biCompression
            put_u32(&mut b, 20, frame_bytes); // biSizeImage
            Ok(b)
        }
    }
}

#[cfg(test)]
mod mux_tests;
#[cfg(test)]
mod stream_tests;

#[cfg(test)]
mod tests {
    use super::*;
    use std::io::{Cursor, Write};
    use std::sync::{Arc, Mutex};

    /// A `Write` sink whose bytes can be recovered after the muxer drops it.
    #[derive(Clone)]
    struct SharedBuf(Arc<Mutex<Vec<u8>>>);

    impl Write for SharedBuf {
        fn write(&mut self, buf: &[u8]) -> std::io::Result<usize> {
            self.0.lock().unwrap().extend_from_slice(buf);
            Ok(buf.len())
        }
        fn flush(&mut self) -> std::io::Result<()> {
            Ok(())
        }
    }

    fn chunk(id: &[u8; 4], data: &[u8]) -> Vec<u8> {
        let mut o = Vec::new();
        o.extend_from_slice(id);
        o.extend_from_slice(&(data.len() as u32).to_le_bytes());
        o.extend_from_slice(data);
        if data.len() % 2 == 1 {
            o.push(0); // pad to even
        }
        o
    }

    fn list(list_type: &[u8; 4], children: &[u8]) -> Vec<u8> {
        let mut body = Vec::new();
        body.extend_from_slice(list_type);
        body.extend_from_slice(children);
        chunk(b"LIST", &body)
    }

    fn riff_avi(children: &[u8]) -> Vec<u8> {
        let mut body = Vec::new();
        body.extend_from_slice(b"AVI ");
        body.extend_from_slice(children);
        let mut o = Vec::new();
        o.extend_from_slice(b"RIFF");
        o.extend_from_slice(&(body.len() as u32).to_le_bytes());
        o.extend_from_slice(&body);
        o
    }

    #[test]
    fn demuxes_minimal_avi() {
        let (w, h) = (320u32, 240u32);

        let mut avih = vec![0u8; 56];
        avih[24..28].copy_from_slice(&1u32.to_le_bytes()); // dwStreams
        avih[32..36].copy_from_slice(&w.to_le_bytes()); // dwWidth
        avih[36..40].copy_from_slice(&h.to_le_bytes()); // dwHeight

        let mut strh = vec![0u8; 56];
        strh[0..4].copy_from_slice(b"vids"); // fccType
        strh[4..8].copy_from_slice(b"H264"); // fccHandler
        strh[20..24].copy_from_slice(&1u32.to_le_bytes()); // dwScale
        strh[24..28].copy_from_slice(&30u32.to_le_bytes()); // dwRate

        let mut strf = vec![0u8; 40]; // BITMAPINFOHEADER
        strf[0..4].copy_from_slice(&40u32.to_le_bytes()); // biSize
        strf[4..8].copy_from_slice(&w.to_le_bytes()); // biWidth
        strf[8..12].copy_from_slice(&h.to_le_bytes()); // biHeight
        strf[16..20].copy_from_slice(b"H264"); // biCompression

        let mut strl_children = Vec::new();
        strl_children.extend_from_slice(&chunk(b"strh", &strh));
        strl_children.extend_from_slice(&chunk(b"strf", &strf));
        let strl = list(b"strl", &strl_children);

        let mut hdrl_children = Vec::new();
        hdrl_children.extend_from_slice(&chunk(b"avih", &avih));
        hdrl_children.extend_from_slice(&strl);
        let hdrl = list(b"hdrl", &hdrl_children);

        let payload = b"H264-frame-bytes";
        let movi = list(b"movi", &chunk(b"00dc", payload));

        let mut top = Vec::new();
        top.extend_from_slice(&hdrl);
        top.extend_from_slice(&movi);
        let file = riff_avi(&top);

        let mut dem = AviDemuxer::new(Box::new(Cursor::new(file)));
        let streams = dem.read_header().unwrap();
        assert_eq!(streams.len(), 1);
        assert_eq!(streams[0].media_type, MediaType::Video);
        assert_eq!(streams[0].codec_id, CodecId::H264);
        assert_eq!((streams[0].width, streams[0].height), (w, h));

        let packet = dem.read_packet().unwrap();
        assert_eq!(packet.stream_index, 0);
        assert_eq!(packet.data, payload);
        assert!(matches!(dem.read_packet(), Err(Error::Eof)));
    }

    /// Regression: MJPEG was in neither fourcc map, so an MJPEG AVI we wrote
    /// carried a zero fourcc (FFmpeg fell back to `rawvideo` and rejected every
    /// frame) and an MJPEG AVI FFmpeg wrote failed on our side with "no decoder
    /// found for codec none". Both directions were broken.
    #[test]
    fn mjpeg_fourcc_maps_both_ways() {
        assert_eq!(codec_to_fourcc(CodecId::Jpeg), *b"MJPG");
        // The tag FFmpeg and cameras actually write, plus the long tail.
        for tag in [b"MJPG", b"mjpg", b"JPEG", b"MJPA", b"MJPB", b"DMB1"] {
            assert_eq!(map_video_fourcc(*tag), CodecId::Jpeg, "{:?}", tag);
        }
        // Round-trip: what we write is what we read.
        assert_eq!(
            map_video_fourcc(codec_to_fourcc(CodecId::Jpeg)),
            CodecId::Jpeg
        );
    }

    #[test]
    fn mux_then_demux_roundtrips() {
        let (w, h) = (320u32, 240u32);
        let mut vstream = Stream::new(0, CodecId::H264);
        vstream.media_type = MediaType::Video;
        vstream.width = w;
        vstream.height = h;
        vstream.time_base = Rational::new(1, 30);

        let sink = SharedBuf(Arc::new(Mutex::new(Vec::new())));
        {
            let mut mux = AviMuxer::new(Box::new(sink.clone()));
            mux.write_header(&[vstream]).unwrap();
            mux.write_packet(&Packet::from_data(0, b"frame-one".to_vec()))
                .unwrap();
            let mut p2 = Packet::from_data(0, b"frame-two!".to_vec());
            p2.flags.keyframe = true;
            mux.write_packet(&p2).unwrap();
            mux.write_trailer().unwrap();
        }
        let file = sink.0.lock().unwrap().clone();

        // Looks like a RIFF/AVI file...
        assert_eq!(&file[0..4], b"RIFF");
        assert_eq!(&file[8..12], b"AVI ");

        // ...and our own demuxer reads the streams and packets back.
        let mut dem = AviDemuxer::new(Box::new(Cursor::new(file)));
        let streams = dem.read_header().unwrap();
        assert_eq!(streams.len(), 1);
        assert_eq!(streams[0].media_type, MediaType::Video);
        assert_eq!(streams[0].codec_id, CodecId::H264);
        assert_eq!((streams[0].width, streams[0].height), (w, h));

        assert_eq!(dem.read_packet().unwrap().data, b"frame-one");
        assert_eq!(dem.read_packet().unwrap().data, b"frame-two!");
        assert!(matches!(dem.read_packet(), Err(Error::Eof)));
    }

    #[test]
    fn sniffs_avi_by_magic() {
        assert_eq!(probe_avi(&riff_avi(&[])), 100);
        assert_eq!(probe_avi(b"RIFF\0\0\0\0WAVE"), 0); // RIFF, but not AVI
        assert_eq!(probe_avi(b"not a riff file"), 0);
        assert_eq!(probe_avi(b"RIFF"), 0); // too short
    }
}
