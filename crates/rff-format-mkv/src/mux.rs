//! Matroska / WebM muxer.
//!
//! Two muxers share the track planning, block layout and cluster policy:
//!
//! * [`MkvStreamingMuxer`] — for seekable outputs (local files). Streams:
//!   writes the header up front with fixed-width placeholders, writes Clusters
//!   as packets arrive, and at the trailer writes the Cues and seeks back to
//!   patch the Segment size, Duration and SeekHead. Memory is the interleave
//!   queues plus one Cue per Cluster, not the file.
//! * [`MkvMuxer`] — for plain byte sinks (pipes, sockets). Everything is
//!   buffered until [`Muxer::write_trailer`], then laid out in one pass:
//!   cluster offsets are known before the Cues are built, and the SeekHead uses
//!   fixed 8-byte position encodings so its own size never shifts the offsets
//!   it points at.
//!
//! Both write SeekHead + Info + Tracks + Clusters + Cues, in that order.
//!
//! Codec packet contracts follow the rest of rff:
//! * H.264 arrives Annex-B (SPS/PPS in-band on keyframes) and is stored AVCC
//!   with a CodecPrivate `AVCDecoderConfigurationRecord` — the inverse of the
//!   demuxer's normalisation, mirroring `rff-format-mp4`.
//! * Vorbis: three setup headers either packed in `extradata` (u32-LE length
//!   prefixed, the rff contract) or arriving as the first three packets (a
//!   fresh `rusty_vorbis` encode) become the Xiph-laced CodecPrivate.
//! * Opus/AAC/FLAC CodecPrivate come from `extradata` when present and are
//!   synthesized from the stream parameters otherwise.
//!
//! WebM is the restricted doctype: VP9/AV1 video, Opus/Vorbis audio, WebVTT
//! subtitles; anything else is refused rather than written as an invalid file.

use std::collections::VecDeque;
use std::io::{Seek, SeekFrom, Write};

use rff_core::{CodecId, Error, MediaType, Packet, Result, SampleFormat};
use rff_format::avc::{build_avcc_record, split_annexb};
use rff_format::{Muxer, Output, SeekOutput, Stream};

use crate::{
    ID_AUDIO, ID_BIT_DEPTH, ID_BLOCK, ID_BLOCK_DURATION, ID_BLOCK_GROUP, ID_CHANNELS, ID_CLUSTER,
    ID_CODEC_ID, ID_CODEC_PRIVATE, ID_CUES, ID_DURATION, ID_INFO, ID_PIXEL_HEIGHT, ID_PIXEL_WIDTH,
    ID_SAMPLING_FREQUENCY, ID_SEGMENT, ID_SIMPLE_BLOCK, ID_TIMESTAMP, ID_TIMESTAMP_SCALE,
    ID_TRACKS, ID_TRACK_ENTRY, ID_TRACK_NUMBER, ID_TRACK_TYPE, ID_VIDEO,
};

// ---- EBML element IDs used only by the muxer ------------------------------
const ID_EBML: u32 = 0x1A45_DFA3;
const ID_EBML_VERSION: u32 = 0x4286;
const ID_EBML_READ_VERSION: u32 = 0x42F7;
const ID_EBML_MAX_ID_LENGTH: u32 = 0x42F2;
const ID_EBML_MAX_SIZE_LENGTH: u32 = 0x42F3;
const ID_DOCTYPE: u32 = 0x4282;
const ID_DOCTYPE_VERSION: u32 = 0x4287;
const ID_DOCTYPE_READ_VERSION: u32 = 0x4285;
const ID_SEEK_HEAD: u32 = 0x114D_9B74;
const ID_SEEK: u32 = 0x4DBB;
const ID_SEEK_ID: u32 = 0x53AB;
const ID_SEEK_POSITION: u32 = 0x53AC;
const ID_MUXING_APP: u32 = 0x4D80;
const ID_WRITING_APP: u32 = 0x5741;
const ID_TITLE: u32 = 0x7BA9;
const ID_TRACK_UID: u32 = 0x73C5;
const ID_FLAG_LACING: u32 = 0x9C;
const ID_CUE_POINT: u32 = 0xBB;
const ID_CUE_TIME: u32 = 0xB3;
const ID_CUE_TRACK_POSITIONS: u32 = 0xB7;
const ID_CUE_TRACK: u32 = 0xF7;
const ID_CUE_CLUSTER_POSITION: u32 = 0xF1;

// ---- EBML primitive writers -----------------------------------------------

/// Append an element ID (its bytes carry the length marker already).
fn put_id(out: &mut Vec<u8>, id: u32) {
    let n = 4 - (id.leading_zeros() / 8) as usize;
    for i in (0..n).rev() {
        out.push((id >> (8 * i)) as u8);
    }
}

/// Append an EBML size vint, minimal length (never the all-ones "unknown").
fn put_size(out: &mut Vec<u8>, v: u64) {
    let mut len = 1;
    while len < 8 && v >= (1u64 << (7 * len)) - 1 {
        len += 1;
    }
    let first = (0x80u64 >> (len - 1)) | (v >> (8 * (len - 1)));
    out.push(first as u8);
    for i in (0..len - 1).rev() {
        out.push((v >> (8 * i)) as u8);
    }
}

/// Append a full element: id + size + body.
fn put_elem(out: &mut Vec<u8>, id: u32, body: &[u8]) {
    put_id(out, id);
    put_size(out, body.len() as u64);
    out.extend_from_slice(body);
}

/// Minimal big-endian bytes of an unsigned value (at least one byte).
fn uint_body(v: u64) -> Vec<u8> {
    let n = (64 - v.leading_zeros() as usize).div_ceil(8).max(1);
    (0..n).rev().map(|i| (v >> (8 * i)) as u8).collect()
}

fn put_uint(out: &mut Vec<u8>, id: u32, v: u64) {
    put_elem(out, id, &uint_body(v));
}

/// Unsigned element padded to exactly 8 bytes — used where the value is patched
/// in after layout (SeekPosition), so the element's size never changes.
fn put_uint_fixed8(out: &mut Vec<u8>, id: u32, v: u64) {
    put_elem(out, id, &v.to_be_bytes());
}

fn put_float(out: &mut Vec<u8>, id: u32, v: f64) {
    put_elem(out, id, &v.to_be_bytes());
}

fn put_string(out: &mut Vec<u8>, id: u32, s: &str) {
    put_elem(out, id, s.as_bytes());
}

// ---- Codec-specific CodecPrivate builders ---------------------------------

/// OpusHead (RFC 7845 §5.1): what Matroska stores as A_OPUS CodecPrivate.
fn opus_head(channels: u16, sample_rate: u32) -> Vec<u8> {
    let mut v = b"OpusHead".to_vec();
    v.push(1); // version
    v.push(channels.clamp(1, 255) as u8);
    v.extend_from_slice(&0u16.to_le_bytes()); // pre-skip
    v.extend_from_slice(&sample_rate.to_le_bytes()); // input sample rate
    v.extend_from_slice(&0u16.to_le_bytes()); // output gain
    v.push(0); // mapping family 0 (mono/stereo)
    v
}

/// Unpack rff's `extradata` header packing (u32-LE length + bytes, repeated).
fn unpack_headers(extradata: &[u8]) -> Vec<Vec<u8>> {
    let mut out = Vec::new();
    let mut i = 0;
    while i + 4 <= extradata.len() {
        let len = u32::from_le_bytes([
            extradata[i],
            extradata[i + 1],
            extradata[i + 2],
            extradata[i + 3],
        ]) as usize;
        i += 4;
        if i + len > extradata.len() {
            break;
        }
        out.push(extradata[i..i + len].to_vec());
        i += len;
    }
    out
}

/// Xiph-lace three header packets into a Vorbis CodecPrivate.
fn xiph_lace(headers: &[Vec<u8>]) -> Result<Vec<u8>> {
    if headers.len() != 3 {
        return Err(Error::invalid(format!(
            "mkv mux: vorbis needs 3 setup headers, got {}",
            headers.len()
        )));
    }
    let mut v = vec![2u8]; // packet count - 1
    for h in &headers[..2] {
        let mut len = h.len();
        while len >= 255 {
            v.push(255);
            len -= 255;
        }
        v.push(len as u8);
    }
    for h in headers {
        v.extend_from_slice(h);
    }
    Ok(v)
}

