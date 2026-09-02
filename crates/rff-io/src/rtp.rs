//! RTP input (`rtp://`): RFC 3550 framing carrying the two video payload
//! formats a device sends — **RFC 6184 H.264** and **RFC 2435 JPEG**.
//!
//! `rff -i rtp://@:5004 out.mp4` binds the port, learns the payload type from
//! the first packet, and reassembles access units (H.264: single NAL, STAP-A,
//! FU-A) or JPEG frames (JPEG: quantisation tables in-band or from the RFC's
//! `Q` tables, the Annex K Huffman tables regenerated, restart intervals kept).
//!
//! What comes out of the [`Read`] is **not** a bare elementary stream. RTP
//! carries a 90 kHz timestamp per frame and a raw stream would lose it, so the
//! reader emits one [`FRAME_MAGIC`]-tagged record per frame — codec, flags,
//! timestamp, length, bytes (see [`write_frame`]) — and the `rff-format-rtp`
//! demuxer turns those into timed packets. [`RtpReader::format_name`] names
//! that format so the engine opens the right demuxer without sniffing.
//!
//! Payload types: 26 is JPEG by the static table (RFC 3551); anything dynamic
//! (96..=127) is taken as H.264, which is what every SDP-less sender in this
//! family uses; `?pt=N` in the URL pins it. Loss is **reported, not guessed**:
//! a sequence gap inside a frame drops that frame and the next frame start
//! resynchronises (`RtpReader::stats`).
//!
//! The receiving halves mirror `rusty_esp_video`'s payloaders (the device
//! side), and ffmpeg's `-f rtp` output is the external oracle — see
//! `tests/rtp_ffmpeg.rs`.

use std::io::Read;
use std::net::UdpSocket;
use std::time::Duration;

use rff_core::{Error, Result};

/// Fixed RTP header length (no CSRCs, no extension).
pub const HEADER_LEN: usize = 12;
/// The static payload type for JPEG (RFC 3551).
pub const PT_JPEG: u8 = 26;

/// Tag that starts every frame record the reader emits.
pub const FRAME_MAGIC: [u8; 4] = *b"RFF1";
/// Record codec byte: an H.264 access unit in Annex-B form.
pub const CODEC_H264: u8 = 0;
/// Record codec byte: one complete baseline JPEG (JFIF-less, DQT/SOF0/DHT/SOS).
pub const CODEC_JPEG: u8 = 1;
/// Record flag: the frame is a random-access point.
pub const FLAG_KEYFRAME: u8 = 1;
/// Bytes in a record header: magic, codec, flags, timestamp (u32 BE), length (u32 BE).
pub const FRAME_HEADER_LEN: usize = 4 + 1 + 1 + 4 + 4;

/// Append one frame record to `out`.
pub fn write_frame(out: &mut Vec<u8>, codec: u8, keyframe: bool, timestamp: u32, data: &[u8]) {
    out.extend_from_slice(&FRAME_MAGIC);
    out.push(codec);
    out.push(if keyframe { FLAG_KEYFRAME } else { 0 });
    out.extend_from_slice(&timestamp.to_be_bytes());
    out.extend_from_slice(&(data.len() as u32).to_be_bytes());
    out.extend_from_slice(data);
}

/// A parsed record header, from [`parse_frame_header`].
#[derive(Debug, Clone, Copy, PartialEq, Eq)]
pub struct FrameHeader {
    /// [`CODEC_H264`] or [`CODEC_JPEG`].
    pub codec: u8,
    /// [`FLAG_KEYFRAME`] and friends.
    pub flags: u8,
    /// RTP timestamp, 90 kHz.
    pub timestamp: u32,
    /// Payload length in bytes.
    pub len: usize,
}

/// Parse a [`FRAME_HEADER_LEN`]-byte record header.
pub fn parse_frame_header(head: &[u8; FRAME_HEADER_LEN]) -> Result<FrameHeader> {
    if head[..4] != FRAME_MAGIC {
        return Err(Error::invalid("rtp frame record: bad magic"));
    }
    Ok(FrameHeader {
        codec: head[4],
        flags: head[5],
        timestamp: u32::from_be_bytes([head[6], head[7], head[8], head[9]]),
        len: u32::from_be_bytes([head[10], head[11], head[12], head[13]]) as usize,
    })
}

/// Largest datagram we accept.
const MAX_DATAGRAM: usize = 65_536;
/// Default idle timeout before the stream reports EOF.
const DEFAULT_IDLE: Duration = Duration::from_secs(10);
/// Largest frame we will assemble (a 4K JPEG at quality 100 is ~4 MB).
const MAX_FRAME: usize = 32 << 20;

/// A parsed RTP fixed header.
#[derive(Debug, Clone, Copy, PartialEq, Eq)]
pub struct RtpHeader {
    /// Marker bit: last packet of a frame.
    pub marker: bool,
    /// Payload type.
    pub payload_type: u8,
    /// Sequence number.
    pub seq: u16,
    /// Timestamp (90 kHz for video).
    pub timestamp: u32,
    /// Synchronisation source.
    pub ssrc: u32,
}

