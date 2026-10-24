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

use std::io::{Read, Seek, SeekFrom, Write};

use rff_core::{CodecId, Error, Packet, Rational, Result, SampleFormat};
use rff_format::{
    Demuxer, Format, FormatRegistry, Input, MuxCaps, Muxer, Output, SeekOutput, Stream,
};

/// Register the MP3 format into a [`FormatRegistry`].
pub fn register(registry: &mut FormatRegistry) {
    registry.register(Format {
        name: "mp3",
        long_name: "MP3 (MPEG audio layer III)",
        extensions: &["mp3"],
        demuxer: Some(|input| Box::new(Mp3Demuxer::new(input))),
        muxer: Some(|output| Box::new(Mp3Muxer::new(output))),
        muxer_path: None,
        probe: Some(probe_mp3),
        mux_caps: MuxCaps::single(&[CodecId::Mp3]),
    });
    registry.register_seekable_muxer("mp3", |output| Box::new(Mp3Muxer::new_seekable(output)));
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

/// Bytes requested from the input per refill of the scan window.
const READ_CHUNK: usize = 8 * 1024;
/// An ID3v1 tag: `TAG` + 125 bytes, the last 128 bytes of the file.
const ID3V1_LEN: usize = 128;
/// An APEv2 tag header/footer: `APETAGEX` + version, size, items, flags, reserved.
const APE_HEADER_LEN: usize = 32;

/// Streaming MP3 demuxer. `read_header` skips a leading ID3v2 tag (by its
/// declared size, without buffering it) and parses the first frame header;
/// `read_packet` then walks frame by frame through a small window (a frame
/// is at most ~1.4 KB), so peak memory is O(one frame), not O(file) (issue
/// #26). Works on pipes.
///
/// Frame finding is exactly the whole-buffer walk's: scan byte by byte for a
/// valid header, take `size` bytes, drop Xing/Info frames, and stop at a
/// trailing partial frame. In addition, a trailing ID3v1 tag and an APEv2
/// tag are skipped rather than scanned for false syncs.
struct Mp3Demuxer {
    /// The source before `read_header`.
    input: Option<Input>,
    /// The source after `read_header`.
    reader: Option<Input>,
    /// Scan window; `buf[pos..]` are the bytes not yet consumed.
    buf: Vec<u8>,
    pos: usize,
    /// The input has reported end of file.
    eof: bool,
    next_pts: i64,
}

impl Mp3Demuxer {
    fn new(input: Input) -> Mp3Demuxer {
        Mp3Demuxer {
            input: Some(input),
            reader: None,
            buf: Vec::new(),
            pos: 0,
            eof: false,
            next_pts: 0,
        }
    }

    fn avail(&self) -> usize {
        self.buf.len() - self.pos
    }

    /// Make at least `need` unconsumed bytes available; `false` if the input
    /// ends first (whatever it held is still in the window).
    fn fill(&mut self, need: usize) -> std::io::Result<bool> {
        while self.avail() < need {
            let Some(reader) = self.reader.as_mut().filter(|_| !self.eof) else {
                return Ok(false);
            };
            if self.pos > 0 {
                self.buf.drain(..self.pos); // < `need` bytes move
                self.pos = 0;
            }
            let old = self.buf.len();
            self.buf.resize(old + READ_CHUNK, 0);
            let n = loop {
                match reader.read(&mut self.buf[old..]) {
                    Ok(n) => break n,
                    Err(e) if e.kind() == std::io::ErrorKind::Interrupted => {}
                    Err(e) => {
                        self.buf.truncate(old);
                        return Err(e);
                    }
                }
            };
            self.buf.truncate(old + n);
            if n == 0 {
                self.eof = true;
            }
        }
        Ok(true)
    }

    /// Consume `n` bytes, streaming past whatever the window does not hold.
    fn skip(&mut self, n: u64) -> std::io::Result<()> {
        let avail = self.avail() as u64;
        if n <= avail {
            self.pos += n as usize;
            return Ok(());
        }
        self.buf.clear();
        self.pos = 0;
        if let Some(reader) = self.reader.as_mut() {
            std::io::copy(&mut reader.by_ref().take(n - avail), &mut std::io::sink())?;
        }
        Ok(())
    }

    /// Advance to the next valid frame header; `false` at end of input. Every
    /// step consumes at least one byte or returns.
    fn next_sync(&mut self) -> std::io::Result<bool> {
        loop {
            if !self.fill(4)? {
                return Ok(false);
            }
            let b = &self.buf[self.pos..];
            if b[0] == 0xFF && b[1] & 0xE0 == 0xE0 && parse_header(b).is_some() {
                return Ok(true);
            }
            if b.starts_with(b"TAG") {
                // ID3v1 iff exactly its 128 bytes remain.
                let more = self.fill(ID3V1_LEN + 1)?;
                if !more && self.avail() == ID3V1_LEN {
                    self.pos = self.buf.len();
                    return Ok(false);
                }
            } else if b.starts_with(b"APET") && self.fill(APE_HEADER_LEN)? {
                let h = &self.buf[self.pos..self.pos + APE_HEADER_LEN];
                if &h[..8] == b"APETAGEX" {
                    let size = u32::from_le_bytes([h[12], h[13], h[14], h[15]]) as u64;
                    let flags = u32::from_le_bytes([h[20], h[21], h[22], h[23]]);
                    // At a header (flag bit 29) the declared size covers the
                    // items + footer that follow it; at a bare footer, just it.
                    let len = if flags & (1 << 29) != 0 {
                        APE_HEADER_LEN as u64 + size
                    } else {
                        APE_HEADER_LEN as u64
                    };
                    self.skip(len)?;
                    continue;
                }
            }
            self.pos += 1;
        }
    }
}

impl Demuxer for Mp3Demuxer {
    fn read_header(&mut self) -> Result<Vec<Stream>> {
        let input = self
            .input
            .take()
            .ok_or_else(|| Error::invalid("mp3 demux: header already read"))?;
        self.reader = Some(input);

        // Skip a leading ID3v2 tag, then find the first frame.
        self.fill(10)?;
        let id3 = id3v2_len(&self.buf[self.pos..]);
        self.skip(id3 as u64)?;
        if !self.next_sync()? {
            return Err(Error::invalid("mp3 demux: no MPEG audio frame found"));
        }
        let info = parse_header(&self.buf[self.pos..])
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
            if !self.next_sync()? {
                return Err(Error::Eof);
            }
            let info = match parse_header(&self.buf[self.pos..]) {
                Some(i) => i,
                None => {
                    self.pos += 1;
                    continue;
                }
            };
            if !self.fill(info.size)? {
                return Err(Error::Eof); // trailing partial frame
            }
            let frame = &self.buf[self.pos..self.pos + info.size];
            let header_frame = is_header_frame(frame, &info);
            let data = (!header_frame).then(|| frame.to_vec());
            self.pos += info.size;

            // Drop the Xing/Info header frame (silent; metadata only).
            let Some(data) = data else { continue };
            let mut packet = Packet::from_data(0, data);
            packet.pts = Some(self.next_pts);
            self.next_pts += info.samples as i64;
            return Ok(packet);
        }
    }
}