/// FLAC CodecPrivate: the `fLaC` magic + a STREAMINFO metadata block. Uses the
/// stream's extradata when it already is one; synthesizes a minimal STREAMINFO
/// from the stream parameters otherwise.
fn flac_private(s: &Stream) -> Vec<u8> {
    if s.extradata.starts_with(b"fLaC") {
        return s.extradata.clone();
    }
    let mut v = b"fLaC".to_vec();
    if s.extradata.len() == 34 {
        // A bare STREAMINFO block: wrap it (last-block flag + type 0 + length).
        v.extend_from_slice(&[0x80, 0, 0, 34]);
        v.extend_from_slice(&s.extradata);
        return v;
    }
    // Synthesize: blocksize bounds only, unknown frame sizes/totals, 16-bit.
    v.extend_from_slice(&[0x80, 0, 0, 34]);
    let mut si = Vec::with_capacity(34);
    si.extend_from_slice(&16u16.to_be_bytes()); // min blocksize
    si.extend_from_slice(&65535u16.to_be_bytes()); // max blocksize
    si.extend_from_slice(&[0; 6]); // min/max framesize unknown
    let rate = s.sample_rate.min((1 << 20) - 1) as u64;
    let ch = s.channels.clamp(1, 8) as u64 - 1;
    let bps = 16u64 - 1;
    // rate(20) | channels-1(3) | bps-1(5) | total-samples(36) = 64 bits.
    let packed: u64 = (rate << 44) | (ch << 41) | (bps << 36);
    si.extend_from_slice(&packed.to_be_bytes());
    si.extend_from_slice(&[0; 16]); // MD5 unknown
    v.extend_from_slice(&si);
    v
}

// ---- Track planning -------------------------------------------------------

/// Everything the muxer decided about one output track.
struct TrackPlan {
    stream: Stream,
    /// Matroska CodecID string.
    codec: &'static str,
    /// CodecPrivate, if known up front (H.264/AV1 discover theirs from packets).
    private: Option<Vec<u8>>,
    /// Audio bit depth to declare, when the format pins one (PCM).
    bit_depth: Option<u64>,
    /// H.264: convert Annex-B packets to AVCC blocks.
    is_avc: bool,
    /// AV1: CodecPrivate is harvested from the first packet's sequence header.
    is_av1: bool,
    /// Vorbis with no extradata: capture the first three packets as headers.
    vorbis_pending: usize,
    vorbis_headers: Vec<Vec<u8>>,
}

/// One buffered frame, timestamp already in milliseconds.
struct BufBlock {
    track: usize,
    ts_ms: i64,
    duration_ms: i64,
    keyframe: bool,
    data: Vec<u8>,
}

fn plan_track(s: &Stream, webm: bool) -> Result<TrackPlan> {
    let doctype = if webm { "webm" } else { "matroska" };
    let mut plan = TrackPlan {
        stream: s.clone(),
        codec: "",
        private: None,
        bit_depth: None,
        is_avc: false,
        is_av1: false,
        vorbis_pending: 0,
        vorbis_headers: Vec::new(),
    };
    let webm_reject = |name: &str| {
        Err(Error::unsupported(format!(
            "webm mux: `{name}` is not a WebM codec (VP9/AV1 video, Opus/Vorbis audio); \
             write .mkv instead"
        )))
    };
    match s.codec_id {
        CodecId::Vp9 => plan.codec = "V_VP9",
        CodecId::Avif => {
            plan.codec = "V_AV1";
            plan.is_av1 = true;
        }
        CodecId::H264 => {
            if webm {
                return webm_reject("h264");
            }
            plan.codec = "V_MPEG4/ISO/AVC";
            plan.is_avc = true;
        }
        CodecId::Opus => {
            plan.codec = "A_OPUS";
            plan.private = Some(if s.extradata.starts_with(b"OpusHead") {
                s.extradata.clone()
            } else {
                opus_head(s.channels, s.sample_rate)
            });
        }
        CodecId::Vorbis => {
            plan.codec = "A_VORBIS";
            let headers = unpack_headers(&s.extradata);
            if headers.len() == 3 {
                plan.private = Some(xiph_lace(&headers)?);
            } else {
                // Fresh encode: rusty_vorbis emits ident/comment/setup as the
                // first three packets (the Ogg contract). Capture them.
                plan.vorbis_pending = 3;
            }
        }
        CodecId::Aac => {
            if webm {
                return webm_reject("aac");
            }
            plan.codec = "A_AAC";
            plan.private = Some(if s.extradata.is_empty() {
                rff_format::aac::audio_specific_config(s.sample_rate, s.channels)
            } else {
                s.extradata.clone()
            });
        }
        CodecId::Flac => {
            if webm {
                return webm_reject("flac");
            }
            plan.codec = "A_FLAC";
            plan.private = Some(flac_private(s));
        }
        CodecId::Mp3 => {
            if webm {
                return webm_reject("mp3");
            }
            plan.codec = "A_MPEG/L3";
        }
        CodecId::Pcm => {
            if webm {
                return webm_reject("pcm");
            }
            match s.sample_format {
                Some(SampleFormat::S16) | None => {
                    plan.codec = "A_PCM/INT/LIT";
                    plan.bit_depth = Some(16);
                }
                Some(SampleFormat::F32) => {
                    plan.codec = "A_PCM/FLOAT/IEEE";
                    plan.bit_depth = Some(32);
                }
                Some(other) => {
                    return Err(Error::unsupported(format!(
                        "mkv mux: PCM sample format `{}` (interleaved s16/f32 only)",
                        other.name()
                    )))
                }
            }
        }
        CodecId::Subrip => {
            if webm {
                return webm_reject("subrip (WebM subtitles are WebVTT — use -c:s webvtt)");
            }
            plan.codec = "S_TEXT/UTF8";
        }
        CodecId::WebVtt => plan.codec = "S_TEXT/WEBVTT",
        other => {
            return Err(Error::unsupported(format!(
                "{doctype} mux: codec `{}` has no Matroska mapping",
                other.name()
            )))
        }
    }
    Ok(plan)
}

// ---- Shared element builders ----------------------------------------------

/// Packet timestamp → milliseconds (the TimestampScale we write is 1 ms).
/// Timestamps are in the *stream's* time base, per the muxer contract.
fn to_ms(tb: rff_core::Rational, v: i64) -> i64 {
    if tb.num <= 0 || tb.den <= 0 {
        return v;
    }
    ((v as i128 * 1000 * tb.num as i128 + (tb.den as i128 / 2)) / tb.den as i128) as i64
}

/// The EBML header element.
fn ebml_header(webm: bool) -> Vec<u8> {
    let mut ebml = Vec::new();
    put_uint(&mut ebml, ID_EBML_VERSION, 1);
    put_uint(&mut ebml, ID_EBML_READ_VERSION, 1);
    put_uint(&mut ebml, ID_EBML_MAX_ID_LENGTH, 4);
    put_uint(&mut ebml, ID_EBML_MAX_SIZE_LENGTH, 8);
    put_string(
        &mut ebml,
        ID_DOCTYPE,
        if webm { "webm" } else { "matroska" },
    );
    put_uint(&mut ebml, ID_DOCTYPE_VERSION, 4);
    put_uint(&mut ebml, ID_DOCTYPE_READ_VERSION, 2);
    let mut out = Vec::new();
    put_elem(&mut out, ID_EBML, &ebml);
    out
}

/// The Info element. Returns it with the offset (within the element) of the
/// Duration *element*, when one was written.
fn info_element(duration_ms: Option<f64>, title: Option<&str>) -> (Vec<u8>, Option<usize>) {
    let mut info = Vec::new();
    put_uint(&mut info, ID_TIMESTAMP_SCALE, 1_000_000); // 1 ms ticks
    let duration_at = duration_ms.map(|d| {
        let at = info.len();
        put_float(&mut info, ID_DURATION, d);
        at
    });
    if let Some(title) = title {
        put_string(&mut info, ID_TITLE, title);
    }
    put_string(
        &mut info,
        ID_MUXING_APP,
        concat!("rff ", env!("CARGO_PKG_VERSION")),
    );
    put_string(
        &mut info,
        ID_WRITING_APP,
        concat!("rff ", env!("CARGO_PKG_VERSION")),
    );
    let mut elem = Vec::new();
    put_elem(&mut elem, ID_INFO, &info);
    let header_len = elem.len() - info.len();
    (elem, duration_at.map(|at| header_len + at))
}