impl RtpHeader {
    /// Parse a packet; returns the header and the payload (CSRCs, a header
    /// extension and trailing padding stripped). `None` if it is not RTP v2.
    pub fn parse(packet: &[u8]) -> Option<(RtpHeader, &[u8])> {
        if packet.len() < HEADER_LEN || packet[0] >> 6 != 2 {
            return None;
        }
        let padding = packet[0] & 0x20 != 0;
        let ext = packet[0] & 0x10 != 0;
        let cc = (packet[0] & 0x0F) as usize;
        let mut off = HEADER_LEN + 4 * cc;
        if ext {
            let words = u16::from_be_bytes([*packet.get(off + 2)?, *packet.get(off + 3)?]) as usize;
            off += 4 + 4 * words;
        }
        let mut end = packet.len();
        if padding {
            let pad = *packet.last()? as usize;
            if pad == 0 || pad > end - off {
                return None;
            }
            end -= pad;
        }
        if off > end {
            return None;
        }
        Some((
            RtpHeader {
                marker: packet[1] & 0x80 != 0,
                payload_type: packet[1] & 0x7F,
                seq: u16::from_be_bytes([packet[2], packet[3]]),
                timestamp: u32::from_be_bytes([packet[4], packet[5], packet[6], packet[7]]),
                ssrc: u32::from_be_bytes([packet[8], packet[9], packet[10], packet[11]]),
            },
            &packet[off..end],
        ))
    }
}

/// What the reader saw, for a caller that wants to report loss.
#[derive(Debug, Clone, Copy, Default, PartialEq, Eq)]
pub struct RtpStats {
    /// RTP packets accepted.
    pub packets: u64,
    /// Packets missing by sequence number.
    pub lost: u64,
    /// Frames delivered.
    pub frames: u64,
    /// Frames abandoned because a piece never arrived.
    pub dropped: u64,
    /// Packets of an unsupported payload shape (STAP-B, MTAP, FU-B, JPEG type > 1).
    pub unsupported: u64,
}

/// Sequence-number bookkeeping shared by both depayloaders. Returns the number
/// of packets missing before `seq` (0 for in-order, reordered or duplicate).
fn seq_gap(last: &mut Option<u16>, seq: u16) -> u16 {
    let gap = match *last {
        Some(prev) => {
            let g = seq.wrapping_sub(prev).wrapping_sub(1);
            // A huge "gap" is a reordered or duplicated packet, not a loss.
            if g >= 0x8000 {
                0
            } else {
                g
            }
        }
        None => 0,
    };
    *last = Some(seq);
    gap
}

// ---------------------------------------------------------------------------
// RFC 6184 — H.264
// ---------------------------------------------------------------------------

const NAL_STAP_A: u8 = 24;
const NAL_FU_A: u8 = 28;
const START_CODE: [u8; 4] = [0, 0, 0, 1];

/// Reassembles Annex-B access units from RFC 6184 packets.
#[derive(Debug, Default)]
struct H264Depayloader {
    /// The access unit being assembled, Annex-B.
    au: Vec<u8>,
    /// Timestamp of `au`.
    au_ts: u32,
    /// True once `au` has (or had) a packet for this timestamp.
    have_au: bool,
    /// An FU-A in progress: the reconstructed NAL header plus the fragments so far.
    frag: Vec<u8>,
    frag_ok: bool,
    last_seq: Option<u16>,
}

impl H264Depayloader {
    /// Emit the pending access unit (if any) as a frame record.
    fn flush(&mut self, out: &mut Vec<u8>, stats: &mut RtpStats) {
        if self.have_au && !self.au.is_empty() {
            let keyframe = annexb_has_idr(&self.au);
            write_frame(out, CODEC_H264, keyframe, self.au_ts, &self.au);
            stats.frames += 1;
        }
        self.au.clear();
        self.have_au = false;
    }

    fn push(&mut self, h: &RtpHeader, payload: &[u8], out: &mut Vec<u8>, stats: &mut RtpStats) {
        let gap = seq_gap(&mut self.last_seq, h.seq);
        if gap != 0 {
            stats.lost += u64::from(gap);
            if self.frag_ok {
                // The fragment can never complete; the rest of this AU still can.
                self.frag.clear();
                self.frag_ok = false;
                stats.dropped += 1;
            }
        }
        // A new timestamp without a marker on the previous packet: the sender
        // dropped the marker (or the packet was lost); close the old AU anyway.
        if self.have_au && h.timestamp != self.au_ts {
            self.flush(out, stats);
        }
        let Some(&indicator) = payload.first() else {
            return;
        };
        if !self.have_au {
            self.have_au = true;
            self.au_ts = h.timestamp;
        }
        match indicator & 0x1F {
            1..=23 => {
                self.au.extend_from_slice(&START_CODE);
                self.au.extend_from_slice(payload);
            }
            NAL_STAP_A => {
                let mut p = 1;
                while p + 2 <= payload.len() {
                    let n = u16::from_be_bytes([payload[p], payload[p + 1]]) as usize;
                    p += 2;
                    let Some(nal) = payload.get(p..p + n) else {
                        break;
                    };
                    if !nal.is_empty() {
                        self.au.extend_from_slice(&START_CODE);
                        self.au.extend_from_slice(nal);
                    }
                    p += n;
                }
            }
            NAL_FU_A => {
                let Some(&fu) = payload.get(1) else {
                    return;
                };
                let (start, end) = (fu & 0x80 != 0, fu & 0x40 != 0);
                if start {
                    self.frag.clear();
                    self.frag.push((indicator & 0xE0) | (fu & 0x1F));
                    self.frag_ok = true;
                }
                if self.frag_ok {
                    self.frag.extend_from_slice(&payload[2..]);
                    if end {
                        self.au.extend_from_slice(&START_CODE);
                        self.au.extend_from_slice(&self.frag);
                        self.frag.clear();
                        self.frag_ok = false;
                    }
                }
            }
            _ => stats.unsupported += 1,
        }
        if self.au.len() > MAX_FRAME {
            self.au.clear();
            self.have_au = false;
            stats.dropped += 1;
        }
        if h.marker {
            self.flush(out, stats);
        }
    }
}

