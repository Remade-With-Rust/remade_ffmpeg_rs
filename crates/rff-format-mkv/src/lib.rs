//! Matroska / WebM demuxer and muxer.
//!
//! Matroska is an [EBML](https://www.matroska.org/) (binary XML) container.
//! WebM is its restricted profile (VP8/VP9/AV1 video + Opus/Vorbis audio). The
//! demuxer parses the EBML element tree to extract the track list and then
//! walks the Clusters, turning each (Simple)Block into a [`Packet`]. The whole
//! input is buffered up front since the [`Input`] is not seekable. The muxer
//! (in [`mux`]) writes the inverse: SeekHead + Info + Tracks + Clusters + Cues.

mod mux;

use std::collections::VecDeque;
use std::io::Read;

use rff_core::{CodecId, Error, MediaType, Packet, Rational, Result, SampleFormat};
use rff_format::avc::{avcc_to_annexb, parse_avcc, AvcConfig};
use rff_format::{Demuxer, Format, FormatRegistry, Input, MuxCaps, Stream};

pub use mux::MkvMuxer;

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
const ID_SEGMENT: u32 = 0x1853_8067;
const ID_INFO: u32 = 0x1549_A966;
const ID_TIMESTAMP_SCALE: u32 = 0x2AD7_B1;
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

/// A cursor over the buffered file that reads EBML primitives.
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

    fn remaining(&self) -> usize {
        self.d.len().saturating_sub(self.pos)
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

    fn read_uint(&mut self, len: usize) -> u64 {
        let mut v = 0u64;
        for i in 0..len {
            v = (v << 8) | *self.d.get(self.pos + i).unwrap_or(&0) as u64;
        }
        self.pos += len;
        v
    }

    fn read_float(&mut self, len: usize) -> f64 {
        let v = match len {
            4 => {
                let b = self.d.get(self.pos..self.pos + 4);
                b.map(|b| f32::from_be_bytes([b[0], b[1], b[2], b[3]]) as f64)
                    .unwrap_or(0.0)
            }
            8 => {
                let b = self.d.get(self.pos..self.pos + 8);
                b.map(|b| f64::from_be_bytes(b.try_into().unwrap()))
                    .unwrap_or(0.0)
            }
            _ => 0.0,
        };
        self.pos += len;
        v
    }

    fn read_bytes(&mut self, len: usize) -> &'a [u8] {
        let end = (self.pos + len).min(self.d.len());
        let s = &self.d[self.pos..end];
        self.pos = end;
        s
    }
}

struct MkvDemuxer {
    input: Option<Input>,
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
    packets: VecDeque<Packet>,
    parsed: bool,
}

impl MkvDemuxer {
    fn new(input: Input) -> MkvDemuxer {
        MkvDemuxer {
            input: Some(input),
            streams: Vec::new(),
            track_map: Vec::new(),
            avc: Vec::new(),
            ass: Vec::new(),
            timestamp_scale: 1_000_000, // default: 1 ms
            duration_ticks: None,
            packets: VecDeque::new(),
            parsed: false,
        }
    }

    fn parse(&mut self, data: &[u8]) -> Result<()> {
        let mut top = Ebml::new(data);
        // EBML header, then the Segment.
        while top.remaining() > 0 {
            let Some(id) = top.read_id() else { break };
            let Some((size, unknown)) = top.read_size() else {
                break;
            };
            let end = if unknown {
                data.len()
            } else {
                (top.pos + size as usize).min(data.len())
            };
            if id == ID_SEGMENT {
                self.parse_segment(data, top.pos, end);
                break;
            }
            top.pos = end;
        }
        if self.streams.is_empty() {
            return Err(Error::invalid("mkv: no tracks found"));
        }
        // The whole file is buffered, so totals are exact, not declarations.
        for s in &mut self.streams {
            s.duration = self.duration_ticks;
            let n = self.packets.iter().filter(|p| p.stream_index == s.index).count();
            if n > 0 {
                s.nb_frames = Some(n as u64);
            }
        }
        Ok(())
    }

