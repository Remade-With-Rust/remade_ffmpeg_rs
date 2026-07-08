//! `rff-format-flv` — FLV (Flash Video) container.
//!
//! FLV is the RTMP/Flash wire format: a 9-byte header, then a chain of tags —
//! audio (8), video (9), or script/metadata (18) — each prefixed by the previous
//! tag's size. Video tags carry AVC (H.264) as a sequence header (`AVCDecoder
//! ConfigurationRecord`) followed by length-prefixed NALUs; audio tags carry AAC
//! as an `AudioSpecificConfig` header followed by raw frames. Timestamps are in
//! milliseconds.

use std::io::Read;

use rff_core::{CodecId, Error, Packet, Rational, Result};
use rff_format::{Demuxer, Format, FormatRegistry, Input, Stream};

mod mux;
pub use mux::FlvMuxer;

/// Register the FLV format (demuxer + muxer).
pub fn register(registry: &mut FormatRegistry) {
    registry.register(Format {
        name: "flv",
        long_name: "FLV (Flash Video)",
        extensions: &["flv"],
        demuxer: Some(|input| Box::new(FlvDemuxer::new(input))),
        muxer: Some(|output| Box::new(mux::FlvMuxer::new(output))),
        probe: Some(probe_flv),
    });
}

fn probe_flv(d: &[u8]) -> i32 {
    if d.len() >= 3 && &d[0..3] == b"FLV" {
        90
    } else {
        0
    }
}

const TAG_AUDIO: u8 = 8;
const TAG_VIDEO: u8 = 9;
const TAG_SCRIPT: u8 = 18;

pub struct FlvDemuxer {
    input: Input,
    streams: Vec<Stream>,
    video_idx: Option<usize>,
    audio_idx: Option<usize>,
    has_audio: bool,
    has_video: bool,
    header_read: bool,
}

impl FlvDemuxer {
    pub fn new(input: Input) -> FlvDemuxer {
        FlvDemuxer {
            input,
            streams: Vec::new(),
            video_idx: None,
            audio_idx: None,
            has_audio: false,
            has_video: false,
            header_read: false,
        }
    }

    fn read_file_header(&mut self) -> Result<()> {
        let mut h = [0u8; 9];
        self.input
            .read_exact(&mut h)
            .map_err(|_| Error::invalid("flv: short header"))?;
        if &h[0..3] != b"FLV" {
            return Err(Error::invalid("flv: bad signature"));
        }
        self.has_audio = h[4] & 0x04 != 0;
        self.has_video = h[4] & 0x01 != 0;
        let data_offset = u32::from_be_bytes([h[5], h[6], h[7], h[8]]) as usize;
        // Skip any bytes between the 9-byte header and the first tag. `data_offset`
        // is attacker-controlled (up to 4 GB); skip via a chunked copy that stops
        // at real EOF, NOT a byte-at-a-time loop to `data_offset` that ignores the
        // read result and spins billions of times on a short/malformed file.
        let skip = (data_offset.saturating_sub(9)) as u64;
        let _ = std::io::copy(&mut self.input.by_ref().take(skip), &mut std::io::sink());
        self.header_read = true;
        Ok(())
    }

    /// Read one tag: returns `(tag_type, timestamp_ms, data)` or `None` at EOF.
    fn read_tag(&mut self) -> Result<Option<(u8, i64, Vec<u8>)>> {
        let mut prev = [0u8; 4]; // PreviousTagSize
        if self.input.read_exact(&mut prev).is_err() {
            return Ok(None);
        }
        let mut th = [0u8; 11];
        if self.input.read_exact(&mut th).is_err() {
            return Ok(None);
        }
        let tag_type = th[0];
        let size = u32::from_be_bytes([0, th[1], th[2], th[3]]) as usize;
        let ts =
            ((th[7] as i64) << 24) | ((th[4] as i64) << 16) | ((th[5] as i64) << 8) | th[6] as i64;
        // Read up to `size` bytes without pre-allocating `size` (a claimed size
        // beyond the actual input must not drive an eager allocation); short read
        // ⇒ truncated tag ⇒ EOF.
        let mut data = Vec::new();
        let got = self
            .input
            .by_ref()
            .take(size as u64)
            .read_to_end(&mut data)
            .unwrap_or(0);
        if got < size {
            return Ok(None);
        }
        Ok(Some((tag_type, ts, data)))
    }

