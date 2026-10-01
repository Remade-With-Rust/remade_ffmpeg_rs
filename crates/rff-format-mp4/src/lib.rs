//! MP4 / MOV (ISOBMFF) demuxer.
//!
//! MP4 stores sample *data* in `mdat` and the map to it in each track's sample
//! table (`stbl`): `stsd` (codec + config), `stsz` (sizes), `stsc`
//! (samples-per-chunk), `stco`/`co64` (chunk offsets), `stts` (durations), `stss`
//! (sync samples), `ctts` (composition offsets). We reconstruct, per sample, its
//! `(file offset, size, dts, pts, keyframe)` and yield them as packets in file
//! order. Timestamps follow FFmpeg's `mov` demuxer: `pts = dts + ctts`, and a
//! track's edit list (`edts/elst`) rebuilds its index the way `mov_fix_index`
//! does (see [`fix_index`]).
//!
//! Over a seekable input (a local file — see
//! [`FormatRegistry::open_demuxer_seekable`]) the demuxer walks the top-level
//! box headers, seeks past `mdat`, reads only `moov`, and then fetches each
//! sample by offset, so opening a file costs memory proportional to its index,
//! wherever `moov` sits. A non-seekable stream (pipe, socket) can't reach a
//! trailing `moov` without passing the media first, so it is buffered whole.
//!
//! Codec mapping: `avc1`/`avc3` → H.264, `av01` → AV1 (our `avif` decoder),
//! `mp4a` → AAC (labelled; no decoder yet). H.264 samples are stored length-
//! prefixed (AVCC); we convert them to Annex-B and prepend the `avcC` SPS/PPS on
//! keyframes so the H.264 decoder gets a self-contained bitstream.
//!
//! The fragmented profile lives in [`fmp4`] (init/media segments) and powers
//! the DASH VOD muxer in [`dash`].

pub mod dash;
pub mod fmp4;

use std::borrow::Cow;
use std::io::{BufReader, ErrorKind, Read, Seek, SeekFrom};

use rff_core::{CodecId, Error, MediaType, Packet, Rational, Result};
use rff_format::{
    Demuxer, Format, FormatRegistry, Input, MuxCaps, Muxer, Output, ReadSeek, SeekInput, Stream,
};

/// Register the MP4 format into a [`FormatRegistry`].
pub fn register(registry: &mut FormatRegistry) {
    registry.register(Format {
        name: "mp4",
        long_name: "MP4 / MOV (ISO Base Media File Format)",
        extensions: &["mp4", "mov", "m4a", "m4v"],
        demuxer: Some(|input| Box::new(Mp4Demuxer::new(input))),
        muxer: Some(|output| Box::new(Mp4Muxer::new(output))),
        muxer_path: None,
        probe: Some(probe_mp4),
        mux_caps: MuxCaps::container(&[CodecId::H264, CodecId::Avif, CodecId::Aac, CodecId::Opus]),
    });
    // With a seekable source (a local file) read only `moov` and fetch each
    // sample on demand; the plain factory above is the pipe/stream fallback.
    registry.register_seekable_demuxer("mp4", |input| Box::new(Mp4Demuxer::new_seekable(input)));
    registry.register(Format {
        name: "dash",
        long_name: "DASH (static MPD + fMP4 segments)",
        extensions: &["mpd"],
        demuxer: None,
        muxer: None,
        muxer_path: Some(|path, options| {
            let seconds = options
                .get("seg_duration")
                .and_then(|v| v.parse::<f64>().ok())
                .unwrap_or(4.0);
            Ok(Box::new(dash::DashMuxer::new(path, seconds)?))
        }),
        probe: None,
        mux_caps: MuxCaps::container(&[CodecId::H264, CodecId::Avif, CodecId::Aac, CodecId::Opus]),
    });
}

/// Sniff MP4: an ISOBMFF `ftyp` box near the start.
fn probe_mp4(data: &[u8]) -> i32 {
    if data.len() >= 8 && &data[4..8] == b"ftyp" {
        90
    } else {
        0
    }
}

// ---------------------------------------------------------------------------
// Byte / box helpers
// ---------------------------------------------------------------------------

fn be16(b: &[u8], o: usize) -> u32 {
    b.get(o..o + 2)
        .map_or(0, |s| ((s[0] as u32) << 8) | s[1] as u32)
}
fn be32(b: &[u8], o: usize) -> u32 {
    b.get(o..o + 4)
        .map_or(0, |s| u32::from_be_bytes([s[0], s[1], s[2], s[3]]))
}
fn be64(b: &[u8], o: usize) -> u64 {
    b.get(o..o + 8).map_or(0, |s| {
        u64::from_be_bytes([s[0], s[1], s[2], s[3], s[4], s[5], s[6], s[7]])
    })
}

/// Parse sibling boxes in `buf` into `(type, payload)` pairs, handling the
/// 64-bit `largesize` and `size == 0` (to-end) forms.
fn child_boxes(buf: &[u8]) -> Vec<([u8; 4], &[u8])> {
    let mut out = Vec::new();
    let mut i = 0;
    while i + 8 <= buf.len() {
        let size32 = be32(buf, i) as usize;
        let typ = [buf[i + 4], buf[i + 5], buf[i + 6], buf[i + 7]];
        let (start, end) = if size32 == 1 {
            (
                i + 16,
                i.checked_add(be64(buf, i + 8) as usize)
                    .unwrap_or(buf.len()),
            )
        } else if size32 == 0 {
            (i + 8, buf.len())
        } else {
            (i + 8, i + size32)
        };
        // A declared size smaller than its own header (1..7, or a largesize
        // under 16) is malformed: stop rather than slice `start > end`.
        if end > buf.len() || end <= i || start > end {
            break;
        }
        out.push((typ, &buf[start..end]));
        i = end;
    }
    out
}

fn find<'a>(boxes: &[([u8; 4], &'a [u8])], typ: &[u8; 4]) -> Option<&'a [u8]> {
    boxes.iter().find(|(t, _)| t == typ).map(|(_, p)| *p)
}

// ---------------------------------------------------------------------------
// Demuxer
// ---------------------------------------------------------------------------

/// One sample's location and metadata.
struct SampleLoc {
    stream_index: usize,
    offset: usize,
    size: usize,
    dts: i64,
    pts: i64,
    keyframe: bool,
}

use rff_format::avc::{avcc_to_annexb, parse_avcc, split_annexb, AvcConfig};

/// Where sample bytes come from once the header is parsed.
enum Source {
    /// Not yet opened: a plain stream (pipe, socket) or a seekable file.
    Stream(Input),
    Seekable(SeekInput),
    /// Stream fallback: a non-seekable input can't jump to `moov` at the end
    /// of the file and back, so it is buffered whole and sliced per sample.
    Buffered(Vec<u8>),
    /// Seekable: only `moov` was read; each sample is fetched by offset.
    /// `cur` is the reader's position, so contiguous samples need no seek.
    Random {
        reader: BufReader<SeekInput>,
        cur: u64,
    },
    /// Header already taken (or failed).
    Done,
}

struct Mp4Demuxer {
    src: Source,
    samples: Vec<SampleLoc>,
    /// Indexed by stream_index; `Some` for H.264 tracks needing conversion.
    avc: Vec<Option<AvcConfig>>,
    pos: usize,
}

impl Mp4Demuxer {
    fn new(input: Input) -> Mp4Demuxer {
        Mp4Demuxer::with_source(Source::Stream(input))
    }

    /// Open over a seekable input: `read_header` reads only the box headers
    /// and `moov`, so memory and I/O scale with the index, not the media.
    fn new_seekable(input: SeekInput) -> Mp4Demuxer {
        Mp4Demuxer::with_source(Source::Seekable(input))
    }

    fn with_source(src: Source) -> Mp4Demuxer {
        Mp4Demuxer {
            src,
            samples: Vec::new(),
            avc: Vec::new(),
            pos: 0,
        }
    }

    /// Build streams + the file-ordered sample table from a `moov` payload.
    fn parse_moov(&mut self, moov: &[u8]) -> Result<Vec<Stream>> {
        let moov_children = child_boxes(moov);
        // Movie timescale (mvhd), the unit of edit-list segment durations.
        let movie_timescale = find(&moov_children, b"mvhd").map_or(0, |mvhd| {
            if mvhd.first() == Some(&1) {
                be32(mvhd, 20)
            } else {
                be32(mvhd, 12)
            }
        });
        let mut streams = Vec::new();
        let mut tracks: Vec<Vec<SampleLoc>> = Vec::new();
        // FFmpeg's `advanced_editlist`: on until a track has no `stts` entries (a
        // fragmented file's empty `moov`), then off for that track and every later one.
        let mut advanced_editlist = true;
        for (typ, trak) in moov_children {
            if &typ != b"trak" {
                continue;
            }
            if stts_entries(trak) == 0 {
                advanced_editlist = false;
            }
            let timing = (movie_timescale, advanced_editlist);
            if let Some((stream, samples, avc)) = parse_track(trak, streams.len(), timing) {
                streams.push(stream);
                tracks.push(samples);
                self.avc.push(avc);
            }
        }
        if streams.is_empty() {
            return Err(Error::unsupported("mp4 demux: no usable tracks"));
        }
        // Yield samples in file order (natural interleave), each track's in its own
        // order: an edit list can replay samples, so a track's index isn't always in
        // offset order.
        let mut next = vec![0usize; tracks.len()];
        while let Some(t) = (0..tracks.len())
            .filter(|&t| next[t] < tracks[t].len())
            .min_by_key(|&t| tracks[t][next[t]].offset)
        {
            let i = next[t];
            next[t] += 1;
            let loc = &tracks[t][i];
            self.samples.push(SampleLoc { ..*loc });
        }
        Ok(streams)
    }
}

/// Walk the top-level boxes of a seekable file by header alone — seeking past
/// `mdat` and everything else — and return the `moov` payload. Box framing
/// matches [`child_boxes`]: 64-bit `largesize`, `size == 0` meaning "to end of
/// file", and a box that is malformed or overruns the file ends the walk.
fn read_moov(input: &mut dyn ReadSeek) -> Result<Vec<u8>> {
    let file_len = input.seek(SeekFrom::End(0))?;
    let mut pos = 0u64;
    while pos.saturating_add(8) <= file_len {
        input.seek(SeekFrom::Start(pos))?;
        let mut hdr = [0u8; 16];
        input.read_exact(&mut hdr[..8])?;
        let size32 = be32(&hdr, 0) as u64;
        let typ = [hdr[4], hdr[5], hdr[6], hdr[7]];
        let (header_len, size) = match size32 {
            1 => {
                if pos.saturating_add(16) > file_len {
                    break;
                }
                input.read_exact(&mut hdr[8..16])?;
                (16, be64(&hdr, 8))
            }
            0 => (8, file_len - pos),
            n => (8, n),
        };
        let end = match pos.checked_add(size) {
            Some(end) if size >= header_len && end <= file_len => end,
            _ => break,
        };
        if &typ == b"moov" {
            let len = size - header_len;
            let mut moov = Vec::new();
            input.take(len).read_to_end(&mut moov)?;
            if moov.len() as u64 != len {
                return Err(Error::invalid("mp4 demux: truncated `moov`"));
            }
            return Ok(moov);
        }
        pos = end;
    }
    Err(Error::invalid("mp4 demux: no `moov`"))
}

impl Demuxer for Mp4Demuxer {
    fn read_header(&mut self) -> Result<Vec<Stream>> {
        match std::mem::replace(&mut self.src, Source::Done) {
            Source::Stream(mut input) => {
                let mut buf = Vec::new();
                input.read_to_end(&mut buf)?;
                let top = child_boxes(&buf);
                let moov =
                    find(&top, b"moov").ok_or_else(|| Error::invalid("mp4 demux: no `moov`"))?;
                let streams = self.parse_moov(moov)?;
                self.src = Source::Buffered(buf);
                Ok(streams)
            }
            Source::Seekable(mut input) => {
                let moov = read_moov(&mut input)?;
                let streams = self.parse_moov(&moov)?;
                // The reader's position is wherever the moov walk left it;
                // start from an unknown position so the first sample seeks.
                self.src = Source::Random {
                    reader: BufReader::with_capacity(64 * 1024, input),
                    cur: u64::MAX,
                };
                Ok(streams)
            }
            _ => Err(Error::invalid("mp4 demux: header already read")),
        }
    }

