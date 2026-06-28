//! `rff-format-ts` — MPEG-TS (MPEG-2 Transport Stream) container.
//!
//! TS is the backbone of broadcast and HLS: a stream of fixed **188-byte
//! packets**, each tagged with a 13-bit PID. PID 0 carries the PAT (which programs
//! exist → their PMT PID); the PMT lists the elementary streams (PID + stream
//! type). Elementary PIDs carry PES packets, whose headers hold the PTS/DTS the
//! decoder needs. This module demuxes that into [`Stream`]s + [`Packet`]s on the
//! 90 kHz clock; [`mux`] writes the inverse.

use std::collections::HashMap;
use std::io::Read;

use rff_core::{CodecId, Error, Packet, Rational, Result};
use rff_format::{Demuxer, Format, FormatRegistry, Input, Stream};

mod mux;
pub use mux::TsMuxer;

const TS_PACKET: usize = 188;
const SYNC: u8 = 0x47;

/// Register the MPEG-TS format (demuxer + muxer).
pub fn register(registry: &mut FormatRegistry) {
    registry.register(Format {
        name: "mpegts",
        long_name: "MPEG-TS (MPEG-2 Transport Stream)",
        extensions: &["ts", "m2ts", "mts"],
        demuxer: Some(|input| Box::new(TsDemuxer::new(input))),
        muxer: Some(|output| Box::new(mux::TsMuxer::new(output))),
        probe: Some(probe_ts),
    });
}

/// Score by sync-byte cadence: 0x47 at 0, 188, 376 is a strong TS signal.
fn probe_ts(d: &[u8]) -> i32 {
    if d.len() < TS_PACKET * 2 + 1 {
        return if !d.is_empty() && d[0] == SYNC { 1 } else { 0 };
    }
    let hits = (0..3)
        .filter(|i| d.get(i * TS_PACKET) == Some(&SYNC))
        .count();
    match hits {
        3 => 90,
        2 => 50,
        _ => 0,
    }
}

/// MPEG-TS `stream_type` → our [`CodecId`] (the payloads we can carry).
fn codec_for(stream_type: u8) -> Option<CodecId> {
    match stream_type {
        0x1B => Some(CodecId::H264),       // AVC video
        0x0F => Some(CodecId::Aac),        // AAC ADTS audio
        0x03 | 0x04 => Some(CodecId::Mp3), // MPEG-1/2 audio (Layer III)
        _ => None,                         // MPEG-2 video, AC-3, ... not yet
    }
}

/// A PES being reassembled from one PID's TS packets.
#[derive(Default)]
struct Pes {
    data: Vec<u8>,
    pts: Option<i64>,
    dts: Option<i64>,
    /// Declared PES length (0 = unbounded → ends at the next PUSI).
    expect: usize,
    /// Set when this PES began on a TS packet whose adaptation field flagged a
    /// random-access point (a keyframe / IDR).
    keyframe: bool,
}

pub struct TsDemuxer {
    input: Input,
    streams: Vec<Stream>,
    /// elementary PID → output stream index.
    pid_index: HashMap<u16, usize>,
    pmt_pid: Option<u16>,
    /// In-flight PES per elementary PID.
    partial: HashMap<u16, Pes>,
    /// Completed packets ready to hand out.
    ready: std::collections::VecDeque<Packet>,
    eof: bool,
}

impl TsDemuxer {
    pub fn new(input: Input) -> TsDemuxer {
        TsDemuxer {
            input,
            streams: Vec::new(),
            pid_index: HashMap::new(),
            pmt_pid: None,
            partial: HashMap::new(),
            ready: std::collections::VecDeque::new(),
            eof: false,
        }
    }

    /// Read one 188-byte TS packet, resyncing on the 0x47 sync byte. Returns
    /// `None` at end of input.
    fn next_ts(&mut self) -> Result<Option<[u8; TS_PACKET]>> {
        let mut pkt = [0u8; TS_PACKET];
        if self.input.read_exact(&mut pkt).is_err() {
            return Ok(None);
        }
        if pkt[0] != SYNC {
            // Resync: scan forward one byte at a time for the next sync.
            for _ in 0..TS_PACKET {
                let mut b = [0u8; 1];
                if self.input.read_exact(&mut b).is_err() {
                    return Ok(None);
                }
                if b[0] == SYNC {
                    pkt[0] = SYNC;
                    if self.input.read_exact(&mut pkt[1..]).is_err() {
                        return Ok(None);
                    }
                    return Ok(Some(pkt));
                }
            }
            return Err(Error::invalid("mpegts: lost sync"));
        }
        Ok(Some(pkt))
    }