    fn parse_segment(&mut self, data: &[u8], start: usize, end: usize) {
        let mut e = Ebml::at(data, start);
        while e.pos < end {
            let Some(id) = e.read_id() else { break };
            let Some((size, unknown)) = e.read_size() else {
                break;
            };
            let child_end = if unknown {
                end
            } else {
                (e.pos + size as usize).min(end)
            };
            match id {
                ID_INFO => self.parse_info(data, e.pos, child_end),
                ID_TRACKS => self.parse_tracks(data, e.pos, child_end),
                ID_CLUSTER => self.parse_cluster(data, e.pos, child_end, unknown),
                _ => {}
            }
            // For unknown-size Clusters we stop the child run at the next
            // top-level ID; parse_cluster reports where it actually ended.
            e.pos = child_end.max(e.pos);
        }
    }

    fn parse_info(&mut self, data: &[u8], start: usize, end: usize) {
        let mut e = Ebml::at(data, start);
        while e.pos < end {
            let Some(id) = e.read_id() else { break };
            let Some((size, _)) = e.read_size() else {
                break;
            };
            match id {
                ID_TIMESTAMP_SCALE => self.timestamp_scale = e.read_uint(size as usize),
                // Duration is a float in `timestamp_scale` ticks — our time_base.
                ID_DURATION => {
                    let d = e.read_float(size as usize);
                    if d > 0.0 {
                        self.duration_ticks = Some(d as i64);
                    }
                }
                _ => e.pos += size as usize,
            }
        }
    }

    fn parse_tracks(&mut self, data: &[u8], start: usize, end: usize) {
        let mut e = Ebml::at(data, start);
        while e.pos < end {
            let Some(id) = e.read_id() else { break };
            let Some((size, _)) = e.read_size() else {
                break;
            };
            let entry_end = (e.pos + size as usize).min(end);
            if id == ID_TRACK_ENTRY {
                self.parse_track_entry(data, e.pos, entry_end);
            }
            e.pos = entry_end;
        }
    }