    fn read_packet(&mut self) -> Result<Packet> {
        let loc = match self.samples.get(self.pos) {
            Some(loc) => loc,
            None => return Err(Error::Eof),
        };
        self.pos += 1;
        let out_of_range = || Error::invalid("mp4 demux: sample out of range");
        let raw: Cow<[u8]> = match &mut self.src {
            Source::Buffered(buf) => Cow::Borrowed(
                buf.get(loc.offset..loc.offset.checked_add(loc.size).ok_or_else(out_of_range)?)
                    .ok_or_else(out_of_range)?,
            ),
            Source::Random { reader, cur } => {
                let off = loc.offset as u64;
                if *cur != off {
                    reader.seek(SeekFrom::Start(off))?;
                }
                // Read what is there, never a declared size up front: a
                // corrupt `stsz` must not become a giant allocation.
                let mut data = Vec::new();
                let got = reader.take(loc.size as u64).read_to_end(&mut data);
                *cur = match got {
                    Ok(n) => off + n as u64,
                    Err(_) => u64::MAX,
                };
                match got {
                    Ok(n) if n == loc.size => {}
                    Ok(_) => return Err(out_of_range()),
                    Err(e) if e.kind() == ErrorKind::UnexpectedEof => return Err(out_of_range()),
                    Err(e) => return Err(e.into()),
                }
                Cow::Owned(data)
            }
            _ => return Err(Error::invalid("mp4 demux: header not read")),
        };

        // H.264: convert AVCC length-prefixed NALs to Annex-B, prepending the
        // SPS/PPS on keyframes so the decoder is self-contained.
        let data = match &self.avc[loc.stream_index] {
            Some(cfg) => {
                let mut out = Vec::with_capacity(cfg.headers_annexb.len() + raw.len() + 16);
                if loc.keyframe {
                    out.extend_from_slice(&cfg.headers_annexb);
                }
                avcc_to_annexb(&raw, cfg.nal_len, &mut out);
                out
            }
            // A sample read from a seekable file is moved, not copied.
            None => raw.into_owned(),
        };

        let mut packet = Packet::from_data(loc.stream_index, data);
        packet.dts = Some(loc.dts);
        packet.pts = Some(loc.pts);
        packet.flags.keyframe = loc.keyframe;
        Ok(packet)
    }
}

/// The number of `stts` entries a `trak` declares (0 without one).
fn stts_entries(trak: &[u8]) -> u32 {
    let stts = (|| {
        let mdia = child_boxes(find(&child_boxes(trak), b"mdia")?);
        let minf = child_boxes(find(&mdia, b"minf")?);
        let stbl = child_boxes(find(&minf, b"stbl")?);
        find(&stbl, b"stts").map(|stts| be32(stts, 4))
    })();
    stts.unwrap_or(0)
}

/// Build a [`Stream`] + sample list + optional H.264 config from one `trak`.
/// `timing` is the movie timescale and whether FFmpeg's `advanced_editlist` is on.
fn parse_track(
    trak: &[u8],
    index: usize,
    timing: (u32, bool),
) -> Option<(Stream, Vec<SampleLoc>, Option<AvcConfig>)> {
    let trak_children = child_boxes(trak);
    let mdia = child_boxes(find(&trak_children, b"mdia")?);
    let hdlr = find(&mdia, b"hdlr")?;
    // FullBox(4) + pre_defined(4) → handler_type fourcc.
    let handler: [u8; 4] = hdlr.get(8..12)?.try_into().ok()?;
    let minf = child_boxes(find(&mdia, b"minf")?);
    let stbl = child_boxes(find(&minf, b"stbl")?);

    // Media timescale from mdhd (version 0: timescale @ 12 after ver/flags).
    let mdhd = find(&mdia, b"mdhd")?;
    let (timescale, duration) = if mdhd.first() == Some(&1) {
        // v1: 64-bit times push timescale to offset 20, duration to 24.
        (be32(mdhd, 20), be64(mdhd, 24))
    } else {
        (be32(mdhd, 12), be32(mdhd, 16) as u64)
    };
    let timescale = timescale.max(1);
    // All-ones means "unknown" per ISO 14496-12; so does 0 in practice.
    let duration = match duration {
        0 | 0xFFFF_FFFF | u64::MAX => None,
        d => i64::try_from(d).ok(),
    };

    let stsd = find(&stbl, b"stsd")?;
    let fourcc: [u8; 4] = stsd.get(12..16)?.try_into().ok()?; // entry format
    let codec_id = map_codec(&fourcc);

    let mut stream = Stream::new(index, codec_id);
    stream.media_type = match &handler {
        b"vide" => MediaType::Video,
        b"soun" => MediaType::Audio,
        _ => codec_id.media_type(),
    };
    stream.time_base = Rational::new(1, timescale as i32);

    // Codec dimensions / audio params from the sample entry (entry starts at 8).
    if stream.media_type == MediaType::Video {
        stream.width = be16(stsd, 8 + 32);
        stream.height = be16(stsd, 8 + 34);
    } else if stream.media_type == MediaType::Audio {
        stream.channels = be16(stsd, 8 + 24) as u16;
        stream.sample_rate = be32(stsd, 8 + 32) >> 16;
        // AAC: the AudioSpecificConfig lives in an `esds` child box (the
        // AudioSampleEntry's children start 28 bytes into its content).
        if codec_id == CodecId::Aac {
            if let Some(after) = stsd.get(8 + 36..) {
                if let Some((_, esds)) = child_boxes(after).iter().find(|(t, _)| t == b"esds") {
                    if let Some(asc) = parse_esds(esds) {
                        stream.extradata = asc;
                    }
                }
            }
        }
    }

    // H.264 / HEVC: pull the config record (a child of the visual sample
    // entry, which begins 86 bytes in) — `avcC` or `hvcC`. Both yield the NAL
    // length size and the Annex-B parameter sets.
    let avc = match codec_id {
        CodecId::H264 => child_boxes(stsd.get(8 + 86..)?)
            .iter()
            .find(|(t, _)| t == b"avcC")
            .and_then(|(_, p)| parse_avcc(p)),
        CodecId::Hevc => child_boxes(stsd.get(8 + 86..)?)
            .iter()
            .find(|(t, _)| t == b"hvcC")
            .and_then(|(_, p)| rff_format::hvc::parse_hvcc(p)),
        _ => None,
    };

    let (movie_timescale, advanced_editlist) = timing;
    let edits = find(&trak_children, b"edts")
        .and_then(|edts| find(&child_boxes(edts), b"elst"))
        .map(parse_elst)
        .unwrap_or_default();
    let timing = TrackTiming {
        audio: stream.media_type == MediaType::Audio,
        vorbis: codec_id == CodecId::Vorbis,
        video: stream.media_type == MediaType::Video,
        timescale: i64::from(timescale),
        movie_timescale: i64::from(movie_timescale),
        edits,
        advanced_editlist,
    };
    let samples = build_samples(&stbl, index, &timing)?;
    stream.duration = duration; // already in mdhd-timescale = time_base units
    stream.nb_frames = Some(samples.len() as u64);
    Some((stream, samples, avc))
}

/// Extract the AudioSpecificConfig (DecoderSpecificInfo, tag 0x05) from an
/// `esds` box by walking the MPEG-4 descriptor tree.
fn parse_esds(esds: &[u8]) -> Option<Vec<u8>> {
    fn read_len(d: &[u8], p: &mut usize) -> usize {
        let mut len = 0usize;
        for _ in 0..4 {
            let Some(&b) = d.get(*p) else { break };
            *p += 1;
            len = (len << 7) | (b & 0x7f) as usize;
            if b & 0x80 == 0 {
                break;
            }
        }
        len
    }
    let mut p = 4; // skip version/flags
    while p < esds.len() {
        let tag = esds[p];
        p += 1;
        let len = read_len(esds, &mut p);
        match tag {
            0x03 => p += 3,  // ES_Descriptor: ES_ID(2) + flags(1), then nested
            0x04 => p += 13, // DecoderConfigDescriptor fixed fields, then nested
            0x05 => return esds.get(p..p + len).map(|s| s.to_vec()), // ASC
            _ => p += len,
        }
    }
    None
}

fn map_codec(fourcc: &[u8; 4]) -> CodecId {
    match fourcc {
        b"avc1" | b"avc3" => CodecId::H264,
        b"hvc1" | b"hev1" => CodecId::Hevc,
        b"av01" => CodecId::Avif, // AV1 video — decoded by our rav1d-backed codec
        b"Opus" => CodecId::Opus,
        b"mp4a" => CodecId::Aac,
        _ => CodecId::None,
    }
}

/// What a track's timestamps depend on beyond its sample table.
struct TrackTiming {
    audio: bool,
    video: bool,
    /// FFmpeg doesn't skip a Vorbis packet's leading samples for an edit list.
    vorbis: bool,
    /// Media timescale (`mdhd`).
    timescale: i64,
    /// Movie timescale (`mvhd`), the unit of edit segment durations.
    movie_timescale: i64,
    /// `elst` entries: (segment duration in the movie timescale, media time).
    edits: Vec<(i64, i64)>,
    /// FFmpeg's `advanced_editlist` (see [`fix_index`]).
    advanced_editlist: bool,
}

/// An `elst` box's entries, as FFmpeg's `mov_read_elst` reads them: the box size
/// wins over an entry count that disagrees with it, version 0 media times are
/// signed 32-bit, and a negative segment duration makes the whole list invalid.
fn parse_elst(elst: &[u8]) -> Vec<(i64, i64)> {
    let v1 = elst.first() == Some(&1);
    let stride = if v1 { 20 } else { 12 };
    let size = elst.len().saturating_sub(8);
    let declared = be32(elst, 4) as usize;
    let count = if declared.checked_mul(stride) == Some(size) {
        declared
    } else {
        size / stride
    };
    let mut edits = Vec::with_capacity(count.min(1 << 12));
    for k in 0..count {
        let o = 8 + k * stride;
        let (duration, media_time) = if v1 {
            (be64(elst, o) as i64, be64(elst, o + 8) as i64)
        } else {
            (
                i64::from(be32(elst, o)),
                i64::from(be32(elst, o + 4) as i32),
            )
        };
        if duration < 0 {
            return Vec::new();
        }
        edits.push((duration, media_time));
    }
    edits
}

/// `av_rescale`: `a * b / c`, rounded to nearest (halves away from zero).
fn rescale(a: i64, b: i64, c: i64) -> i64 {
    if c == 0 {
        return 0;
    }
    let (a, b, c) = (i128::from(a), i128::from(b), i128::from(c));
    let r = c / 2;
    let v = if a * b >= 0 {
        (a * b + r) / c
    } else {
        -((-(a * b) + r) / c)
    };
    v.clamp(i128::from(i64::MIN), i128::from(i64::MAX)) as i64
}

