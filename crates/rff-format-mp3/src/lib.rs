//! Native MPEG audio elementary-stream container (`.mp3`, and for reading
//! `.mp2` / `.mp1` / `.mpa`).
//!
//! An MP3 file is not really a container — it is a bare sequence of MPEG audio
//! frames, optionally wrapped in ID3 tags. So the muxer just writes each packet's
//! frame bytes in order, and the demuxer walks the frame headers, skipping ID3
//! tags and the leading Xing/Info header, and emits one [`Packet`] per audio
//! frame with a sample-accurate PTS.
//!
//! The demuxer reads Layer I and Layer II streams too (MP1 / MP2, including
//! free format), locking onto the layer of the first frame and labelling the
//! stream [`CodecId::Mp1`] / [`CodecId::Mp2`] / [`CodecId::Mp3`] by it.
//!
//! A header is not enough to be a frame: with no container around them, the
//! frames have to vouch for each other. Both the demuxer and the content
//! sniffing use minimp3's rule (`mp3d_find_frame` / `mp3d_match_frame`): a
//! frame counts when the header after it is of the same stream (version,
//! layer, sample rate), and where the stream is not yet established, when a
//! run of such headers follows it. A frame damaged near its header, and bytes
//! of another kind of file that happen to look like a header, are therefore
//! dropped rather than handed to the decoder. The rule is spelled out on the
//! demuxer type.
//!
//! Frame parsing here is a container concern (finding frame boundaries), kept
//! self-contained — the same small MPEG-1/2/2.5 header fields the decoder
//! reads, but only as far as the byte layout needs.

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
    // The Layer I/II extensions, for reading: the same demuxer (content
    // probing already finds these files through `mp3`; this is the
    // filename fallback). No muxer -- there is no MP1/MP2 encoder, and an
    // `.mp2` output must not silently receive Layer III frames.
    registry.register(Format {
        name: "mp2",
        long_name: "MP2/MP1 (MPEG audio layer 2/1)",
        extensions: &["mp2", "mp1", "mpa", "m2a", "m1a"],
        demuxer: Some(|input| Box::new(Mp3Demuxer::new(input))),
        muxer: None,
        muxer_path: None,
        probe: None,
        mux_caps: MuxCaps::NONE,
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

/// What stays put from frame to frame of one stream, exactly as minimp3's
/// `hdr_compare` reads it: the version bits, the layer, the sample-rate index
/// and whether the bitrate is free format. The bitrate, the padding, the CRC
/// flag and the channel mode may change from frame to frame.
type StreamKey = (u32, u8, usize, bool);

#[derive(Clone, Copy)]
struct FrameInfo {
    /// 1, 2 or 3.
    layer: u8,
    sample_rate: u32,
    channels: u16,
    /// Decoded samples this frame yields (Layer III: 1152 MPEG-1, 576
    /// MPEG-2/2.5; Layer I 384, Layer II 1152).
    samples: u32,
    /// Total frame length in bytes (header + side info + main data + padding);
    /// 0 for a free-format frame, whose length is the distance to the next
    /// header.
    size: usize,
    /// Byte offset of the Xing/Info tag within the frame (after header/CRC/side
    /// info) — used to recognise and drop the header frame.
    tag_offset: usize,
    key: StreamKey,
    /// Bytes the padding bit added (4 in Layer I, 1 otherwise, or 0).
    pad: usize,
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
        layer: 3,
        sample_rate,
        channels,
        samples,
        size,
        tag_offset: 4 + crc + side_info_len,
        key: (version, 3, sr_idx, false),
        pad: padding,
    })
}

/// True if this frame carries a Xing/Info VBR/CBR header (a silent frame players
/// read for duration but skip on playback). Layer III only.
fn is_header_frame(frame: &[u8], info: &FrameInfo) -> bool {
    let o = info.tag_offset;
    info.layer == 3 && frame.len() >= o + 4 && matches!(&frame[o..o + 4], b"Xing" | b"Info")
}

/// Bitrate (kbps) by index: MPEG-1 Layer I, MPEG-1 Layer II, MPEG-2/2.5
/// Layer I, MPEG-2/2.5 Layer II.
const BITRATE_L12: [[u32; 16]; 4] = [
    [
        0, 32, 64, 96, 128, 160, 192, 224, 256, 288, 320, 352, 384, 416, 448, 0,
    ],
    [
        0, 32, 48, 56, 64, 80, 96, 112, 128, 160, 192, 224, 256, 320, 384, 0,
    ],
    [
        0, 32, 48, 56, 64, 80, 96, 112, 128, 144, 160, 176, 192, 224, 256, 0,
    ],
    [
        0, 8, 16, 24, 32, 40, 48, 56, 64, 80, 96, 112, 128, 144, 160, 0,
    ],
];
/// Largest free-format frame searched for (bytes).
const MAX_FREE_FRAME: usize = 4096;
/// The longest frame a table bitrate gives: MPEG-2.5 Layer II, 160 kbps at
/// 8 kHz, padded.
const MAX_FRAME: usize = 2881;
/// How many headers after a frame found by searching must be its stream's,
/// each where the frame before it ends, unless the data ends first (minimp3's
/// `MAX_FRAME_SYNC_MATCHES`).
const MAX_MATCHES: usize = 10;