/// Does this Annex-B access unit carry an IDR slice (`nal_unit_type == 5`)?
pub fn annexb_has_idr(au: &[u8]) -> bool {
    let mut i = 0;
    while i + 3 < au.len() {
        if au[i] == 0 && au[i + 1] == 0 && au[i + 2] == 1 {
            if au[i + 3] & 0x1F == 5 {
                return true;
            }
            i += 3;
        } else {
            i += 1;
        }
    }
    false
}

// ---------------------------------------------------------------------------
// RFC 2435 — JPEG
// ---------------------------------------------------------------------------

/// RFC 2435 Appendix B: the ITU-T T.81 Annex K Huffman tables. A JPEG over
/// RTP carries no DHT; the receiver writes these, so the sender's scan must
/// have been coded with them (cameras and `rusty_jpeg`'s default are).
mod huffman {
    pub struct Table {
        /// DHT class (0 = DC, 1 = AC) and destination id, as the `Tc|Th` byte.
        pub class_id: u8,
        pub codelens: [u8; 16],
        pub symbols: &'static [u8],
    }

    pub const LUM_DC: Table = Table {
        class_id: 0x00,
        codelens: [0, 1, 5, 1, 1, 1, 1, 1, 1, 0, 0, 0, 0, 0, 0, 0],
        symbols: &[0, 1, 2, 3, 4, 5, 6, 7, 8, 9, 10, 11],
    };
    pub const LUM_AC: Table = Table {
        class_id: 0x10,
        codelens: [0, 2, 1, 3, 3, 2, 4, 3, 5, 5, 4, 4, 0, 0, 1, 0x7d],
        symbols: &[
            0x01, 0x02, 0x03, 0x00, 0x04, 0x11, 0x05, 0x12, 0x21, 0x31, 0x41, 0x06, 0x13, 0x51,
            0x61, 0x07, 0x22, 0x71, 0x14, 0x32, 0x81, 0x91, 0xa1, 0x08, 0x23, 0x42, 0xb1, 0xc1,
            0x15, 0x52, 0xd1, 0xf0, 0x24, 0x33, 0x62, 0x72, 0x82, 0x09, 0x0a, 0x16, 0x17, 0x18,
            0x19, 0x1a, 0x25, 0x26, 0x27, 0x28, 0x29, 0x2a, 0x34, 0x35, 0x36, 0x37, 0x38, 0x39,
            0x3a, 0x43, 0x44, 0x45, 0x46, 0x47, 0x48, 0x49, 0x4a, 0x53, 0x54, 0x55, 0x56, 0x57,
            0x58, 0x59, 0x5a, 0x63, 0x64, 0x65, 0x66, 0x67, 0x68, 0x69, 0x6a, 0x73, 0x74, 0x75,
            0x76, 0x77, 0x78, 0x79, 0x7a, 0x83, 0x84, 0x85, 0x86, 0x87, 0x88, 0x89, 0x8a, 0x92,
            0x93, 0x94, 0x95, 0x96, 0x97, 0x98, 0x99, 0x9a, 0xa2, 0xa3, 0xa4, 0xa5, 0xa6, 0xa7,
            0xa8, 0xa9, 0xaa, 0xb2, 0xb3, 0xb4, 0xb5, 0xb6, 0xb7, 0xb8, 0xb9, 0xba, 0xc2, 0xc3,
            0xc4, 0xc5, 0xc6, 0xc7, 0xc8, 0xc9, 0xca, 0xd2, 0xd3, 0xd4, 0xd5, 0xd6, 0xd7, 0xd8,
            0xd9, 0xda, 0xe1, 0xe2, 0xe3, 0xe4, 0xe5, 0xe6, 0xe7, 0xe8, 0xe9, 0xea, 0xf1, 0xf2,
            0xf3, 0xf4, 0xf5, 0xf6, 0xf7, 0xf8, 0xf9, 0xfa,
        ],
    };
    pub const CHM_DC: Table = Table {
        class_id: 0x01,
        codelens: [0, 3, 1, 1, 1, 1, 1, 1, 1, 1, 1, 0, 0, 0, 0, 0],
        symbols: &[0, 1, 2, 3, 4, 5, 6, 7, 8, 9, 10, 11],
    };
    pub const CHM_AC: Table = Table {
        class_id: 0x11,
        codelens: [0, 2, 1, 2, 4, 4, 3, 4, 7, 5, 4, 4, 0, 1, 2, 0x77],
        symbols: &[
            0x00, 0x01, 0x02, 0x03, 0x11, 0x04, 0x05, 0x21, 0x31, 0x06, 0x12, 0x41, 0x51, 0x07,
            0x61, 0x71, 0x13, 0x22, 0x32, 0x81, 0x08, 0x14, 0x42, 0x91, 0xa1, 0xb1, 0xc1, 0x09,
            0x23, 0x33, 0x52, 0xf0, 0x15, 0x62, 0x72, 0xd1, 0x0a, 0x16, 0x24, 0x34, 0xe1, 0x25,
            0xf1, 0x17, 0x18, 0x19, 0x1a, 0x26, 0x27, 0x28, 0x29, 0x2a, 0x35, 0x36, 0x37, 0x38,
            0x39, 0x3a, 0x43, 0x44, 0x45, 0x46, 0x47, 0x48, 0x49, 0x4a, 0x53, 0x54, 0x55, 0x56,
            0x57, 0x58, 0x59, 0x5a, 0x63, 0x64, 0x65, 0x66, 0x67, 0x68, 0x69, 0x6a, 0x73, 0x74,
            0x75, 0x76, 0x77, 0x78, 0x79, 0x7a, 0x82, 0x83, 0x84, 0x85, 0x86, 0x87, 0x88, 0x89,
            0x8a, 0x92, 0x93, 0x94, 0x95, 0x96, 0x97, 0x98, 0x99, 0x9a, 0xa2, 0xa3, 0xa4, 0xa5,
            0xa6, 0xa7, 0xa8, 0xa9, 0xaa, 0xb2, 0xb3, 0xb4, 0xb5, 0xb6, 0xb7, 0xb8, 0xb9, 0xba,
            0xc2, 0xc3, 0xc4, 0xc5, 0xc6, 0xc7, 0xc8, 0xc9, 0xca, 0xd2, 0xd3, 0xd4, 0xd5, 0xd6,
            0xd7, 0xd8, 0xd9, 0xda, 0xe2, 0xe3, 0xe4, 0xe5, 0xe6, 0xe7, 0xe8, 0xe9, 0xea, 0xf2,
            0xf3, 0xf4, 0xf5, 0xf6, 0xf7, 0xf8, 0xf9, 0xfa,
        ],
    };
    pub const ALL: [Table; 4] = [LUM_DC, LUM_AC, CHM_DC, CHM_AC];
}