    /// Parse one TS packet, updating PAT/PMT tables and PES assembly. Completed
    /// PES packets are pushed to `ready`.
    fn consume(&mut self, pkt: &[u8; TS_PACKET]) {
        let pusi = pkt[1] & 0x40 != 0;
        let pid = (((pkt[1] & 0x1F) as u16) << 8) | pkt[2] as u16;
        let afc = (pkt[3] >> 4) & 0x3;
        // Payload offset: skip the 4-byte header + the adaptation field if present.
        let mut off = 4;
        let mut rai = false;
        if afc & 0x2 != 0 {
            let af_len = pkt[4] as usize;
            // The adaptation flags byte (when present) carries the
            // random_access_indicator (0x40) — the keyframe marker.
            if af_len >= 1 {
                rai = pkt[5] & 0x40 != 0;
            }
            off += 1 + af_len; // adaptation_field_length + the field
        }
        if afc & 0x1 == 0 || off >= TS_PACKET {
            return; // no payload (adaptation only)
        }
        let payload = &pkt[off..];

        if pid == 0 {
            self.parse_pat(payload, pusi);
        } else if Some(pid) == self.pmt_pid {
            self.parse_pmt(payload, pusi);
        } else if self.pid_index.contains_key(&pid) {
            self.feed_pes(pid, payload, pusi, rai);
        }
    }

    fn parse_pat(&mut self, payload: &[u8], pusi: bool) {
        let sec = section_body(payload, pusi);
        // PAT: skip the 8-byte section header, then program loop (4 bytes each)
        // up to the 4-byte CRC. The first program with a non-zero PID gives the PMT.
        if sec.len() < 12 {
            return;
        }
        let mut i = 8;
        while i + 4 <= sec.len() - 4 {
            let prog = ((sec[i] as u16) << 8) | sec[i + 1] as u16;
            let pid = (((sec[i + 2] & 0x1F) as u16) << 8) | sec[i + 3] as u16;
            if prog != 0 {
                self.pmt_pid = Some(pid);
                break;
            }
            i += 4;
        }
    }

    fn parse_pmt(&mut self, payload: &[u8], pusi: bool) {
        if !self.streams.is_empty() {
            return; // already built
        }
        let sec = section_body(payload, pusi);
        if sec.len() < 16 {
            return;
        }
        let prog_info_len = (((sec[10] & 0x0F) as usize) << 8) | sec[11] as usize;
        let mut i = 12 + prog_info_len;
        let end = sec.len().saturating_sub(4); // before CRC
        while i + 5 <= end {
            let stream_type = sec[i];
            let pid = (((sec[i + 1] & 0x1F) as u16) << 8) | sec[i + 2] as u16;
            let es_info_len = (((sec[i + 3] & 0x0F) as usize) << 8) | sec[i + 4] as usize;
            if let Some(codec) = codec_for(stream_type) {
                let idx = self.streams.len();
                let mut s = Stream::new(idx, codec);
                s.time_base = Rational::new(1, 90_000); // TS 90 kHz clock
                self.streams.push(s);
                self.pid_index.insert(pid, idx);
            }
            i += 5 + es_info_len;
        }
    }

    fn feed_pes(&mut self, pid: u16, payload: &[u8], pusi: bool, rai: bool) {
        if pusi {
            // A new PES starts here; finalize any in-flight one for this PID first.
            if let Some(done) = self.partial.remove(&pid) {
                self.emit(pid, done);
            }
            let mut pes = Pes {
                keyframe: rai,
                ..Default::default()
            };
            self.start_pes(&mut pes, payload);
            self.partial.insert(pid, pes);
        } else if let Some(pes) = self.partial.get_mut(&pid) {
            pes.data.extend_from_slice(payload);
        }
        // If a bounded PES has reached its declared length, emit immediately.
        if let Some(pes) = self.partial.get(&pid) {
            if pes.expect != 0 && pes.data.len() >= pes.expect {
                let done = self.partial.remove(&pid).unwrap();
                self.emit(pid, done);
            }
        }
    }

