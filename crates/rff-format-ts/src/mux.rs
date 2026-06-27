//! MPEG-TS muxer: wrap elementary packets back into 188-byte TS packets with a
//! PAT/PMT, PES headers (PTS/DTS), per-PID continuity counters, and PCR.

use std::collections::HashMap;
use std::io::Write;

use rff_core::{CodecId, MediaType, Packet, Result};
use rff_format::{Muxer, Output, Stream};

const TS_PACKET: usize = 188;
const PMT_PID: u16 = 0x1000;
const PAT_PID: u16 = 0x0000;

/// Our [`CodecId`] → MPEG-TS `stream_type`.
fn stream_type(codec: CodecId) -> u8 {
    match codec {
        CodecId::H264 => 0x1B,
        CodecId::Aac => 0x0F,
        CodecId::Mp3 => 0x03,
        _ => 0x06, // private data fallback
    }
}

struct OutStream {
    pid: u16,
    codec: CodecId,
    media: MediaType,
    cc: u8, // continuity counter (4-bit, wraps)
}

pub struct TsMuxer {
    out: Output,
    streams: Vec<OutStream>,
    by_index: HashMap<usize, usize>, // packet.stream_index → streams[] slot
    pcr_pid: u16,
    pat_cc: u8,
    pmt_cc: u8,
    wrote_psi: bool,
}

impl TsMuxer {
    pub fn new(out: Output) -> TsMuxer {
        TsMuxer {
            out,
            streams: Vec::new(),
            by_index: HashMap::new(),
            pcr_pid: 0x0100,
            pat_cc: 0,
            pmt_cc: 0,
            wrote_psi: false,
        }
    }

    /// Emit one finished 188-byte TS packet.
    fn emit(&mut self, pkt: &[u8; TS_PACKET]) -> Result<()> {
        self.out.write_all(pkt)?;
        Ok(())
    }

    /// Build + write a PSI section (PAT/PMT) as a single TS packet on `pid`.
    fn write_psi(&mut self, pid: u16, cc: u8, table_id: u8, body: &[u8]) -> Result<()> {
        // section = table_id, section_syntax+len, then `body`, then CRC32.
        let section_len = body.len() + 4; // body + CRC
        let mut sec = Vec::with_capacity(3 + section_len);
        sec.push(table_id);
        sec.push(0xB0 | ((section_len >> 8) & 0x0F) as u8); // syntax=1, len hi
        sec.push((section_len & 0xFF) as u8);
        sec.extend_from_slice(body);
        let crc = mpeg_crc32(&sec);
        sec.extend_from_slice(&crc.to_be_bytes());

        let mut ts = [0xFFu8; TS_PACKET];
        ts[0] = 0x47;
        ts[1] = 0x40 | ((pid >> 8) & 0x1F) as u8; // PUSI=1
        ts[2] = (pid & 0xFF) as u8;
        ts[3] = 0x10 | (cc & 0x0F); // payload only
        ts[4] = 0x00; // pointer_field
        let n = sec.len().min(TS_PACKET - 5);
        ts[5..5 + n].copy_from_slice(&sec[..n]);
        self.emit(&ts)
    }

    fn write_pat(&mut self) -> Result<()> {
        // body: transport_stream_id(2), version/cur(1), section#(1), last#(1),
        //       then program loop: program_number(2), reserved+PMT_PID(2).
        let mut body = vec![0x00, 0x01, 0xC1, 0x00, 0x00];
        body.extend_from_slice(&[0x00, 0x01]); // program_number 1
        body.push(0xE0 | ((PMT_PID >> 8) & 0x1F) as u8);
        body.push((PMT_PID & 0xFF) as u8);
        let cc = self.pat_cc;
        self.pat_cc = (self.pat_cc + 1) & 0x0F;
        self.write_psi(PAT_PID, cc, 0x00, &body)
    }

