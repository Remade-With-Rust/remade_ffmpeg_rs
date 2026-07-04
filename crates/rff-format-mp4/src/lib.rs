//! MP4 / MOV (ISOBMFF) demuxer.
//!
//! MP4 stores sample *data* in `mdat` and the map to it in each track's sample
//! table (`stbl`): `stsd` (codec + config), `stsz` (sizes), `stsc`
//! (samples-per-chunk), `stco`/`co64` (chunk offsets), `stts` (durations), `stss`
//! (sync samples). We reconstruct, per sample, its `(file offset, size, pts,
//! keyframe)` and yield them as packets in file order.
//!
//! Codec mapping: `avc1`/`avc3` → H.264, `av01` → AV1 (our `avif` decoder),
//! `mp4a` → AAC (labelled; no decoder yet). H.264 samples are stored length-
//! prefixed (AVCC); we convert them to Annex-B and prepend the `avcC` SPS/PPS on
//! keyframes so the H.264 decoder gets a self-contained bitstream.
//!
//! Demux only (no MP4 muxer yet).

use std::io::Read;

use rff_core::{CodecId, Error, MediaType, Packet, Rational, Result};
use rff_format::{Demuxer, Format, FormatRegistry, Input, Muxer, Output, Stream};

/// Register the MP4 format into a [`FormatRegistry`].
pub fn register(registry: &mut FormatRegistry) {
    registry.register(Format {
        name: "mp4",
        long_name: "MP4 / MOV (ISO Base Media File Format)",
        extensions: &["mp4", "mov", "m4a", "m4v"],
        demuxer: Some(|input| Box::new(Mp4Demuxer::new(input))),
        muxer: Some(|output| Box::new(Mp4Muxer::new(output))),
        probe: Some(probe_mp4),
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
        if end > buf.len() || end <= i {
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
    pts: i64,
    keyframe: bool,
}

/// Per-track H.264 config for AVCC→Annex-B conversion.
struct AvcConfig {
    nal_len: usize,
    headers_annexb: Vec<u8>,
}

struct Mp4Demuxer {
    input: Option<Input>,
    buf: Vec<u8>,
    samples: Vec<SampleLoc>,
    /// Indexed by stream_index; `Some` for H.264 tracks needing conversion.
    avc: Vec<Option<AvcConfig>>,
    pos: usize,
}

impl Mp4Demuxer {
    fn new(input: Input) -> Mp4Demuxer {
        Mp4Demuxer {
            input: Some(input),
            buf: Vec::new(),
            samples: Vec::new(),
            avc: Vec::new(),
            pos: 0,
        }
    }
}

impl Demuxer for Mp4Demuxer {
    fn read_header(&mut self) -> Result<Vec<Stream>> {
        let mut input = self
            .input
            .take()
            .ok_or_else(|| Error::invalid("mp4 demux: header already read"))?;
        input.read_to_end(&mut self.buf)?;
        let buf = std::mem::take(&mut self.buf);

        let top = child_boxes(&buf);
        let moov = find(&top, b"moov").ok_or_else(|| Error::invalid("mp4 demux: no `moov`"))?;

        let mut streams = Vec::new();
        for (typ, trak) in child_boxes(moov) {
            if &typ != b"trak" {
                continue;
            }
            if let Some((stream, samples, avc)) = parse_track(trak, streams.len()) {
                streams.push(stream);
                self.samples.extend(samples);
                self.avc.push(avc);
            }
        }
        if streams.is_empty() {
            return Err(Error::unsupported("mp4 demux: no usable tracks"));
        }

        // Yield samples in file order (natural interleave).
        self.samples.sort_by_key(|s| s.offset);
        self.buf = buf;
        Ok(streams)
    }

    fn read_packet(&mut self) -> Result<Packet> {
        let loc = match self.samples.get(self.pos) {
            Some(loc) => loc,
            None => return Err(Error::Eof),
        };
        self.pos += 1;
        let raw = self
            .buf
            .get(loc.offset..loc.offset + loc.size)
            .ok_or_else(|| Error::invalid("mp4 demux: sample out of range"))?;

        // H.264: convert AVCC length-prefixed NALs to Annex-B, prepending the
        // SPS/PPS on keyframes so the decoder is self-contained.
        let data = match &self.avc[loc.stream_index] {
            Some(cfg) => {
                let mut out = Vec::with_capacity(cfg.headers_annexb.len() + raw.len() + 16);
                if loc.keyframe {
                    out.extend_from_slice(&cfg.headers_annexb);
                }
                avcc_to_annexb(raw, cfg.nal_len, &mut out);
                out
            }
            None => raw.to_vec(),
        };

        let mut packet = Packet::from_data(loc.stream_index, data);
        packet.pts = Some(loc.pts);
        packet.flags.keyframe = loc.keyframe;
        Ok(packet)
    }
}

/// Convert one AVCC sample (a series of `nal_len`-byte length + NAL) to Annex-B
/// (each NAL prefixed with a `00 00 00 01` start code), appending to `out`.
fn avcc_to_annexb(sample: &[u8], nal_len: usize, out: &mut Vec<u8>) {
    let mut i = 0;
    while i + nal_len <= sample.len() {
        let mut len = 0usize;
        for _ in 0..nal_len {
            len = (len << 8) | sample[i] as usize;
            i += 1;
        }
        if i + len > sample.len() {
            break;
        }
        out.extend_from_slice(&[0, 0, 0, 1]);
        out.extend_from_slice(&sample[i..i + len]);
        i += len;
    }
}

/// Build a [`Stream`] + sample list + optional H.264 config from one `trak`.
fn parse_track(trak: &[u8], index: usize) -> Option<(Stream, Vec<SampleLoc>, Option<AvcConfig>)> {
    let trak_children = child_boxes(trak);
    let mdia = child_boxes(find(&trak_children, b"mdia")?);
    let hdlr = find(&mdia, b"hdlr")?;
    // FullBox(4) + pre_defined(4) → handler_type fourcc.
    let handler: [u8; 4] = hdlr.get(8..12)?.try_into().ok()?;
    let minf = child_boxes(find(&mdia, b"minf")?);
    let stbl = child_boxes(find(&minf, b"stbl")?);

    // Media timescale from mdhd (version 0: timescale @ 12 after ver/flags).
    let mdhd = find(&mdia, b"mdhd")?;
    let timescale = if mdhd.first() == Some(&1) {
        be32(mdhd, 20) // v1: 64-bit times push timescale to offset 20
    } else {
        be32(mdhd, 12)
    }
    .max(1);

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

    // H.264: pull avcC (child of the avc1 entry, which begins 86 bytes in).
    let avc = if codec_id == CodecId::H264 {
        child_boxes(stsd.get(8 + 86..)?)
            .iter()
            .find(|(t, _)| t == b"avcC")
            .and_then(|(_, p)| parse_avcc(p))
    } else {
        None
    };

    let samples = build_samples(&stbl, index, timescale)?;
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
        b"av01" => CodecId::Avif, // AV1 video — decoded by our rav1d-backed codec
        b"Opus" => CodecId::Opus,
        b"mp4a" => CodecId::Aac,
        _ => CodecId::None,
    }
}

/// Parse an `avcC` box into the NAL length size and Annex-B SPS/PPS headers.
fn parse_avcc(avcc: &[u8]) -> Option<AvcConfig> {
    if avcc.len() < 6 {
        return None;
    }
    let nal_len = (avcc[4] & 0x03) as usize + 1;
    let mut headers = Vec::new();
    let mut i = 5;
    let num_sps = avcc[i] & 0x1F;
    i += 1;
    for _ in 0..num_sps {
        let len = be16(avcc, i) as usize;
        i += 2;
        let nal = avcc.get(i..i + len)?;
        headers.extend_from_slice(&[0, 0, 0, 1]);
        headers.extend_from_slice(nal);
        i += len;
    }
    let num_pps = *avcc.get(i)?;
    i += 1;
    for _ in 0..num_pps {
        let len = be16(avcc, i) as usize;
        i += 2;
        let nal = avcc.get(i..i + len)?;
        headers.extend_from_slice(&[0, 0, 0, 1]);
        headers.extend_from_slice(nal);
        i += len;
    }
    Some(AvcConfig {
        nal_len,
        headers_annexb: headers,
    })
}

/// Reconstruct per-sample `(offset, size, pts, keyframe)` from the sample table.
fn build_samples(
    stbl: &[([u8; 4], &[u8])],
    stream_index: usize,
    _timescale: u32,
) -> Option<Vec<SampleLoc>> {
    let stsz = find(stbl, b"stsz")?;
    let stsc = find(stbl, b"stsc")?;
    let stts = find(stbl, b"stts")?;
    let stss = find(stbl, b"stss");
    let chunk_offsets: Vec<u64> = match find(stbl, b"stco") {
        Some(stco) => (0..be32(stco, 4))
            .map(|n| be32(stco, 8 + 4 * n as usize) as u64)
            .collect(),
        None => {
            let co64 = find(stbl, b"co64")?;
            (0..be32(co64, 4))
                .map(|n| be64(co64, 8 + 8 * n as usize))
                .collect()
        }
    };

    // Sample sizes (uniform if stsz[4] != 0).
    let uniform = be32(stsz, 4);
    let sample_count = be32(stsz, 8) as usize;
    let size_of = |i: usize| -> usize {
        if uniform != 0 {
            uniform as usize
        } else {
            be32(stsz, 12 + 4 * i) as usize
        }
    };

    // stsc entries: (first_chunk, samples_per_chunk).
    let stsc_n = be32(stsc, 4) as usize;
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

    // Walk chunks, laying out samples sequentially within each.
    let mut samples = Vec::with_capacity(sample_count);
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
                pts: 0,
                keyframe: false,
            });
            off += size;
            sample_idx += 1;
        }
    }

    // Timestamps from stts (run-length of per-sample deltas), in media units.
    let stts_n = be32(stts, 4) as usize;
    let mut pts: i64 = 0;
    let mut s = 0usize;
    for e in 0..stts_n {
        let o = 8 + 8 * e;
        let (count, delta) = (be32(stts, o), be32(stts, o + 4) as i64);
        for _ in 0..count {
            if let Some(sample) = samples.get_mut(s) {
                sample.pts = pts;
            }
            pts += delta;
            s += 1;
        }
    }

    // Keyframes: from stss if present, else every sample is a sync sample.
    match stss {
        Some(stss) => {
            let n = be32(stss, 4) as usize;
            for e in 0..n {
                let num = be32(stss, 8 + 4 * e) as usize; // 1-based
                if let Some(sample) = samples.get_mut(num.wrapping_sub(1)) {
                    sample.keyframe = true;
                }
            }
        }
        None => samples.iter_mut().for_each(|s| s.keyframe = true),
    }

    Some(samples)
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

