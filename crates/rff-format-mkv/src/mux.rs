//! Matroska / WebM muxer.
//!
//! Writes an EBML Segment with SeekHead + Info + Tracks + Clusters + Cues.
//! Everything is buffered until [`Muxer::write_trailer`] (the [`Output`] is not
//! seekable), then laid out in one pass: cluster offsets are known before the
//! Cues are built, and the SeekHead uses fixed 8-byte position encodings so its
//! own size never shifts the offsets it points at.
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

use rff_core::{CodecId, Error, MediaType, Packet, Result, SampleFormat};
use rff_format::avc::{build_avcc_record, split_annexb};
use rff_format::{Muxer, Output, Stream};

use crate::{
    ID_AUDIO, ID_BIT_DEPTH, ID_BLOCK, ID_BLOCK_DURATION, ID_BLOCK_GROUP, ID_CHANNELS,
    ID_CLUSTER, ID_CODEC_ID, ID_CODEC_PRIVATE, ID_CUES, ID_DURATION, ID_INFO, ID_PIXEL_HEIGHT,
    ID_PIXEL_WIDTH, ID_SAMPLING_FREQUENCY, ID_SEGMENT, ID_SIMPLE_BLOCK, ID_TIMESTAMP,
    ID_TIMESTAMP_SCALE, ID_TRACKS, ID_TRACK_ENTRY, ID_TRACK_NUMBER, ID_TRACK_TYPE, ID_VIDEO,
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
    let n = ((64 - v.leading_zeros() as usize) + 7) / 8;
    let n = n.max(1);
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

// ---- The muxer ------------------------------------------------------------

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

    /// Packet timestamp → milliseconds (the TimestampScale we write is 1 ms).
    /// Timestamps are in the *stream's* time base, per the muxer contract.
    fn to_ms(&self, track: usize, v: i64) -> i64 {
        let tb = self.tracks[track].stream.time_base;
        if tb.num <= 0 || tb.den <= 0 {
            return v;
        }
        ((v as i128 * 1000 * tb.num as i128 + (tb.den as i128 / 2)) / tb.den as i128) as i64
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
        let idx = packet.stream_index;
        let Some(plan) = self.tracks.get_mut(idx) else {
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
            return Ok(());
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

        let audio = plan.stream.media_type == MediaType::Audio;
        let ts_ms = self.to_ms(idx, packet.pts.or(packet.dts).unwrap_or(0));
        let duration_ms = self.to_ms(idx, packet.duration.max(0));
        self.blocks.push(BufBlock {
            track: idx,
            ts_ms,
            duration_ms,
            // Audio frames are all random-access; trust the flag on video.
            keyframe: audio || packet.flags.keyframe,
            data,
        });
        Ok(())
    }

    fn write_trailer(&mut self) -> Result<()> {
        let mut out = self
            .out
            .take()
            .ok_or_else(|| Error::invalid("mkv mux: trailer already written"))?;

        // --- EBML header ---
        let mut file = Vec::new();
        let mut ebml = Vec::new();
        put_uint(&mut ebml, ID_EBML_VERSION, 1);
        put_uint(&mut ebml, ID_EBML_READ_VERSION, 1);
        put_uint(&mut ebml, ID_EBML_MAX_ID_LENGTH, 4);
        put_uint(&mut ebml, ID_EBML_MAX_SIZE_LENGTH, 8);
        put_string(&mut ebml, ID_DOCTYPE, if self.webm { "webm" } else { "matroska" });
        put_uint(&mut ebml, ID_DOCTYPE_VERSION, 4);
        put_uint(&mut ebml, ID_DOCTYPE_READ_VERSION, 2);
        put_elem(&mut file, ID_EBML, &ebml);

        // --- Info ---
        let duration_ms = self
            .blocks
            .iter()
            .map(|b| b.ts_ms + b.duration_ms.max(0))
            .max()
            .unwrap_or(0);
        let mut info = Vec::new();
        put_uint(&mut info, ID_TIMESTAMP_SCALE, 1_000_000); // 1 ms ticks
        if duration_ms > 0 {
            put_float(&mut info, ID_DURATION, duration_ms as f64);
        }
        if let Some(title) = &self.title {
            put_string(&mut info, ID_TITLE, title);
        }
        put_string(&mut info, ID_MUXING_APP, concat!("rff ", env!("CARGO_PKG_VERSION")));
        put_string(&mut info, ID_WRITING_APP, concat!("rff ", env!("CARGO_PKG_VERSION")));
        let mut info_elem = Vec::new();
        put_elem(&mut info_elem, ID_INFO, &info);

        // --- Tracks ---
        let mut tracks_body = Vec::new();
        for (i, plan) in self.tracks.iter().enumerate() {
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

        // A missing H.264 config means no SPS/PPS ever appeared — the file
        // would be undecodable; refuse rather than write it silently broken.
        for (i, plan) in self.tracks.iter().enumerate() {
            if plan.is_avc && plan.private.is_none() && self.blocks.iter().any(|b| b.track == i) {
                return Err(Error::invalid(
                    "mkv mux: H.264 stream carried no SPS/PPS (need Annex-B with in-band headers)",
                ));
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
            let roll = match clusters.last() {
                None => true,
                Some((cts, _)) => {
                    let rel = b.ts_ms - cts;
                    // 5 s cap keeps every relative timestamp far inside i16 ms.
                    rel >= 5_000 || (is_video[b.track] && b.keyframe && rel >= 1_000)
                }
            };
            if roll {
                let mut body = Vec::new();
                put_uint(&mut body, ID_TIMESTAMP, b.ts_ms.max(0) as u64);
                clusters.push((b.ts_ms.max(0), body));
            }
            let (cts, body) = clusters.last_mut().expect("just ensured");
            let rel = (b.ts_ms - *cts).clamp(i16::MIN as i64, i16::MAX as i64) as i16;

            // Block payload: track vint + relative i16 + flags + frame bytes.
            let mut blk = Vec::new();
            put_size(&mut blk, b.track as u64 + 1); // track numbers are vints
            blk.extend_from_slice(&rel.to_be_bytes());
            if is_sub[b.track] {
                blk.push(0x00);
                blk.extend_from_slice(&b.data);
                // Subtitles need a duration: BlockGroup { Block, BlockDuration }.
                let mut group = Vec::new();
                put_elem(&mut group, ID_BLOCK, &blk);
                put_uint(&mut group, ID_BLOCK_DURATION, b.duration_ms.max(0) as u64);
                put_elem(body, ID_BLOCK_GROUP, &group);
            } else {
                blk.push(if b.keyframe { 0x80 } else { 0x00 });
                blk.extend_from_slice(&b.data);
                put_elem(body, ID_SIMPLE_BLOCK, &blk);
            }
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

        // Cues: one point per cluster, referencing the first video track (or
        // track 1 when there is no video).
        let cue_track = is_video.iter().position(|v| *v).unwrap_or(0) as u64 + 1;
        let mut cues_body = Vec::new();
        for ((cts, _), off) in clusters.iter().zip(&cluster_offsets) {
            let mut ctp = Vec::new();
            put_uint(&mut ctp, ID_CUE_TRACK, cue_track);
            put_uint(&mut ctp, ID_CUE_CLUSTER_POSITION, *off);
            let mut point = Vec::new();
            put_uint(&mut point, ID_CUE_TIME, *cts as u64);
            put_elem(&mut point, ID_CUE_TRACK_POSITIONS, &ctp);
            put_elem(&mut cues_body, ID_CUE_POINT, &point);
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
}