/// Zigzag scan order: entry `i` is the natural (row-major) index of the
/// `i`-th coefficient in scan order. A DQT lists its 64 values in this order.
const ZIGZAG: [u8; 64] = [
    0, 1, 8, 16, 9, 2, 3, 10, 17, 24, 32, 25, 18, 11, 4, 5, 12, 19, 26, 33, 40, 48, 41, 34, 27, 20,
    13, 6, 7, 14, 21, 28, 35, 42, 49, 56, 57, 50, 43, 36, 29, 22, 15, 23, 30, 37, 44, 51, 58, 59,
    52, 45, 38, 31, 39, 46, 53, 60, 61, 54, 47, 55, 62, 63,
];

/// T.81 Table K.1, natural order (RFC 2435 Appendix A lists it this way).
const LUMA_QUANT: [u8; 64] = [
    16, 11, 10, 16, 24, 40, 51, 61, 12, 12, 14, 19, 26, 58, 60, 55, 14, 13, 16, 24, 40, 57, 69, 56,
    14, 17, 22, 29, 51, 87, 80, 62, 18, 22, 37, 56, 68, 109, 103, 77, 24, 35, 55, 64, 81, 104, 113,
    92, 49, 64, 78, 87, 103, 121, 120, 101, 72, 92, 95, 98, 112, 100, 103, 99,
];
/// T.81 Table K.2, natural order.
const CHROMA_QUANT: [u8; 64] = [
    17, 18, 24, 47, 99, 99, 99, 99, 18, 21, 26, 66, 99, 99, 99, 99, 24, 26, 56, 99, 99, 99, 99, 99,
    47, 66, 99, 99, 99, 99, 99, 99, 99, 99, 99, 99, 99, 99, 99, 99, 99, 99, 99, 99, 99, 99, 99, 99,
    99, 99, 99, 99, 99, 99, 99, 99, 99, 99, 99, 99, 99, 99, 99, 99,
];

/// RFC 2435 Appendix A: the quantisation tables a `Q` in `1..=127` stands
/// for, in the zigzag order a DQT uses (the RFC lists them in natural order;
/// the receivers that matter, ffmpeg among them, apply the zigzag).
pub fn default_quant_tables(q: u8) -> [[u8; 64]; 2] {
    let q = u32::from(q.clamp(1, 99));
    let factor = if q < 50 { 5000 / q } else { 200 - q * 2 };
    let scale = |base: &[u8; 64]| {
        let mut out = [0u8; 64];
        for (i, slot) in out.iter_mut().enumerate() {
            let v = (u32::from(base[usize::from(ZIGZAG[i])]) * factor + 50) / 100;
            *slot = v.clamp(1, 255) as u8;
        }
        out
    };
    [scale(&LUMA_QUANT), scale(&CHROMA_QUANT)]
}