    fn parse_track_entry(&mut self, data: &[u8], start: usize, end: usize) {
        let mut number = 0u64;
        let mut codec = String::new();
        let mut codec_private: Vec<u8> = Vec::new();
        let mut track_type = 0u64;
        let (mut width, mut height) = (0u32, 0u32);
        let (mut rate, mut channels) = (0u32, 0u16);
        let mut bit_depth = 0u64;

        let mut e = Ebml::at(data, start);
        while e.pos < end {
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
                    let ve = e.pos + len;
                    while v.pos < ve {
                        let Some(vid) = v.read_id() else { break };
                        let Some((vsz, _)) = v.read_size() else { break };
                        match vid {
                            ID_PIXEL_WIDTH => width = v.read_uint(vsz as usize) as u32,
                            ID_PIXEL_HEIGHT => height = v.read_uint(vsz as usize) as u32,
                            _ => v.pos += vsz as usize,
                        }
                    }
                    e.pos += len;
                }
                ID_AUDIO => {
                    let mut a = Ebml::at(data, e.pos);
                    let ae = e.pos + len;
                    while a.pos < ae {
                        let Some(aid) = a.read_id() else { break };
                        let Some((asz, _)) = a.read_size() else { break };
                        match aid {
                            ID_SAMPLING_FREQUENCY => rate = a.read_float(asz as usize) as u32,
                            ID_CHANNELS => channels = a.read_uint(asz as usize) as u16,
                            ID_BIT_DEPTH => bit_depth = a.read_uint(asz as usize),
                            _ => a.pos += asz as usize,
                        }
                    }
                    e.pos += len;
                }
                _ => e.pos += len,
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
        // H.264 is stored AVCC (CodecPrivate = avcC, length-prefixed blocks).
        // Normalise to rff's Annex-B packet contract — same as rff-format-mp4 —
        // so extradata stays empty and the SPS/PPS ride in keyframe packets.
        let avc = (codec_id == CodecId::H264)
            .then(|| parse_avcc(&codec_private))
            .flatten();
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

    fn parse_cluster(&mut self, data: &[u8], start: usize, end: usize, _unknown: bool) {
        let mut cluster_ts = 0i64;
        let mut e = Ebml::at(data, start);
        while e.pos < end {
            let Some(id) = e.read_id() else { break };
            let Some((size, _)) = e.read_size() else {
                break;
            };
            let len = size as usize;
            match id {
                ID_TIMESTAMP => cluster_ts = e.read_uint(len) as i64,
                ID_SIMPLE_BLOCK => {
                    let block = e.read_bytes(len);
                    self.parse_block(block, cluster_ts, 0);
                }
                ID_BLOCK_GROUP => {
                    // BlockDuration may come before or after the Block: collect
                    // both, then emit (subtitle cues need their duration).
                    let ge = e.pos + len;
                    let mut g = Ebml::at(data, e.pos);
                    let mut block: Option<&[u8]> = None;
                    let mut duration = 0i64;
                    while g.pos < ge {
                        let Some(gid) = g.read_id() else { break };
                        let Some((gsz, _)) = g.read_size() else { break };
                        match gid {
                            ID_BLOCK => block = Some(g.read_bytes(gsz as usize)),
                            ID_BLOCK_DURATION => duration = g.read_uint(gsz as usize) as i64,
                            _ => g.pos += gsz as usize,
                        }
                    }
                    if let Some(block) = block {
                        self.parse_block(block, cluster_ts, duration);
                    }
                    e.pos = ge;
                }
                _ => e.pos += len,
            }
        }
    }

    /// Parse a (Simple)Block body: track vint, int16 relative timestamp, flags,
    /// then the frame payload (no-lacing only for now).
    fn parse_block(&mut self, block: &[u8], cluster_ts: i64, duration: i64) {
        let mut b = Ebml::new(block);
        let Some((track_num, _)) = b.read_size() else {
            return;
        };
        if b.pos + 3 > block.len() {
            return;
        }
        let rel = i16::from_be_bytes([block[b.pos], block[b.pos + 1]]) as i64;
        b.pos += 2;
        let flags = block[b.pos];
        b.pos += 1;
        let lacing = (flags >> 1) & 0x03;
        let keyframe = flags & 0x80 != 0;
        let Some(&(_, index)) = self.track_map.iter().find(|(n, _)| *n == track_num) else {
            return;
        };
        if lacing != 0 {
            return; // laced blocks unsupported for now (rare for video/Opus)
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
        packet.pts = Some(cluster_ts + rel);
        packet.duration = duration.max(0);
        packet.flags.keyframe = keyframe;
        self.packets.push_back(packet);
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
            let mut buf = Vec::new();
            self.input
                .as_mut()
                .ok_or_else(|| Error::invalid("mkv: no input"))?
                .read_to_end(&mut buf)?;
            self.parse(&buf)?;
            self.parsed = true;
        }
        Ok(self.streams.clone())
    }

    fn read_packet(&mut self) -> Result<Packet> {
        self.packets.pop_front().ok_or(Error::Eof)
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
        for nal in [&[0x67u8, 0xAA, 0xBB][..], &[0x68, 0xCC][..], &[0x65, 1, 2, 3][..]] {
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
        let file =
            mux_streams(false, &[v, s], &[pkt(0, 0, true, &[0x11; 10]), cue]).unwrap();

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
}

fn map_codec(codec: &str) -> CodecId {
    match codec {
        "V_AV1" => CodecId::Avif, // our AV1 (rav1d) decoder
        "V_VP9" => CodecId::Vp9,
        "V_MPEG4/ISO/AVC" => CodecId::H264,
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