/// The Tracks element.
fn tracks_element(tracks: &[TrackPlan]) -> Vec<u8> {
    let mut tracks_body = Vec::new();
    for (i, plan) in tracks.iter().enumerate() {
        let s = &plan.stream;
        let mut te = Vec::new();
        put_uint(&mut te, ID_TRACK_NUMBER, i as u64 + 1);
        put_uint(&mut te, ID_TRACK_UID, i as u64 + 1);
        let track_type = match s.media_type {
            MediaType::Video => 1,
            MediaType::Audio => 2,
            MediaType::Subtitle => 17,
            _ => 0,
        };
        put_uint(&mut te, ID_TRACK_TYPE, track_type);
        put_uint(&mut te, ID_FLAG_LACING, 0);
        put_string(&mut te, ID_CODEC_ID, plan.codec);
        if let Some(private) = &plan.private {
            put_elem(&mut te, ID_CODEC_PRIVATE, private);
        }
        match s.media_type {
            MediaType::Video => {
                let mut v = Vec::new();
                put_uint(&mut v, ID_PIXEL_WIDTH, s.width as u64);
                put_uint(&mut v, ID_PIXEL_HEIGHT, s.height as u64);
                put_elem(&mut te, ID_VIDEO, &v);
            }
            MediaType::Audio => {
                let mut a = Vec::new();
                put_float(&mut a, ID_SAMPLING_FREQUENCY, s.sample_rate.max(1) as f64);
                put_uint(&mut a, ID_CHANNELS, s.channels.max(1) as u64);
                if let Some(bd) = plan.bit_depth {
                    put_uint(&mut a, ID_BIT_DEPTH, bd);
                }
                put_elem(&mut te, ID_AUDIO, &a);
            }
            _ => {}
        }
        put_elem(&mut tracks_body, ID_TRACK_ENTRY, &te);
    }
    let mut tracks_elem = Vec::new();
    put_elem(&mut tracks_elem, ID_TRACKS, &tracks_body);
    tracks_elem
}

fn missing_avc_config() -> Error {
    Error::invalid("mkv mux: H.264 stream carried no SPS/PPS (need Annex-B with in-band headers)")
}

/// Cluster policy: open a new Cluster for a block at `ts_ms` when there is
/// none yet, when the current one spans 5 s (which keeps every relative
/// timestamp far inside i16 ms), or on a video keyframe at least 1 s in.
fn starts_cluster(cluster_ts: Option<i64>, ts_ms: i64, video_keyframe: bool) -> bool {
    match cluster_ts {
        None => true,
        Some(cts) => {
            let rel = ts_ms - cts;
            rel >= 5_000 || (video_keyframe && rel >= 1_000)
        }
    }
}

/// Serialize one block around its payload: the bytes before the frame data
/// and the bytes after it (a BlockGroup's BlockDuration). Subtitles need a
/// duration, so they get BlockGroup { Block, BlockDuration }; everything else
/// is a SimpleBlock carrying the keyframe flag.
fn block_parts(b: &BufBlock, rel: i16, is_sub: bool) -> (Vec<u8>, Vec<u8>) {
    // Block header: track vint + relative i16 + flags.
    let mut hdr = Vec::with_capacity(4);
    put_size(&mut hdr, b.track as u64 + 1); // track numbers are vints
    hdr.extend_from_slice(&rel.to_be_bytes());
    let block_len = (hdr.len() + 1 + b.data.len()) as u64;
    let mut head = Vec::with_capacity(32);
    let mut tail = Vec::new();
    if is_sub {
        hdr.push(0x00);
        put_uint(&mut tail, ID_BLOCK_DURATION, b.duration_ms.max(0) as u64);
        let group_len = id_len(ID_BLOCK) + size_len(block_len) + block_len + tail.len() as u64;
        put_id(&mut head, ID_BLOCK_GROUP);
        put_size(&mut head, group_len);
        put_id(&mut head, ID_BLOCK);
        put_size(&mut head, block_len);
    } else {
        hdr.push(if b.keyframe { 0x80 } else { 0x00 });
        put_id(&mut head, ID_SIMPLE_BLOCK);
        put_size(&mut head, block_len);
    }
    head.extend_from_slice(&hdr);
    (head, tail)
}

fn id_len(id: u32) -> u64 {
    4 - (id.leading_zeros() / 8) as u64
}

/// Length of the minimal size vint [`put_size`] writes for `v`.
fn size_len(v: u64) -> u64 {
    let mut len = 1;
    while len < 8 && v >= (1u64 << (7 * len)) - 1 {
        len += 1;
    }
    len
}

/// One CuePoint: `cts` (ms) at Segment-relative `pos`, for `cue_track`.
fn put_cue_point(cues_body: &mut Vec<u8>, cts: i64, cue_track: u64, pos: u64) {
    let mut ctp = Vec::new();
    put_uint(&mut ctp, ID_CUE_TRACK, cue_track);
    put_uint(&mut ctp, ID_CUE_CLUSTER_POSITION, pos);
    let mut point = Vec::new();
    put_uint(&mut point, ID_CUE_TIME, cts as u64);
    put_elem(&mut point, ID_CUE_TRACK_POSITIONS, &ctp);
    put_elem(cues_body, ID_CUE_POINT, &point);
}

/// Cues reference the first video track (or track 1 when there is no video).
fn cue_track(tracks: &[TrackPlan]) -> u64 {
    tracks
        .iter()
        .position(|p| p.stream.media_type == MediaType::Video)
        .unwrap_or(0) as u64
        + 1
}

/// Turn one packet into a block for `tracks[packet.stream_index]`, updating
/// the plan's CodecPrivate as packets reveal it. `None` = the packet was a
/// Vorbis setup header, absorbed into CodecPrivate.
fn prepare_block(tracks: &mut [TrackPlan], packet: &Packet) -> Result<Option<BufBlock>> {
    let idx = packet.stream_index;
    let Some(plan) = tracks.get_mut(idx) else {
        return Err(Error::invalid(format!(
            "mkv mux: packet for unknown stream {idx}"
        )));
    };

    // Vorbis fresh-encode: the first three packets are the setup headers,
    // which belong in CodecPrivate, not in the stream.
    if plan.vorbis_pending > 0 {
        plan.vorbis_headers.push(packet.data.clone());
        plan.vorbis_pending -= 1;
        if plan.vorbis_pending == 0 {
            plan.private = Some(xiph_lace(&plan.vorbis_headers)?);
            plan.vorbis_headers = Vec::new();
        }
        return Ok(None);
    }

    // H.264: Annex-B → AVCC block; hoist SPS/PPS into CodecPrivate.
    let data = if plan.is_avc {
        let mut sample = Vec::new();
        let (mut sps, mut pps) = (None, None);
        for nal in split_annexb(&packet.data) {
            match nal.first().map(|b| b & 0x1F) {
                Some(7) => sps = Some(nal.to_vec()),
                Some(8) => pps = Some(nal.to_vec()),
                _ => {
                    sample.extend_from_slice(&(nal.len() as u32).to_be_bytes());
                    sample.extend_from_slice(nal);
                }
            }
        }
        if plan.private.is_none() {
            if let (Some(s), Some(p)) = (&sps, &pps) {
                plan.private = Some(build_avcc_record(s, p));
            }
        }
        sample
    } else {
        if plan.is_av1 && plan.private.is_none() {
            plan.private = rff_format::av1::config_record(&packet.data);
        }
        packet.data.clone()
    };

    let tb = plan.stream.time_base;
    let audio = plan.stream.media_type == MediaType::Audio;
    Ok(Some(BufBlock {
        track: idx,
        ts_ms: to_ms(tb, packet.pts.or(packet.dts).unwrap_or(0)),
        duration_ms: to_ms(tb, packet.duration.max(0)),
        // Audio frames are all random-access; trust the flag on video.
        keyframe: audio || packet.flags.keyframe,
        data,
    }))
}

// ---- The in-memory muxer (non-seekable outputs) ---------------------------

/// Matroska/WebM muxer for a plain byte sink (a pipe, a socket): every block
/// is buffered and the file is laid out in one pass at the trailer. Seekable
/// outputs get [`MkvStreamingMuxer`] instead, which streams.
pub struct MkvMuxer {
    out: Option<Output>,
    webm: bool,
    tracks: Vec<TrackPlan>,
    blocks: Vec<BufBlock>,
    /// `-metadata title=...` → the Segment Info Title element.
    title: Option<String>,
}

impl MkvMuxer {
    pub fn new(out: Output, webm: bool) -> MkvMuxer {
        MkvMuxer {
            out: Some(out),
            webm,
            tracks: Vec::new(),
            blocks: Vec::new(),
            title: None,
        }
    }
}

impl Muxer for MkvMuxer {
    fn set_metadata(&mut self, metadata: &rff_core::Dictionary) {
        self.title = metadata.get("title").map(str::to_owned);
    }

    fn write_header(&mut self, streams: &[Stream]) -> Result<()> {
        if streams.is_empty() {
            return Err(Error::invalid("mkv mux: no streams"));
        }
        self.tracks = streams
            .iter()
            .map(|s| plan_track(s, self.webm))
            .collect::<Result<Vec<_>>>()?;
        Ok(())
    }