/// Write everything a baseline JPEG needs before its scan: SOI, DQT, SOF0,
/// the four Annex K DHTs, DRI when `restart_interval` is set, and SOS. The
/// scan bytes then follow, and `FF D9` ends the file. `type_` is the RFC 2435
/// type without the restart bit (0 = 4:2:2, 1 = 4:2:0).
fn write_jpeg_headers(
    out: &mut Vec<u8>,
    width: u16,
    height: u16,
    type_: u8,
    restart_interval: Option<u16>,
    qtables: &[[u8; 64]],
) {
    out.extend_from_slice(&[0xFF, 0xD8]);
    // DQT: one segment, all tables, 8-bit precision.
    out.extend_from_slice(&[0xFF, 0xDB]);
    out.extend_from_slice(&((2 + 65 * qtables.len()) as u16).to_be_bytes());
    for (id, t) in qtables.iter().enumerate() {
        out.push(id as u8);
        out.extend_from_slice(t);
    }
    // SOF0: 8-bit, three components; chroma on table 1 when there is one.
    let hv = if type_ == 0 { 0x21 } else { 0x22 };
    let ctab = u8::from(qtables.len() > 1);
    out.extend_from_slice(&[0xFF, 0xC0, 0, 17, 8]);
    out.extend_from_slice(&height.to_be_bytes());
    out.extend_from_slice(&width.to_be_bytes());
    out.extend_from_slice(&[3, 1, hv, 0, 2, 0x11, ctab, 3, 0x11, ctab]);
    // DHT x4
    for t in &huffman::ALL {
        out.extend_from_slice(&[0xFF, 0xC4]);
        out.extend_from_slice(&((2 + 1 + 16 + t.symbols.len()) as u16).to_be_bytes());
        out.push(t.class_id);
        out.extend_from_slice(&t.codelens);
        out.extend_from_slice(t.symbols);
    }
    if let Some(ri) = restart_interval {
        out.extend_from_slice(&[0xFF, 0xDD, 0, 4]);
        out.extend_from_slice(&ri.to_be_bytes());
    }
    // SOS
    out.extend_from_slice(&[0xFF, 0xDA, 0, 12, 3, 1, 0x00, 2, 0x11, 3, 0x11, 0, 63, 0]);
}

/// Rebuilds one JPEG at a time from RFC 2435 packets.
#[derive(Debug)]
struct JpegDepayloader {
    scan: Vec<u8>,
    in_frame: bool,
    ts: u32,
    width: u16,
    height: u16,
    type_: u8,
    restart_interval: Option<u16>,
    qtables: [[u8; 64]; 2],
    qcount: usize,
    last_seq: Option<u16>,
    jpeg: Vec<u8>,
}

impl Default for JpegDepayloader {
    fn default() -> Self {
        JpegDepayloader {
            scan: Vec::new(),
            in_frame: false,
            ts: 0,
            width: 0,
            height: 0,
            type_: 0,
            restart_interval: None,
            qtables: [[0; 64]; 2],
            qcount: 0,
            last_seq: None,
            jpeg: Vec::new(),
        }
    }
}

impl JpegDepayloader {
    fn push(&mut self, h: &RtpHeader, payload: &[u8], out: &mut Vec<u8>, stats: &mut RtpStats) {
        let gap = seq_gap(&mut self.last_seq, h.seq);
        if gap != 0 {
            stats.lost += u64::from(gap);
        }
        if payload.len() < 8 {
            return;
        }
        let off = (usize::from(payload[1]) << 16)
            | (usize::from(payload[2]) << 8)
            | usize::from(payload[3]);
        let type_ = payload[4];
        let q = payload[5];
        let width = u16::from(payload[6]) * 8;
        let height = u16::from(payload[7]) * 8;
        if width == 0 || height == 0 || (type_ & 0x3F) > 1 {
            stats.unsupported += 1;
            return;
        }
        let mut p = 8usize;
        let mut restart = None;
        if type_ >= 64 {
            let Some(r) = payload.get(p..p + 4) else {
                return;
            };
            restart = Some(u16::from_be_bytes([r[0], r[1]]));
            p += 4;
        }
        if off == 0 {
            if self.in_frame && !self.scan.is_empty() {
                stats.dropped += 1;
            }
            self.in_frame = true;
            self.scan.clear();
            self.ts = h.timestamp;
            self.width = width;
            self.height = height;
            self.type_ = type_ & 0x3F;
            self.restart_interval = restart;
            if q >= 128 {
                let Some(qh) = payload.get(p..p + 4) else {
                    self.in_frame = false;
                    return;
                };
                let precision = qh[1];
                let len = usize::from(u16::from_be_bytes([qh[2], qh[3]]));
                p += 4;
                match len {
                    // Q = 255 with no tables: the previous frame's apply.
                    0 => {
                        if self.qcount == 0 {
                            self.in_frame = false;
                            return;
                        }
                    }
                    64 | 128 if precision == 0 => {
                        let Some(tables) = payload.get(p..p + len) else {
                            self.in_frame = false;
                            return;
                        };
                        self.qcount = len / 64;
                        for (t, chunk) in self.qtables.iter_mut().zip(tables.chunks(64)) {
                            t.copy_from_slice(chunk);
                        }
                        p += len;
                    }
                    _ => {
                        // 16-bit tables or an odd length.
                        stats.unsupported += 1;
                        self.in_frame = false;
                        return;
                    }
                }
            } else if q == 0 {
                self.in_frame = false;
                return;
            } else {
                self.qtables = default_quant_tables(q);
                self.qcount = 2;
            }
        } else {
            if !self.in_frame {
                // Joined mid-frame; wait for the next frame start.
                return;
            }
            if gap != 0 || off != self.scan.len() || h.timestamp != self.ts {
                self.in_frame = false;
                stats.dropped += 1;
                return;
            }
        }
        let data = &payload[p..];
        if self.scan.len() + data.len() > MAX_FRAME {
            self.in_frame = false;
            stats.dropped += 1;
            return;
        }
        self.scan.extend_from_slice(data);
        if !h.marker {
            return;
        }
        self.jpeg.clear();
        write_jpeg_headers(
            &mut self.jpeg,
            self.width,
            self.height,
            self.type_,
            self.restart_interval,
            &self.qtables[..self.qcount],
        );
        self.jpeg.extend_from_slice(&self.scan);
        self.jpeg.extend_from_slice(&[0xFF, 0xD9]);
        self.in_frame = false;
        stats.frames += 1;
        write_frame(out, CODEC_JPEG, true, self.ts, &self.jpeg);
    }
}