    fn ensure_video_stream(&mut self, extradata: Vec<u8>) -> usize {
        *self.video_idx.get_or_insert_with(|| {
            let idx = self.streams.len();
            let mut s = Stream::new(idx, CodecId::H264);
            s.time_base = Rational::new(1, 1000);
            s.extradata = extradata;
            self.streams.push(s);
            idx
        })
    }

    fn ensure_audio_stream(&mut self, extradata: Vec<u8>) -> usize {
        *self.audio_idx.get_or_insert_with(|| {
            let idx = self.streams.len();
            let mut s = Stream::new(idx, CodecId::Aac);
            s.time_base = Rational::new(1, 1000);
            s.extradata = extradata;
            self.streams.push(s);
            idx
        })
    }
}

impl Demuxer for FlvDemuxer {
    fn read_header(&mut self) -> Result<Vec<Stream>> {
        self.read_file_header()?;
        // Read tags until both present streams have surfaced their sequence
        // headers (which carry the codec extradata). Bounded for safety.
        for _ in 0..200 {
            let want = (self.has_video && self.video_idx.is_none())
                || (self.has_audio && self.audio_idx.is_none());
            if !want {
                break;
            }
            match self.read_tag()? {
                Some((TAG_VIDEO, _, data)) if data.len() >= 5 && data[0] & 0x0F == 7 => {
                    // AVC: packet type 0 is the AVCDecoderConfigurationRecord.
                    if data[1] == 0 {
                        self.ensure_video_stream(data[5..].to_vec());
                    } else {
                        self.ensure_video_stream(Vec::new());
                    }
                }
                Some((TAG_AUDIO, _, data)) if data.len() >= 2 && data[0] >> 4 == 10 => {
                    if data[1] == 0 {
                        self.ensure_audio_stream(data[2..].to_vec());
                    } else {
                        self.ensure_audio_stream(Vec::new());
                    }
                }
                Some(_) => {}
                None => break,
            }
        }
        if self.streams.is_empty() {
            return Err(Error::invalid("flv: no audio/video streams"));
        }
        Ok(self.streams.clone())
    }

    fn read_packet(&mut self) -> Result<Packet> {
        loop {
            match self.read_tag()? {
                Some((TAG_VIDEO, ts, data)) if data.len() >= 5 && data[0] & 0x0F == 7 => {
                    let avc_type = data[1];
                    if avc_type != 1 {
                        continue; // 0 = seq header (already), 2 = end of sequence
                    }
                    let idx = self.ensure_video_stream(Vec::new());
                    let cts =
                        ((data[2] as i64) << 16 | (data[3] as i64) << 8 | data[4] as i64) as i64;
                    let mut pkt = Packet::from_data(idx, data[5..].to_vec());
                    pkt.dts = Some(ts);
                    pkt.pts = Some(ts + cts);
                    pkt.flags.keyframe = data[0] >> 4 == 1;
                    return Ok(pkt);
                }
                Some((TAG_AUDIO, ts, data)) if data.len() >= 2 && data[0] >> 4 == 10 => {
                    if data[1] != 1 {
                        continue; // 0 = AudioSpecificConfig header
                    }
                    let idx = self.ensure_audio_stream(Vec::new());
                    let mut pkt = Packet::from_data(idx, data[2..].to_vec());
                    pkt.pts = Some(ts);
                    pkt.dts = Some(ts);
                    return Ok(pkt);
                }
                Some((TAG_SCRIPT, _, _)) | Some(_) => continue, // metadata / unknown
                None => return Err(Error::Eof),
            }
        }
    }
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn probe_matches_signature() {
        assert_eq!(probe_flv(b"FLV\x01\x05\x00\x00\x00\x09"), 90);
        assert_eq!(probe_flv(b"RIFF"), 0);
    }
}