/// Split an Annex-B bitstream into NAL units (start codes removed).
fn split_annexb(data: &[u8]) -> Vec<&[u8]> {
    let mut nals = Vec::new();
    let mut i = 0;
    let mut nal_start: Option<usize> = None;
    while i + 3 <= data.len() {
        if data[i] == 0 && data[i + 1] == 0 && data[i + 2] == 1 {
            if let Some(s) = nal_start {
                // A leading zero before this start code belongs to a 4-byte code.
                let end = if i > s && data[i - 1] == 0 { i - 1 } else { i };
                if end > s {
                    nals.push(&data[s..end]);
                }
            }
            i += 3;
            nal_start = Some(i);
        } else {
            i += 1;
        }
    }
    if let Some(s) = nal_start {
        if s < data.len() {
            nals.push(&data[s..]);
        }
    }
    nals
}

/// Build an `avcC` box from the SPS and PPS NALs.
fn build_avcc(sps: &[u8], pps: &[u8]) -> Vec<u8> {
    let mut b = Vec::new();
    b.push(1); // configurationVersion
    b.push(*sps.get(1).unwrap_or(&0x42)); // AVCProfileIndication
    b.push(*sps.get(2).unwrap_or(&0)); // profile_compatibility
    b.push(*sps.get(3).unwrap_or(&30)); // AVCLevelIndication
    b.push(0xFF); // 6 bits reserved + lengthSizeMinusOne = 3 (4-byte lengths)
    b.push(0xE1); // 3 bits reserved + numOfSPS = 1
    pu16(&mut b, sps.len() as u16);
    b.extend_from_slice(sps);
    b.push(1); // numOfPPS
    pu16(&mut b, pps.len() as u16);
    b.extend_from_slice(pps);
    bx(b"avcC", &b)
}

