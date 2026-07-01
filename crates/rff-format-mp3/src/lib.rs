//! Native MP3 elementary-stream container (`.mp3`).
//!
//! An MP3 file is not really a container — it is a bare sequence of MPEG audio
//! frames, optionally wrapped in ID3 tags. So the muxer just writes each packet's
//! frame bytes in order, and the demuxer walks the frame headers, skipping ID3
//! tags and the leading Xing/Info header, and emits one [`Packet`] per audio
//! frame with a sample-accurate PTS.
//!
//! Frame parsing here is a container concern (finding frame boundaries), kept
//! self-contained — the same small MPEG-1/2/2.5 Layer III header fields the
//! decoder reads, but only as far as the byte layout needs.

use std::io::{Read, Write};

use rff_core::{CodecId, Error, Packet, Rational, Result, SampleFormat};
use rff_format::{Demuxer, Format, FormatRegistry, Input, Muxer, Output, Stream};

/// Register the MP3 format into a [`FormatRegistry`].
pub fn register(registry: &mut FormatRegistry) {
    registry.register(Format {
        name: "mp3",
        long_name: "MP3 (MPEG audio layer III)",
        extensions: &["mp3"],
        demuxer: Some(|input| Box::new(Mp3Demuxer::new(input))),
        muxer: Some(|output| Box::new(Mp3Muxer::new(output))),
        probe: Some(probe_mp3),
    });
}

// ── frame-header parsing (just enough to find boundaries) ─────────────────────

/// Bitrate (kbps) by index — MPEG-1 then MPEG-2/2.5 Layer III. Index 0/15 invalid.
const BITRATE_V1: [u32; 16] = [
    0, 32, 40, 48, 56, 64, 80, 96, 112, 128, 160, 192, 224, 256, 320, 0,
];
const BITRATE_V2: [u32; 16] = [
    0, 8, 16, 24, 32, 40, 48, 56, 64, 80, 96, 112, 128, 144, 160, 0,
];
/// Base sample rates (Hz) by index, MPEG-1; halved for MPEG-2, quartered for 2.5.
const SAMPLE_RATE: [u32; 4] = [44100, 48000, 32000, 0];

struct FrameInfo {
    sample_rate: u32,
    channels: u16,
    /// Decoded samples this frame yields (1152 MPEG-1, 576 MPEG-2/2.5).
    samples: u32,
    /// Total frame length in bytes (header + side info + main data + padding).
    size: usize,
    /// Byte offset of the Xing/Info tag within the frame (after header/CRC/side
    /// info) — used to recognise and drop the header frame.
    tag_offset: usize,
}

/// Parse a 4-byte MPEG audio Layer III frame header. Returns `None` for a bad
/// sync, a reserved/free field, or a non-Layer-III frame.
fn parse_header(b: &[u8]) -> Option<FrameInfo> {
    if b.len() < 4 {
        return None;
    }
    let h = u32::from_be_bytes([b[0], b[1], b[2], b[3]]);
    if (h >> 21) & 0x7FF != 0x7FF {
        return None; // frame sync
    }
    let version = (h >> 19) & 0x3; // 00 = 2.5, 01 = reserved, 10 = 2, 11 = 1
    if version == 0b01 {
        return None;
    }
    if (h >> 17) & 0x3 != 0b01 {
        return None; // Layer III only
    }
    let mpeg1 = version == 0b11;
    let br_idx = ((h >> 12) & 0xF) as usize;
    let sr_idx = ((h >> 10) & 0x3) as usize;
    if br_idx == 0 || br_idx == 15 || sr_idx == 3 {
        return None; // free-format / reserved
    }
    let padding = ((h >> 9) & 1) as usize;
    let crc = if (h >> 16) & 1 == 0 { 2 } else { 0 };
    let channels: u16 = if (h >> 6) & 0x3 == 0b11 { 1 } else { 2 };

    let bitrate = if mpeg1 {
        BITRATE_V1[br_idx]
    } else {
        BITRATE_V2[br_idx]
    };
    let base = SAMPLE_RATE[sr_idx];
    let sample_rate = match version {
        0b11 => base,     // MPEG-1
        0b10 => base / 2, // MPEG-2
        _ => base / 4,    // MPEG-2.5
    };
    let samples: u32 = if mpeg1 { 1152 } else { 576 };

    // frame size = floor(samples/8 * bitrate*1000 / sample_rate) + padding.
    let size = (samples as usize / 8) * (bitrate as usize * 1000) / sample_rate as usize + padding;
    if size < 4 {
        return None;
    }
    let side_info_len = match (mpeg1, channels == 2) {
        (true, true) => 32,
        (true, false) => 17,
        (false, true) => 17,
        (false, false) => 9,
    };
    Some(FrameInfo {
        sample_rate,
        channels,
        samples,
        size,
        tag_offset: 4 + crc + side_info_len,
    })
}