    fn write_packet(&mut self, packet: &Packet) -> Result<()> {
        if let Some(block) = prepare_block(&mut self.tracks, packet)? {
            self.blocks.push(block);
        }
        Ok(())
    }

    fn write_trailer(&mut self) -> Result<()> {
        let mut out = self
            .out
            .take()
            .ok_or_else(|| Error::invalid("mkv mux: trailer already written"))?;

        let mut file = ebml_header(self.webm);

        // --- Info ---
        let duration_ms = self
            .blocks
            .iter()
            .map(|b| b.ts_ms + b.duration_ms.max(0))
            .max()
            .unwrap_or(0);
        let (info_elem, _) = info_element(
            (duration_ms > 0).then_some(duration_ms as f64),
            self.title.as_deref(),
        );

        // --- Tracks ---
        let tracks_elem = tracks_element(&self.tracks);

        // A missing H.264 config means no SPS/PPS ever appeared — the file
        // would be undecodable; refuse rather than write it silently broken.
        for (i, plan) in self.tracks.iter().enumerate() {
            if plan.is_avc && plan.private.is_none() && self.blocks.iter().any(|b| b.track == i) {
                return Err(missing_avc_config());
            }
        }

        // --- Clusters: stable-sort by timestamp, roll on keyframes/5 s ---
        self.blocks.sort_by_key(|b| b.ts_ms);
        let is_video: Vec<bool> = self
            .tracks
            .iter()
            .map(|p| p.stream.media_type == MediaType::Video)
            .collect();
        let is_sub: Vec<bool> = self
            .tracks
            .iter()
            .map(|p| p.stream.media_type == MediaType::Subtitle)
            .collect();

        let mut clusters: Vec<(i64, Vec<u8>)> = Vec::new(); // (cluster ts, body)
        for b in &self.blocks {
            let video_key = is_video[b.track] && b.keyframe;
            if starts_cluster(clusters.last().map(|c| c.0), b.ts_ms, video_key) {
                let mut body = Vec::new();
                put_uint(&mut body, ID_TIMESTAMP, b.ts_ms.max(0) as u64);
                clusters.push((b.ts_ms.max(0), body));
            }
            let (cts, body) = clusters.last_mut().expect("just ensured");
            let rel = (b.ts_ms - *cts).clamp(i16::MIN as i64, i16::MAX as i64) as i16;
            let (head, tail) = block_parts(b, rel, is_sub[b.track]);
            body.extend_from_slice(&head);
            body.extend_from_slice(&b.data);
            body.extend_from_slice(&tail);
        }
        let cluster_elems: Vec<Vec<u8>> = clusters
            .iter()
            .map(|(_, body)| {
                let mut e = Vec::new();
                put_elem(&mut e, ID_CLUSTER, body);
                e
            })
            .collect();

        // --- Layout: SeekHead, Info, Tracks, Clusters..., Cues ---
        // Positions are relative to the start of the Segment's data. The
        // SeekHead's size is invariant (fixed-8-byte positions), so measure it
        // with placeholders first.
        let seekhead_len = build_seekhead(0, 0, 0).len();
        let info_pos = seekhead_len as u64;
        let tracks_pos = info_pos + info_elem.len() as u64;
        let mut cluster_pos = tracks_pos + tracks_elem.len() as u64;
        let mut cluster_offsets = Vec::with_capacity(cluster_elems.len());
        for c in &cluster_elems {
            cluster_offsets.push(cluster_pos);
            cluster_pos += c.len() as u64;
        }
        let cues_pos = cluster_pos;

        // Cues: one point per cluster.
        let cue_track = cue_track(&self.tracks);
        let mut cues_body = Vec::new();
        for ((cts, _), off) in clusters.iter().zip(&cluster_offsets) {
            put_cue_point(&mut cues_body, *cts, cue_track, *off);
        }
        let mut cues_elem = Vec::new();
        put_elem(&mut cues_elem, ID_CUES, &cues_body);

        let seekhead = build_seekhead(info_pos, tracks_pos, cues_pos);
        debug_assert_eq!(seekhead.len(), seekhead_len);

        // --- Segment ---
        let segment_len = seekhead.len()
            + info_elem.len()
            + tracks_elem.len()
            + cluster_elems.iter().map(Vec::len).sum::<usize>()
            + cues_elem.len();
        put_id(&mut file, ID_SEGMENT);
        put_size(&mut file, segment_len as u64);
        file.extend_from_slice(&seekhead);
        file.extend_from_slice(&info_elem);
        file.extend_from_slice(&tracks_elem);
        for c in &cluster_elems {
            file.extend_from_slice(c);
        }
        file.extend_from_slice(&cues_elem);

        out.write_all(&file)?;
        out.flush()?;
        Ok(())
    }
}

/// Build the SeekHead pointing at Info/Tracks/Cues. Positions are encoded as
/// fixed 8-byte uints so the element's size is independent of their values.
fn build_seekhead(info_pos: u64, tracks_pos: u64, cues_pos: u64) -> Vec<u8> {
    let mut body = Vec::new();
    for (target, pos) in [
        (ID_INFO, info_pos),
        (ID_TRACKS, tracks_pos),
        (ID_CUES, cues_pos),
    ] {
        let mut id_bytes = Vec::new();
        put_id(&mut id_bytes, target);
        let mut seek = Vec::new();
        put_elem(&mut seek, ID_SEEK_ID, &id_bytes);
        put_uint_fixed8(&mut seek, ID_SEEK_POSITION, pos);
        put_elem(&mut body, ID_SEEK, &seek);
    }
    let mut out = Vec::new();
    put_elem(&mut out, ID_SEEK_HEAD, &body);
    out
}

// ---- The streaming muxer (seekable outputs) -------------------------------

/// An 8-byte EBML size vint holding `v` — the fixed width every size this
/// muxer patches after the fact is written with, so patching never moves a
/// byte. Covers sizes up to 2^56 - 2 (64 PiB).
fn size_vint8(v: u64) -> [u8; 8] {
    debug_assert!(v < (1 << 56) - 1, "EBML size {v} exceeds 8-byte vint");
    let mut b = v.to_be_bytes();
    b[0] = 0x01;
    b
}

/// The 8-byte "unknown size" vint. Placeholders start as this, so a file cut
/// short before the trailer (a crash, a full disk) still parses as a live
/// stream: unknown-size Segment and Clusters, which end at the next Cluster.
const UNKNOWN_SIZE8: [u8; 8] = [0x01, 0xFF, 0xFF, 0xFF, 0xFF, 0xFF, 0xFF, 0xFF];

/// Interleaving: a block is written once every non-subtitle track has one
/// queued (so the earliest is known), or once the queue spans this much time
/// — FFmpeg's default `max_interleave_delta` — so a track that ended or went
/// quiet cannot hold the others back for ever.
const MAX_INTERLEAVE_MS: i64 = 10_000;
/// ...or once this many payload bytes are queued, whatever the timestamps.
const MAX_INTERLEAVE_BYTES: usize = 64 << 20;

/// A block waiting in its track's interleave queue.
struct Queued {
    /// Interleave key: decode time (ms) when known, else presentation time.
    key_ms: i64,
    /// Arrival order — breaks key ties the way a stable sort would.
    seq: u64,
    block: BufBlock,
}

/// The Cluster being written: its size is patched in when it closes.
struct OpenCluster {
    ts_ms: i64,
    /// Position of the Cluster's 8-byte size field.
    size_at: u64,
    /// Position of the first byte of the Cluster's body.
    body_at: u64,
}

/// Matroska/WebM muxer for a seekable output: streams, like FFmpeg's
/// matroskaenc on a seekable file.
///
/// The header (EBML header; a Segment with an 8-byte size placeholder; a
/// SeekHead with fixed 8-byte positions; Info with a fixed-width Duration
/// placeholder; Tracks) is written as soon as every track's CodecPrivate is
/// known — immediately, unless it is harvested from the first packet (H.264,
/// AV1, a fresh Vorbis encode). Blocks are then interleaved by timestamp
/// across tracks through small per-track queues and written straight into
/// Clusters, each Cluster's 8-byte size patched when it closes. Only the Cues
/// (one point per Cluster) accumulate in memory; the trailer writes them and
/// seeks back to patch the Segment size, the Duration and the SeekHead's Cues
/// position. Cluster policy, block layout and timestamps are those of the
/// in-memory [`MkvMuxer`]. All positions are 64-bit, so files past 4 GiB are
/// fine.
pub struct MkvStreamingMuxer {
    out: SeekOutput,
    webm: bool,
    tracks: Vec<TrackPlan>,
    title: Option<String>,
    is_video: Vec<bool>,
    is_sub: Vec<bool>,
    /// Per-track queues, each in arrival (= decode) order.
    queues: Vec<VecDeque<Queued>>,
    queued_bytes: usize,
    /// Largest interleave key queued so far.
    max_key_ms: i64,
    seq: u64,
    header_written: bool,
    finished: bool,
    /// Absolute output position the file starts at.
    base: u64,
    /// Bytes written so far (positions below are relative to `base`).
    pos: u64,
    segment_size_at: u64,
    /// Start of the Segment body — Segment-relative positions count from here.
    segment_data: u64,
    duration_at: u64,
    seekhead_cues_at: u64,
    cluster: Option<OpenCluster>,
    /// (Cluster timestamp ms, Segment-relative position) per Cluster.
    cues: Vec<(i64, u64)>,
    /// Segment duration so far: the max block end (ms).
    end_ms: Option<i64>,
}