    fn write_pmt(&mut self) -> Result<()> {
        // body: program_number(2), version/cur(1), section#(1), last#(1),
        //       reserved+PCR_PID(2), reserved+prog_info_len(2=0), then ES loop.
        let mut body = vec![0x00, 0x01, 0xC1, 0x00, 0x00];
        body.push(0xE0 | ((self.pcr_pid >> 8) & 0x1F) as u8);
        body.push((self.pcr_pid & 0xFF) as u8);
        body.push(0xF0);
        body.push(0x00); // program_info_length = 0
        for s in &self.streams {
            body.push(stream_type(s.codec));
            body.push(0xE0 | ((s.pid >> 8) & 0x1F) as u8);
            body.push((s.pid & 0xFF) as u8);
            body.push(0xF0);
            body.push(0x00); // ES_info_length = 0
        }
        let cc = self.pmt_cc;
        self.pmt_cc = (self.pmt_cc + 1) & 0x0F;
        self.write_psi(PMT_PID, cc, 0x02, &body)
    }
}

impl Muxer for TsMuxer {
    fn write_header(&mut self, streams: &[Stream]) -> Result<()> {
        for (slot, s) in streams.iter().enumerate() {
            let pid = 0x0100 + s.index as u16;
            self.by_index.insert(s.index, slot);
            if s.media_type == MediaType::Video && self.streams.iter().all(|o| o.media != MediaType::Video) {
                self.pcr_pid = pid;
            }
            self.streams.push(OutStream { pid, codec: s.codec_id, media: s.media_type, cc: 0 });
        }
        if self.streams.is_empty() {
            return Err(rff_core::Error::invalid("mpegts: no streams to mux"));
        }
        if self.streams.iter().all(|o| o.media != MediaType::Video) {
            self.pcr_pid = self.streams[0].pid; // audio-only: PCR on the first PID
        }
        self.write_pat()?;
        self.write_pmt()?;
        self.wrote_psi = true;
        Ok(())
    }

    fn write_packet(&mut self, packet: &Packet) -> Result<()> {
        let Some(&slot) = self.by_index.get(&packet.stream_index) else {
            return Ok(());
        };
        let (pid, media, mut cc) = {
            let s = &self.streams[slot];
            (s.pid, s.media, s.cc)
        };
        let is_pcr = pid == self.pcr_pid;
        let pts = packet.pts.unwrap_or(0);
        let dts = packet.dts.unwrap_or(pts);

        // --- build the PES ---
        let stream_id = if media == MediaType::Video { 0xE0 } else { 0xC0 };
        let has_dts = packet.dts.is_some() && packet.dts != packet.pts;
        let mut pes = vec![0x00, 0x00, 0x01, stream_id];
        let mut hdr = Vec::new();
        let pts_dts_flags = if has_dts { 0xC0u8 } else { 0x80 };
        hdr.push(0x80); // marker '10'
        hdr.push(pts_dts_flags);
        let ptsdts_len = if has_dts { 10 } else { 5 };
        hdr.push(ptsdts_len as u8);
        push_ts33(&mut hdr, if has_dts { 0x3 } else { 0x2 }, pts);
        if has_dts {
            push_ts33(&mut hdr, 0x1, dts);
        }
        // PES_packet_length: 0 (unbounded) for video, else the actual length.
        let pes_len = hdr.len() + packet.data.len();
        let len_field = if media == MediaType::Video { 0 } else { pes_len.min(0xFFFF) };
        pes.push((len_field >> 8) as u8);
        pes.push((len_field & 0xFF) as u8);
        pes.extend_from_slice(&hdr);
        pes.extend_from_slice(&packet.data);

        // --- split the PES into TS packets ---
        let mut payload = &pes[..];
        let mut first = true;
        while !payload.is_empty() {
            let mut ts = [0u8; TS_PACKET];
            ts[0] = 0x47;
            ts[1] = ((pid >> 8) & 0x1F) as u8;
            if first {
                ts[1] |= 0x40; // PUSI
            }
            ts[2] = (pid & 0xFF) as u8;

            // Adaptation field if we need PCR (first pkt of PCR PID) or stuffing.
            let want_pcr = first && is_pcr;
            let max_payload = if want_pcr { TS_PACKET - 4 - 8 } else { TS_PACKET - 4 };
            let take = payload.len().min(max_payload);
            let need_stuffing = take < TS_PACKET - 4 || want_pcr;

            if need_stuffing {
                ts[3] = 0x30 | (cc & 0x0F); // adaptation + payload
                let payload_start = TS_PACKET - take;
                let af_len = payload_start - 5; // bytes after the af_length byte
                ts[4] = af_len as u8;
                if af_len > 0 {
                    let flags_idx = 5;
                    if want_pcr {
                        ts[flags_idx] = 0x10; // PCR_flag
                        write_pcr(&mut ts[flags_idx + 1..flags_idx + 7], dts);
                        for b in ts.iter_mut().take(payload_start).skip(flags_idx + 7) {
                            *b = 0xFF;
                        }
                    } else {
                        ts[flags_idx] = 0x00;
                        for b in ts.iter_mut().take(payload_start).skip(flags_idx + 1) {
                            *b = 0xFF;
                        }
                    }
                }
                ts[payload_start..].copy_from_slice(&payload[..take]);
            } else {
                ts[3] = 0x10 | (cc & 0x0F); // payload only
                ts[4..4 + take].copy_from_slice(&payload[..take]);
            }

            cc = (cc + 1) & 0x0F;
            self.emit(&ts)?;
            payload = &payload[take..];
            first = false;
        }
        self.streams[slot].cc = cc;
        Ok(())
    }