fn read_leb128(data: &[u8]) -> Option<(u64, usize)> {
    let mut v = 0u64;
    for i in 0..8 {
        let byte = *data.get(i)?;
        v |= ((byte & 0x7f) as u64) << (i * 7);
        if byte & 0x80 == 0 {
            return Some((v, i + 1));
        }
    }
    None
}

/// Find the AV1 sequence-header OBU (type 1) in a temporal unit; return its bytes.
fn find_seq_header_obu(data: &[u8]) -> Option<&[u8]> {
    let mut i = 0;
    while i < data.len() {
        let start = i;
        let header = data[i];
        i += 1;
        let obu_type = (header >> 3) & 0x0f;
        if (header >> 2) & 1 == 1 {
            i += 1; // extension header
        }
        let len = if (header >> 1) & 1 == 1 {
            let (l, used) = read_leb128(data.get(i..)?)?;
            i += used;
            l as usize
        } else {
            data.len() - i
        };
        if i + len > data.len() {
            return None;
        }
        i += len;
        if obu_type == 1 {
            return Some(&data[start..i]);
        }
    }
    None
}

/// Best-effort AV1 config record (`av1C`) with the sequence header embedded as
/// configOBUs. Fixed fields assume 8-bit 4:2:0 (the common case); compliant
/// decoders read the embedded sequence header regardless.
fn build_av1c(sample: &[u8]) -> Option<Vec<u8>> {
    let seq = find_seq_header_obu(sample)?;
    // marker(1)|version(7)=1, profile 0 + level 0, then 8-bit/4:2:0 flags.
    let mut b = vec![0x81u8, 0x00, 0x0C, 0x00];
    b.extend_from_slice(seq);
    Some(bx(b"av1C", &b))
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
    packets: Vec<(usize, Vec<u8>, bool, Option<i64>)>, // (stream, data, keyframe, pts)
}