    /// Parse a PES header (start code, PTS/DTS) and seed `pes` with the payload.
    fn start_pes(&self, pes: &mut Pes, p: &[u8]) {
        if p.len() < 9 || p[0] != 0 || p[1] != 0 || p[2] != 1 {
            pes.data.extend_from_slice(p); // not a PES start — keep raw
            return;
        }
        let pes_len = ((p[4] as usize) << 8) | p[5] as usize;
        let flags = p[7];
        let hdr_len = p[8] as usize;
        let mut payload_off = 9 + hdr_len;
        if (flags & 0x80) != 0 && p.len() >= 14 {
            pes.pts = Some(read_ts33(&p[9..14]));
        }
        if (flags & 0xC0) == 0xC0 && p.len() >= 19 {
            pes.dts = Some(read_ts33(&p[14..19]));
        }
        // PES length counts the bytes after byte 6; the payload is what's left.
        pes.expect = if pes_len == 0 { 0 } else { pes_len + 6 };
        if payload_off > p.len() {
            payload_off = p.len();
        }
        pes.data.extend_from_slice(&p[payload_off..]);
        if pes.expect != 0 {
            pes.expect = pes.expect.saturating_sub(payload_off); // remaining payload bytes
        }
    }

    fn emit(&mut self, pid: u16, pes: Pes) {
        let Some(&idx) = self.pid_index.get(&pid) else {
            return;
        };
        if pes.data.is_empty() {
            return;
        }
        let mut pkt = Packet::from_data(idx, pes.data);
        pkt.pts = pes.pts;
        pkt.dts = pes.dts.or(pes.pts);
        pkt.flags.keyframe = pes.keyframe;
        self.ready.push_back(pkt);
    }
}

impl Demuxer for TsDemuxer {
    fn read_header(&mut self) -> Result<Vec<Stream>> {
        // Read until the PMT yields streams (bounded so a non-TS input can't spin).
        for _ in 0..10_000 {
            match self.next_ts()? {
                Some(pkt) => {
                    self.consume(&pkt);
                    if !self.streams.is_empty() {
                        return Ok(self.streams.clone());
                    }
                }
                None => break,
            }
        }
        if self.streams.is_empty() {
            return Err(Error::invalid("mpegts: no program/streams found"));
        }
        Ok(self.streams.clone())
    }

    fn read_packet(&mut self) -> Result<Packet> {
        loop {
            if let Some(pkt) = self.ready.pop_front() {
                return Ok(pkt);
            }
            if self.eof {
                return Err(Error::Eof);
            }
            match self.next_ts()? {
                Some(pkt) => self.consume(&pkt),
                None => {
                    // Flush any trailing in-flight PES at end of input.
                    self.eof = true;
                    let pids: Vec<u16> = self.partial.keys().copied().collect();
                    for pid in pids {
                        if let Some(pes) = self.partial.remove(&pid) {
                            self.emit(pid, pes);
                        }
                    }
                }
            }
        }
    }
}

/// Strip the `pointer_field` from the first TS packet of a PSI section (only
/// present when PUSI is set) and return the section bytes.
fn section_body(payload: &[u8], pusi: bool) -> &[u8] {
    if pusi {
        let ptr = *payload.first().unwrap_or(&0) as usize;
        payload.get(1 + ptr..).unwrap_or(&[])
    } else {
        payload
    }
}

/// Decode a 33-bit PTS/DTS from its 5-byte marker-interleaved encoding.
fn read_ts33(b: &[u8]) -> i64 {
    (((b[0] as i64 >> 1) & 0x07) << 30)
        | ((b[1] as i64) << 22)
        | (((b[2] as i64 >> 1) & 0x7F) << 15)
        | ((b[3] as i64) << 7)
        | ((b[4] as i64 >> 1) & 0x7F)
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn probe_recognizes_sync_cadence() {
        let mut d = vec![0u8; TS_PACKET * 3];
        d[0] = SYNC;
        d[TS_PACKET] = SYNC;
        d[TS_PACKET * 2] = SYNC;
        assert_eq!(probe_ts(&d), 90);
        d[TS_PACKET] = 0;
        assert!(probe_ts(&d) < 90);
    }

    #[test]
    fn pts_decode_matches_spec_example() {
        // PTS = 0 with marker bits: 0x21 0x00 0x01 0x00 0x01 → 0.
        assert_eq!(read_ts33(&[0x21, 0x00, 0x01, 0x00, 0x01]), 0);
        // A known value: encode 90000 (1s) and round-trip the field math.
        let v = 90_000i64;
        let bytes = [
            0x21 | (((v >> 30) & 0x07) << 1) as u8,
            ((v >> 22) & 0xFF) as u8,
            0x01 | (((v >> 15) & 0x7F) << 1) as u8,
            ((v >> 7) & 0xFF) as u8,
            0x01 | ((v & 0x7F) << 1) as u8,
        ];
        assert_eq!(read_ts33(&bytes), v);
    }
}