    fn write_trailer(&mut self) -> Result<()> {
        self.out.flush()?;
        Ok(())
    }
}

/// Append a 33-bit timestamp in the 5-byte marker-interleaved PES form. `guard`
/// is the high nibble's guard pattern (`0x2` PTS-only, `0x3` PTS-of-PTS+DTS,
/// `0x1` DTS).
fn push_ts33(out: &mut Vec<u8>, guard: u8, v: i64) {
    out.push((guard << 4) as u8 | (((v >> 30) & 0x07) << 1) as u8 | 0x01);
    out.push(((v >> 22) & 0xFF) as u8);
    out.push((((v >> 15) & 0x7F) << 1) as u8 | 0x01);
    out.push(((v >> 7) & 0xFF) as u8);
    out.push(((v & 0x7F) << 1) as u8 | 0x01);
}

/// Write a 6-byte PCR (33-bit base @90 kHz, extension 0).
fn write_pcr(out: &mut [u8], base: i64) {
    let b = base & 0x1_FFFF_FFFF;
    out[0] = (b >> 25) as u8;
    out[1] = (b >> 17) as u8;
    out[2] = (b >> 9) as u8;
    out[3] = (b >> 1) as u8;
    out[4] = (((b & 1) << 7) as u8) | 0x7E; // 6 reserved bits set
    out[5] = 0x00; // extension low
}

/// MPEG-2 systems CRC-32 (poly 0x04C11DB7, init 0xFFFFFFFF, no final xor).
fn mpeg_crc32(data: &[u8]) -> u32 {
    let mut crc = 0xFFFF_FFFFu32;
    for &byte in data {
        crc ^= (byte as u32) << 24;
        for _ in 0..8 {
            crc = if crc & 0x8000_0000 != 0 {
                (crc << 1) ^ 0x04C1_1DB7
            } else {
                crc << 1
            };
        }
    }
    crc
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn crc32_known_vector() {
        // MPEG-2 CRC-32 of "123456789" is 0x0376E6E7.
        assert_eq!(mpeg_crc32(b"123456789"), 0x0376_E6E7);
    }

    #[test]
    fn pts33_roundtrips_through_demux_decoder() {
        let mut v = Vec::new();
        push_ts33(&mut v, 0x2, 90_000);
        assert_eq!(super::super::read_ts33(&v), 90_000);
    }
}