/// Parse a Layer I or II header (`None` for anything else, or reserved
/// fields). `size` is 0 for a free-format frame.
fn parse_l12_header(b: &[u8]) -> Option<FrameInfo> {
    let h = u32::from_be_bytes(*b.first_chunk::<4>()?);
    if (h >> 21) & 0x7FF != 0x7FF {
        return None;
    }
    let version = (h >> 19) & 0x3;
    let layer: u8 = match (h >> 17) & 0x3 {
        0b11 => 1,
        0b10 => 2,
        _ => return None,
    };
    let br_idx = ((h >> 12) & 0xF) as usize;
    let sr_idx = ((h >> 10) & 0x3) as usize;
    if version == 0b01 || br_idx == 15 || sr_idx == 3 {
        return None;
    }
    let mpeg1 = version == 0b11;
    let bitrate = BITRATE_L12[usize::from(!mpeg1) * 2 + usize::from(layer - 1)][br_idx];
    let sample_rate = match version {
        0b11 => SAMPLE_RATE[sr_idx],
        0b10 => SAMPLE_RATE[sr_idx] / 2,
        _ => SAMPLE_RATE[sr_idx] / 4,
    };
    let padding = ((h >> 9) & 1) as usize;
    let (samples, pad, size) = if layer == 1 {
        (
            384,
            padding * 4,
            (12 * bitrate as usize * 1000 / sample_rate as usize + padding) * 4,
        )
    } else {
        (
            1152,
            padding,
            144 * bitrate as usize * 1000 / sample_rate as usize + padding,
        )
    };
    let size = if br_idx == 0 { 0 } else { size };
    let channels: u16 = if (h >> 6) & 0x3 == 0b11 { 1 } else { 2 };
    Some(FrameInfo {
        layer,
        sample_rate,
        channels,
        samples,
        size,
        tag_offset: 0,
        key: (version, layer, sr_idx, br_idx == 0),
        pad,
    })
}

/// The frame header of any layer `b` starts with, if it is one.
fn parse_any(b: &[u8]) -> Option<FrameInfo> {
    if b.len() < 4 || b[0] != 0xFF || b[1] & 0xE0 != 0xE0 {
        return None;
    }
    if b[1] & 0x04 == 0 {
        // Layer III (or the reserved layer, which never parses).
        parse_header(b)
    } else {
        parse_l12_header(b)
    }
}

/// The header `b` starts with, if it can follow `h` in one stream (minimp3's
/// `hdr_compare`: same version, layer, sample rate and free-format-ness).
fn next_in_stream(h: &FrameInfo, b: &[u8]) -> Option<FrameInfo> {
    parse_any(b).filter(|n| n.key == h.key)
}

/// Bytes that can follow the last frame of a stream: a tag.
fn is_tag_start(b: &[u8]) -> bool {
    b.starts_with(b"TAG") || b.starts_with(b"APE") || b.starts_with(b"ID3") || b.starts_with(b"LYR")
}

/// Whether the frame `h` of `size` bytes at the start of `buf` is followed by
/// its stream: minimp3's `mp3d_match_frame`. Up to [`MAX_MATCHES`] headers
/// must follow, each of `h`'s stream and each where the frame before it ends;
/// the walk ends early, and well, where the data ends (or a tag or zero
/// padding begins) after at least ONE of them matched -- so a frame whose
/// successor is cut off by the end of the file still counts (#125), and a lone
/// header does not. `buf` must reach the end of the input or hold the whole
/// walk.
fn stream_follows(h: &FrameInfo, size: usize, buf: &[u8]) -> bool {
    // A free-format stream keeps its frame length; only the padding varies.
    let free = h.key.3.then(|| size - h.pad);
    let mut at = size;
    for matched in 0..MAX_MATCHES {
        let rest = buf.get(at..).unwrap_or(&[]);
        if rest.len() < 4 || is_tag_start(rest) || rest[..4] == [0; 4] {
            return matched > 0;
        }
        let Some(next) = next_in_stream(h, rest) else {
            return false;
        };
        at += match free {
            Some(unpadded) => unpadded + next.pad,
            None => next.size,
        };
    }
    true
}