/// True if this frame carries a Xing/Info VBR/CBR header (a silent frame players
/// read for duration but skip on playback).
fn is_header_frame(frame: &[u8], info: &FrameInfo) -> bool {
    let o = info.tag_offset;
    frame.len() >= o + 4 && matches!(&frame[o..o + 4], b"Xing" | b"Info")
}

/// First byte offset of a valid frame sync at or after `start`.
fn find_sync(buf: &[u8], start: usize) -> Option<usize> {
    let mut i = start;
    while i + 4 <= buf.len() {
        if buf[i] == 0xFF && buf[i + 1] & 0xE0 == 0xE0 && parse_header(&buf[i..]).is_some() {
            return Some(i);
        }
        i += 1;
    }
    None
}

/// Length of a leading ID3v2 tag, if present (`ID3` + ver(2) + flags(1) +
/// syncsafe size(4); the size excludes the 10-byte header).
fn id3v2_len(buf: &[u8]) -> usize {
    if buf.len() < 10 || &buf[0..3] != b"ID3" {
        return 0;
    }
    let s = &buf[6..10];
    // Syncsafe integer: 7 bits per byte.
    let size = ((s[0] as usize) << 21)
        | ((s[1] as usize) << 14)
        | ((s[2] as usize) << 7)
        | (s[3] as usize);
    10 + size
}

/// Sniff MP3 by an ID3 tag or a valid frame sync near the start.
fn probe_mp3(data: &[u8]) -> i32 {
    if data.len() >= 3 && &data[0..3] == b"ID3" {
        return 90;
    }
    match find_sync(data, 0) {
        Some(i) if i < 4 => 80, // sync right at the front
        Some(_) => 40,          // sync after some junk — weaker
        None => 0,
    }
}

// ── demuxer ───────────────────────────────────────────────────────────────────

struct Mp3Demuxer {
    input: Option<Input>,
    buf: Vec<u8>,
    pos: usize,
    next_pts: i64,
}

impl Mp3Demuxer {
    fn new(input: Input) -> Mp3Demuxer {
        Mp3Demuxer {
            input: Some(input),
            buf: Vec::new(),
            pos: 0,
            next_pts: 0,
        }
    }
}

impl Demuxer for Mp3Demuxer {
    fn read_header(&mut self) -> Result<Vec<Stream>> {
        let mut input = self
            .input
            .take()
            .ok_or_else(|| Error::invalid("mp3 demux: header already read"))?;
        input.read_to_end(&mut self.buf)?;

        // Skip a leading ID3v2 tag, then find the first frame.
        self.pos = id3v2_len(&self.buf);
        let first = find_sync(&self.buf, self.pos)
            .ok_or_else(|| Error::invalid("mp3 demux: no MPEG audio frame found"))?;
        self.pos = first;
        let info = parse_header(&self.buf[first..])
            .ok_or_else(|| Error::invalid("mp3 demux: bad frame header"))?;

        let mut stream = Stream::new(0, CodecId::Mp3);
        stream.sample_rate = info.sample_rate;
        stream.channels = info.channels;
        stream.sample_format = Some(SampleFormat::F32);
        stream.time_base = Rational::new(1, info.sample_rate.max(1) as i32);
        Ok(vec![stream])
    }

    fn read_packet(&mut self) -> Result<Packet> {
        loop {
            let start = match find_sync(&self.buf, self.pos) {
                Some(i) => i,
                None => return Err(Error::Eof),
            };
            let info = match parse_header(&self.buf[start..]) {
                Some(i) => i,
                None => {
                    self.pos = start + 1;
                    continue;
                }
            };
            let end = start + info.size;
            if end > self.buf.len() {
                return Err(Error::Eof); // trailing partial frame
            }
            let frame = &self.buf[start..end];
            self.pos = end;

            // Drop the Xing/Info header frame (silent; metadata only).
            if is_header_frame(frame, &info) {
                continue;
            }
            let mut packet = Packet::from_data(0, frame.to_vec());
            packet.pts = Some(self.next_pts);
            self.next_pts += info.samples as i64;
            return Ok(packet);
        }
    }
}

// ── muxer ─────────────────────────────────────────────────────────────────────

struct Mp3Muxer {
    out: Output,
}

impl Mp3Muxer {
    fn new(out: Output) -> Mp3Muxer {
        Mp3Muxer { out }
    }
}

impl Muxer for Mp3Muxer {
    fn write_header(&mut self, streams: &[Stream]) -> Result<()> {
        match streams.first() {
            Some(s) if s.codec_id == CodecId::Mp3 => Ok(()),
            Some(_) => Err(Error::unsupported(
                "mp3 mux: only the `mp3` codec is supported",
            )),
            None => Err(Error::invalid("mp3 mux: no streams")),
        }
    }