// ── muxer ─────────────────────────────────────────────────────────────────────

/// Where the muxer writes: a pipe/socket, or a file it can seek back into.
enum Sink {
    Plain(Output),
    Seekable(SeekOutput),
}

impl Sink {
    fn write_all(&mut self, bytes: &[u8]) -> std::io::Result<()> {
        match self {
            Sink::Plain(out) => out.write_all(bytes),
            Sink::Seekable(out) => out.write_all(bytes),
        }
    }

    fn flush(&mut self) -> std::io::Result<()> {
        match self {
            Sink::Plain(out) => out.flush(),
            Sink::Seekable(out) => out.flush(),
        }
    }
}

/// The MP3 muxer: packets (whole frames) are written straight through.
///
/// One exception keeps the output gapless. An encoder that streams its frames
/// (`rusty_mp3`, like LAME) emits its Xing/Info tag frame FIRST as a
/// placeholder -- its frame count, size, TOC and end padding are only known at
/// the end -- and the completed tag LAST. On a seekable output that final tag
/// is written back over the placeholder (same length, at the offset the
/// placeholder was written to); on a pipe, where nothing can be rewritten, it
/// is dropped, so a tag frame never lands after the audio (decoders would play
/// it as a frame of silence).
struct Mp3Muxer {
    sink: Sink,
    /// Packets written so far.
    packets: u64,
    /// Offset and length of the tag frame the stream started with, if it did.
    lead_tag: Option<(u64, usize)>,
}

