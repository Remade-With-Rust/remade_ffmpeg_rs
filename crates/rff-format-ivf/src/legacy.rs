//! The pre-#26 whole-buffer IVF demuxer, kept verbatim (test-only) as the
//! equivalence oracle for the streaming one: on every well-formed input the two
//! must yield the same streams and the same packet sequence.

use std::io::Read;

use rff_core::{Error, Packet, Rational, Result};
use rff_format::{Demuxer, Input, Stream};

use super::{codec_from_fourcc, probe_ivf, rd_u16, rd_u32};

pub(crate) struct LegacyIvfDemuxer {
    input: Option<Input>,
    buf: Vec<u8>,
    pos: usize,
}

impl LegacyIvfDemuxer {
    pub(crate) fn new(input: Input) -> LegacyIvfDemuxer {
        LegacyIvfDemuxer {
            input: Some(input),
            buf: Vec::new(),
            pos: 0,
        }
    }
}

impl Demuxer for LegacyIvfDemuxer {
    fn read_header(&mut self) -> Result<Vec<Stream>> {
        let mut input = self
            .input
            .take()
            .ok_or_else(|| Error::invalid("ivf demux: header already read"))?;
        input.read_to_end(&mut self.buf)?;
        if self.buf.len() < 32 || probe_ivf(&self.buf) == 0 {
            return Err(Error::invalid("ivf demux: not an IVF file"));
        }
        let width = rd_u16(&self.buf, 12) as u32;
        let height = rd_u16(&self.buf, 14) as u32;
        let fnum = rd_u32(&self.buf, 16).max(1);
        let fden = rd_u32(&self.buf, 20).max(1);
        self.pos = rd_u16(&self.buf, 6).max(32) as usize; // header length

        let mut stream = Stream::new(0, codec_from_fourcc(&self.buf[8..12])?);
        stream.width = width;
        stream.height = height;
        stream.time_base = Rational::new(fden as i32, fnum as i32);
        Ok(vec![stream])
    }

    fn read_packet(&mut self) -> Result<Packet> {
        if self.pos + 12 > self.buf.len() {
            return Err(Error::Eof);
        }
        let size = rd_u32(&self.buf, self.pos) as usize;
        let pts = u64::from_le_bytes(self.buf[self.pos + 4..self.pos + 12].try_into().unwrap());
        let start = self.pos + 12;
        let end = start + size;
        if end > self.buf.len() {
            return Err(Error::invalid("ivf demux: truncated frame"));
        }
        let mut packet = Packet::from_data(0, self.buf[start..end].to_vec());
        packet.pts = Some(pts as i64);
        self.pos = end;
        Ok(packet)
    }
}