/// FFmpeg's `mov_fix_index`: a track's sample index rebuilt by its edit list.
/// `ts` are the index timestamps (decode times less `dts_shift`), `offsets` each
/// sample's composition offset. Returns `(sample, dts, discard)` in index order.
///
/// Each non-empty edit replays the samples from the keyframe before its media time
/// (for audio, a second earlier, for decoder priming) to the first keyframe past its
/// end (the second, with B-frames), timed by a counter that runs along the
/// presentation; samples shown outside the edit stay in for decoding but are
/// discarded (rff has no flag for that: they come out timed before the edit).
/// An audio sample straddling the edit start isn't discarded; the timeline moves
/// back by its leading part instead. A video track's timeline is finally shifted
/// so its earliest shown frame starts at the leading empty edits.
fn fix_index(
    t: &TrackTiming,
    ts: &[i64],
    keyframe: &[bool],
    offsets: &[i64],
    has_ctts: bool,
    dts_shift: i64,
) -> Vec<(usize, i64, bool)> {
    let n = ts.len();
    let mut out: Vec<(usize, i64, bool)> = Vec::new();
    if n == 0 || t.movie_timescale == 0 {
        return (0..n).map(|i| (i, ts[i], false)).collect();
    }
    let mut entry_end = dts_shift.saturating_neg();
    let mut empty_sum = 0i64;
    let mut found_non_empty = false;
    let mut first_non_zero_audio_edit = -1;
    let mut min_corrected = -1i64;
    for &(segment, media_time) in &t.edits {
        // get_edit_list_entry: rescaled, and zeroed where duration + media time overflows.
        let mut duration = rescale(segment, t.timescale, t.movie_timescale);
        if (duration as u64).wrapping_add(media_time as u64) > i64::MAX as u64 {
            duration = 0;
        }
        let mut counter = entry_end;
        entry_end = entry_end.saturating_add(duration);
        if entry_end == i64::MAX {
            break;
        }
        if !found_non_empty && media_time == -1 {
            empty_sum = empty_sum.saturating_add(duration);
            continue;
        }
        found_non_empty = true;
        if t.audio {
            first_non_zero_audio_edit = if first_non_zero_audio_edit < 0 { 1 } else { 0 };
        }
        let search = if t.audio {
            media_time.saturating_sub(t.timescale).max(ts[0])
        } else {
            media_time
        };
        let edit_end = media_time.saturating_add(duration);
        let mut started = false;
        // (index in `out`, frame duration) of the samples discarded before the edit
        // starts, retimed backwards from it once it does.
        let mut lead_in: Vec<(usize, i64)> = Vec::new();
        let mut keyframe_after_edit = false;
        let mut j = edit_start(
            ts,
            keyframe,
            offsets,
            has_ctts,
            search.saturating_sub(dts_shift),
        );
        while j < n {
            let frame_duration = match ts.get(j + 1) {
                Some(next) => next.saturating_sub(ts[j]),
                None => duration,
            };
            let cts = ts[j].saturating_add(dts_shift).saturating_add(offsets[j]);
            let mut discard = false;
            let mut starts_here = false;
            if cts < media_time || cts >= edit_end {
                if t.audio
                    && !t.vorbis
                    && cts < media_time
                    && cts.saturating_add(frame_duration) > media_time
                    && first_non_zero_audio_edit > 0
                {
                    counter = counter.saturating_sub(media_time - cts);
                    starts_here = !started;
                } else {
                    discard = true;
                    if !started {
                        lead_in.push((out.len(), frame_duration));
                    }
                }
            } else {
                let pts = counter.saturating_add(offsets[j]).saturating_add(dts_shift);
                // (FFmpeg restarts the minimum whenever it has gone negative.)
                min_corrected = if min_corrected < 0 {
                    pts
                } else {
                    min_corrected.min(pts)
                };
                starts_here = !started;
            }
            if starts_here {
                started = true;
                let mut end = counter;
                for &(k, d) in lead_in.iter().rev() {
                    end = end.saturating_sub(d);
                    out[k].1 = end;
                }
                lead_in.clear();
            }
            if out.len() >= MAX_INDEX_ENTRIES {
                break;
            }
            out.push((j, counter, discard));
            if started {
                counter = counter.saturating_add(frame_duration);
            }
            if cts.saturating_add(frame_duration) >= edit_end && (keyframe[j] || t.audio) {
                // With B-frames, frames after this keyframe in decode order can still
                // belong to the edit: go on to the next keyframe.
                if has_ctts && !t.audio && !keyframe_after_edit {
                    keyframe_after_edit = true;
                    j += 1;
                    continue;
                }
                break;
            }
            j += 1;
        }
    }
    let min_corrected = min_corrected.saturating_sub(empty_sum);
    if t.video && min_corrected > 0 {
        for e in &mut out {
            e.1 = e.1.saturating_sub(min_corrected);
        }
    }
    out
}

/// The most index entries an edit list may expand a track to: replayed edits can
/// list a sample many times over, so bound it like the sample count itself.
const MAX_INDEX_ENTRIES: usize = 1 << 24;

/// Where `mov_fix_index` starts replaying an edit (`find_prev_closest_index`): the
/// last keyframe whose index timestamp is at or before `target`, moved to the first
/// keyframe sharing that timestamp, then (with B-frames) back to a keyframe shown at
/// or before it; failing that any frame, failing that the first.
fn edit_start(
    ts: &[i64],
    keyframe: &[bool],
    offsets: &[i64],
    has_ctts: bool,
    target: i64,
) -> usize {
    let search = |any: bool| -> Option<usize> {
        let mut m = ts.partition_point(|&d| d <= target).checked_sub(1)?;
        if !any {
            while !keyframe[m] {
                m = m.checked_sub(1)?;
            }
        }
        let mut i = m;
        while i > 0 && ts[i] == ts[i - 1] {
            if any || keyframe[i - 1] {
                m = i - 1;
            }
            i -= 1;
        }
        if has_ctts {
            while !(keyframe[m] && ts[m].saturating_add(offsets[m]) <= target) {
                m = m.checked_sub(1)?;
            }
        }
        Some(m)
    };
    search(false).or_else(|| search(true)).unwrap_or(0)
}

/// Reconstruct per-sample `(offset, size, dts, pts, keyframe)` from the sample table.
fn build_samples(
    stbl: &[([u8; 4], &[u8])],
    stream_index: usize,
    timing: &TrackTiming,
) -> Option<Vec<SampleLoc>> {
    let stsz = find(stbl, b"stsz")?;
    let stsc = find(stbl, b"stsc")?;
    let stts = find(stbl, b"stts")?;
    let stss = find(stbl, b"stss");
    // A count field is attacker-controlled; clamp every one to the entries the
    // box can physically hold (`(box_len - header) / stride`) so a malformed file
    // claiming billions of entries can't drive an eager multi-GB allocation
    // (OOM-abort DoS). Well-formed boxes never exceed this, so it's lossless.
    let entries = |b: &[u8], stride: usize| (b.len().saturating_sub(8)) / stride;
    let chunk_offsets: Vec<u64> = match find(stbl, b"stco") {
        Some(stco) => {
            let n = be32(stco, 4).min(entries(stco, 4) as u32);
            (0..n)
                .map(|n| be32(stco, 8 + 4 * n as usize) as u64)
                .collect()
        }
        None => {
            let co64 = find(stbl, b"co64")?;
            let n = be32(co64, 4).min(entries(co64, 8) as u32);
            (0..n).map(|n| be64(co64, 8 + 8 * n as usize)).collect()
        }
    };

    // Sample sizes (uniform if stsz[4] != 0). Non-uniform stores a 4-byte size
    // per sample, so a claimed count beyond the box is bogus; uniform has no size
    // table, so cap it against a generous sanity ceiling (bounds the walk below).
    const MAX_SAMPLES: usize = 1 << 24; // ~16M, far beyond any real media
    let uniform = be32(stsz, 4);
    let sample_count = if uniform != 0 {
        (be32(stsz, 8) as usize).min(MAX_SAMPLES)
    } else {
        (be32(stsz, 8) as usize).min((stsz.len().saturating_sub(12)) / 4)
    };
    let size_of = |i: usize| -> usize {
        if uniform != 0 {
            uniform as usize
        } else {
            be32(stsz, 12 + 4 * i) as usize
        }
    };

    // stsc entries: (first_chunk, samples_per_chunk) — 12 bytes each.
    let stsc_n = (be32(stsc, 4) as usize).min(entries(stsc, 12));
    let stsc_entries: Vec<(u32, u32)> = (0..stsc_n)
        .map(|e| {
            let o = 8 + 12 * e;
            (be32(stsc, o), be32(stsc, o + 4))
        })
        .collect();
    let samples_in_chunk = |chunk_1based: u32| -> u32 {
        stsc_entries
            .iter()
            .rev()
            .find(|(first, _)| *first <= chunk_1based)
            .map_or(0, |(_, spc)| *spc)
    };

    // Walk chunks, laying out samples sequentially within each. The capacity is
    // only a hint (bounded so a huge uniform sample_count can't pre-allocate GBs);
    // the actual pushes are bounded by sample_count (clamped above).
    let mut samples = Vec::with_capacity(sample_count.min(1 << 16));
    let mut sample_idx = 0usize;
    for (ci, &chunk_off) in chunk_offsets.iter().enumerate() {
        let spc = samples_in_chunk(ci as u32 + 1);
        let mut off = chunk_off as usize;
        for _ in 0..spc {
            if sample_idx >= sample_count {
                break;
            }
            let size = size_of(sample_idx);
            samples.push(SampleLoc {
                stream_index,
                offset: off,
                size,
                dts: 0,
                pts: 0,
                keyframe: false,
            });
            off += size;
            sample_idx += 1;
        }
    }

    // Decode times from stts (run-length of per-sample deltas), in media units.
    // Clamp the entry count to the box; stop once every sample is timestamped so
    // a malicious run-length can't spin billions of empty iterations.
    let stts_n = (be32(stts, 4) as usize).min(entries(stts, 8));
    let mut dts: i64 = 0;
    let mut s = 0usize;
    'stts: for e in 0..stts_n {
        let o = 8 + 8 * e;
        let (count, delta) = (be32(stts, o), be32(stts, o + 4) as i64);
        for _ in 0..count {
            if s >= samples.len() {
                break 'stts;
            }
            samples[s].dts = dts;
            dts = dts.saturating_add(delta);
            s += 1;
        }
    }

    // Composition offsets from ctts, read as FFmpeg reads them: signed whatever the
    // box version, runs with a count that isn't positive skipped. A negative offset
    // sets `dts_shift`, which keeps pts non-negative (FFmpeg leaves the last two
    // entries out of it, a quirk kept for identical timestamps).
    let mut offsets = vec![0i64; samples.len()];
    let mut has_ctts = false;
    let mut dts_shift = 0i64;
    if let Some(ctts) = find(stbl, b"ctts") {
        let n = (be32(ctts, 4) as usize).min(entries(ctts, 8));
        let mut s = 0usize;
        for e in 0..n {
            let o = 8 + 8 * e;
            let (count, offset) = (be32(ctts, o) as i32, be32(ctts, o + 4) as i32);
            if count <= 0 {
                continue;
            }
            has_ctts = true;
            if e + 2 < n && offset < 0 {
                dts_shift = dts_shift.max(-i64::from(offset.max(i32::MIN + 1)));
            }
            for _ in 0..count {
                if s >= offsets.len() {
                    break;
                }
                offsets[s] = i64::from(offset);
                s += 1;
            }
        }
    }

    // Keyframes: from stss if present, else every sample is a sync sample.
    match stss {
        Some(stss) => {
            let n = (be32(stss, 4) as usize).min(entries(stss, 4));
            for e in 0..n {
                let num = be32(stss, 8 + 4 * e) as usize; // 1-based
                if let Some(sample) = samples.get_mut(num.wrapping_sub(1)) {
                    sample.keyframe = true;
                }
            }
        }
        None => samples.iter_mut().for_each(|s| s.keyframe = true),
    }

    // FFmpeg's index starts at -dts_shift, then (mov_build_index) without
    // advanced_editlist moves by the edit's offset, or (mov_fix_index) is rebuilt
    // by the edit list. pts = dts + dts_shift + composition offset.
    let ts: Vec<i64> = samples
        .iter()
        .map(|s| s.dts.saturating_sub(dts_shift))
        .collect();
    let index: Vec<(usize, i64, bool)> = if timing.edits.is_empty() || !timing.advanced_editlist {
        let offset = time_offset(timing);
        (0..samples.len())
            .map(|i| (i, ts[i].saturating_sub(offset), false))
            .collect()
    } else {
        let keyframe: Vec<bool> = samples.iter().map(|s| s.keyframe).collect();
        fix_index(timing, &ts, &keyframe, &offsets, has_ctts, dts_shift)
    };
    Some(
        index
            .into_iter()
            .map(|(i, dts, _discard)| SampleLoc {
                dts,
                pts: if has_ctts {
                    dts.saturating_add(dts_shift).saturating_add(offsets[i])
                } else {
                    dts
                },
                ..samples[i]
            })
            .collect(),
    )
}

/// What `mov_build_index` subtracts from every decode time when FFmpeg doesn't
/// rebuild the index by the edit list: the first non-empty edit's media time, less
/// a leading empty edit, for a list of at most those two.
fn time_offset(t: &TrackTiming) -> i64 {
    if t.advanced_editlist || t.movie_timescale <= 0 {
        return 0;
    }
    let (mut empty, mut start, mut start_index) = (0i64, 0i64, 0);
    for (i, &(duration, media_time)) in t.edits.iter().enumerate() {
        if i == 0 && media_time == -1 {
            empty = duration;
            start_index = 1;
        } else if i == start_index && media_time >= 0 {
            start = media_time;
        }
    }
    if empty == 0 && start == 0 {
        return 0;
    }
    start.wrapping_sub(rescale(empty, t.timescale, t.movie_timescale))
}

// ===========================================================================
// Muxer
// ===========================================================================

fn pu16(v: &mut Vec<u8>, x: u16) {
    v.extend_from_slice(&x.to_be_bytes());
}
fn pu32(v: &mut Vec<u8>, x: u32) {
    v.extend_from_slice(&x.to_be_bytes());
}