impl MkvStreamingMuxer {
    pub fn new(out: SeekOutput, webm: bool) -> MkvStreamingMuxer {
        MkvStreamingMuxer {
            out,
            webm,
            tracks: Vec::new(),
            title: None,
            is_video: Vec::new(),
            is_sub: Vec::new(),
            queues: Vec::new(),
            queued_bytes: 0,
            max_key_ms: i64::MIN,
            seq: 0,
            header_written: false,
            finished: false,
            base: 0,
            pos: 0,
            segment_size_at: 0,
            segment_data: 0,
            duration_at: 0,
            seekhead_cues_at: 0,
            cluster: None,
            cues: Vec::new(),
            end_ms: None,
        }
    }

    fn write(&mut self, bytes: &[u8]) -> Result<()> {
        self.out.write_all(bytes)?;
        self.pos += bytes.len() as u64;
        Ok(())
    }

    /// Overwrite `bytes` at `at` (already written), then return to the end.
    fn patch(&mut self, at: u64, bytes: &[u8]) -> Result<()> {
        debug_assert!(at + bytes.len() as u64 <= self.pos);
        self.out.seek(SeekFrom::Start(self.base + at))?;
        self.out.write_all(bytes)?;
        self.out.seek(SeekFrom::Start(self.base + self.pos))?;
        Ok(())
    }

    /// Every CodecPrivate that packets can still reveal has been revealed.
    fn header_ready(&self) -> bool {
        self.tracks
            .iter()
            .all(|p| p.vorbis_pending == 0 && !((p.is_avc || p.is_av1) && p.private.is_none()))
    }

    /// The interleave queues hold more than they should: write regardless.
    fn over_cap(&self) -> bool {
        if self.queued_bytes > MAX_INTERLEAVE_BYTES {
            return true;
        }
        let min_head = self
            .queues
            .iter()
            .filter_map(|q| q.front().map(|e| e.key_ms))
            .min();
        min_head.is_some_and(|m| self.max_key_ms.saturating_sub(m) > MAX_INTERLEAVE_MS)
    }

    fn write_file_header(&mut self) -> Result<()> {
        // An H.264 track that already has blocks but no SPS/PPS would be
        // undecodable (the in-memory muxer refuses it at the trailer).
        for (i, plan) in self.tracks.iter().enumerate() {
            if plan.is_avc && plan.private.is_none() && !self.queues[i].is_empty() {
                return Err(missing_avc_config());
            }
        }
        self.base = self.out.stream_position()?;
        let ebml = ebml_header(self.webm);
        self.write(&ebml)?;

        let mut seg = Vec::with_capacity(12);
        put_id(&mut seg, ID_SEGMENT);
        self.segment_size_at = self.pos + seg.len() as u64;
        seg.extend_from_slice(&UNKNOWN_SIZE8);
        self.write(&seg)?;
        self.segment_data = self.pos;

        // Duration is always written (an 8-byte float placeholder) and patched
        // at the trailer — or turned into a Void if the file has no duration.
        let (info, duration_at) = info_element(Some(0.0), self.title.as_deref());
        let tracks = tracks_element(&self.tracks);
        let seekhead_len = build_seekhead(0, 0, 0).len() as u64;
        let info_pos = seekhead_len;
        let tracks_pos = info_pos + info.len() as u64;
        // The Cues position is the SeekHead's last 8 bytes; patched at the end.
        let seekhead = build_seekhead(info_pos, tracks_pos, 0);
        self.seekhead_cues_at = self.segment_data + seekhead_len - 8;
        self.duration_at =
            self.segment_data + info_pos + duration_at.expect("duration written") as u64;
        self.write(&seekhead)?;
        self.write(&info)?;
        self.write(&tracks)?;
        self.header_written = true;
        Ok(())
    }

    /// Write queued blocks in interleaved order: all of them when `flush`,
    /// otherwise as long as the earliest is known (or the queues are full).
    fn pump(&mut self, flush: bool) -> Result<()> {
        if !self.header_written {
            if !flush && !self.header_ready() && !self.over_cap() {
                return Ok(());
            }
            self.write_file_header()?;
        }
        loop {
            let mut best: Option<(i64, u64, usize)> = None;
            let mut waiting = false;
            for (i, q) in self.queues.iter().enumerate() {
                match q.front() {
                    Some(e) => {
                        if best.is_none_or(|(k, s, _)| (e.key_ms, e.seq) < (k, s)) {
                            best = Some((e.key_ms, e.seq, i));
                        }
                    }
                    // Subtitles are sparse: an empty subtitle queue says
                    // nothing about what comes next, so it never holds back.
                    None => waiting |= !self.is_sub[i],
                }
            }
            let Some((_, _, track)) = best else {
                return Ok(());
            };
            if waiting && !flush && !self.over_cap() {
                return Ok(());
            }
            let e = self.queues[track].pop_front().expect("non-empty");
            self.queued_bytes -= e.block.data.len();
            self.write_block(&e.block)?;
        }
    }

    fn write_block(&mut self, b: &BufBlock) -> Result<()> {
        let plan = &self.tracks[b.track];
        if plan.is_avc && plan.private.is_none() {
            return Err(missing_avc_config());
        }
        let video_key = self.is_video[b.track] && b.keyframe;
        let cluster_ts = self.cluster.as_ref().map(|c| c.ts_ms);
        // Blocks arrive interleaved, not globally sorted: a block too far
        // *before* its Cluster for the i16 relative timestamp starts another.
        let too_early = cluster_ts.is_some_and(|c| b.ts_ms >= 0 && b.ts_ms - c < i16::MIN as i64);
        if too_early || starts_cluster(cluster_ts, b.ts_ms, video_key) {
            self.close_cluster()?;
            self.open_cluster(b.ts_ms.max(0))?;
        }
        let cts = self.cluster.as_ref().expect("cluster open").ts_ms;
        let rel = (b.ts_ms - cts).clamp(i16::MIN as i64, i16::MAX as i64) as i16;
        let (head, tail) = block_parts(b, rel, self.is_sub[b.track]);
        self.write(&head)?;
        self.write(&b.data)?;
        self.write(&tail)?;
        let end = b.ts_ms + b.duration_ms.max(0);
        self.end_ms = Some(self.end_ms.map_or(end, |e| e.max(end)));
        Ok(())
    }

    fn open_cluster(&mut self, ts_ms: i64) -> Result<()> {
        self.cues.push((ts_ms, self.pos - self.segment_data));
        let mut head = Vec::with_capacity(20);
        put_id(&mut head, ID_CLUSTER);
        let size_at = self.pos + head.len() as u64;
        head.extend_from_slice(&UNKNOWN_SIZE8);
        let body_at = self.pos + head.len() as u64;
        put_uint(&mut head, ID_TIMESTAMP, ts_ms as u64);
        self.write(&head)?;
        self.cluster = Some(OpenCluster {
            ts_ms,
            size_at,
            body_at,
        });
        Ok(())
    }

    fn close_cluster(&mut self) -> Result<()> {
        if let Some(c) = self.cluster.take() {
            self.patch(c.size_at, &size_vint8(self.pos - c.body_at))?;
        }
        Ok(())
    }
}

impl Muxer for MkvStreamingMuxer {
    fn set_metadata(&mut self, metadata: &rff_core::Dictionary) {
        self.title = metadata.get("title").map(str::to_owned);
    }

    fn write_header(&mut self, streams: &[Stream]) -> Result<()> {
        if streams.is_empty() {
            return Err(Error::invalid("mkv mux: no streams"));
        }
        self.tracks = streams
            .iter()
            .map(|s| plan_track(s, self.webm))
            .collect::<Result<Vec<_>>>()?;
        let kind = |t: MediaType| {
            self.tracks
                .iter()
                .map(|p| p.stream.media_type == t)
                .collect()
        };
        self.is_video = kind(MediaType::Video);
        self.is_sub = kind(MediaType::Subtitle);
        self.queues = self.tracks.iter().map(|_| VecDeque::new()).collect();
        if self.header_ready() {
            self.write_file_header()?;
        }
        Ok(())
    }