// ---------------------------------------------------------------------------
// The reader
// ---------------------------------------------------------------------------

enum Depayloader {
    H264(H264Depayloader),
    Jpeg(JpegDepayloader),
}

/// Receives RTP over UDP and serves reassembled frames as [`write_frame`]
/// records — a byte stream the `rff-format-rtp` demuxer reads.
pub struct RtpReader {
    socket: UdpSocket,
    depay: Depayloader,
    payload_type: u8,
    /// Records not yet handed to the caller.
    out: Vec<u8>,
    pos: usize,
    eof: bool,
    dgram: Vec<u8>,
    /// What the reader has seen so far.
    pub stats: RtpStats,
}

/// `rtp://[@]host:port[?timeout=SECONDS][&pt=N]` → (address, idle timeout, pinned PT).
fn parse_rtp_url(path: &str) -> Result<(&str, Duration, Option<u8>)> {
    let rest = path
        .strip_prefix("rtp://")
        .ok_or_else(|| Error::invalid(format!("not an rtp:// URL: {path}")))?;
    let (addr, query) = rest.split_once('?').unwrap_or((rest, ""));
    let addr = addr.split('/').next().unwrap_or("");
    let addr = addr.strip_prefix('@').unwrap_or(addr);
    if addr.is_empty() {
        return Err(Error::invalid("rtp:// needs host:port"));
    }
    let mut timeout = DEFAULT_IDLE;
    let mut pt = None;
    for kv in query.split('&').filter(|s| !s.is_empty()) {
        match kv.split_once('=') {
            Some(("timeout", v)) => {
                timeout = v
                    .parse::<f64>()
                    .ok()
                    .filter(|s| *s > 0.0)
                    .map(Duration::from_secs_f64)
                    .ok_or_else(|| Error::invalid(format!("rtp: bad timeout `{v}`")))?;
            }
            Some(("pt", v)) => {
                pt = Some(
                    v.parse::<u8>()
                        .ok()
                        .filter(|p| *p < 128)
                        .ok_or_else(|| Error::invalid(format!("rtp: bad payload type `{v}`")))?,
                );
            }
            _ => return Err(Error::invalid(format!("rtp: unknown option `{kv}`"))),
        }
    }
    Ok((addr, timeout, pt))
}

impl RtpReader {
    /// Bind `rtp://[@]host:port[?timeout=SECONDS][&pt=N]` and wait for the
    /// first packet, which decides the payload format. Silence for the idle
    /// timeout (default 10 s) before that first packet is an error; after it,
    /// silence is end of stream.
    pub fn bind(path: &str) -> Result<RtpReader> {
        let (addr, timeout, pt) = parse_rtp_url(path)?;
        let socket = crate::udp_bind(addr)?;
        Self::with_socket(socket, pt, timeout)
    }

    /// The same over a socket the caller bound (tests use an ephemeral port).
    pub fn with_socket(socket: UdpSocket, pt: Option<u8>, timeout: Duration) -> Result<RtpReader> {
        socket.set_read_timeout(Some(timeout))?;
        let mut dgram = vec![0u8; MAX_DATAGRAM];
        // Skip anything that is not RTP (a stray probe, RTCP on the wrong port).
        let (header, first_len) = loop {
            let n = match socket.recv(&mut dgram) {
                Ok(n) => n,
                Err(e)
                    if matches!(
                        e.kind(),
                        std::io::ErrorKind::WouldBlock | std::io::ErrorKind::TimedOut
                    ) =>
                {
                    return Err(Error::invalid(format!(
                        "rtp: no RTP packet within {:.1}s",
                        timeout.as_secs_f64()
                    )));
                }
                Err(e) => return Err(e.into()),
            };
            if let Some((h, _)) = RtpHeader::parse(&dgram[..n]) {
                break (h, n);
            }
        };
        let payload_type = pt.unwrap_or(header.payload_type);
        let depay = if payload_type == PT_JPEG {
            Depayloader::Jpeg(JpegDepayloader::default())
        } else if (96..=127).contains(&payload_type) {
            Depayloader::H264(H264Depayloader::default())
        } else {
            return Err(Error::unsupported(format!(
                "rtp: payload type {payload_type} (only JPEG (26) and dynamic H.264 (96..127); pass ?pt=)"
            )));
        };
        let mut reader = RtpReader {
            socket,
            depay,
            payload_type,
            out: Vec::new(),
            pos: 0,
            eof: false,
            dgram,
            stats: RtpStats::default(),
        };
        let first = reader.dgram[..first_len].to_vec();
        reader.push_packet(&first);
        Ok(reader)
    }

    /// The `rff-format-rtp` format name: what the engine opens this stream as.
    pub fn format_name(&self) -> &'static str {
        "rtp"
    }

    /// The RTP payload type the stream was classified by.
    pub fn payload_type(&self) -> u8 {
        self.payload_type
    }

    /// The codec byte the records carry.
    pub fn codec(&self) -> u8 {
        match self.depay {
            Depayloader::H264(_) => CODEC_H264,
            Depayloader::Jpeg(_) => CODEC_JPEG,
        }
    }

    fn push_packet(&mut self, packet: &[u8]) {
        let Some((h, payload)) = RtpHeader::parse(packet) else {
            return;
        };
        if h.payload_type != self.payload_type {
            return;
        }
        self.stats.packets += 1;
        match &mut self.depay {
            Depayloader::H264(d) => d.push(&h, payload, &mut self.out, &mut self.stats),
            Depayloader::Jpeg(d) => d.push(&h, payload, &mut self.out, &mut self.stats),
        }
    }

    /// Close out whatever is pending at end of stream (an H.264 AU whose
    /// marker never came).
    fn finish(&mut self) {
        if let Depayloader::H264(d) = &mut self.depay {
            d.flush(&mut self.out, &mut self.stats);
        }
        self.eof = true;
    }
}

