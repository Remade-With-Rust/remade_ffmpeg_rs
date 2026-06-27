//! FLV muxer: header + audio/video tags. AVC/AAC sequence headers (from each
//! stream's extradata) are written first, then per-packet NALU/raw-frame tags.

use std::collections::HashMap;
use std::io::Write;

use rff_core::{MediaType, Packet, Result};
use rff_format::{Muxer, Output, Stream};

pub struct FlvMuxer {
    out: Output,
    /// packet.stream_index → (media type, extradata).
    streams: HashMap<usize, (MediaType, Vec<u8>)>,
    order: Vec<(usize, MediaType, Vec<u8>)>,
    last_tag_size: u32,
    wrote_seq: bool,
}

impl FlvMuxer {
    pub fn new(out: Output) -> FlvMuxer {
        FlvMuxer {
            out,
            streams: HashMap::new(),
            order: Vec::new(),
            last_tag_size: 0,
            wrote_seq: false,
        }
    }

    /// Write one tag: the previous tag's size, then the 11-byte tag header + data.
    fn write_tag(&mut self, tag_type: u8, timestamp: i64, data: &[u8]) -> Result<()> {
        self.out.write_all(&self.last_tag_size.to_be_bytes())?;
        let ts = timestamp.max(0);
        let size = data.len() as u32;
        let hdr = [
            tag_type,
            (size >> 16) as u8,
            (size >> 8) as u8,
            size as u8,
            (ts >> 16) as u8,
            (ts >> 8) as u8,
            ts as u8,
            (ts >> 24) as u8, // timestamp extended (high byte)
            0,
            0,
            0, // stream id
        ];
        self.out.write_all(&hdr)?;
        self.out.write_all(data)?;
        self.last_tag_size = 11 + size;
        Ok(())
    }

    /// Write the AVC/AAC sequence-header tags once, before the first media tag.
    fn write_sequence_headers(&mut self) -> Result<()> {
        if self.wrote_seq {
            return Ok(());
        }
        let order = self.order.clone();
        for (_, media, extradata) in order {
            if extradata.is_empty() {
                continue;
            }
            match media {
                MediaType::Video => {
                    let mut tag = vec![0x17u8, 0x00, 0x00, 0x00, 0x00]; // key|AVC, seq, cts=0
                    tag.extend_from_slice(&extradata);
                    self.write_tag(9, 0, &tag)?;
                }
                MediaType::Audio => {
                    let mut tag = vec![0xAFu8, 0x00]; // AAC, seq header
                    tag.extend_from_slice(&extradata);
                    self.write_tag(8, 0, &tag)?;
                }
                _ => {}
            }
        }
        self.wrote_seq = true;
        Ok(())
    }
}

impl Muxer for FlvMuxer {
    fn write_header(&mut self, streams: &[Stream]) -> Result<()> {
        let has_audio = streams.iter().any(|s| s.media_type == MediaType::Audio);
        let has_video = streams.iter().any(|s| s.media_type == MediaType::Video);
        let flags = (has_audio as u8) << 2 | has_video as u8;
        // "FLV", version 1, flags, data_offset = 9.
        self.out.write_all(&[b'F', b'L', b'V', 0x01, flags, 0, 0, 0, 9])?;
        for s in streams {
            if matches!(s.media_type, MediaType::Video | MediaType::Audio) {
                self.streams.insert(s.index, (s.media_type, s.extradata.clone()));
                self.order.push((s.index, s.media_type, s.extradata.clone()));
            }
        }
        Ok(())
    }

    fn write_packet(&mut self, packet: &Packet) -> Result<()> {
        self.write_sequence_headers()?;
        let Some((media, _)) = self.streams.get(&packet.stream_index).cloned() else {
            return Ok(());
        };
        match media {
            MediaType::Video => {
                let dts = packet.dts.or(packet.pts).unwrap_or(0);
                let cts = packet.pts.unwrap_or(dts) - dts;
                let frame = if packet.flags.keyframe { 0x17u8 } else { 0x27 }; // key/inter | AVC
                let mut tag = vec![
                    frame,
                    0x01, // AVC NALU
                    (cts >> 16) as u8,
                    (cts >> 8) as u8,
                    cts as u8,
                ];
                tag.extend_from_slice(&packet.data);
                self.write_tag(9, dts, &tag)?;
            }
            MediaType::Audio => {
                let pts = packet.pts.unwrap_or(0);
                let mut tag = vec![0xAFu8, 0x01]; // AAC raw
                tag.extend_from_slice(&packet.data);
                self.write_tag(8, pts, &tag)?;
            }
            _ => {}
        }
        Ok(())
    }

    fn write_trailer(&mut self) -> Result<()> {
        // Final PreviousTagSize so the last tag is walkable backwards.
        self.out.write_all(&self.last_tag_size.to_be_bytes())?;
        self.out.flush()?;
        Ok(())
    }
}