/// `[u32 size][4cc type][body]`.
fn bx(typ: &[u8; 4], body: &[u8]) -> Vec<u8> {
    let mut o = Vec::with_capacity(body.len() + 8);
    pu32(&mut o, (body.len() + 8) as u32);
    o.extend_from_slice(typ);
    o.extend_from_slice(body);
    o
}

/// FullBox: `bx` with a leading `version`+`flags`.
fn fbx(typ: &[u8; 4], version: u8, flags: u32, body: &[u8]) -> Vec<u8> {
    let mut b = Vec::with_capacity(body.len() + 4);
    b.push(version);
    b.extend_from_slice(&flags.to_be_bytes()[1..]);
    b.extend_from_slice(body);
    bx(typ, &b)
}

/// 3×3 video transform matrix (identity), as stored in `tkhd`/`mvhd`.
fn identity_matrix(v: &mut Vec<u8>) {
    for &x in &[0x0001_0000u32, 0, 0, 0, 0x0001_0000, 0, 0, 0, 0x4000_0000] {
        pu32(v, x);
    }
}

/// Build an `avcC` box from the SPS and PPS NALs (shared record + box header).
fn build_avcc(sps: &[u8], pps: &[u8]) -> Vec<u8> {
    bx(b"avcC", &rff_format::avc::build_avcc_record(sps, pps))
}

/// Best-effort `av1C` box with the sequence header embedded as configOBUs.
fn build_av1c(sample: &[u8]) -> Option<Vec<u8>> {
    rff_format::av1::config_record(sample).map(|r| bx(b"av1C", &r))
}

/// What a track contributes to the file: its samples + codec-specific entry.
struct TrackOut {
    stream: Stream,
    fourcc: [u8; 4],
    /// Codec configuration box appended to the sample entry (`avcC` / `av1C`).
    config: Option<Vec<u8>>,
    samples: Vec<(Vec<u8>, bool, Option<i64>)>, // (data, keyframe, pts)
}

struct Mp4Muxer {
    out: Output,
    streams: Vec<Stream>,
    /// Sample-entry fourcc per stream, resolved once in
    /// [`write_header`](Muxer::write_header) and parallel to `streams`.
    ///
    /// Resolving here rather than at trailer time is the point: an unmappable
    /// codec is rejected before a single byte is written, and by construction
    /// there is no path that reaches the box writer without a real fourcc.
    fourccs: Vec<[u8; 4]>,
    packets: Vec<(usize, Vec<u8>, bool, Option<i64>)>, // (stream, data, keyframe, pts)
}

impl Mp4Muxer {
    fn new(out: Output) -> Mp4Muxer {
        Mp4Muxer {
            out,
            streams: Vec::new(),
            fourccs: Vec::new(),
            packets: Vec::new(),
        }
    }
}

impl Muxer for Mp4Muxer {
    fn write_header(&mut self, streams: &[Stream]) -> Result<()> {
        if streams.is_empty() {
            return Err(Error::invalid("mp4 mux: no streams"));
        }
        // Refuse anything we cannot describe, rather than writing a file that
        // only fails later, in someone else's player.
        self.fourccs = streams
            .iter()
            .map(mp4_fourcc)
            .collect::<Result<Vec<[u8; 4]>>>()?;
        self.streams = streams.to_vec();
        Ok(())
    }

    fn write_packet(&mut self, packet: &Packet) -> Result<()> {
        self.packets.push((
            packet.stream_index,
            packet.data.clone(),
            packet.is_keyframe(),
            packet.pts,
        ));
        Ok(())
    }

    fn write_trailer(&mut self) -> Result<()> {
        // --- Per-track: gather samples (H.264 → AVCC + avcC; else as-is) ---
        let mut tracks: Vec<TrackOut> = self
            .streams
            .iter()
            .zip(&self.fourccs)
            .map(|(s, fourcc)| TrackOut {
                stream: s.clone(),
                fourcc: *fourcc,
                config: None,
                samples: Vec::new(),
            })
            .collect();

        for (idx, data, keyframe, pts) in &self.packets {
            let Some(track) = tracks.get_mut(*idx) else {
                continue;
            };
            if track.stream.codec_id == CodecId::H264 {
                // Split Annex-B; hoist SPS/PPS into avcC, length-prefix the rest.
                let mut sample = Vec::new();
                let (mut sps, mut pps) = (None, None);
                for nal in split_annexb(data) {
                    match nal.first().map(|b| b & 0x1F) {
                        Some(7) => sps = Some(nal.to_vec()),
                        Some(8) => pps = Some(nal.to_vec()),
                        _ => {
                            pu32(&mut sample, nal.len() as u32);
                            sample.extend_from_slice(nal);
                        }
                    }
                }
                if let (Some(s), Some(p)) = (&sps, &pps) {
                    if track.config.is_none() {
                        track.config = Some(build_avcc(s, p));
                    }
                }
                track.samples.push((sample, *keyframe, *pts));
            } else {
                // AV1 (av01): build av1C from the first sample's sequence header.
                if track.stream.codec_id == CodecId::Avif && track.config.is_none() {
                    track.config = build_av1c(data);
                }
                track.samples.push((data.clone(), *keyframe, *pts));
            }
        }

        // --- every video track must carry its decoder config box ---
        // A VisualSampleEntry without `avcC`/`av1C` is undecodable: the file
        // looks well-formed, opens, and then produces nothing. Same silent
        // class as the zero fourcc, so it gets the same treatment — recover if
        // we can, refuse if we cannot.
        for track in tracks.iter_mut() {
            if track.config.is_some() || track.samples.is_empty() {
                continue;
            }
            match track.stream.codec_id {
                CodecId::H264 => {
                    // The parameter sets may live in `extradata` (an avcC, as a
                    // remux hands us) rather than in-band. Re-derive the box from
                    // the SPS/PPS instead of copying the record verbatim: our
                    // samples are written with 4-byte length prefixes, and a
                    // copied record declaring a different `nal_len` would be a
                    // corruption of its own.
                    track.config = parse_avcc(&track.stream.extradata).and_then(|cfg| {
                        let nals = split_annexb(&cfg.headers_annexb);
                        let nal_type = |n: &&[u8]| n.first().map(|b| b & 0x1F);
                        let sps = nals.iter().find(|n| nal_type(n) == Some(7))?;
                        let pps = nals.iter().find(|n| nal_type(n) == Some(8))?;
                        Some(build_avcc(sps, pps))
                    });
                    if track.config.is_none() {
                        return Err(Error::invalid(
                            "mp4 mux: the h264 stream carries no SPS/PPS, in-band or \
                             in extradata, so its `avcC` cannot be built — the output \
                             would open but decode nothing",
                        ));
                    }
                }
                CodecId::Avif => {
                    return Err(Error::invalid(
                        "mp4 mux: the av1 stream has no sequence header in its first \
                         sample, so its `av1C` cannot be built — the output would \
                         open but decode nothing",
                    ));
                }
                _ => {}
            }
        }

        // --- per-track timing: media timescale + per-sample durations ---
        // (durations come from packet PTS deltas, nominal fallback otherwise).
        let movie_timescale = 1000u32;
        let plans: Vec<(u32, Vec<u32>)> = tracks
            .iter()
            .map(|t| {
                let ts = pick_timescale(&t.stream);
                (ts, sample_durations(&t.samples, ts, t.stream.media_type))
            })
            .collect();

        // --- interleave: order every sample by start time across all tracks ---
        // start time = cumulative duration / timescale (seconds). A *stable* sort
        // keeps each track's samples in order; ties fall back to track order.
        let mut entries: Vec<(f64, usize, usize)> = Vec::new(); // (start_s, track, local)
        for (t, track) in tracks.iter().enumerate() {
            let ts = plans[t].0.max(1) as f64;
            let mut acc = 0i64;
            for i in 0..track.samples.len() {
                entries.push((acc as f64 / ts, t, i));
                acc += plans[t].1[i] as i64;
            }
        }
        entries.sort_by(|a, b| a.0.partial_cmp(&b.0).unwrap_or(std::cmp::Ordering::Equal));

        // --- mdat: write samples interleaved; group each track's contiguous runs
        // into chunks (offset, sample_count) for stco/stsc. ---
        let ftyp = bx(b"ftyp", b"isom\x00\x00\x02\x00isomiso2mp41");
        let mut mdat_body = Vec::new();
        let mdat_data_start = ftyp.len() + 8;
        let sizes: Vec<Vec<u32>> = tracks
            .iter()
            .map(|t| t.samples.iter().map(|(d, _, _)| d.len() as u32).collect())
            .collect();
        let mut chunks: Vec<Vec<(u32, u32)>> = vec![Vec::new(); tracks.len()];
        let mut prev: Option<usize> = None;
        for &(_, t, i) in &entries {
            let off = (mdat_data_start + mdat_body.len()) as u32;
            mdat_body.extend_from_slice(&tracks[t].samples[i].0);
            match (prev, chunks[t].last_mut()) {
                (Some(p), Some(chunk)) if p == t => chunk.1 += 1, // extend run
                _ => chunks[t].push((off, 1)),                    // new chunk
            }
            prev = Some(t);
        }
        let mdat = bx(b"mdat", &mdat_body);

        // --- moov ---
        let mut traks = Vec::new();
        let mut max_movie_dur = 0u32;
        for (t, track) in tracks.iter().enumerate() {
            let (timescale, durations) = &plans[t];
            let media_dur: u64 = durations.iter().map(|&d| d as u64).sum();
            let movie_dur =
                (media_dur * movie_timescale as u64 / (*timescale).max(1) as u64) as u32;
            max_movie_dur = max_movie_dur.max(movie_dur);
            traks.push(build_trak(
                track,
                (t + 1) as u32,
                *timescale,
                durations,
                media_dur as u32,
                movie_dur,
                &chunks[t],
                &sizes[t],
            ));
        }
        let mut moov_body = build_mvhd(movie_timescale, max_movie_dur, tracks.len() as u32 + 1);
        for trak in traks {
            moov_body.extend_from_slice(&trak);
        }
        let moov = bx(b"moov", &moov_body);

        self.out.write_all(&ftyp)?;
        self.out.write_all(&mdat)?;
        self.out.write_all(&moov)?;
        self.out.flush()?;
        Ok(())
    }
}

/// Pick a media timescale for a track: the stream's `time_base` denominator
/// when it looks like `1/N`, else the audio sample rate, else 1000.
fn pick_timescale(s: &Stream) -> u32 {
    let den = s.time_base.den;
    if s.time_base.num == 1 && den > 0 && den <= (1 << 28) {
        den as u32
    } else if s.media_type == MediaType::Audio && s.sample_rate > 0 {
        s.sample_rate
    } else {
        1000
    }
}

/// Per-sample durations (in `timescale` units) from packet PTS deltas. The last
/// sample reuses the previous delta. Falls back to a nominal rate when PTS are
/// missing or non-monotonic (≈30 fps video, ≈20 ms audio).
fn sample_durations(
    samples: &[(Vec<u8>, bool, Option<i64>)],
    timescale: u32,
    media: MediaType,
) -> Vec<u32> {
    let n = samples.len();
    let nominal = match media {
        MediaType::Audio => (timescale / 50).max(1),
        _ => (timescale / 30).max(1),
    };
    if n == 0 {
        return Vec::new();
    }
    let pts: Vec<i64> = samples.iter().map(|(_, _, p)| p.unwrap_or(0)).collect();
    let have_all = samples.iter().all(|(_, _, p)| p.is_some());
    let monotonic = pts.windows(2).all(|w| w[1] >= w[0]);
    if have_all && monotonic && n >= 2 && pts[n - 1] > pts[0] {
        let mut d: Vec<u32> = (0..n - 1)
            .map(|i| (pts[i + 1] - pts[i]).max(0) as u32)
            .collect();
        let last = *d.iter().rev().find(|x| **x > 0).unwrap_or(&nominal);
        d.push(last);
        d
    } else {
        vec![nominal; n]
    }
}

/// Our [`CodecId`] → the MP4 sample-entry fourcc, or `None` when this build has
/// no MP4 mapping for the codec.
///
/// Returning `Option` is load-bearing. This used to fall back to a **zero
/// fourcc**, which is not a valid sample entry: the muxer accepted (say) VP9,
/// wrote a `\0\0\0\0` box, and produced a file every reader misparses — FFmpeg
/// falls back to `rawvideo` and then rejects each frame. A file that silently
/// fails to play is a far worse outcome than a conversion that refuses up
/// front, so the type now forces every caller to handle the unmappable case.
pub(crate) fn codec_fourcc(id: CodecId) -> Option<[u8; 4]> {
    match id {
        CodecId::H264 => Some(*b"avc1"),
        CodecId::Avif => Some(*b"av01"),
        CodecId::Aac => Some(*b"mp4a"),
        CodecId::Opus => Some(*b"Opus"),
        _ => None,
    }
}