impl Mp4Muxer {
    fn new(out: Output) -> Mp4Muxer {
        Mp4Muxer {
            out,
            streams: Vec::new(),
            packets: Vec::new(),
        }
    }
}

impl Muxer for Mp4Muxer {
    fn write_header(&mut self, streams: &[Stream]) -> Result<()> {
        if streams.is_empty() {
            return Err(Error::invalid("mp4 mux: no streams"));
        }
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
            .map(|s| TrackOut {
                stream: s.clone(),
                fourcc: codec_fourcc(s.codec_id),
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

fn codec_fourcc(id: CodecId) -> [u8; 4] {
    match id {
        CodecId::H264 => *b"avc1",
        CodecId::Avif => *b"av01",
        CodecId::Aac => *b"mp4a",
        CodecId::Opus => *b"Opus",
        _ => *b"\x00\x00\x00\x00",
    }
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

/// AAC sample-rate → 4-bit samplingFrequencyIndex (ISO 14496-3); 44.1 kHz default.
fn aac_sf_index(rate: u32) -> u32 {
    const RATES: [u32; 13] = [
        96000, 88200, 64000, 48000, 44100, 32000, 24000, 22050, 16000, 12000, 11025, 8000, 7350,
    ];
    RATES.iter().position(|&r| r == rate).unwrap_or(4) as u32
}

/// AudioSpecificConfig for AAC-LC: objectType=2 (5b) + samplingFrequencyIndex (4b)
/// + channelConfiguration (4b) + GASpecificConfig (3b, all zero) = 16 bits.
fn build_asc(sample_rate: u32, channels: u16) -> Vec<u8> {
    let bits =
        (2u32 << 11) | (aac_sf_index(sample_rate) << 7) | ((channels.clamp(1, 7) as u32) << 3);
    vec![(bits >> 8) as u8, (bits & 0xff) as u8]
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
                    let mut p = Packet::from_data(s, tag(base + n as u8));
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
        assert_eq!(by_stream[&0], vec![tag(0), tag(1), tag(2)]);
        assert_eq!(by_stream[&1], vec![tag(0x10), tag(0x11), tag(0x12)]);
    }
}