/// The largest free-format frame of `h`'s stream, padding included: the frame
/// at the layer's highest table bitrate (Layer I 448, Layer II 384 kbps; 256
/// and 160 at the MPEG-2 rates). ISO/IEC 11172-3 does not require a decoder to
/// support free-format bitrates above those (FFmpeg supports no free format at
/// all), and the rows of an uncompressed picture otherwise pass for free-format
/// frames of 2 to 3 KB. The same bound as `rusty_mp3`'s
/// `Layer12Header::max_free_format_size`.
fn max_free_size(h: &FrameInfo) -> usize {
    let mpeg1 = h.key.0 == 0b11;
    let top = BITRATE_L12[usize::from(!mpeg1) * 2 + usize::from(h.layer - 1)][14] as usize;
    let rate = (h.sample_rate as usize).max(1);
    if h.layer == 1 {
        (12 * top * 1000 / rate) * 4 + h.pad
    } else {
        144 * top * 1000 / rate + h.pad
    }
}

/// Free-format frame size at the start of `buf`: the distance to the next
/// header of the stream (`None` if none is in reach). As in minimp3, the
/// header after THAT one must be the stream's too -- the first was found by
/// looking for it, so on its own it proves nothing -- or, with `at_end` (the
/// data ends with `buf`), the second frame may end the data exactly. A Layer I
/// frame is a whole number of 4-byte slots.
fn free_size_in(h: &FrameInfo, buf: &[u8], at_end: bool) -> Option<usize> {
    let min = if h.layer == 1 { 24 } else { 48 };
    let max = max_free_size(h).min(MAX_FREE_FRAME);
    (min..=max.min(buf.len().saturating_sub(4))).find(|&i| {
        if buf[i] != 0xFF || (h.layer == 1 && (i - h.pad) % 4 != 0) {
            return false;
        }
        let Some(next) = next_in_stream(h, &buf[i..]) else {
            return false;
        };
        match buf.get(i + (i - h.pad + next.pad)..) {
            Some(rest) if rest.len() >= 4 => {
                is_tag_start(rest) || next_in_stream(h, rest).is_some()
            }
            Some(rest) => rest.is_empty() && at_end,
            None => false,
        }
    })
}

/// Offset of the first frame in `data` that minimp3's sync rule
/// (`mp3d_find_frame`) takes: a valid header whose whole frame is there and
/// whose stream follows it ([`stream_follows`]) -- or, at offset 0 only, a
/// frame that is all of `data` (a one-frame file).
fn find_frame(data: &[u8]) -> Option<usize> {
    let mut i = 0;
    while i + 4 <= data.len() {
        let buf = &data[i..];
        if let Some(h) = parse_any(buf) {
            let size = if h.key.3 {
                free_size_in(&h, buf, true)
            } else {
                Some(h.size)
            };
            if let Some(size) = size.filter(|&s| s > 6 && s <= buf.len()) {
                if (i == 0 && size == buf.len()) || stream_follows(&h, size, buf) {
                    return Some(i);
                }
            }
        }
        i += 1;
    }
    None
}

/// First byte offset of a Layer III header at or after `start` (the legacy
/// whole-buffer walk's sync, kept for the equivalence tests).
#[cfg(test)]
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