/// The fourcc for `stream`, or the error explaining what to do instead.
///
/// Keep the accepted set in step with this format's `MuxCaps` declaration in
/// [`register`] — `crates/rff/tests/mux_caps.rs` gates that the two agree.
pub(crate) fn mp4_fourcc(stream: &Stream) -> Result<[u8; 4]> {
    if let Some(fourcc) = codec_fourcc(stream.codec_id) {
        return Ok(fourcc);
    }
    let name = stream.codec_id.name();
    Err(Error::unsupported(match stream.media_type {
        MediaType::Subtitle => "mp4 mux: subtitle streams are not supported here (no `mov_text`); \
             write Matroska (`.mkv`) instead, or extract the subtitles on their \
             own with `-c:s subrip out.srt`"
            .to_string(),
        MediaType::Video => format!(
            "mp4 mux: video codec `{name}` has no MP4 mapping here (MP4 carries \
             h264 or av1); re-encode with `-c:v h264`, or write `.mkv`/`.webm` \
             to keep `{name}` as a stream copy"
        ),
        MediaType::Audio => format!(
            "mp4 mux: audio codec `{name}` has no MP4 mapping here (MP4 carries \
             aac or opus); re-encode with `-c:a aac`, or write `.mkv` to keep \
             `{name}` as a stream copy"
        ),
        other => format!("mp4 mux: cannot carry a `{other}` stream (codec `{name}`)"),
    }))
}

/// `dOps` (OpusSpecificBox): the MP4 mapping of `OpusHead` (big-endian fields).
fn build_dops(channels: u16, sample_rate: u32) -> Vec<u8> {
    let mut b = Vec::new();
    b.push(0); // Version
    b.push(channels.clamp(1, 255) as u8); // OutputChannelCount
    pu16(&mut b, 0); // PreSkip
    pu32(&mut b, sample_rate); // InputSampleRate
    pu16(&mut b, 0); // OutputGain
    b.push(0); // ChannelMappingFamily (0 = mono/stereo)
    bx(b"dOps", &b)
}

/// AudioSpecificConfig for AAC-LC (shared with the Matroska muxer).
fn build_asc(sample_rate: u32, channels: u16) -> Vec<u8> {
    rff_format::aac::audio_specific_config(sample_rate, channels)
}

/// `esds` box: ES_Descriptor → DecoderConfigDescriptor → DecoderSpecificInfo
/// (the AudioSpecificConfig). The AAC audio sample entry references this for config.
fn build_esds(asc: &[u8]) -> Vec<u8> {
    // DecoderSpecificInfo (0x05) = the ASC.
    let mut dsi = vec![0x05u8, asc.len() as u8];
    dsi.extend_from_slice(asc);
    // DecoderConfigDescriptor (0x04): objectTypeIndication=0x40 (MPEG-4 Audio),
    // streamType=audio(5)<<2|reserved(1)=0x15, bufferSizeDB(3), max+avg bitrate(4+4).
    let mut dcd = vec![0x04u8, (13 + dsi.len()) as u8];
    dcd.extend_from_slice(&[0x40, 0x15, 0, 0, 0, 0, 1, 0, 0, 0, 1, 0, 0]);
    dcd.extend_from_slice(&dsi);
    // ES_Descriptor (0x03): ES_ID(2)=0 + flags(1)=0 + DCD + SLConfigDescriptor(predef=2).
    let mut es = vec![0x00u8, 0x00, 0x00];
    es.extend_from_slice(&dcd);
    es.extend_from_slice(&[0x06, 0x01, 0x02]);
    let mut esd = vec![0x03u8, es.len() as u8];
    esd.extend_from_slice(&es);
    fbx(b"esds", 0, 0, &esd)
}

fn build_mvhd(timescale: u32, duration: u32, next_track_id: u32) -> Vec<u8> {
    let mut b = Vec::new();
    pu32(&mut b, 0); // creation
    pu32(&mut b, 0); // modification
    pu32(&mut b, timescale);
    pu32(&mut b, duration);
    pu32(&mut b, 0x0001_0000); // rate 1.0
    pu16(&mut b, 0x0100); // volume 1.0
    pu16(&mut b, 0); // reserved
    pu32(&mut b, 0);
    pu32(&mut b, 0); // reserved
    identity_matrix(&mut b);
    for _ in 0..6 {
        pu32(&mut b, 0); // pre_defined
    }
    pu32(&mut b, next_track_id);
    fbx(b"mvhd", 0, 0, &b)
}

#[allow(clippy::too_many_arguments)]
fn build_trak(
    track: &TrackOut,
    track_id: u32,
    timescale: u32,
    durations: &[u32],
    media_duration: u32,
    movie_duration: u32,
    chunks: &[(u32, u32)],
    sizes: &[u32],
) -> Vec<u8> {
    let s = &track.stream;

    // tkhd — duration is in the *movie* timescale.
    let mut tk = Vec::new();
    pu32(&mut tk, 0);
    pu32(&mut tk, 0); // creation, modification
    pu32(&mut tk, track_id);
    pu32(&mut tk, 0); // reserved
    pu32(&mut tk, movie_duration);
    pu32(&mut tk, 0);
    pu32(&mut tk, 0); // reserved
    pu16(&mut tk, 0); // layer
    pu16(&mut tk, 0); // alternate_group
    pu16(&mut tk, 0); // volume
    pu16(&mut tk, 0); // reserved
    identity_matrix(&mut tk);
    pu32(&mut tk, s.width << 16);
    pu32(&mut tk, s.height << 16);
    let tkhd = fbx(b"tkhd", 0, 7, &tk);

    // mdia/mdhd — timescale + duration are in the *media* timescale.
    let mut md = Vec::new();
    pu32(&mut md, 0);
    pu32(&mut md, 0);
    pu32(&mut md, timescale);
    pu32(&mut md, media_duration);
    pu16(&mut md, 0x55C4); // language 'und'
    pu16(&mut md, 0);
    let mdhd = fbx(b"mdhd", 0, 0, &md);

    let is_video = s.media_type == MediaType::Video;

    // hdlr
    let mut hd = Vec::new();
    pu32(&mut hd, 0); // pre_defined
    hd.extend_from_slice(if is_video { b"vide" } else { b"soun" });
    pu32(&mut hd, 0);
    pu32(&mut hd, 0);
    pu32(&mut hd, 0); // reserved
    hd.extend_from_slice(b"rff\x00"); // name
    let hdlr = fbx(b"hdlr", 0, 0, &hd);

    // minf: media header (vmhd / smhd) + dinf + stbl
    let media_header = if is_video {
        fbx(b"vmhd", 0, 1, &[0, 0, 0, 0, 0, 0, 0, 0])
    } else {
        fbx(b"smhd", 0, 0, &[0, 0, 0, 0]) // balance + reserved
    };
    let url = fbx(b"url ", 0, 1, &[]);
    let mut dref_b = Vec::new();
    pu32(&mut dref_b, 1); // entry_count
    dref_b.extend_from_slice(&url);
    let dref = fbx(b"dref", 0, 0, &dref_b);
    let dinf = bx(b"dinf", &dref);

    let stbl = build_stbl(track, durations, chunks, sizes);
    let minf = bx(b"minf", &[media_header, dinf, stbl].concat());

    let mdia = bx(b"mdia", &[mdhd, hdlr, minf].concat());
    bx(b"trak", &[tkhd, mdia].concat())
}

fn build_stbl(
    track: &TrackOut,
    durations: &[u32],
    chunks: &[(u32, u32)],
    sizes: &[u32],
) -> Vec<u8> {
    let s = &track.stream;
    let n = sizes.len() as u32;

    // stsd: a video (VisualSampleEntry [+avcC]) or audio (AudioSampleEntry
    // [+dOps]) sample entry, depending on the track's media type.
    let mut entry = Vec::new();
    entry.extend_from_slice(&[0u8; 6]); // reserved
    pu16(&mut entry, 1); // data_reference_index
    if s.media_type == MediaType::Video {
        pu16(&mut entry, 0); // pre_defined
        pu16(&mut entry, 0); // reserved
        for _ in 0..3 {
            pu32(&mut entry, 0); // pre_defined[3]
        }
        pu16(&mut entry, s.width as u16);
        pu16(&mut entry, s.height as u16);
        pu32(&mut entry, 0x0048_0000); // horizresolution 72dpi
        pu32(&mut entry, 0x0048_0000); // vertresolution
        pu32(&mut entry, 0); // reserved
        pu16(&mut entry, 1); // frame_count
        entry.extend_from_slice(&[0u8; 32]); // compressorname
        pu16(&mut entry, 0x0018); // depth
        pu16(&mut entry, 0xFFFF); // pre_defined
        if let Some(cfg) = &track.config {
            entry.extend_from_slice(cfg);
        }
    } else {
        // AudioSampleEntry: channelcount @16, samplerate (16.16) @24.
        pu32(&mut entry, 0);
        pu32(&mut entry, 0); // reserved (2× u32)
        pu16(&mut entry, s.channels.max(1)); // channelcount
        pu16(&mut entry, 16); // samplesize
        pu16(&mut entry, 0); // pre_defined
        pu16(&mut entry, 0); // reserved
        pu32(&mut entry, s.sample_rate << 16); // samplerate
        if s.codec_id == CodecId::Opus {
            entry.extend_from_slice(&build_dops(s.channels, s.sample_rate));
        } else if s.codec_id == CodecId::Aac {
            // esds carries the AudioSpecificConfig: use the stream's extradata (a
            // remux) or synthesize it from rate/channels (a fresh encode).
            let asc = if s.extradata.is_empty() {
                build_asc(s.sample_rate, s.channels)
            } else {
                s.extradata.clone()
            };
            entry.extend_from_slice(&build_esds(&asc));
        }
    }
    let sample_entry = bx(&track.fourcc, &entry);
    let mut stsd_b = Vec::new();
    pu32(&mut stsd_b, 1); // entry_count
    stsd_b.extend_from_slice(&sample_entry);
    let stsd = fbx(b"stsd", 0, 0, &stsd_b);

    // stts: run-length-encode consecutive equal per-sample durations.
    let mut runs: Vec<(u32, u32)> = Vec::new(); // (count, delta)
    for &d in durations {
        match runs.last_mut() {
            Some((count, delta)) if *delta == d => *count += 1,
            _ => runs.push((1, d)),
        }
    }
    let mut stts_b = Vec::new();
    pu32(&mut stts_b, runs.len() as u32);
    for (count, delta) in &runs {
        pu32(&mut stts_b, *count);
        pu32(&mut stts_b, *delta);
    }
    let stts = fbx(b"stts", 0, 0, &stts_b);

    // stsc: run-length over per-chunk sample counts (1-based first_chunk).
    let mut stsc_runs: Vec<(u32, u32)> = Vec::new();
    for (ci, &(_, count)) in chunks.iter().enumerate() {
        if stsc_runs.last().map(|(_, spc)| *spc) != Some(count) {
            stsc_runs.push((ci as u32 + 1, count));
        }
    }
    let mut stsc_b = Vec::new();
    pu32(&mut stsc_b, stsc_runs.len() as u32);
    for (first, spc) in &stsc_runs {
        pu32(&mut stsc_b, *first);
        pu32(&mut stsc_b, *spc);
        pu32(&mut stsc_b, 1); // sample_description_index
    }
    let stsc = fbx(b"stsc", 0, 0, &stsc_b);

    // stsz: explicit sizes.
    let mut stsz_b = Vec::new();
    pu32(&mut stsz_b, 0); // sample_size 0 → per-sample
    pu32(&mut stsz_b, n);
    for &sz in sizes {
        pu32(&mut stsz_b, sz);
    }
    let stsz = fbx(b"stsz", 0, 0, &stsz_b);

    // stco: one offset per chunk.
    let mut stco_b = Vec::new();
    pu32(&mut stco_b, chunks.len() as u32);
    for &(off, _) in chunks {
        pu32(&mut stco_b, off);
    }
    let stco = fbx(b"stco", 0, 0, &stco_b);

    // stss: keyframe sample numbers (1-based).
    let keyframes: Vec<u32> = track
        .samples
        .iter()
        .enumerate()
        .filter(|(_, (_, k, _))| *k)
        .map(|(i, _)| i as u32 + 1)
        .collect();
    let mut stss_b = Vec::new();
    pu32(&mut stss_b, keyframes.len() as u32);
    for &k in &keyframes {
        pu32(&mut stss_b, k);
    }
    let stss = fbx(b"stss", 0, 0, &stss_b);

    bx(b"stbl", &[stsd, stts, stsc, stsz, stco, stss].concat())
}