impl Read for RtpReader {
    fn read(&mut self, buf: &mut [u8]) -> std::io::Result<usize> {
        while self.pos >= self.out.len() {
            if self.eof {
                return Ok(0);
            }
            self.out.clear();
            self.pos = 0;
            match self.socket.recv(&mut self.dgram) {
                Ok(n) => {
                    let packet = std::mem::take(&mut self.dgram);
                    self.push_packet(&packet[..n]);
                    self.dgram = packet;
                }
                Err(e)
                    if matches!(
                        e.kind(),
                        std::io::ErrorKind::WouldBlock | std::io::ErrorKind::TimedOut
                    ) =>
                {
                    // Silence is end of stream; deliver what is pending first.
                    self.finish();
                }
                Err(e) => return Err(e),
            }
        }
        let n = buf.len().min(self.out.len() - self.pos);
        buf[..n].copy_from_slice(&self.out[self.pos..self.pos + n]);
        self.pos += n;
        Ok(n)
    }
}

#[cfg(test)]
mod tests {
    use super::*;

    /// Minimal RTP packet builder for the tests: no CSRC, no extension.
    pub(crate) fn packet(pt: u8, marker: bool, seq: u16, ts: u32, payload: &[u8]) -> Vec<u8> {
        let mut p = Vec::with_capacity(HEADER_LEN + payload.len());
        p.push(0x80);
        p.push(pt | if marker { 0x80 } else { 0 });
        p.extend_from_slice(&seq.to_be_bytes());
        p.extend_from_slice(&ts.to_be_bytes());
        p.extend_from_slice(&0xDEAD_BEEFu32.to_be_bytes());
        p.extend_from_slice(payload);
        p
    }

    /// Split a record stream back into (codec, keyframe, ts, bytes).
    pub(crate) fn records(mut stream: &[u8]) -> Vec<(u8, bool, u32, Vec<u8>)> {
        let mut out = Vec::new();
        while !stream.is_empty() {
            let head: [u8; FRAME_HEADER_LEN] = stream[..FRAME_HEADER_LEN].try_into().unwrap();
            let h = parse_frame_header(&head).unwrap();
            let data = &stream[FRAME_HEADER_LEN..FRAME_HEADER_LEN + h.len];
            out.push((
                h.codec,
                h.flags & FLAG_KEYFRAME != 0,
                h.timestamp,
                data.to_vec(),
            ));
            stream = &stream[FRAME_HEADER_LEN + h.len..];
        }
        out
    }

    #[test]
    fn header_parse_handles_csrc_extension_and_padding() {
        let mut p = packet(96, true, 7, 90_000, &[0x65, 1, 2, 3]);
        let (h, payload) = RtpHeader::parse(&p).unwrap();
        assert_eq!(
            (h.marker, h.payload_type, h.seq, h.timestamp),
            (true, 96, 7, 90_000)
        );
        assert_eq!(payload, &[0x65, 1, 2, 3]);

        // One CSRC, a 1-word extension, and 2 bytes of padding.
        p[0] = 0x80 | 0x20 | 0x10 | 0x01;
        let payload = p.split_off(HEADER_LEN);
        p.extend_from_slice(&[0, 0, 0, 9]); // CSRC
        p.extend_from_slice(&[0xBE, 0xDE, 0, 1, 1, 2, 3, 4]); // extension: 1 word
        p.extend_from_slice(&payload);
        p.extend_from_slice(&[0, 2]); // padding: 2 bytes, count last
        let (_, got) = RtpHeader::parse(&p).unwrap();
        assert_eq!(got, &[0x65, 1, 2, 3]);

        assert!(RtpHeader::parse(&[0x40; 12]).is_none(), "RTP v1 is refused");
        assert!(RtpHeader::parse(&[0x80; 5]).is_none(), "short");
    }