    fn write_packet(&mut self, packet: &Packet) -> Result<()> {
        if self.finished {
            return Err(Error::invalid("mkv mux: packet after trailer"));
        }
        let Some(block) = prepare_block(&mut self.tracks, packet)? else {
            return self.pump(false);
        };
        let tb = self.tracks[block.track].stream.time_base;
        let key_ms = match packet.dts {
            Some(dts) => to_ms(tb, dts),
            None => block.ts_ms,
        };
        self.max_key_ms = self.max_key_ms.max(key_ms);
        self.queued_bytes += block.data.len();
        self.seq += 1;
        self.queues[block.track].push_back(Queued {
            key_ms,
            seq: self.seq,
            block,
        });
        self.pump(false)
    }

    fn write_trailer(&mut self) -> Result<()> {
        if self.finished {
            return Err(Error::invalid("mkv mux: trailer already written"));
        }
        self.finished = true;
        if self.tracks.is_empty() {
            return Err(Error::invalid("mkv mux: no streams"));
        }
        self.pump(true)?;
        self.close_cluster()?;

        // --- Cues: one point per Cluster ---
        let cue_track = cue_track(&self.tracks);
        let mut cues_body = Vec::with_capacity(self.cues.len() * 24);
        for &(cts, off) in &self.cues {
            put_cue_point(&mut cues_body, cts, cue_track, off);
        }
        let mut cues = Vec::with_capacity(cues_body.len() + 12);
        put_elem(&mut cues, ID_CUES, &cues_body);
        let cues_pos = self.pos - self.segment_data;
        self.write(&cues)?;

        // --- Patch the header: Segment size, Duration, SeekHead → Cues ---
        let segment_len = self.pos - self.segment_data;
        self.patch(self.segment_size_at, &size_vint8(segment_len))?;
        let duration_ms = self.end_ms.unwrap_or(0);
        if duration_ms > 0 {
            // Duration element: 2-byte ID + 1-byte size, then the f64.
            self.patch(self.duration_at + 3, &(duration_ms as f64).to_be_bytes())?;
        } else {
            // No duration (the in-memory muxer omits the element): blank the
            // 11-byte placeholder with a Void of the same length.
            let mut void = vec![0xEC, 0x80 | 9];
            void.resize(11, 0);
            self.patch(self.duration_at, &void)?;
        }
        self.patch(self.seekhead_cues_at, &cues_pos.to_be_bytes())?;
        self.out.flush()?;
        Ok(())
    }
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn size_vints_are_minimal_and_valid() {
        let mut v = Vec::new();
        put_size(&mut v, 2);
        assert_eq!(v, vec![0x82]);
        v.clear();
        put_size(&mut v, 0x7F); // all-ones in 1 byte is reserved → 2 bytes
        assert_eq!(v, vec![0x40, 0x7F]);
        v.clear();
        put_size(&mut v, 500);
        assert_eq!(v, vec![0x41, 0xF4]);
    }

    #[test]
    fn xiph_lacing_encodes_255_boundaries() {
        let h = vec![vec![1u8; 255], vec![2u8; 10], vec![3u8; 4]];
        let laced = xiph_lace(&h).unwrap();
        assert_eq!(laced[0], 2); // count-1
        assert_eq!(&laced[1..4], &[255, 0, 10]); // 255 needs a 0 continuation
        assert_eq!(laced.len(), 4 + 255 + 10 + 4);
    }

    #[test]
    fn seekhead_size_is_position_invariant() {
        assert_eq!(
            build_seekhead(0, 0, 0).len(),
            build_seekhead(u64::MAX / 2, 123, 456).len()
        );
    }

    #[test]
    fn synthesized_flac_streaminfo_is_34_bytes() {
        let mut s = Stream::new(0, CodecId::Flac);
        s.sample_rate = 44_100;
        s.channels = 2;
        let p = flac_private(&s);
        assert_eq!(&p[..4], b"fLaC");
        assert_eq!(p.len(), 4 + 4 + 34);
        // rate(20) at the top of the packed word: 4 (magic) + 4 (block header)
        // + 2+2+6 (blocksize/framesize bounds) = offset 18.
        let packed = u64::from_be_bytes(p[18..26].try_into().unwrap());
        assert_eq!(packed >> 44, 44_100);
        assert_eq!((packed >> 41) & 0x7, 1); // channels-1
    }

    // ---- Streaming (seekable) muxer -----------------------------------------

    use std::collections::BTreeMap;
    use std::io::Cursor;
    use std::sync::{Arc, Mutex};

    use rff_core::Rational;
    use rff_format::Demuxer;

    use crate::MkvDemuxer;

    fn pkt(stream: usize, pts: i64, key: bool, data: &[u8]) -> Packet {
        let mut p = Packet::from_data(stream, data.to_vec());
        p.pts = Some(pts);
        p.flags.keyframe = key;
        p
    }

    /// A `Cursor<Vec<u8>>` the test keeps a handle on after the muxer owns it.
    #[derive(Clone, Default)]
    struct SharedCursor(Arc<Mutex<Cursor<Vec<u8>>>>);
    impl Write for SharedCursor {
        fn write(&mut self, buf: &[u8]) -> std::io::Result<usize> {
            self.0.lock().unwrap().write(buf)
        }
        fn flush(&mut self) -> std::io::Result<()> {
            Ok(())
        }
    }
    impl Seek for SharedCursor {
        fn seek(&mut self, pos: SeekFrom) -> std::io::Result<u64> {
            self.0.lock().unwrap().seek(pos)
        }
    }
    impl SharedCursor {
        fn bytes(&self) -> Vec<u8> {
            self.0.lock().unwrap().get_ref().clone()
        }
    }

    /// Mux through the in-memory muxer (a plain, non-seekable sink).
    fn mux_buffered(webm: bool, streams: &[Stream], packets: &[Packet]) -> Vec<u8> {
        let sink = SharedCursor::default();
        let mut mux = MkvMuxer::new(Box::new(sink.clone()), webm);
        mux.write_header(streams).unwrap();
        for p in packets {
            mux.write_packet(p).unwrap();
        }
        mux.write_trailer().unwrap();
        sink.bytes()
    }

    /// Mux through the streaming muxer into a `Cursor<Vec<u8>>`.
    fn mux_streamed(webm: bool, streams: &[Stream], packets: &[Packet]) -> Vec<u8> {
        let sink = SharedCursor::default();
        let mut mux = MkvStreamingMuxer::new(Box::new(sink.clone()), webm);
        mux.write_header(streams).unwrap();
        for p in packets {
            mux.write_packet(p).unwrap();
        }
        mux.write_trailer().unwrap();
        sink.bytes()
    }

    type Demuxed = (usize, Option<i64>, bool, Vec<u8>);

    /// Everything our demuxer reads back: streams, then (track, pts, key, data).
    fn demux_all(file: Vec<u8>) -> (Vec<Stream>, Vec<Demuxed>) {
        let mut dem = MkvDemuxer::new(Box::new(Cursor::new(file)));
        let streams = dem.read_header().unwrap();
        let mut out = Vec::new();
        loop {
            match dem.read_packet() {
                Ok(p) => out.push((p.stream_index, p.pts, p.flags.keyframe, p.data)),
                Err(Error::Eof) => break,
                Err(e) => panic!("demux: {e}"),
            }
        }
        (streams, out)
    }

    // -- A minimal EBML walk over a random-access byte source ----------------

    /// Random-access bytes: a whole file, or a sparse one (see [`Sparse`]).
    trait Bytes {
        fn at(&self, off: u64, n: usize) -> Vec<u8>;
        fn total(&self) -> u64;
    }
    impl Bytes for Vec<u8> {
        fn at(&self, off: u64, n: usize) -> Vec<u8> {
            self[off as usize..off as usize + n].to_vec()
        }
        fn total(&self) -> u64 {
            self.len() as u64
        }
    }

    /// Element header at `off`: (id, size, data offset). Panics on the
    /// unknown size — a finished file must have none.
    fn elem_at(src: &dyn Bytes, off: u64) -> (u32, u64, u64) {
        let b = src.at(off, 1)[0];
        let id_len = b.leading_zeros() as usize + 1;
        let id = src
            .at(off, id_len)
            .iter()
            .fold(0u32, |a, &x| (a << 8) | x as u32);
        let first = src.at(off + id_len as u64, 1)[0];
        let len = first.leading_zeros() as usize + 1;
        let raw = src.at(off + id_len as u64, len);
        let mut v = (raw[0] as u64) & (0xFF >> len);
        for &x in &raw[1..] {
            v = (v << 8) | x as u64;
        }
        assert_ne!(v, (1u64 << (7 * len)) - 1, "unknown size left at {off}");
        (id, v, off + (id_len + len) as u64)
    }