#[cfg(test)]
mod tests {
    use super::*;
    use std::io::Cursor;

    fn bx(typ: &[u8; 4], body: &[u8]) -> Vec<u8> {
        let mut v = Vec::new();
        v.extend_from_slice(&((body.len() + 8) as u32).to_be_bytes());
        v.extend_from_slice(typ);
        v.extend_from_slice(body);
        v
    }
    fn full(typ: &[u8; 4], body: &[u8]) -> Vec<u8> {
        let mut b = vec![0u8; 4]; // version+flags
        b.extend_from_slice(body);
        bx(typ, &b)
    }

    #[test]
    fn esds_builds_and_parses_back() {
        // Stereo 44.1 kHz AAC-LC ASC is the canonical 0x12 0x10.
        let asc = build_asc(44_100, 2);
        assert_eq!(asc, vec![0x12, 0x10]);
        // build_esds → parse_esds must recover exactly that ASC (the descriptor
        // tree the demuxer walks). Strip the 8-byte box header first.
        let esds = build_esds(&asc);
        assert_eq!(&esds[4..8], b"esds");
        assert_eq!(parse_esds(&esds[8..]), Some(asc));
        // Mono 48 kHz too.
        let asc = build_asc(48_000, 1);
        let esds = build_esds(&asc);
        assert_eq!(parse_esds(&esds[8..]), Some(asc));
    }

    #[test]
    fn demuxes_minimal_h264_mp4() {
        // One H.264 sample (AVCC: 4-byte length + 3 NAL bytes), avcC with no
        // SPS/PPS, 16×16, single chunk.
        let sample = [0x00, 0x00, 0x00, 0x03, 0x65, 0xAA, 0xBB];

        // avcC: version, profile, compat, level, lengthSizeMinusOne=3, 0 SPS, 0 PPS.
        let avcc = bx(b"avcC", &[1, 66, 0, 30, 0xFF, 0xE0, 0x00]);

        // avc1 visual sample entry: 78 fixed bytes (width@32, height@34) + avcC.
        let mut avc1_body = vec![0u8; 78];
        avc1_body[24..26].copy_from_slice(&16u16.to_be_bytes()); // width
        avc1_body[26..28].copy_from_slice(&16u16.to_be_bytes()); // height
        avc1_body.extend_from_slice(&avcc);
        let avc1 = bx(b"avc1", &avc1_body);

        let mut stsd_body = vec![0u8; 4]; // entry_count = 1
        stsd_body[3] = 1;
        stsd_body.extend_from_slice(&avc1);
        let stsd = full(b"stsd", &stsd_body); // full() prepends ver/flags

        let stts = full(b"stts", &[0, 0, 0, 1, 0, 0, 0, 1, 0, 0, 0x03, 0xE8]); // 1 sample, delta 1000
        let stsc = full(b"stsc", &[0, 0, 0, 1, 0, 0, 0, 1, 0, 0, 0, 1, 0, 0, 0, 1]); // chunk1, 1 spc
        let stsz = full(b"stsz", &[0, 0, 0, 0, 0, 0, 0, 1, 0, 0, 0, 7]); // size 0→per-sample, 1 sample, size 7
                                                                         // stco offset patched after assembly.
        let stco = full(b"stco", &[0, 0, 0, 1, 0, 0, 0, 0]);

        let stbl = bx(b"stbl", &[stsd, stts, stsc, stsz, stco].concat());
        let minf = bx(b"minf", &stbl);
        let mdhd = full(
            b"mdhd",
            &[0, 0, 0, 0, 0, 0, 0, 0, 0, 0, 0x03, 0xE8, 0, 0, 0, 0],
        ); // timescale 1000
        let hdlr = full(b"hdlr", &{
            let mut h = vec![0u8; 4];
            h.extend_from_slice(b"vide");
            h.extend_from_slice(&[0u8; 12]);
            h.push(0);
            h
        });
        let mdia = bx(b"mdia", &[mdhd, hdlr, minf].concat());
        let trak = bx(b"trak", &mdia);
        let mvhd = full(b"mvhd", &[0u8; 96]);
        let moov = bx(b"moov", &[mvhd, trak].concat());
        let ftyp = bx(b"ftyp", b"isom\0\0\0\0isom");

        // mdat with the sample; record its absolute offset for stco.
        let mut file = Vec::new();
        file.extend_from_slice(&ftyp);
        file.extend_from_slice(&moov);
        let mdat_data_off = file.len() + 8;
        file.extend_from_slice(&bx(b"mdat", &sample));

        // Patch stco offset: locate it inside the assembled file.
        let stco_marker = file.windows(4).position(|w| w == b"stco").unwrap();
        // stco payload: type@marker, then size before it; offset field = marker+4 (size)+4(ver/flags)+4(count) = marker+12.
        let off_field = stco_marker + 4 + 4 + 4;
        file[off_field..off_field + 4].copy_from_slice(&(mdat_data_off as u32).to_be_bytes());

        assert_eq!(probe_mp4(&file), 90);
        let mut dem = Mp4Demuxer::new(Box::new(Cursor::new(file)));
        let streams = dem.read_header().unwrap();
        assert_eq!(streams.len(), 1);
        assert_eq!(streams[0].codec_id, CodecId::H264);
        assert_eq!(streams[0].media_type, MediaType::Video);
        assert_eq!((streams[0].width, streams[0].height), (16, 16));

        let packet = dem.read_packet().unwrap();
        // AVCC [00 00 00 03 | 65 AA BB] → Annex-B [00 00 00 01 | 65 AA BB].
        assert_eq!(packet.data, vec![0, 0, 0, 1, 0x65, 0xAA, 0xBB]);
        assert!(packet.is_keyframe());
        assert!(matches!(dem.read_packet(), Err(Error::Eof)));
    }

    // ---- timing: ctts, edit lists (FFmpeg's mov_fix_index) ---------------------

    /// One track's tables for [`timed_file`]; samples are one byte each, in one chunk.
    struct TimedTrack {
        handler: &'static [u8; 4],
        /// Sample entry fourcc (`mp4v`, `mp4a`: payloads kept as they are).
        format: &'static [u8; 4],
        timescale: u32,
        stts: Vec<(u32, u32)>,
        ctts: Vec<(u32, i32)>,
        /// 1-based sync samples; empty: no `stss` (every sample is a keyframe).
        stss: Vec<u32>,
        /// (segment duration in the movie timescale, media time); empty: no `edts`.
        elst: Vec<(u32, i32)>,
    }

    fn table(typ: &[u8; 4], rows: &[Vec<u32>]) -> Vec<u8> {
        let mut b = (rows.len() as u32).to_be_bytes().to_vec();
        for row in rows {
            row.iter()
                .for_each(|v| b.extend_from_slice(&v.to_be_bytes()));
        }
        full(typ, &b)
    }

    /// A file of `tracks` (movie timescale 1000), each sample's byte its index.
    fn timed_file(tracks: &[TimedTrack]) -> Vec<u8> {
        let ftyp = bx(b"ftyp", b"isom\0\0\0\0isom");
        let build = |chunk_offsets: &[u32]| -> Vec<u8> {
            let mut traks = Vec::new();
            for (t, track) in tracks.iter().enumerate() {
                let n: u32 = track.stts.iter().map(|&(count, _)| count).sum();
                let mut entry = vec![0u8; 78];
                entry[16..18].copy_from_slice(&1u16.to_be_bytes()); // channels (audio)
                entry[24..28].copy_from_slice(&(48_000u32 << 16).to_be_bytes()); // rate
                let stsd = full(
                    b"stsd",
                    &[&1u32.to_be_bytes()[..], &bx(track.format, &entry)].concat(),
                );
                let rows =
                    |v: &[(u32, u32)]| v.iter().map(|&(a, b)| vec![a, b]).collect::<Vec<_>>();
                let mut stbl = vec![
                    stsd,
                    table(b"stts", &rows(&track.stts)),
                    full(
                        b"stsz",
                        &[&1u32.to_be_bytes()[..], &n.to_be_bytes()].concat(),
                    ),
                ];
                // A track without samples has no chunks either, as in a fragmented file.
                let chunks: &[Vec<u32>] = if n == 0 { &[] } else { &[vec![1, n, 1]] };
                stbl.push(table(b"stsc", chunks));
                let offsets = if n == 0 {
                    vec![]
                } else {
                    vec![vec![chunk_offsets[t]]]
                };
                stbl.push(table(b"stco", &offsets));
                if !track.ctts.is_empty() {
                    let ctts: Vec<(u32, u32)> =
                        track.ctts.iter().map(|&(c, o)| (c, o as u32)).collect();
                    stbl.push(table(b"ctts", &rows(&ctts)));
                }
                if !track.stss.is_empty() {
                    let stss: Vec<Vec<u32>> = track.stss.iter().map(|&k| vec![k]).collect();
                    stbl.push(table(b"stss", &stss));
                }
                let mut mdhd = vec![0u8; 16];
                mdhd[8..12].copy_from_slice(&track.timescale.to_be_bytes());
                let hdlr = full(
                    b"hdlr",
                    &[&[0u8; 4][..], track.handler, &[0u8; 13]].concat(),
                );
                let mdia = bx(
                    b"mdia",
                    &[
                        full(b"mdhd", &mdhd),
                        hdlr,
                        bx(b"minf", &bx(b"stbl", &stbl.concat())),
                    ]
                    .concat(),
                );
                let mut trak = Vec::new();
                if !track.elst.is_empty() {
                    let elst: Vec<Vec<u32>> = track
                        .elst
                        .iter()
                        .map(|&(d, m)| vec![d, m as u32, 0x0001_0000])
                        .collect();
                    trak.extend_from_slice(&bx(b"edts", &table(b"elst", &elst)));
                }
                trak.extend_from_slice(&mdia);
                traks.push(bx(b"trak", &trak));
            }
            let mut mvhd = vec![0u8; 96];
            mvhd[8..12].copy_from_slice(&1000u32.to_be_bytes());
            bx(b"moov", &[full(b"mvhd", &mvhd), traks.concat()].concat())
        };
        // Lay out: ftyp, moov, then each track's samples in one mdat.
        let moov_len = build(&vec![0; tracks.len()]).len();
        let mut offsets = Vec::new();
        let mut data = Vec::new();
        let mut at = (ftyp.len() + moov_len + 8) as u32;
        for track in tracks {
            let n: u32 = track.stts.iter().map(|&(count, _)| count).sum();
            offsets.push(at);
            data.extend((0..n).map(|i| i as u8));
            at += n;
        }
        [ftyp, build(&offsets), bx(b"mdat", &data)].concat()
    }

    /// Every packet of stream `stream` as (sample byte, pts, dts).
    fn timed_packets(file: Vec<u8>, stream: usize) -> Vec<(u8, i64, i64)> {
        let mut dem = Mp4Demuxer::new(Box::new(Cursor::new(file)));
        dem.read_header().unwrap();
        std::iter::from_fn(|| dem.read_packet().ok())
            .filter(|p| p.stream_index == stream)
            .map(|p| (p.data[0], p.pts.unwrap(), p.dts.unwrap()))
            .collect()
    }

    fn video(stts: Vec<(u32, u32)>, ctts: Vec<(u32, i32)>, elst: Vec<(u32, i32)>) -> TimedTrack {
        TimedTrack {
            handler: b"vide",
            format: b"mp4v",
            timescale: 12_288,
            stts,
            ctts,
            stss: vec![1],
            elst,
        }
    }

    fn audio(elst: Vec<(u32, i32)>) -> TimedTrack {
        TimedTrack {
            handler: b"soun",
            format: b"mp4a",
            timescale: 48_000,
            stts: vec![(4, 1024)],
            ctts: vec![],
            stss: vec![],
            elst,
        }
    }

    #[test]
    fn b_frames_take_their_composition_offsets_and_the_edit_list() {
        // Decode order I0 P3 B1 B2 at 512 per frame, shown two frames late (ctts), with
        // the edit starting at that delay (as x264 + FFmpeg's muxer write it).
        let track = video(
            vec![(4, 512)],
            vec![(1, 1024), (1, 2048), (2, 512)],
            vec![(167, 1024)],
        );
        assert_eq!(
            timed_packets(timed_file(&[track]), 0),
            [(0, 0, -1024), (1, 1536, -512), (2, 512, 0), (3, 1024, 512)]
        );
    }