    #[test]
    fn h264_single_stap_a_and_fu_a_reassemble_to_annex_b() {
        let sps = [0x67u8, 0x42, 0x00, 0x1E, 0xAB];
        let pps = [0x68u8, 0xCE, 0x38, 0x80];
        let mut idr = vec![0x65u8];
        idr.extend((0..3000u32).map(|i| (i % 251) as u8));
        let ts = 3600u32;

        let mut d = H264Depayloader::default();
        let mut out = Vec::new();
        let mut stats = RtpStats::default();
        // STAP-A with SPS + PPS.
        let mut stap = vec![NAL_STAP_A | 0x60];
        for nal in [&sps[..], &pps[..]] {
            stap.extend_from_slice(&(nal.len() as u16).to_be_bytes());
            stap.extend_from_slice(nal);
        }
        let pk = packet(96, false, 10, ts, &stap);
        let (h, pl) = RtpHeader::parse(&pk).unwrap();
        d.push(&h, pl, &mut out, &mut stats);
        // FU-A fragments of the IDR, 1000-byte pieces.
        let body = &idr[1..];
        let pieces: Vec<&[u8]> = body.chunks(1000).collect();
        for (i, piece) in pieces.iter().enumerate() {
            let mut fu = vec![(idr[0] & 0xE0) | NAL_FU_A];
            let mut hdr = idr[0] & 0x1F;
            if i == 0 {
                hdr |= 0x80;
            }
            if i + 1 == pieces.len() {
                hdr |= 0x40;
            }
            fu.push(hdr);
            fu.extend_from_slice(piece);
            let last = i + 1 == pieces.len();
            let pk = packet(96, last, 11 + i as u16, ts, &fu);
            let (h, pl) = RtpHeader::parse(&pk).unwrap();
            d.push(&h, pl, &mut out, &mut stats);
        }
        let recs = records(&out);
        assert_eq!(recs.len(), 1);
        let (codec, key, rts, au) = &recs[0];
        assert_eq!((*codec, *key, *rts), (CODEC_H264, true, ts));
        let mut want = Vec::new();
        for nal in [&sps[..], &pps[..], &idr[..]] {
            want.extend_from_slice(&START_CODE);
            want.extend_from_slice(nal);
        }
        assert_eq!(au, &want);
        assert_eq!(stats.frames, 1);
        assert_eq!(stats.lost, 0);
    }

    #[test]
    fn h264_loss_inside_a_fragment_drops_it_and_a_new_timestamp_closes_the_au() {
        let mut d = H264Depayloader::default();
        let mut out = Vec::new();
        let mut stats = RtpStats::default();
        let single = |seq: u16, ts: u32, marker: bool| packet(96, marker, seq, ts, &[0x41, 9, 9]);
        // AU 1: one slice, no marker (sender forgot it).
        let pk = single(1, 100, false);
        let (h, pl) = RtpHeader::parse(&pk).unwrap();
        d.push(&h, pl, &mut out, &mut stats);
        // AU 2 arrives: AU 1 must be emitted on the timestamp change.
        let start = {
            let mut fu = vec![0x60 | NAL_FU_A, 0x80 | 5];
            fu.extend_from_slice(&[1, 2, 3]);
            fu
        };
        let pk = packet(96, false, 2, 200, &start);
        let (h, pl) = RtpHeader::parse(&pk).unwrap();
        d.push(&h, pl, &mut out, &mut stats);
        assert_eq!(records(&out).len(), 1, "AU 1 closed by the new timestamp");
        // The middle fragment (seq 3) is lost; the end fragment arrives.
        let end = {
            let mut fu = vec![0x60 | NAL_FU_A, 0x40 | 5];
            fu.extend_from_slice(&[7, 8, 9]);
            fu
        };
        let pk = packet(96, true, 4, 200, &end);
        let (h, pl) = RtpHeader::parse(&pk).unwrap();
        d.push(&h, pl, &mut out, &mut stats);
        let recs = records(&out);
        // AU 2 had nothing but the broken fragment: nothing is emitted for it.
        assert_eq!(recs.len(), 1);
        assert_eq!(stats.lost, 1);
        assert_eq!(stats.dropped, 1);
        assert!(!recs[0].1, "a P slice is not a keyframe");
    }

    #[test]
    fn default_quant_tables_scale_and_zigzag() {
        // Q = 50 is the base table (factor 100): K.1 in zigzag order.
        let [luma, chroma] = default_quant_tables(50);
        assert_eq!(luma[0], 16);
        assert_eq!(luma[1], 11, "zigzag index 1 is natural index 1");
        assert_eq!(luma[2], 12, "zigzag index 2 is natural index 8");
        assert_eq!(chroma[0], 17);
        // Q = 100 clamps to 99 -> factor 2 -> 1 for small entries, 2 for the
        // largest (121 * 2 + 50) / 100.
        let fine = default_quant_tables(100);
        assert!(fine[0].iter().all(|&v| (1..=2).contains(&v)));
        assert_eq!(fine[0][0], 1);
        // Q = 1 -> factor 5000 -> saturates at 255.
        assert!(default_quant_tables(1)[0].iter().all(|&v| v == 255));
    }

    #[test]
    fn jpeg_headers_have_the_documented_length() {
        let mut v = Vec::new();
        write_jpeg_headers(&mut v, 320, 240, 1, Some(8), &[[1u8; 64], [2u8; 64]]);
        // SOI + DQT + SOF0 + four DHTs + DRI + SOS
        assert_eq!(
            v.len(),
            2 + (4 + 65 * 2) + 19 + (33 + 183 + 33 + 183) + 6 + 14
        );
        assert_eq!(&v[..2], &[0xFF, 0xD8]);
        assert_eq!(&v[v.len() - 14..v.len() - 12], &[0xFF, 0xDA]);
    }

    #[test]
    fn url_parsing() {
        let (a, t, pt) = parse_rtp_url("rtp://@:5004").unwrap();
        assert_eq!((a, t, pt), (":5004", DEFAULT_IDLE, None));
        let (a, t, pt) = parse_rtp_url("rtp://0.0.0.0:5004?timeout=0.5&pt=96").unwrap();
        assert_eq!(
            (a, t, pt),
            ("0.0.0.0:5004", Duration::from_millis(500), Some(96))
        );
        assert!(parse_rtp_url("rtp://").is_err());
        assert!(parse_rtp_url("rtp://:1?pt=200").is_err());
        assert!(parse_rtp_url("rtp://:1?bogus=1").is_err());
    }
}