    fn write_packet(&mut self, packet: &Packet) -> Result<()> {
        // A packet is one (or more) complete MPEG frames; an MP3 file is just the
        // frames concatenated, so write them straight through.
        self.out.write_all(&packet.data)?;
        Ok(())
    }

    fn write_trailer(&mut self) -> Result<()> {
        self.out.flush()?;
        Ok(())
    }
}

#[cfg(test)]
mod tests {
    use super::*;
    use std::io::Cursor;
    use std::sync::{Arc, Mutex};

    /// A `Write` sink whose bytes can be read back after the muxer drops it.
    #[derive(Clone)]
    struct SharedSink(Arc<Mutex<Vec<u8>>>);
    impl Write for SharedSink {
        fn write(&mut self, b: &[u8]) -> std::io::Result<usize> {
            self.0.lock().unwrap().extend_from_slice(b);
            Ok(b.len())
        }
        fn flush(&mut self) -> std::io::Result<()> {
            Ok(())
        }
    }

    /// A 4-byte MPEG-1 Layer III header: 128 kbps, 44.1 kHz, stereo, no CRC.
    const HDR: [u8; 4] = [0xFF, 0xFB, 0x90, 0x00];

    fn make_frame(hdr: [u8; 4], fill: u8) -> Vec<u8> {
        let info = parse_header(&hdr).unwrap();
        let mut f = hdr.to_vec();
        f.resize(info.size, fill);
        f
    }

    #[test]
    fn parses_frame_geometry() {
        let info = parse_header(&HDR).unwrap();
        assert_eq!(info.sample_rate, 44100);
        assert_eq!(info.channels, 2);
        assert_eq!(info.samples, 1152);
        assert_eq!(info.size, 417); // the canonical 128k/44.1k frame size
    }

    #[test]
    fn probe_recognizes_sync_and_id3() {
        assert_eq!(probe_mp3(&HDR), 80);
        assert_eq!(probe_mp3(b"ID3\x04\x00\x00\x00\x00\x00\x00"), 90);
        assert_eq!(probe_mp3(b"RIFFxxxxWAVE"), 0);
    }

    #[test]
    fn demuxes_frames_with_pts_skipping_id3_and_xing() {
        // ID3v2 (header + 5 bytes) · Xing header frame · two audio frames.
        let mut file = Vec::new();
        file.extend_from_slice(b"ID3\x04\x00\x00\x00\x00\x00\x05"); // 10-byte hdr, size 5
        file.extend_from_slice(&[0u8; 5]);
        // Xing frame: header + "Xing" at the tag offset (4 + 32 side info).
        let mut xing = make_frame(HDR, 0);
        xing[36..40].copy_from_slice(b"Xing");
        file.extend_from_slice(&xing);
        file.extend_from_slice(&make_frame(HDR, 0xAA));
        file.extend_from_slice(&make_frame(HDR, 0xBB));

        let mut dem = Mp3Demuxer::new(Box::new(Cursor::new(file)));
        let streams = dem.read_header().unwrap();
        assert_eq!(streams[0].codec_id, CodecId::Mp3);
        assert_eq!(streams[0].sample_rate, 44100);
        assert_eq!(streams[0].channels, 2);

        let p0 = dem.read_packet().unwrap();
        assert_eq!(p0.pts, Some(0));
        assert_eq!(p0.data.len(), 417);
        assert_eq!(p0.data[4], 0xAA); // the first *audio* frame, not the Xing one
        let p1 = dem.read_packet().unwrap();
        assert_eq!(p1.pts, Some(1152)); // sample-accurate PTS
        assert_eq!(p1.data[4], 0xBB);
        assert!(matches!(dem.read_packet(), Err(Error::Eof)));
    }

    #[test]
    fn muxer_round_trips_frames() {
        let sink = SharedSink(Arc::new(Mutex::new(Vec::new())));
        let mut mux = Mp3Muxer::new(Box::new(sink.clone()));
        mux.write_header(&[Stream::new(0, CodecId::Mp3)]).unwrap();
        mux.write_packet(&Packet::from_data(0, make_frame(HDR, 0x11)))
            .unwrap();
        mux.write_packet(&Packet::from_data(0, make_frame(HDR, 0x22)))
            .unwrap();
        mux.write_trailer().unwrap();

        // Demux it back: two frames, in order.
        let out = sink.0.lock().unwrap().clone();
        let mut dem = Mp3Demuxer::new(Box::new(Cursor::new(out)));
        dem.read_header().unwrap();
        assert_eq!(dem.read_packet().unwrap().data[4], 0x11);
        assert_eq!(dem.read_packet().unwrap().data[4], 0x22);
        assert!(matches!(dem.read_packet(), Err(Error::Eof)));
    }

    #[test]
    fn muxer_rejects_non_mp3() {
        let mut mux = Mp3Muxer::new(Box::new(std::io::sink()));
        assert!(mux.write_header(&[Stream::new(0, CodecId::Opus)]).is_err());
    }
}