    #[test]
    fn audio_priming_before_the_edit_is_timed_before_zero() {
        // AAC's 1024 samples of priming: the first frame is decoded, not shown.
        assert_eq!(
            timed_packets(timed_file(&[audio(vec![(64, 1024)])]), 0),
            [
                (0, -1024, -1024),
                (1, 0, 0),
                (2, 1024, 1024),
                (3, 2048, 2048)
            ]
        );
    }

    #[test]
    fn an_audio_edit_starting_inside_a_frame_moves_the_timeline_back() {
        // The edit starts 476 samples into frame 1, which FFmpeg keeps (skipping its
        // start) and times back by that much.
        assert_eq!(
            timed_packets(timed_file(&[audio(vec![(64, 1500)])]), 0),
            [
                (0, -1500, -1500),
                (1, -476, -476),
                (2, 548, 548),
                (3, 1572, 1572)
            ]
        );
    }

    #[test]
    fn an_empty_edit_delays_the_track() {
        // 500 ms of nothing (6144 ticks), then the media from its start.
        let mut track = video(vec![(4, 512)], vec![], vec![(500, -1), (1000, 0)]);
        track.stss = vec![];
        assert_eq!(
            timed_packets(timed_file(&[track]), 0),
            [
                (0, 6144, 6144),
                (1, 6656, 6656),
                (2, 7168, 7168),
                (3, 7680, 7680)
            ]
        );
    }

    #[test]
    fn after_a_track_without_stts_entries_the_edit_is_only_an_offset() {
        // FFmpeg turns its edit-list rebuild off at a track with no stts entries (a
        // fragmented file's empty moov) and subtracts the edit's media time instead.
        let empty = TimedTrack {
            stts: vec![],
            ..audio(vec![])
        };
        let mut track = video(vec![(4, 512)], vec![], vec![(1000, 1024)]);
        track.stss = vec![];
        assert_eq!(
            timed_packets(timed_file(&[empty, track]), 1),
            [(0, -1024, -1024), (1, -512, -512), (2, 0, 0), (3, 512, 512)]
        );
    }

    #[test]
    fn split_annexb_handles_4byte_start_codes() {
        let data = [0, 0, 0, 1, 0x67, 0xAA, 0, 0, 0, 1, 0x68, 0xBB, 0xCC];
        let nals = split_annexb(&data);
        assert_eq!(nals, vec![&[0x67u8, 0xAA][..], &[0x68, 0xBB, 0xCC][..]]);
    }

    #[derive(Clone)]
    struct SharedBuf(std::sync::Arc<std::sync::Mutex<Vec<u8>>>);
    impl std::io::Write for SharedBuf {
        fn write(&mut self, b: &[u8]) -> std::io::Result<usize> {
            self.0.lock().unwrap().extend_from_slice(b);
            Ok(b.len())
        }
        fn flush(&mut self) -> std::io::Result<()> {
            Ok(())
        }
    }

    #[test]
    fn mux_then_demux_h264_roundtrips() {
        // Fake Annex-B keyframe: SPS (type 7) + PPS (type 8) + IDR (type 5).
        let sps = [0x67u8, 0x42, 0x00, 0x1E, 0xAA];
        let pps = [0x68u8, 0xCE, 0xBB];
        let idr = [0x65u8, 0x11, 0x22, 0x33];
        let mut annexb = Vec::new();
        for nal in [sps.as_slice(), pps.as_slice(), idr.as_slice()] {
            annexb.extend_from_slice(&[0, 0, 0, 1]);
            annexb.extend_from_slice(nal);
        }

        let sink = SharedBuf(std::sync::Arc::new(std::sync::Mutex::new(Vec::new())));
        {
            let mut mux = Mp4Muxer::new(Box::new(sink.clone()));
            let mut s = Stream::new(0, CodecId::H264);
            s.media_type = MediaType::Video;
            s.width = 16;
            s.height = 16;
            mux.write_header(&[s]).unwrap();
            let mut p = Packet::from_data(0, annexb.clone());
            p.flags.keyframe = true;
            mux.write_packet(&p).unwrap();
            mux.write_trailer().unwrap();
        }
        let file = sink.0.lock().unwrap().clone();
        assert_eq!(&file[4..8], b"ftyp");
        assert_eq!(probe_mp4(&file), 90);

        let mut dem = Mp4Demuxer::new(Box::new(Cursor::new(file)));
        let streams = dem.read_header().unwrap();
        assert_eq!(streams[0].codec_id, CodecId::H264);
        assert_eq!((streams[0].width, streams[0].height), (16, 16));

        // Round-trip: SPS/PPS went into avcC, the demuxer prepends them back on
        // the keyframe → identical Annex-B.
        let packet = dem.read_packet().unwrap();
        assert_eq!(packet.data, annexb);
        assert!(packet.is_keyframe());
        assert!(matches!(dem.read_packet(), Err(Error::Eof)));
    }

    #[test]
    fn mux_then_demux_av_mp4() {
        // Video: a fake H.264 keyframe. Audio: a fake Opus packet.
        let (sps, pps, idr) = (
            [0x67u8, 0x42, 0, 0x1E, 0xAA],
            [0x68u8, 0xCE, 0xBB],
            [0x65u8, 0x11, 0x22],
        );
        let mut video = Vec::new();
        for nal in [sps.as_slice(), pps.as_slice(), idr.as_slice()] {
            video.extend_from_slice(&[0, 0, 0, 1]);
            video.extend_from_slice(nal);
        }
        let opus_pkt = vec![0xFCu8, 0x01, 0x02, 0x03];

        let sink = SharedBuf(std::sync::Arc::new(std::sync::Mutex::new(Vec::new())));
        {
            let mut mux = Mp4Muxer::new(Box::new(sink.clone()));
            let mut vs = Stream::new(0, CodecId::H264);
            vs.media_type = MediaType::Video;
            vs.width = 16;
            vs.height = 16;
            let mut as_ = Stream::new(1, CodecId::Opus);
            as_.media_type = MediaType::Audio;
            as_.channels = 2;
            as_.sample_rate = 48_000;
            mux.write_header(&[vs, as_]).unwrap();
            let mut vp = Packet::from_data(0, video.clone());
            vp.flags.keyframe = true;
            mux.write_packet(&vp).unwrap();
            mux.write_packet(&Packet::from_data(1, opus_pkt.clone()))
                .unwrap();
            mux.write_trailer().unwrap();
        }
        let file = sink.0.lock().unwrap().clone();

        let mut dem = Mp4Demuxer::new(Box::new(Cursor::new(file)));
        let streams = dem.read_header().unwrap();
        assert_eq!(streams.len(), 2);
        let v = streams
            .iter()
            .find(|s| s.media_type == MediaType::Video)
            .unwrap();
        assert_eq!(v.codec_id, CodecId::H264);
        assert_eq!((v.width, v.height), (16, 16));
        let a = streams
            .iter()
            .find(|s| s.media_type == MediaType::Audio)
            .unwrap();
        assert_eq!(a.codec_id, CodecId::Opus);
        assert_eq!(a.channels, 2);
        assert_eq!(a.sample_rate, 48_000);

        let mut by_stream: std::collections::BTreeMap<usize, Vec<Vec<u8>>> = Default::default();
        while let Ok(p) = dem.read_packet() {
            by_stream.entry(p.stream_index).or_default().push(p.data);
        }
        // Video reconstructs the Annex-B; the Opus packet passes through as-is.
        assert!(by_stream.values().any(|ps| ps[0] == video));
        assert!(by_stream.values().any(|ps| ps[0] == opus_pkt));
    }

    #[test]
    fn real_pts_drive_stts_and_timescale() {
        // A 48 kHz audio track with 20 ms (960-sample) frames. The muxer must
        // take its timescale from the stream's time_base and its per-sample
        // durations from the packet PTS — not a nominal 30 fps.
        let pts_in = [0i64, 960, 1920, 2880, 3840];
        let sink = SharedBuf(std::sync::Arc::new(std::sync::Mutex::new(Vec::new())));
        {
            let mut mux = Mp4Muxer::new(Box::new(sink.clone()));
            let mut s = Stream::new(0, CodecId::Opus);
            s.media_type = MediaType::Audio;
            s.channels = 2;
            s.sample_rate = 48_000;
            s.time_base = Rational::new(1, 48_000);
            mux.write_header(&[s]).unwrap();
            for (i, &pts) in pts_in.iter().enumerate() {
                let mut p = Packet::from_data(0, vec![(i as u8) + 1; 8]);
                p.pts = Some(pts);
                mux.write_packet(&p).unwrap();
            }
            mux.write_trailer().unwrap();
        }
        let file = sink.0.lock().unwrap().clone();

        let mut dem = Mp4Demuxer::new(Box::new(Cursor::new(file)));
        let streams = dem.read_header().unwrap();
        // Timescale survived: time_base is 1/48000, not 1/1000.
        assert_eq!(streams[0].time_base, Rational::new(1, 48_000));

        let mut pts_out = Vec::new();
        while let Ok(p) = dem.read_packet() {
            pts_out.push(p.pts.unwrap());
        }
        // PTS reconstructed exactly (last sample reuses the 960 delta).
        assert_eq!(pts_out, pts_in);
    }

    #[test]
    fn samples_are_time_interleaved_in_mdat() {
        // Two tracks, three samples each at the same timestamps (0, 1, 2 s). The
        // mdat must alternate V0, A0, V1, A1, V2, A2 — not V0 V1 V2 A0 A1 A2.
        let tag = |k: u8| vec![0xAB, 0xCD, k, 0xEF];
        // The AV1 track's first sample needs a real sequence-header OBU (type 1,
        // size-field set) or the muxer rightly refuses to write a sample entry
        // with no `av1C`. The tag bytes follow it, so the position checks below
        // are unaffected.
        let seq_header = [0x0Au8, 0x03, 0x00, 0x00, 0x00];
        let sink = SharedBuf(std::sync::Arc::new(std::sync::Mutex::new(Vec::new())));
        {
            let mut mux = Mp4Muxer::new(Box::new(sink.clone()));
            let mut vs = Stream::new(0, CodecId::Avif);
            vs.media_type = MediaType::Video;
            vs.width = 8;
            vs.height = 8;
            vs.time_base = Rational::new(1, 1000);
            let mut as_ = Stream::new(1, CodecId::Opus);
            as_.media_type = MediaType::Audio;
            as_.channels = 1;
            as_.sample_rate = 1000;
            as_.time_base = Rational::new(1, 1000);
            mux.write_header(&[vs, as_]).unwrap();
            for (s, base) in [(0usize, 0u8), (1, 0x10)] {
                for (n, &pts) in [0i64, 1000, 2000].iter().enumerate() {
                    let mut data = Vec::new();
                    if s == 0 && n == 0 {
                        data.extend_from_slice(&seq_header);
                    }
                    data.extend_from_slice(&tag(base + n as u8));
                    let mut p = Packet::from_data(s, data);
                    p.pts = Some(pts);
                    p.flags.keyframe = true;
                    mux.write_packet(&p).unwrap();
                }
            }
            mux.write_trailer().unwrap();
        }
        let file = sink.0.lock().unwrap().clone();

        // File positions must alternate: V0 < A0 < V1 < A1 < V2 < A2.
        let pos = |k: u8| {
            file.windows(4)
                .position(|w| w == tag(k).as_slice())
                .unwrap()
        };
        let order = [pos(0), pos(0x10), pos(1), pos(0x11), pos(2), pos(0x12)];
        assert!(
            order.windows(2).all(|w| w[0] < w[1]),
            "mdat not interleaved: {order:?}"
        );

        // And both tracks still demux back in their own order.
        let mut dem = Mp4Demuxer::new(Box::new(Cursor::new(file)));
        assert_eq!(dem.read_header().unwrap().len(), 2);
        let mut by_stream: std::collections::BTreeMap<usize, Vec<Vec<u8>>> = Default::default();
        while let Ok(p) = dem.read_packet() {
            by_stream.entry(p.stream_index).or_default().push(p.data);
        }
        // Sample 0 of the video track still carries the sequence header it was
        // given: the muxer copies AV1 samples through untouched.
        let mut first = seq_header.to_vec();
        first.extend_from_slice(&tag(0));
        assert_eq!(by_stream[&0], vec![first, tag(1), tag(2)]);
        assert_eq!(by_stream[&1], vec![tag(0x10), tag(0x11), tag(0x12)]);
    }