impl Mp3Muxer {
    fn new(out: Output) -> Mp3Muxer {
        Mp3Muxer {
            sink: Sink::Plain(out),
            packets: 0,
            lead_tag: None,
        }
    }

    fn new_seekable(out: SeekOutput) -> Mp3Muxer {
        Mp3Muxer {
            sink: Sink::Seekable(out),
            packets: 0,
            lead_tag: None,
        }
    }
}

/// True if `data` is exactly one frame carrying a Xing/Info tag.
fn is_tag_packet(data: &[u8]) -> bool {
    parse_header(data).is_some_and(|info| data.len() == info.size && is_header_frame(data, &info))
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
        let data = &packet.data;
        let first = self.packets == 0;
        self.packets += 1;
        if is_tag_packet(data) {
            if first {
                let at = match &mut self.sink {
                    Sink::Seekable(out) => out.stream_position()?,
                    Sink::Plain(_) => 0,
                };
                self.lead_tag = Some((at, data.len()));
            } else {
                // The completed tag for the placeholder this stream began with.
                if let (Sink::Seekable(out), Some((at, len))) = (&mut self.sink, self.lead_tag) {
                    if len == data.len() {
                        let end = out.stream_position()?;
                        out.seek(SeekFrom::Start(at))?;
                        out.write_all(data)?;
                        out.seek(SeekFrom::Start(end))?;
                    }
                }
                return Ok(());
            }
        }
        self.sink.write_all(data)?;
        Ok(())
    }

    fn write_trailer(&mut self) -> Result<()> {
        self.sink.flush()?;
        Ok(())
    }
}