    /// Children (id, element offset, size, data offset) of a body.
    fn children(src: &dyn Bytes, start: u64, end: u64) -> Vec<(u32, u64, u64, u64)> {
        let mut out = Vec::new();
        let mut off = start;
        while off < end {
            let (id, size, data) = elem_at(src, off);
            out.push((id, off, size, data));
            off = data + size;
        }
        assert_eq!(off, end, "children overrun their parent");
        out
    }

    fn uint(src: &dyn Bytes, data: u64, size: u64) -> u64 {
        src.at(data, size as usize)
            .iter()
            .fold(0, |a, &x| (a << 8) | x as u64)
    }

    /// What the walk found (positions Segment-relative).
    #[derive(Debug)]
    struct Walk {
        segment: u64,
        duration: Option<f64>,
        clusters: Vec<u64>,
        cue_positions: Vec<u64>,
    }

    /// Check the Segment's structure: its size is the bytes to EOF, the
    /// SeekHead's positions land on the elements they name, every Cluster
    /// starts with its Timestamp, and every Cue position lands on a Cluster.
    /// Reads element headers only, never a frame payload.
    fn walk(src: &dyn Bytes) -> Walk {
        let (id, size, data) = elem_at(src, 0);
        assert_eq!(id, ID_EBML);
        let (id, seg_size, seg) = elem_at(src, data + size);
        assert_eq!(id, ID_SEGMENT);
        assert_eq!(seg + seg_size, src.total(), "Segment size != bytes to EOF");
        let top = children(src, seg, seg + seg_size);
        let at = |rel: u64| top.iter().find(|c| c.1 == seg + rel).map(|c| c.0);
        let clusters: Vec<u64> = top
            .iter()
            .filter(|c| c.0 == ID_CLUSTER)
            .map(|c| c.1 - seg)
            .collect();
        for c in top.iter().filter(|c| c.0 == ID_CLUSTER) {
            assert_eq!(elem_at(src, c.3).0, ID_TIMESTAMP);
        }

        let seekhead = top.iter().find(|c| c.0 == ID_SEEK_HEAD).expect("SeekHead");
        assert_eq!(seekhead.1, seg, "SeekHead first");
        let mut seen = Vec::new();
        for (sid, _, ssize, sdata) in children(src, seekhead.3, seekhead.3 + seekhead.2) {
            assert_eq!(sid, ID_SEEK);
            let (mut target, mut pos) = (0, 0);
            for (cid, _, csize, cdata) in children(src, sdata, sdata + ssize) {
                match cid {
                    ID_SEEK_ID => target = uint(src, cdata, csize) as u32,
                    ID_SEEK_POSITION => pos = uint(src, cdata, csize),
                    _ => {}
                }
            }
            assert_eq!(at(pos), Some(target), "SeekHead entry {target:#x} -> {pos}");
            seen.push(target);
        }
        assert_eq!(seen, vec![ID_INFO, ID_TRACKS, ID_CUES]);

        let info = top.iter().find(|c| c.0 == ID_INFO).expect("Info");
        let duration = children(src, info.3, info.3 + info.2)
            .into_iter()
            .find(|c| c.0 == ID_DURATION)
            .map(|c| f64::from_bits(uint(src, c.3, c.2)));

        let cues = top.iter().find(|c| c.0 == ID_CUES).expect("Cues");
        assert_eq!(cues.3 + cues.2, seg + seg_size, "Cues last");
        let mut cue_positions = Vec::new();
        for (pid, _, psize, pdata) in children(src, cues.3, cues.3 + cues.2) {
            assert_eq!(pid, ID_CUE_POINT);
            for (tid, _, tsize, tdata) in children(src, pdata, pdata + psize) {
                if tid != ID_CUE_TRACK_POSITIONS {
                    continue;
                }
                for (cid, _, csize, cdata) in children(src, tdata, tdata + tsize) {
                    if cid == ID_CUE_CLUSTER_POSITION {
                        let pos = uint(src, cdata, csize);
                        assert_eq!(at(pos), Some(ID_CLUSTER), "Cue -> {pos} not a Cluster");
                        cue_positions.push(pos);
                    }
                }
            }
        }
        Walk {
            segment: seg,
            duration,
            clusters,
            cue_positions,
        }
    }

    fn vp9_opus_streams() -> Vec<Stream> {
        let mut v = Stream::new(0, CodecId::Vp9);
        v.width = 320;
        v.height = 240;
        let mut a = Stream::new(1, CodecId::Opus);
        a.sample_rate = 48_000;
        a.channels = 2;
        a.time_base = Rational::new(1, 48_000);
        vec![v, a]
    }

    /// 12 s of 25 fps video (keyframe every 2 s) + 20 ms audio frames,
    /// delivered in bursts rather than interleaved: the streaming muxer must
    /// interleave them back into timestamp order.
    fn vp9_opus_packets() -> Vec<Packet> {
        let mut video: Vec<Packet> = (0..300)
            .map(|i| pkt(0, i * 40, i % 50 == 0, &[(i % 251) as u8; 300]))
            .collect();
        let mut audio: Vec<Packet> = (0..600)
            .map(|i| pkt(1, i * 960, true, &[(i % 13) as u8 + 1; 60]))
            .collect();
        for p in video.iter_mut().chain(audio.iter_mut()) {
            p.duration = if p.stream_index == 0 { 40 } else { 960 };
        }
        let mut out = Vec::new();
        while !video.is_empty() || !audio.is_empty() {
            out.extend(audio.drain(..audio.len().min(25)));
            out.extend(video.drain(..video.len().min(10)));
        }
        out
    }

    #[test]
    fn streamed_file_matches_buffered_packets_and_walks() {
        let streams = vp9_opus_streams();
        let packets = vp9_opus_packets();
        let buffered = mux_buffered(false, &streams, &packets);
        let streamed = mux_streamed(false, &streams, &packets);

        let (s_old, p_old) = demux_all(buffered.clone());
        let (s_new, p_new) = demux_all(streamed.clone());
        assert_eq!(s_old.len(), s_new.len());
        for (a, b) in s_old.iter().zip(&s_new) {
            assert_eq!(
                (a.codec_id, a.width, a.height, a.sample_rate, a.channels),
                (b.codec_id, b.width, b.height, b.sample_rate, b.channels)
            );
            assert_eq!((&a.extradata, a.duration), (&b.extradata, b.duration));
        }
        assert_eq!(p_old.len(), 900);
        assert_eq!(p_old, p_new, "same blocks, same order");

        let w_old = walk(&buffered);
        let w_new = walk(&streamed);
        assert_eq!(w_new.duration, Some(12_000.0));
        assert_eq!(w_old.duration, w_new.duration);
        // Same cluster policy: cluster and Cue counts agree, each Cue on a
        // Cluster.
        assert_eq!(w_old.clusters.len(), w_new.clusters.len());
        assert_eq!(w_new.cue_positions, w_new.clusters);
        assert_eq!(w_old.cue_positions, w_old.clusters);
    }

    #[test]
    fn streamed_h264_subtitles_and_fresh_vorbis_round_trip() {
        // H.264 (CodecPrivate harvested from the first packet, so the header
        // waits for it), SubRip (BlockGroup + BlockDuration) and a fresh
        // Vorbis encode (headers arrive as the first three packets).
        let mut v = Stream::new(0, CodecId::H264);
        v.width = 64;
        v.height = 64;
        let mut s = Stream::new(1, CodecId::Subrip);
        s.media_type = MediaType::Subtitle;
        let mut a = Stream::new(2, CodecId::Vorbis);
        a.sample_rate = 44_100;
        a.channels = 1;
        let annexb = |nals: &[&[u8]]| {
            let mut out = Vec::new();
            for n in nals {
                out.extend_from_slice(&[0, 0, 0, 1]);
                out.extend_from_slice(n);
            }
            out
        };
        let key = annexb(&[&[0x67, 0xAA, 0xBB], &[0x68, 0xCC], &[0x65, 1, 2, 3]]);
        let mut packets = vec![
            pkt(2, 0, false, b"\x01vorbis-ident"),
            pkt(2, 0, false, b"\x03vorbis-comment"),
            pkt(2, 0, false, b"\x05vorbis-setup"),
        ];
        for i in 0..100 {
            let data = if i % 25 == 0 {
                key.clone()
            } else {
                annexb(&[&[0x41, i as u8, 7]])
            };
            packets.push(pkt(0, i * 40, i % 25 == 0, &data));
            packets.push(pkt(2, i * 40, true, &[i as u8; 20]));
            if i % 30 == 5 {
                let mut sub = pkt(1, i * 40, true, b"hello");
                sub.duration = 1500;
                packets.push(sub);
            }
        }
        let streams = [v, s, a];
        let buffered = mux_buffered(false, &streams, &packets);
        let streamed = mux_streamed(false, &streams, &packets);
        let (s_old, p_old) = demux_all(buffered.clone());
        let (s_new, p_new) = demux_all(streamed.clone());
        let privates = |s: &[Stream]| s.iter().map(|s| s.extradata.clone()).collect::<Vec<_>>();
        assert_eq!(privates(&s_old), privates(&s_new));
        assert!(
            !s_new[2].extradata.is_empty(),
            "vorbis CodecPrivate written"
        );
        assert_eq!(p_old, p_new);
        assert_eq!(p_new.iter().filter(|p| p.0 == 1).count(), 4);
        let w = walk(&streamed);
        assert_eq!(w.duration, walk(&buffered).duration);
        assert_eq!(w.cue_positions, w.clusters);
    }

