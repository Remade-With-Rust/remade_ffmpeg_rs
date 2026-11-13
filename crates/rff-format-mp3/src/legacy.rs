//! The pre-#26 whole-buffer MP3 demuxer, kept verbatim (test-only) as the
//! equivalence oracle for the streaming one: on every well-formed input the two
//! must yield the same stream and the same packet sequence.

use std::io::Read;

use rff_core::{CodecId, Error, Packet, Rational, Result, SampleFormat};
use rff_format::{Demuxer, Input, Stream};

use super::{find_sync, id3v2_len, is_header_frame, parse_header};

pub(crate) struct LegacyMp3Demuxer {
    input: Option<Input>,
    buf: Vec<u8>,
    pos: usize,
    next_pts: i64,
}

impl LegacyMp3Demuxer {
    pub(crate) fn new(input: Input) -> LegacyMp3Demuxer {
        LegacyMp3Demuxer {
            input: Some(input),
            buf: Vec::new(),
            pos: 0,
            next_pts: 0,
        }
    }
}

impl Demuxer for LegacyMp3Demuxer {
    fn read_header(&mut self) -> Result<Vec<Stream>> {
        let mut input = self
            .input
            .take()
            .ok_or_else(|| Error::invalid("mp3 demux: header already read"))?;
        input.read_to_end(&mut self.buf)?;

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
                return Err(Error::Eof);
            }
            let frame = &self.buf[start..end];
            self.pos = end;
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