/// Sniff MPEG audio by an ID3 tag, or by a frame near the start that its
/// stream vouches for (minimp3's rule, [`find_frame`]): one header on its own
/// -- which a movie, a picture or any file with enough `0xFF` bytes holds
/// somewhere -- is not a stream. `data` is a prefix of the file, or all of it.
fn probe_mp3(data: &[u8]) -> i32 {
    if data.len() >= 3 && &data[0..3] == b"ID3" {
        return 90;
    }
    match find_frame(data) {
        Some(i) if i < 4 => 80, // a frame right at the front
        Some(_) => 40,          // a frame after some junk — weaker
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
/// declared size, without buffering it) and finds the first frame;
/// `read_packet` then walks frame by frame through a small window, so peak
/// memory is O(a few frames), not O(file) (issue #26). Works on pipes.
///
/// **Which bytes are a frame** is minimp3's rule (`mp3dec_decode_frame` /
/// `mp3d_find_frame`), for every layer: a header is not enough, the stream has
/// to vouch for the frame.
///
/// * A frame that continues the stream -- it starts where the frame before it
///   ended, with a header of the same version, layer and sample rate -- is
///   taken when the header right after it continues the stream too.
/// * Anywhere else (the first frame, after damage or junk) a frame is taken
///   only when up to [`MAX_MATCHES`] headers of its stream follow it, each
///   where the frame before ends; the data may end first once one has matched.
/// * The last frame has no header after it: it is taken when it starts where
///   the frame before it ended (or the data began) and ends the data exactly,
///   or a trailing tag follows it. So a file holding exactly one frame demuxes.
///
/// So a frame whose header is damaged -- its length is then wrong, and the next
/// header is not where it says -- is dropped instead of handed to the decoder
/// as noise, together with the frame before it, which nothing vouches for
/// either; and bytes that only look like a frame are not one. Xing/Info frames
/// are dropped, and a trailing ID3v1 tag and an APEv2 tag are skipped rather
/// than scanned.
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
    /// Layer locked from the first frame (0 before `read_header`): a stream
    /// takes only headers of its own layer.
    layer: u8,
    /// `pos` is where the data began or the previous frame ended: no byte has
    /// been skipped to get here.
    contiguous: bool,
    /// The previous frame's stream.
    prev: Option<StreamKey>,
    /// Measured free-format frame size, without padding.
    free_size: Option<usize>,
    /// `read_packet` has reported the end: it keeps doing so.
    done: bool,
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
            layer: 0,
            contiguous: true,
            prev: None,
            free_size: None,
            done: false,
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

    /// Whether the stream vouches for the frame `h` of `size` bytes at `pos`
    /// (see the type's docs). `true` leaves the whole frame in the window.
    fn vouched(&mut self, h: &FrameInfo, size: usize) -> std::io::Result<bool> {
        if size <= 6 {
            return Ok(false);
        }
        // The frame and the header after it.
        let more = self.fill(size + 4)?;
        let buf = &self.buf[self.pos..];
        let Some(rest) = buf.get(size..) else {
            return Ok(false); // cut off by the end of the input
        };
        if self.contiguous {
            // The last frame: nothing can follow it but a tag.
            if (rest.is_empty() && !more) || is_tag_start(rest) {
                return Ok(true);
            }
            // Mid-stream: the next header is enough.
            if self.prev == Some(h.key) && next_in_stream(h, rest).is_some() {
                return Ok(true);
            }
        }
        // Found by searching: the whole walk (or as far as the input goes).
        let longest = if h.key.3 { size + 4 } else { MAX_FRAME };
        self.fill(size + MAX_MATCHES * longest + 4)?;
        Ok(stream_follows(h, size, &self.buf[self.pos..]))
    }

    /// The frame at `pos`, if one the stream vouches for is there: its
    /// geometry, with the free-format size resolved.
    fn frame_here(&mut self) -> std::io::Result<Option<FrameInfo>> {
        let Some(h) = parse_any(&self.buf[self.pos..]) else {
            return Ok(None);
        };
        if self.layer != 0 && h.layer != self.layer {
            return Ok(None);
        }
        if !h.key.3 {
            return Ok(self.vouched(&h, h.size)?.then_some(h));
        }
        // Free format: the measured size while the stream agrees with it, else
        // measure (again) by searching for the next header.
        let cached = self.free_size.map(|s| s + h.pad);
        if let Some(size) = cached {
            if self.vouched(&h, size)? {
                return Ok(Some(FrameInfo { size, ..h }));
            }
        }
        let complete = self.fill(2 * (MAX_FREE_FRAME + 4) + 4)?;
        let measured = free_size_in(&h, &self.buf[self.pos..], !complete);
        if let Some(size) = measured.filter(|&s| Some(s) != cached) {
            if self.vouched(&h, size)? {
                self.free_size = Some(size - h.pad);
                return Ok(Some(FrameInfo { size, ..h }));
            }
        }
        Ok(None)
    }

    /// Advance to the next frame and return its geometry; `None` at end of
    /// input. Every step consumes at least one byte or returns.
    fn next_sync(&mut self) -> std::io::Result<Option<FrameInfo>> {
        loop {
            if !self.fill(4)? {
                return Ok(None);
            }
            if let Some(info) = self.frame_here()? {
                return Ok(Some(info));
            }
            self.contiguous = false;
            let b = &self.buf[self.pos..];
            if b.starts_with(b"TAG") {
                // ID3v1 iff exactly its 128 bytes remain.
                let more = self.fill(ID3V1_LEN + 1)?;
                if !more && self.avail() == ID3V1_LEN {
                    self.pos = self.buf.len();
                    return Ok(None);
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
        let Some(info) = self.next_sync()? else {
            return Err(Error::invalid("mp3 demux: no MPEG audio frame found"));
        };
        self.layer = info.layer;
        let codec = match info.layer {
            1 => CodecId::Mp1,
            2 => CodecId::Mp2,
            _ => CodecId::Mp3,
        };

        let mut stream = Stream::new(0, codec);
        stream.sample_rate = info.sample_rate;
        stream.channels = info.channels;
        stream.sample_format = Some(SampleFormat::F32);
        stream.time_base = Rational::new(1, info.sample_rate.max(1) as i32);
        Ok(vec![stream])
    }

    fn read_packet(&mut self) -> Result<Packet> {
        if self.done {
            return Err(Error::Eof);
        }
        loop {
            let Some(info) = self.next_sync()? else {
                self.done = true;
                return Err(Error::Eof);
            };
            // `next_sync` left the whole frame in the window.
            let frame = &self.buf[self.pos..self.pos + info.size];
            let header_frame = is_header_frame(frame, &info);
            let data = (!header_frame).then(|| frame.to_vec());
            self.pos += info.size;
            self.contiguous = true;
            self.prev = Some(info.key);

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
    fn probe_recognizes_frames_and_id3() {
        let two = cat(&[&make_frame(HDR, 1), &make_frame(HDR, 2)]);
        assert_eq!(probe_mp3(&two), 80);
        // A prefix cut inside the second frame: its header still vouches.
        assert_eq!(probe_mp3(&two[..500]), 80);
        assert_eq!(probe_mp3(&cat(&[&[0u8; 100], &two])), 40);
        assert_eq!(probe_mp3(b"ID3\x04\x00\x00\x00\x00\x00\x00"), 90);
        assert_eq!(probe_mp3(b"RIFFxxxxWAVE"), 0);
        // A header alone is not a stream (it scored 80 before).
        assert_eq!(probe_mp3(&HDR), 0);
        // The 4096-byte head the engine sniffs, of a long stream.
        assert_eq!(probe_mp3(&frames(40)[..4096]), 80);
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
    /// One MPEG-1 44.1 kHz stream whose frames differ in everything a stream
    /// may vary: 128k stereo (+padding), 320k mono with CRC, 64k joint stereo,
    /// 192k dual channel -- a mix of frame sizes.
    const HDRS: [[u8; 4]; 4] = [
        [0xFF, 0xFB, 0x92, 0x00],
        [0xFF, 0xFA, 0xE0, 0xC0],
        [0xFF, 0xFB, 0x50, 0x60],
        [0xFF, 0xFB, 0xB0, 0x80],
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

    // ---- Layer I / II (MP1 / MP2) ----

    /// MPEG-1 Layer II, 192 kbps, 44.1 kHz, stereo, no CRC (626/627 bytes).
    const L2: [u8; 4] = [0xFF, 0xFD, 0xA0, 0x04];
    /// MPEG-1 Layer I, 384 kbps, 48 kHz, mono (384 bytes).
    const L1: [u8; 4] = [0xFF, 0xFF, 0xC4, 0xC0];
    /// MPEG-2 Layer II, 24 kHz, 96 kbps, mono (576 bytes).
    const L2_LSF: [u8; 4] = [0xFF, 0xF5, 0xA4, 0xC0];

    fn l12_frame(hdr: [u8; 4], fill: u8) -> Vec<u8> {
        let size = parse_l12_header(&hdr).unwrap().size;
        let mut f = hdr.to_vec();
        f.resize(size, fill);
        f
    }
    fn l12_frames(hdr: [u8; 4], n: usize) -> Vec<u8> {
        (0..n)
            .flat_map(|i| l12_frame(hdr, 1 + (i % 200) as u8))
            .collect()
    }

    #[test]
    fn demuxes_layer1_and_layer2_with_their_codec_ids() {
        for (hdr, codec, rate, ch, spf, size) in [
            (L2, CodecId::Mp2, 44100, 2, 1152, 626),
            (L1, CodecId::Mp1, 48000, 1, 384, 384),
            (L2_LSF, CodecId::Mp2, 24000, 1, 1152, 576),
        ] {
            let file = cat(&[&id3v2(30), &l12_frames(hdr, 5)]);
            let (head, packets, end) =
                drive(Box::new(Mp3Demuxer::new(Box::new(Cursor::new(file)))));
            assert!(head.contains(&format!("{codec:?}")), "{head}");
            assert!(head.contains(&format!("sample_rate: {rate}")), "{head}");
            assert!(head.contains(&format!("channels: {ch}")), "{head}");
            assert_eq!(packets.len(), 5);
            for (i, p) in packets.iter().enumerate() {
                assert_eq!(p.1, Some((i * spf) as i64));
                assert_eq!(p.6.len(), size);
                assert_eq!(p.6[4], 1 + i as u8);
            }
            assert_eq!(end, "end of stream");
        }
    }

    /// #125 follow-up: one whole frame, then a cut-off second one (the shape
    /// of a short file truncated at 60%). The first frame is confirmed by the
    /// second's header and is delivered; the partial frame is not.
    #[test]
    fn one_whole_frame_before_a_cut_off_one_is_delivered() {
        for hdr in [L2, L1, L2_LSF] {
            let whole = l12_frame(hdr, 7);
            let mut file = whole.clone();
            file.extend_from_slice(&l12_frame(hdr, 8)[..whole.len() * 6 / 10]);
            let (_, packets, end) = drive(Box::new(Mp3Demuxer::new(Box::new(Cursor::new(file)))));
            assert!(!packets.is_empty(), "{hdr:02X?}: {end}");
            assert_eq!(packets[0].6, whole, "{hdr:02X?}");
        }
        // A lone cut-off frame is still nothing to lock onto.
        let cut = l12_frame(L2, 7)[..400].to_vec();
        assert!(Mp3Demuxer::new(Box::new(Cursor::new(cut)))
            .read_header()
            .is_err());
    }

    #[test]
    fn probes_layer1_and_layer2() {
        assert_eq!(probe_mp3(&l12_frames(L2, 4)), 80);
        assert_eq!(probe_mp3(&l12_frames(L1, 4)), 80);
        assert_eq!(probe_mp3(&cat(&[&[0u8; 100], &l12_frames(L2_LSF, 4)])), 40);
        // One lone header is not a stream.
        let mut lone = L2.to_vec();
        lone.resize(3000, 0);
        assert_eq!(probe_mp3(&lone), 0);
        // The Layer I/II extensions fall back to this demuxer, read-only.
        let mut reg = FormatRegistry::new();
        register(&mut reg);
        for ext in ["mp2", "mp1", "mpa", "m2a"] {
            let f = reg.by_extension(ext).unwrap();
            assert_eq!(f.name, "mp2");
            assert!(f.can_demux() && !f.can_mux());
        }
        assert_eq!(reg.by_extension("mp3").unwrap().name, "mp3");
        assert_eq!(reg.probe(&l12_frames(L2, 4)).unwrap().name, "mp3");
    }

    /// Free format: bitrate index 0, each frame's size is the distance to the
    /// next header; the demuxer measures it and emits whole frames.
    #[test]
    fn demuxes_free_format() {
        let mut file = Vec::new();
        for i in 0..6u8 {
            let mut f = vec![0xFF, 0xFD, 0x02 * (i % 2), 0x04]; // free, padding alternates
            f.resize(500 + usize::from(i % 2), 7 + i);
            file.extend_from_slice(&f);
        }
        let (head, packets, _) = drive(Box::new(Mp3Demuxer::new(Box::new(Cursor::new(file)))));
        assert!(head.contains("Mp2"), "{head}");
        assert_eq!(packets.len(), 6);
        for (i, p) in packets.iter().enumerate() {
            assert_eq!(p.6.len(), 500 + i % 2, "frame {i}");
            assert_eq!(p.6[4], 7 + i as u8);
        }
    }

    /// A free-format frame is no longer than the frame at the layer's highest
    /// table bitrate (Layer II at 44.1 kHz: 384 kbps, 1,253 bytes): rows of an
    /// uncompressed picture otherwise pass for free-format frames of 2 to 3 KB.
    #[test]
    fn free_format_frames_beyond_the_top_bitrate_are_not_frames() {
        let stream = |size: usize| {
            let mut file = Vec::new();
            for i in 0..12u8 {
                let mut f = vec![0xFF, 0xFD, 0x00, 0x04];
                f.resize(size, 7 + i);
                file.extend_from_slice(&f);
            }
            drive(Box::new(Mp3Demuxer::new(Box::new(Cursor::new(file)))))
        };
        let (head, packets, _) = stream(1253);
        assert!(head.contains("Mp2"), "{head}");
        assert_eq!(packets.len(), 12);
        let (head, packets, _) = stream(1300);
        assert!(head.starts_with("Err"), "{head}");
        assert!(packets.is_empty());
    }

    /// Sync-heavy garbage around and between Layer II frames: every frame the
    /// stream vouches for comes out, nothing else -- not the garbage, and not
    /// the last frame before each run of garbage, whose next header is not
    /// there (minimp3 drops it too); truncation anywhere never panics.
    #[test]
    fn layer2_garbage_and_truncation() {
        let mut x = 0x9E37_79B9_u32;
        let mut garbage = |n: usize| -> Vec<u8> {
            (0..n)
                .map(|_| {
                    x ^= x << 13;
                    x ^= x >> 17;
                    x ^= x << 5;
                    if x % 3 == 0 {
                        0xFF
                    } else {
                        (x >> 8) as u8
                    }
                })
                .collect()
        };
        let frames = l12_frames(L2, 28);
        let size = 626;
        let file = cat(&[
            &garbage(4000),
            &frames[..14 * size],
            &garbage(3000),
            &frames[14 * size..],
            &garbage(1500),
        ]);
        for chunk in [usize::MAX, 333] {
            let (input, _) = counting(file.clone(), chunk);
            let (_, packets, _) = drive(Box::new(Mp3Demuxer::new(input)));
            let got: Vec<u8> = packets.iter().flat_map(|p| p.6.clone()).collect();
            let want = cat(&[&frames[..13 * size], &frames[14 * size..27 * size]]);
            assert!(got == want, "chunk {chunk}: {} packets", packets.len());
        }
        let short = cat(&[&l12_frames(L1, 3), &l12_frames(L2_LSF, 2)]);
        for cut in 0..short.len() {
            let _ = drive(Box::new(Mp3Demuxer::new(Box::new(Cursor::new(
                short[..cut].to_vec(),
            )))));
        }
    }

    /// A Layer III stream ignores Layer I/II-shaped bytes (as it always did),
    /// and a Layer II stream ignores Layer III-shaped ones. The frame right
    /// before the foreign bytes goes with them: its next header is not its
    /// stream's.
    #[test]
    fn the_first_frame_locks_the_layer() {
        let file = cat(&[&frames(12), &l12_frames(L2, 3), &frames(12)]);
        let (head, packets, _) = drive(Box::new(Mp3Demuxer::new(Box::new(Cursor::new(file)))));
        assert!(head.contains("Mp3"));
        assert_eq!(packets.len(), 11 + 12);
        assert!(
            packets.iter().all(|p| p.6[1] & 0x06 == 0x02),
            "Layer III only"
        );
        let file = cat(&[&l12_frames(L2, 12), &frames(3), &l12_frames(L2, 12)]);
        let (head, packets, _) = drive(Box::new(Mp3Demuxer::new(Box::new(Cursor::new(file)))));
        assert!(head.contains("Mp2"));
        assert_eq!(packets.len(), 11 + 12);
    }

    // ---- the stream has to vouch for a frame (minimp3's sync rule) ----

    /// The packets (as their fill bytes) a file demuxes to, the same whatever
    /// the read chunking; `None` when no stream is found.
    fn fills(file: &[u8]) -> Option<Vec<u8>> {
        let mut seen = None;
        for chunk in [usize::MAX, 1, 7, 1000] {
            let (input, _) = counting(file.to_vec(), chunk);
            let (head, packets, _) = drive(Box::new(Mp3Demuxer::new(input)));
            let got = (!head.starts_with("Err")).then(|| packets.iter().map(|p| p.6[10]).collect());
            assert!(seen.is_none() || seen == Some(got.clone()), "chunk {chunk}");
            seen = Some(got);
        }
        seen.flatten()
    }
    /// `n` frames of header `hdr`, filled with 1, 2, 3...
    fn run_of(hdr: [u8; 4], n: usize) -> Vec<Vec<u8>> {
        (1..=n as u8).map(|i| any_frame(hdr, i)).collect()
    }
    fn any_frame(hdr: [u8; 4], fill: u8) -> Vec<u8> {
        let mut f = hdr.to_vec();
        f.resize(parse_any(&hdr).unwrap().size, fill);
        f
    }

    /// Bytes that pass a single-header check -- a valid header in a file that
    /// is not MPEG audio -- are neither sniffed as MP3 nor demuxed. (Both
    /// happened before: any Layer III header was a frame on sight.)
    #[test]
    fn a_lone_header_in_other_bytes_is_not_a_stream() {
        for hdr in [HDR, L2, L1] {
            let mut junk: Vec<u8> = (0..4000u32)
                .map(|i| (i.wrapping_mul(2_654_435_761) >> 24) as u8 & 0x7F)
                .collect();
            junk[1000..1004].copy_from_slice(&hdr);
            assert_eq!(probe_mp3(&junk), 0, "{hdr:02X?}");
            assert_eq!(fills(&junk), None, "{hdr:02X?}");
            // Right at the front, as a movie or a picture may happen to start.
            junk[..4].copy_from_slice(&hdr);
            assert_eq!(probe_mp3(&junk), 0, "{hdr:02X?}");
            assert_eq!(fills(&junk), None, "{hdr:02X?}");
        }
        // Two headers a frame apart, then junk: still one match short of the
        // ten the rule asks for in the middle of a file.
        let mut junk = vec![0x11u8; 20_000];
        junk[100..104].copy_from_slice(&HDR);
        junk[517..521].copy_from_slice(&HDR);
        assert_eq!(fills(&junk), None);
    }

    /// A frame followed by a header of another stream (here: another sample
    /// rate, as one flipped bit makes of a good header) is not taken, and
    /// neither is the frame that header starts: the next frame's header is
    /// not where its (wrong) length says. The stream resumes at the first
    /// frame a run of matching headers confirms. minimp3 drops the same two.
    #[test]
    fn a_frame_before_a_mismatched_header_is_dropped() {
        for hdr in [HDR, L2, L1] {
            let mut f = run_of(hdr, 6);
            f[1][2] ^= 0x04; // frame 2: 44.1 <-> 48 kHz
            assert!(parse_any(&f[1]).is_some(), "still a valid header");
            assert_eq!(fills(&f.concat()), Some(vec![3, 4, 5, 6]), "{hdr:02X?}");
            // Once the stream is established (ten headers followed the first
            // frame), damage costs those two frames and no more.
            let kept: Vec<u8> = (1..=16).filter(|i| ![12, 13].contains(i)).collect();
            let mut f = run_of(hdr, 16);
            f[12][2] ^= 0x04;
            assert_eq!(fills(&f.concat()), Some(kept.clone()), "{hdr:02X?}");
            // The sync word itself gone.
            let mut f = run_of(hdr, 16);
            f[12][0] = 0;
            assert_eq!(fills(&f.concat()), Some(kept), "{hdr:02X?}");
            // Damage among the FIRST frames: no run of ten vouches for the
            // frames ahead of it, so the stream starts after it.
            let mut f = run_of(hdr, 8);
            f[5][2] ^= 0x04;
            assert_eq!(fills(&f.concat()), Some(vec![7, 8]), "{hdr:02X?}");
        }
        // A header damaged in its BITRATE is still of the stream, so the frame
        // before it stays; the damaged frame's length is wrong, so it goes.
        let mut f = run_of(HDR, 16);
        f[12][2] ^= 0x40;
        let kept: Vec<u8> = (1..=16).filter(|&i| i != 13).collect();
        assert_eq!(fills(&f.concat()), Some(kept));
    }

    /// The last frame of a file has no header after it. It is taken when it
    /// ends the data exactly, or a tag follows it -- which keeps the one-frame
    /// file (#125) -- and not when stray bytes follow it (as minimp3).
    #[test]
    fn the_last_frame_and_the_one_frame_file() {
        let mut id3v1 = b"TAG".to_vec();
        id3v1.resize(ID3V1_LEN, b' ');
        for hdr in [HDR, L2, L1, L2_LSF] {
            let one = any_frame(hdr, 1);
            assert_eq!(fills(&one), Some(vec![1]), "{hdr:02X?}: one frame");
            assert_eq!(probe_mp3(&one), 80, "{hdr:02X?}: one frame sniffs");
            assert_eq!(fills(&cat(&[&id3v2(40), &one])), Some(vec![1]));
            assert_eq!(fills(&cat(&[&one, &id3v1])), Some(vec![1]));
            assert_eq!(fills(&cat(&[&one, &[0u8; 3]])), None, "{hdr:02X?}");
            let f = run_of(hdr, 3).concat();
            assert_eq!(fills(&f), Some(vec![1, 2, 3]));
            assert_eq!(fills(&cat(&[&f, &id3v1])), Some(vec![1, 2, 3]));
            assert_eq!(fills(&cat(&[&f, &[0u8; 2]])), Some(vec![1, 2]));
            // Zero padding ends a stream like the end of the data does (the
            // frames before the last are vouched for); other bytes there say
            // the run of headers before them was not a stream.
            assert_eq!(fills(&cat(&[&f, &[0u8; 64]])), Some(vec![1, 2]));
            assert_eq!(fills(&cat(&[&f, &[0x55u8; 64]])), None, "{hdr:02X?}");
            // A frame cut short confirms the one before it and is not a frame.
            assert_eq!(fills(&f[..f.len() - 9]), Some(vec![1, 2]));
            // One frame after junk is a lone header again.
            assert_eq!(fills(&cat(&[&[0x22u8; 50], &one])), None, "{hdr:02X?}");
        }
    }

    /// Two streams end to end: the last frame of the first is not followed by
    /// its own stream and goes; the second stream is found by its own run.
    #[test]
    fn a_change_of_stream_costs_one_frame() {
        let a = run_of(L2, 14).concat(); // 44.1 kHz
        let b = run_of([0xFF, 0xFD, 0xA4, 0x04], 4).concat(); // 48 kHz
        let want: Vec<u8> = (1..=13).chain(1..=4).collect();
        assert_eq!(fills(&cat(&[&a, &b])), Some(want));
    }

    /// A free-format Layer I frame is a whole number of 4-byte slots, and its
    /// size is only believed when the header after the next one agrees.
    #[test]
    fn free_format_sizes_are_checked() {
        let free_l1 = [0xFF, 0xFF, 0x00, 0xC0];
        let h = parse_any(&free_l1).unwrap();
        let build = |size: usize, n: usize| -> Vec<u8> {
            let mut f = Vec::new();
            for i in 0..n {
                f.extend_from_slice(&free_l1);
                f.resize((i + 1) * size, 3);
            }
            f
        };
        assert_eq!(free_size_in(&h, &build(100, 3), false), Some(100));
        assert_eq!(
            free_size_in(&h, &build(102, 3), false),
            None,
            "not whole slots"
        );
        // Two frames and the end of the data; two frames and junk.
        assert_eq!(free_size_in(&h, &build(100, 2), true), Some(100));
        assert_eq!(free_size_in(&h, &build(100, 2), false), None);
        assert_eq!(
            free_size_in(&h, &cat(&[&build(100, 2), &[9u8; 50]]), true),
            None
        );
        assert_eq!(fills(&build(100, 4)), Some(vec![3, 3, 3, 3]));
    }
}