    #[test]
    fn streamed_file_without_duration_voids_the_placeholder() {
        let streamed = mux_streamed(false, &vp9_opus_streams(), &[]);
        let w = walk(&streamed);
        assert_eq!(w.duration, None);
        assert!(w.clusters.is_empty() && w.cue_positions.is_empty());
        let (s, p) = demux_all(streamed);
        assert_eq!(s.len(), 2);
        assert!(p.is_empty());
    }

    #[test]
    fn streamed_header_and_clusters_leave_before_the_trailer() {
        // The point of streaming: bytes leave as packets arrive.
        let sink = SharedCursor::default();
        let mut mux = MkvStreamingMuxer::new(Box::new(sink.clone()), true);
        mux.write_header(&vp9_opus_streams()).unwrap();
        assert!(!sink.bytes().is_empty(), "header written at write_header");
        for p in vp9_opus_packets() {
            mux.write_packet(&p).unwrap();
        }
        // All but the interleave tail (< one burst) is already out.
        let before_trailer = sink.bytes().len();
        assert!(before_trailer > 300 * 300 + 500 * 60, "{before_trailer}");
        mux.write_trailer().unwrap();
        assert!(mux.write_trailer().is_err());
        assert!(mux.write_packet(&pkt(0, 0, true, &[1])).is_err());
    }

    #[test]
    fn webm_streaming_refuses_non_webm_codecs() {
        let mut mux = MkvStreamingMuxer::new(Box::new(Cursor::new(Vec::new())), true);
        let err = mux
            .write_header(&[Stream::new(0, CodecId::H264)])
            .unwrap_err();
        assert!(err.to_string().contains("webm"), "got: {err}");
    }

    #[test]
    fn h264_without_sps_is_refused_like_the_buffered_muxer() {
        let mut v = Stream::new(0, CodecId::H264);
        v.width = 64;
        v.height = 64;
        let mut mux = MkvStreamingMuxer::new(Box::new(Cursor::new(Vec::new())), false);
        mux.write_header(&[v]).unwrap();
        mux.write_packet(&pkt(0, 0, true, &[0, 0, 0, 1, 0x65, 1, 2]))
            .unwrap();
        let err = mux.write_trailer().unwrap_err();
        assert!(err.to_string().contains("SPS/PPS"), "got: {err}");
    }

    #[test]
    fn eight_byte_sizes_cover_past_4_gib() {
        let v = 5u64 << 30;
        let b = size_vint8(v);
        assert_eq!(b[0], 0x01);
        let back = b[1..].iter().fold(0u64, |a, &x| (a << 8) | x as u64);
        assert_eq!(back, v);
        assert_eq!(size_vint8(0), [1, 0, 0, 0, 0, 0, 0, 0]);
    }

    // -- Past 4 GiB without writing gigabytes ---------------------------------

    /// A seekable sink that keeps only small writes: a write over 1 MiB (a
    /// frame payload) becomes a hole, so a multi-GiB file costs a few KiB.
    /// A patch (seek back + overwrite) must land inside one stored write.
    #[derive(Default)]
    struct Sparse {
        pos: u64,
        len: u64,
        chunks: BTreeMap<u64, Vec<u8>>,
        /// Absolute position of the sink's byte 0 (seeks are absolute).
        origin: u64,
    }
    impl Write for Sparse {
        fn write(&mut self, buf: &[u8]) -> std::io::Result<usize> {
            if self.pos < self.len {
                let (&start, chunk) = self
                    .chunks
                    .range_mut(..=self.pos)
                    .next_back()
                    .expect("patch inside a stored write");
                let off = (self.pos - start) as usize;
                assert!(off + buf.len() <= chunk.len(), "patch spans writes");
                chunk[off..off + buf.len()].copy_from_slice(buf);
            } else if buf.len() <= 1 << 20 {
                self.chunks.insert(self.pos, buf.to_vec());
            }
            self.pos += buf.len() as u64;
            self.len = self.len.max(self.pos);
            Ok(buf.len())
        }
        fn flush(&mut self) -> std::io::Result<()> {
            Ok(())
        }
    }
    impl Seek for Sparse {
        fn seek(&mut self, pos: SeekFrom) -> std::io::Result<u64> {
            let abs = match pos {
                SeekFrom::Start(p) => p,
                SeekFrom::Current(d) => (self.origin + self.pos).checked_add_signed(d).unwrap(),
                SeekFrom::End(d) => (self.origin + self.len).checked_add_signed(d).unwrap(),
            };
            assert!(abs >= self.origin, "seek before the sink's start");
            self.pos = abs - self.origin;
            Ok(abs)
        }
    }
    #[derive(Clone)]
    struct SharedSparse(Arc<Mutex<Sparse>>);
    impl Write for SharedSparse {
        fn write(&mut self, buf: &[u8]) -> std::io::Result<usize> {
            self.0.lock().unwrap().write(buf)
        }
        fn flush(&mut self) -> std::io::Result<()> {
            Ok(())
        }
    }
    impl Seek for SharedSparse {
        fn seek(&mut self, pos: SeekFrom) -> std::io::Result<u64> {
            self.0.lock().unwrap().seek(pos)
        }
    }
    impl Bytes for SharedSparse {
        fn at(&self, off: u64, n: usize) -> Vec<u8> {
            let s = self.0.lock().unwrap();
            let (&start, chunk) = s.chunks.range(..=off).next_back().expect("stored");
            let o = (off - start) as usize;
            assert!(o + n <= chunk.len(), "read of {n} at {off} falls in a hole");
            chunk[o..o + n].to_vec()
        }
        fn total(&self) -> u64 {
            self.0.lock().unwrap().len
        }
    }

    #[test]
    fn segment_cluster_and_cue_positions_past_4_gib() {
        // The sink starts 4.5 GiB into its file (absolute patch seeks past 32
        // bits), and ten 512 MiB frames put the first Cluster's size, the
        // second Cluster's Cue position and the Segment size past 4 GiB too.
        // The frames are one zeroed allocation the sink never reads.
        let origin = 9u64 << 29;
        let mut sink = SharedSparse(Arc::new(Mutex::new(Sparse {
            origin,
            ..Default::default()
        })));
        sink.seek(SeekFrom::Start(origin)).unwrap();
        let mut mux = MkvStreamingMuxer::new(Box::new(sink.clone()), false);
        let mut v = Stream::new(0, CodecId::Vp9);
        v.width = 64;
        v.height = 64;
        mux.write_header(&[v]).unwrap();
        let big = BufBlock {
            track: 0,
            ts_ms: 0,
            duration_ms: 40,
            keyframe: true,
            data: vec![0u8; 512 << 20],
        };
        for _ in 0..10 {
            mux.write_block(&big).unwrap();
        }
        // A keyframe 6 s in opens the second Cluster.
        mux.write_packet(&pkt(0, 6_000, true, &[1, 2, 3])).unwrap();
        mux.write_trailer().unwrap();

        assert!(sink.total() > 5 << 30);
        let w = walk(&sink);
        assert_eq!(w.clusters.len(), 2);
        assert!(
            w.clusters[1] > 5 << 30,
            "second Cluster at {}",
            w.clusters[1]
        );
        assert_eq!(w.cue_positions, w.clusters);
        assert_eq!(w.duration, Some(6_000.0));
        // The first Cluster holds the ten frames plus a few bytes of headers.
        let (id, size, _) = elem_at(&sink, w.segment + w.clusters[0]);
        assert_eq!(id, ID_CLUSTER);
        assert!(size > 5 << 30 && size < (5 << 30) + 4096, "{size}");
    }
}