    // ---- silent-corruption regressions -----------------------------------
    //
    // The muxer used to accept ANY codec and write a zero fourcc for the ones
    // it could not describe, producing a file that opens and then decodes
    // nothing. These pin the refusal.

    /// A stream plausible enough that only the codec check can reject it.
    fn probe_stream(codec: CodecId) -> Stream {
        let mut s = Stream::new(0, codec);
        s.time_base = Rational::new(1, 1000);
        match codec.media_type() {
            MediaType::Video => {
                s.width = 320;
                s.height = 240;
            }
            MediaType::Audio => {
                s.sample_rate = 48_000;
                s.channels = 2;
            }
            _ => {}
        }
        s
    }

    #[test]
    fn refuses_a_video_codec_it_cannot_describe() {
        let mut mux = Mp4Muxer::new(Box::new(Vec::new()));
        let err = mux
            .write_header(&[probe_stream(CodecId::Vp9)])
            .expect_err("vp9 has no MP4 sample entry here");
        let msg = err.to_string();
        assert!(msg.contains("vp9"), "the error must name the codec: {msg}");
        // ...and point somewhere useful, rather than just saying no.
        assert!(
            msg.contains("-c:v h264") || msg.contains(".mkv"),
            "the error should offer a way forward: {msg}"
        );
    }

    #[test]
    fn refuses_an_audio_codec_it_cannot_describe() {
        for codec in [CodecId::Mp3, CodecId::Flac, CodecId::Vorbis, CodecId::Pcm] {
            let mut mux = Mp4Muxer::new(Box::new(Vec::new()));
            let err = mux
                .write_header(&[probe_stream(codec)])
                .expect_err("no MP4 mapping here");
            assert!(
                err.to_string().contains(codec.name()),
                "`{}` must be named in its own rejection",
                codec.name()
            );
        }
    }

    #[test]
    fn refuses_subtitles_and_says_where_they_can_go() {
        let mut mux = Mp4Muxer::new(Box::new(Vec::new()));
        let err = mux
            .write_header(&[probe_stream(CodecId::Subrip)])
            .expect_err("no mov_text support here");
        let msg = err.to_string();
        assert!(msg.contains("subtitle"), "{msg}");
        assert!(msg.contains(".mkv") || msg.contains("subrip"), "{msg}");
    }

    #[test]
    fn accepts_exactly_the_codecs_it_can_describe() {
        for codec in [CodecId::H264, CodecId::Avif, CodecId::Aac, CodecId::Opus] {
            let mut mux = Mp4Muxer::new(Box::new(Vec::new()));
            assert!(
                mux.write_header(&[probe_stream(codec)]).is_ok(),
                "`{}` is declared muxable and must be accepted",
                codec.name()
            );
        }
    }

    #[test]
    fn no_codec_maps_to_a_zero_fourcc() {
        // The corruption was a *value*, not a branch: any mapping that returns
        // all-zero bytes is an invalid sample entry, whatever produced it.
        for codec in [
            CodecId::H264,
            CodecId::Avif,
            CodecId::Aac,
            CodecId::Opus,
            CodecId::Vp9,
            CodecId::Mp3,
            CodecId::Flac,
            CodecId::Pcm,
            CodecId::Vorbis,
            CodecId::Subrip,
            CodecId::WebVtt,
            CodecId::Png,
            CodecId::RawVideo,
        ] {
            if let Some(fourcc) = codec_fourcc(codec) {
                assert_ne!(
                    fourcc,
                    [0, 0, 0, 0],
                    "`{}` maps to a zero fourcc",
                    codec.name()
                );
                assert!(
                    fourcc.iter().all(|b| b.is_ascii_graphic()),
                    "`{}` maps to a non-printable fourcc {fourcc:?}",
                    codec.name()
                );
            }
        }
    }

    #[test]
    fn an_h264_track_without_parameter_sets_is_refused_not_written() {
        // A VisualSampleEntry with no `avcC` opens fine and decodes nothing.
        let mut mux = Mp4Muxer::new(Box::new(Vec::new()));
        let mut s = probe_stream(CodecId::H264);
        s.extradata.clear();
        mux.write_header(&[s]).unwrap();
        // A slice NAL (type 1), so there is a sample but no SPS/PPS anywhere.
        mux.write_packet(&Packet::from_data(0, vec![0, 0, 0, 1, 0x41, 0x9A, 0x00]))
            .unwrap();
        let err = mux
            .write_trailer()
            .expect_err("no SPS/PPS to build avcC from");
        assert!(err.to_string().contains("SPS/PPS"), "{err}");
    }

    #[test]
    fn an_h264_track_recovers_its_avcc_from_extradata() {
        // Parameter sets out-of-band (as a remux hands them over) are enough:
        // the box is re-derived rather than the file being refused.
        let sps = [0x67u8, 0x42, 0x00, 0x1E, 0xAB];
        let pps = [0x68u8, 0xCE, 0x38, 0x80];
        let mut mux = Mp4Muxer::new(Box::new(Vec::new()));
        let mut s = probe_stream(CodecId::H264);
        s.extradata = rff_format::avc::build_avcc_record(&sps, &pps);
        mux.write_header(&[s]).unwrap();
        mux.write_packet(&Packet::from_data(0, vec![0, 0, 0, 1, 0x41, 0x9A, 0x00]))
            .unwrap();
        assert!(
            mux.write_trailer().is_ok(),
            "extradata carries the parameter sets, so the file is writable"
        );
    }

    // ---- seekable input (issue #26) ----------------------------------------

    /// An H.264 + Opus file of `frames` samples per track, each video sample
    /// `frame_bytes` long and distinct, written by our muxer (`moov` last, the
    /// ffmpeg-default layout that forced whole-file buffering).
    fn mux_av(frames: usize, frame_bytes: usize) -> Vec<u8> {
        let sink = SharedBuf(std::sync::Arc::new(std::sync::Mutex::new(Vec::new())));
        {
            let mut mux = Mp4Muxer::new(Box::new(sink.clone()));
            let mut vs = Stream::new(0, CodecId::H264);
            vs.media_type = MediaType::Video;
            vs.width = 16;
            vs.height = 16;
            vs.time_base = Rational::new(1, 1000);
            let mut as_ = Stream::new(1, CodecId::Opus);
            as_.media_type = MediaType::Audio;
            as_.channels = 2;
            as_.sample_rate = 48_000;
            as_.time_base = Rational::new(1, 1000);
            mux.write_header(&[vs, as_]).unwrap();
            for i in 0..frames {
                let key = i % 30 == 0;
                let mut v = Vec::new();
                if key {
                    for nal in [&[0x67u8, 0x42, 0, 0x1E, 0xAA][..], &[0x68, 0xCE, 0xBB]] {
                        v.extend_from_slice(&[0, 0, 0, 1]);
                        v.extend_from_slice(nal);
                    }
                }
                v.extend_from_slice(&[0, 0, 0, 1, if key { 0x65 } else { 0x41 }]);
                v.extend((0..frame_bytes).map(|j| (i * 7 + j) as u8 | 1));
                let mut vp = Packet::from_data(0, v);
                vp.pts = Some(i as i64 * 40);
                vp.flags.keyframe = key;
                mux.write_packet(&vp).unwrap();
                let mut ap = Packet::from_data(1, vec![0xFC, i as u8, (i >> 8) as u8]);
                ap.pts = Some(i as i64 * 40);
                ap.flags.keyframe = true;
                mux.write_packet(&ap).unwrap();
            }
            mux.write_trailer().unwrap();
        }
        let file = sink.0.lock().unwrap().clone();
        file
    }

    #[test]
    fn a_box_smaller_than_its_header_ends_the_walk() {
        // size 4 < 8-byte header: used to slice `buf[8..4]` and panic.
        assert!(child_boxes(&[0, 0, 0, 4, b'f', b'r', b'e', b'e']).is_empty());
    }

    fn drain(dem: &mut Mp4Demuxer) -> Vec<(usize, Option<i64>, bool, Vec<u8>)> {
        let mut out = Vec::new();
        loop {
            match dem.read_packet() {
                Ok(p) => out.push((p.stream_index, p.pts, p.flags.keyframe, p.data)),
                Err(Error::Eof) => return out,
                Err(e) => panic!("read_packet: {e}"),
            }
        }
    }

    /// Counts the bytes actually read through it (seeks are free).
    struct Counting {
        inner: Cursor<Vec<u8>>,
        read: std::sync::Arc<std::sync::atomic::AtomicU64>,
    }
    impl Read for Counting {
        fn read(&mut self, buf: &mut [u8]) -> std::io::Result<usize> {
            let n = self.inner.read(buf)?;
            self.read
                .fetch_add(n as u64, std::sync::atomic::Ordering::Relaxed);
            Ok(n)
        }
    }
    impl Seek for Counting {
        fn seek(&mut self, pos: SeekFrom) -> std::io::Result<u64> {
            self.inner.seek(pos)
        }
    }

    #[test]
    fn seekable_and_streamed_inputs_demux_identically() {
        let file = mux_av(90, 300);
        let mut streamed = Mp4Demuxer::new(Box::new(Cursor::new(file.clone())));
        let mut seekable = Mp4Demuxer::new_seekable(Box::new(Cursor::new(file)));
        let (a, b) = (
            streamed.read_header().unwrap(),
            seekable.read_header().unwrap(),
        );
        assert_eq!(format!("{a:?}"), format!("{b:?}"));
        let (pa, pb) = (drain(&mut streamed), drain(&mut seekable));
        assert_eq!(pa.len(), 180);
        assert_eq!(pa, pb);
    }

    #[test]
    fn seekable_header_reads_only_box_headers_and_moov() {
        // ~4 MiB of media; moov is a few KiB.
        let file = mux_av(400, 10_000);
        let total = file.len() as u64;
        let read = std::sync::Arc::new(std::sync::atomic::AtomicU64::new(0));
        let mut dem = Mp4Demuxer::new_seekable(Box::new(Counting {
            inner: Cursor::new(file),
            read: read.clone(),
        }));
        dem.read_header().unwrap();
        let header_bytes = read.load(std::sync::atomic::Ordering::Relaxed);
        assert!(
            header_bytes < 64 * 1024,
            "read_header read {header_bytes} of {total} bytes"
        );
        // Every sample still arrives, each fetched on demand.
        assert_eq!(drain(&mut dem).len(), 800);
    }

    #[test]
    fn opening_through_the_registry_picks_the_seekable_demuxer() {
        let mut registry = FormatRegistry::new();
        register(&mut registry);
        let file = mux_av(5, 20);
        let read = std::sync::Arc::new(std::sync::atomic::AtomicU64::new(0));
        let mut big = file.clone();
        big.extend(std::iter::repeat_n(0u8, 1 << 20)); // trailing junk past moov
        let input: SeekInput = Box::new(Counting {
            inner: Cursor::new(big),
            read: read.clone(),
        });
        let mut dem = registry.open_demuxer_seekable("mp4", input).unwrap();
        assert_eq!(dem.read_header().unwrap().len(), 2);
        assert!(read.load(std::sync::atomic::Ordering::Relaxed) < 64 * 1024);
    }

    #[test]
    fn malformed_top_level_boxes_error_without_panicking() {
        let file = mux_av(3, 10);
        let mut cases: Vec<Vec<u8>> = vec![
            Vec::new(),
            vec![0, 0, 0, 4, b'f', b't', b'y', b'p'], // size < header
            vec![0, 0, 0, 1, b'm', b'o', b'o', b'v', 0, 0, 0, 0, 0, 0, 0, 3], // largesize < 16
            vec![0xFF, 0xFF, 0xFF, 0xFF, b'm', b'd', b'a', b't'], // overruns the file
            file[..file.len() - 10].to_vec(),         // moov truncated
        ];
        // size == 0 ("to end of file") on a lone moov with garbage inside.
        cases.push([&[0u8, 0, 0, 0][..], b"moov", &[1, 2, 3]].concat());
        for case in cases {
            for seekable in [false, true] {
                let mut dem = if seekable {
                    Mp4Demuxer::new_seekable(Box::new(Cursor::new(case.clone())))
                } else {
                    Mp4Demuxer::new(Box::new(Cursor::new(case.clone())))
                };
                if dem.read_header().is_ok() {
                    // Whatever parsed must still drain to an end.
                    while dem.read_packet().is_ok() {}
                }
            }
        }
    }
}