#[cfg(test)]
mod legacy;

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

    /// A tag frame (header + `tag` at the Xing offset), `fill` after it.
    fn tag_frame(tag: &[u8; 4], fill: u8) -> Vec<u8> {
        let mut f = make_frame(HDR, fill);
        f[36..40].copy_from_slice(tag);
        f
    }

    /// The streaming encoder's packet order -- placeholder tag, audio, final
    /// tag -- leaves the FINAL tag at the front of a seekable file, and no tag
    /// frame after the audio of either sink.
    #[test]
    fn muxer_patches_the_leading_tag_frame() {
        let packets = [
            tag_frame(b"Info", 0x00),
            make_frame(HDR, 0x11),
            make_frame(HDR, 0x22),
            tag_frame(b"Info", 0x77),
        ];
        let cur = Arc::new(Mutex::new(Cursor::new(Vec::new())));
        struct Shared(Arc<Mutex<Cursor<Vec<u8>>>>);
        impl Write for Shared {
            fn write(&mut self, b: &[u8]) -> std::io::Result<usize> {
                self.0.lock().unwrap().write(b)
            }
            fn flush(&mut self) -> std::io::Result<()> {
                Ok(())
            }
        }
        impl Seek for Shared {
            fn seek(&mut self, to: SeekFrom) -> std::io::Result<u64> {
                self.0.lock().unwrap().seek(to)
            }
        }
        let mut mux = Mp3Muxer::new_seekable(Box::new(Shared(cur.clone())));
        mux.write_header(&[Stream::new(0, CodecId::Mp3)]).unwrap();
        for p in &packets {
            mux.write_packet(&Packet::from_data(0, p.clone())).unwrap();
        }
        mux.write_trailer().unwrap();
        let out = cur.lock().unwrap().get_ref().clone();
        let want: Vec<u8> = [&packets[3][..], &packets[1], &packets[2]].concat();
        assert_eq!(out, want, "seekable: final tag over the placeholder");

        let sink = SharedSink(Arc::new(Mutex::new(Vec::new())));
        let mut mux = Mp3Muxer::new(Box::new(sink.clone()));
        mux.write_header(&[Stream::new(0, CodecId::Mp3)]).unwrap();
        for p in &packets {
            mux.write_packet(&Packet::from_data(0, p.clone())).unwrap();
        }
        mux.write_trailer().unwrap();
        let want: Vec<u8> = packets[..3].concat();
        assert_eq!(*sink.0.lock().unwrap(), want, "pipe: trailing tag dropped");
    }

    #[test]
    fn muxer_rejects_non_mp3() {
        let mut mux = Mp3Muxer::new(Box::new(std::io::sink()));
        assert!(mux.write_header(&[Stream::new(0, CodecId::Opus)]).is_err());
    }

    // ---- streaming (issue #26): equivalence, header cost, robustness ----

    use std::sync::atomic::{AtomicU64, Ordering};

    /// A reader that counts the bytes pulled from it and hands out at most
    /// `chunk` bytes per `read` (a pipe-like source).
    struct Counting {
        inner: Cursor<Vec<u8>>,
        read: Arc<AtomicU64>,
        chunk: usize,
    }
    impl Read for Counting {
        fn read(&mut self, buf: &mut [u8]) -> std::io::Result<usize> {
            let n = buf.len().min(self.chunk);
            let got = self.inner.read(&mut buf[..n])?;
            self.read.fetch_add(got as u64, Ordering::Relaxed);
            Ok(got)
        }
    }
    fn counting(data: Vec<u8>, chunk: usize) -> (Input, Arc<AtomicU64>) {
        let read = Arc::new(AtomicU64::new(0));
        let r = Counting {
            inner: Cursor::new(data),
            read: read.clone(),
            chunk,
        };
        (Box::new(r), read)
    }

    type Sig = (
        usize,
        Option<i64>,
        Option<i64>,
        i64,
        (i32, i32),
        bool,
        Vec<u8>,
    );
    /// Everything a demuxer reports: the header (or its error), every packet,
    /// and how the packet loop ended. Bounded so a looping demuxer fails.
    fn drive(mut d: Box<dyn Demuxer>) -> (String, Vec<Sig>, String) {
        let head = match d.read_header() {
            Ok(s) => format!("{s:?}"),
            Err(e) => return (format!("Err({e})"), Vec::new(), String::new()),
        };
        let mut packets = Vec::new();
        for _ in 0..1_000_000 {
            match d.read_packet() {
                Ok(p) => packets.push((
                    p.stream_index,
                    p.pts,
                    p.dts,
                    p.duration,
                    (p.time_base.num, p.time_base.den),
                    p.flags.keyframe,
                    p.data,
                )),
                Err(e) => {
                    // The end is sticky: asking again says the same thing.
                    let again = d.read_packet().err().map(|e| e.to_string());
                    assert_eq!(again, Some(e.to_string()), "end not sticky");
                    return (head, packets, e.to_string());
                }
            }
        }
        panic!("demuxer did not terminate");
    }
    fn legacy_run(file: &[u8]) -> (String, Vec<Sig>, String) {
        drive(Box::new(legacy::LegacyMp3Demuxer::new(Box::new(
            Cursor::new(file.to_vec()),
        ))))
    }
    fn assert_same_as_legacy(name: &str, file: &[u8], chunks: &[usize]) {
        let old = legacy_run(file);
        for &chunk in chunks {
            let (input, _) = counting(file.to_vec(), chunk);
            let new = drive(Box::new(Mp3Demuxer::new(input)));
            assert_eq!(new.0, old.0, "{name}: header (chunk {chunk})");
            assert_eq!(new.1.len(), old.1.len(), "{name}: packet count");
            assert!(new.1 == old.1, "{name}: packets differ (chunk {chunk})");
            assert_eq!(new.2, old.2, "{name}: end (chunk {chunk})");
        }
    }

    fn id3v2(payload: usize) -> Vec<u8> {
        let s = payload as u32;
        let mut t = b"ID3\x04\x00\x00".to_vec();
        t.extend_from_slice(&[
            (s >> 21) as u8 & 0x7F,
            (s >> 14) as u8 & 0x7F,
            (s >> 7) as u8 & 0x7F,
            s as u8 & 0x7F,
        ]);
        t.extend((0..payload).map(|i| (i % 251) as u8));
        t
    }
    /// MPEG-1 128k stereo (+padding), MPEG-1 320k mono with CRC, MPEG-2 64k,
    /// MPEG-2.5 32k: a mix of frame sizes and sample counts.
    const HDRS: [[u8; 4]; 4] = [
        [0xFF, 0xFB, 0x92, 0x00],
        [0xFF, 0xFA, 0xE0, 0xC0],
        [0xFF, 0xF3, 0x80, 0x00],
        [0xFF, 0xE3, 0x40, 0x40],
    ];
    fn frames(n: usize) -> Vec<u8> {
        let mut f = Vec::new();
        for i in 0..n {
            f.extend_from_slice(&make_frame(HDRS[i % 4], i as u8));
        }
        f
    }
    fn xing() -> Vec<u8> {
        let mut x = make_frame(HDR, 0);
        x[36..40].copy_from_slice(b"Info");
        x
    }
    fn cat(parts: &[&[u8]]) -> Vec<u8> {
        parts.concat()
    }

    #[test]
    fn streaming_matches_whole_buffer_demuxer() {
        let mut id3v1 = b"TAG".to_vec();
        id3v1.resize(ID3V1_LEN, b' ');
        let mut ape = b"APETAGEX".to_vec();
        ape.extend_from_slice(&2000u32.to_le_bytes());
        ape.extend_from_slice(&40u32.to_le_bytes()); // size: items + footer
        ape.extend_from_slice(&1u32.to_le_bytes());
        ape.extend_from_slice(&(0xA000_0000u32).to_le_bytes()); // has header, is header
        ape.extend_from_slice(&[0u8; 8]);
        ape.extend_from_slice(b"title=xx");
        ape.extend_from_within(..32); // the footer mirrors the header
        let garbage: Vec<u8> = (0..3000u32).map(|i| (i * 13 % 200) as u8).collect();
        let false_sync = [0xFF, 0xFB, 0x90, 0x00, 1, 2, 3]; // a header, then too little
        let cases: Vec<(&str, Vec<u8>)> = vec![
            ("plain", frames(40)),
            ("id3 + info", cat(&[&id3v2(5), &xing(), &frames(9)])),
            ("big id3", cat(&[&id3v2(200_000), &frames(9)])),
            ("id3 past eof", cat(&[&id3v2(10_000)[..500], &frames(0)])),
            ("tiny id3", b"ID3\x04\x00".to_vec()),
            ("leading garbage", cat(&[&garbage, &frames(7)])),
            ("garbage between", cat(&[&frames(5), &garbage, &frames(5)])),
            (
                "partial tail",
                cat(&[&frames(5), &make_frame(HDR, 9)[..300]]),
            ),
            ("false sync tail", cat(&[&frames(5), &false_sync])),
            ("id3v1 tail", cat(&[&frames(5), &id3v1])),
            ("ape tail", cat(&[&frames(5), &ape, &id3v1])),
            ("info mid-stream", cat(&[&frames(3), &xing(), &frames(3)])),
            ("no frames", garbage.clone()),
            ("empty", Vec::new()),
        ];
        for (name, file) in &cases {
            assert_same_as_legacy(name, file, &[1, 7, usize::MAX]);
        }
        // Real files, when the checkout has them.
        let root = std::path::Path::new(env!("CARGO_MANIFEST_DIR")).join("../..");
        for rel in [
            "crates/rusty_mp3/examples/wasmcheck_fixture.mp3",
            "corpus/dl/mus_piano.mp3",
        ] {
            if let Ok(file) = std::fs::read(root.join(rel)) {
                assert_same_as_legacy(rel, &file, &[4093, usize::MAX]);
            }
        }
    }

    /// Trailing tags are skipped, not scanned: a byte pattern inside one that
    /// happens to look like a frame no longer surfaces as a bogus packet.
    #[test]
    fn trailing_tags_are_not_scanned_for_frames() {
        let fake = make_frame(HDR, 0x55); // 417 bytes, a valid-looking frame
        let mut id3v1 = b"TAG".to_vec();
        id3v1.extend_from_slice(&[0xFF, 0xFB, 0x10, 0x00]); // 32k frame = 104 bytes
        id3v1.resize(ID3V1_LEN, 0);
        let mut ape = b"APETAGEX".to_vec();
        ape.extend_from_slice(&2000u32.to_le_bytes());
        ape.extend_from_slice(&((fake.len() + 32) as u32).to_le_bytes());
        ape.extend_from_slice(&1u32.to_le_bytes());
        ape.extend_from_slice(&(0xA000_0000u32).to_le_bytes());
        ape.extend_from_slice(&[0u8; 8]);
        ape.extend_from_slice(&fake); // binary item (cover art, say)
        let footer = ape[..32].to_vec();
        ape.extend_from_slice(&footer);
        for (name, tail) in [("id3v1", id3v1), ("ape", ape)] {
            let file = cat(&[&frames(4), &tail]);
            let new = drive(Box::new(Mp3Demuxer::new(Box::new(Cursor::new(
                file.clone(),
            )))));
            assert_eq!(new.1.len(), 4, "{name}");
            assert_eq!(
                legacy_run(&file).1.len(),
                5,
                "{name}: the old walk was fooled"
            );
        }
    }

    #[test]
    fn read_header_reads_only_the_header() {
        // ~4 MB of frames: the header must not pull the file in.
        let file = cat(&[&id3v2(100), &frames(10_000)]);
        let (input, read) = counting(file, usize::MAX);
        let mut dem = Mp3Demuxer::new(input);
        dem.read_header().unwrap();
        assert!(
            read.load(Ordering::Relaxed) < 64 * 1024,
            "header read too much"
        );
        dem.read_packet().unwrap();
        assert!(read.load(Ordering::Relaxed) < 64 * 1024);

        // A 4 MB ID3v2 tag (embedded cover art) is streamed past, not held.
        let file = cat(&[&id3v2(4 << 20), &frames(3)]);
        let mut dem = Mp3Demuxer::new(Box::new(Cursor::new(file)));
        dem.read_header().unwrap();
        assert!(
            dem.buf.capacity() < 64 * 1024,
            "window grew to {}",
            dem.buf.capacity()
        );
        for _ in 0..3 {
            dem.read_packet().unwrap();
        }
        assert!(matches!(dem.read_packet(), Err(Error::Eof)));
        assert!(dem.buf.capacity() < 64 * 1024);
    }

    #[test]
    fn truncated_and_garbage_inputs_fail_cleanly() {
        // Every truncation of a valid file: never a panic, always an end.
        let good = cat(&[&id3v2(20), &xing(), &frames(4)]);
        for cut in 0..good.len() {
            let _ = drive(Box::new(Mp3Demuxer::new(Box::new(Cursor::new(
                good[..cut].to_vec(),
            )))));
        }
        // Sync-heavy garbage (lots of 0xFF) and an APE header with a huge size.
        let mut x = 0x2545_F491_u32;
        for _ in 0..64 {
            let mut g = HDR.to_vec();
            for _ in 0..2000 {
                x ^= x << 13;
                x ^= x >> 17;
                x ^= x << 5;
                g.push(if x & 3 == 0 { 0xFF } else { x as u8 });
            }
            let _ = drive(Box::new(Mp3Demuxer::new(Box::new(Cursor::new(g)))));
        }
        let mut ape = b"APETAGEX".to_vec();
        ape.extend_from_slice(&2000u32.to_le_bytes());
        ape.extend_from_slice(&u32::MAX.to_le_bytes());
        ape.extend_from_slice(&[0, 0, 0, 0, 0, 0, 0, 0xA0]);
        ape.extend_from_slice(&[0u8; 8]);
        let file = cat(&[&frames(2), &ape, &frames(2)]);
        let (_, packets, end) = drive(Box::new(Mp3Demuxer::new(Box::new(Cursor::new(file)))));
        assert_eq!((packets.len(), end.as_str()), (2, "end of stream"));
        // Reading packets before the header is just end-of-stream.
        let mut dem = Mp3Demuxer::new(Box::new(Cursor::new(good)));
        assert!(matches!(dem.read_packet(), Err(Error::Eof)));
    }
}
