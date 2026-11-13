//! The pre-#26 whole-buffer y4m demuxer, kept (test-only) as the equivalence
//! oracle for the streaming one: on every well-formed input the two must yield
//! the same streams and the same packet sequence. Verbatim except that the
//! frame size uses the new overflow-checked helper.

use std::io::Read;

use rff_core::{CodecId, ColorRange, Error, Packet, Rational, Result};
use rff_format::{Demuxer, Input, Stream};

use super::{colorspace, frame_bytes, probe_y4m, MAGIC};

pub(crate) struct LegacyY4mDemuxer {
    input: Option<Input>,
    buf: Vec<u8>,
    pos: usize,
    frame_len: usize,
    next_pts: i64,
}

impl LegacyY4mDemuxer {
    pub(crate) fn new(input: Input) -> LegacyY4mDemuxer {
        LegacyY4mDemuxer {
            input: Some(input),
            buf: Vec::new(),
            pos: 0,
            frame_len: 0,
            next_pts: 0,
        }
    }
}

impl Demuxer for LegacyY4mDemuxer {
    fn read_header(&mut self) -> Result<Vec<Stream>> {
        let mut input = self
            .input
            .take()
            .ok_or_else(|| Error::invalid("y4m demux: header already read"))?;
        input.read_to_end(&mut self.buf)?;
        if probe_y4m(&self.buf) == 0 {
            return Err(Error::invalid("y4m demux: missing YUV4MPEG2 signature"));
        }
        let nl = self
            .buf
            .iter()
            .position(|&b| b == b'\n')
            .ok_or_else(|| Error::invalid("y4m demux: unterminated header"))?;
        let header = std::str::from_utf8(&self.buf[MAGIC.len()..nl])
            .map_err(|_| Error::invalid("y4m demux: non-UTF8 header"))?;

        let (mut w, mut h) = (0u32, 0u32);
        let (mut fnum, mut fden) = (30i32, 1i32);
        let mut cs: Option<String> = None;
        let mut range = ColorRange::Unspecified;
        for tok in header.split_whitespace() {
            let (tag, val) = tok.split_at(1);
            match tag {
                "W" => w = val.parse().map_err(|_| Error::invalid("y4m: bad width"))?,
                "H" => h = val.parse().map_err(|_| Error::invalid("y4m: bad height"))?,
                "F" => {
                    let (n, d) = val
                        .split_once(':')
                        .ok_or_else(|| Error::invalid("y4m: bad framerate"))?;
                    fnum = n.parse().map_err(|_| Error::invalid("y4m: bad fps num"))?;
                    fden = d.parse().map_err(|_| Error::invalid("y4m: bad fps den"))?;
                }
                "C" => cs = Some(val.to_string()),
                "X" => {
                    if let Some(v) = val.strip_prefix("COLORRANGE=") {
                        range = ColorRange::from_y4m_tag(v);
                    }
                }
                _ => {}
            }
        }
        if w == 0 || h == 0 {
            return Err(Error::invalid("y4m demux: missing/zero W or H"));
        }
        let format = colorspace(cs.as_deref())?;
        self.pos = nl + 1;
        self.frame_len = frame_bytes(format, w as usize, h as usize)?;

        let mut stream = Stream::new(0, CodecId::RawVideo);
        stream.width = w;
        stream.height = h;
        stream.pixel_format = Some(format);
        stream.color_range = range;
        stream.time_base = Rational::new(fden.max(1), fnum.max(1));
        Ok(vec![stream])
    }

    fn read_packet(&mut self) -> Result<Packet> {
        if self.pos >= self.buf.len() {
            return Err(Error::Eof);
        }
        if !self.buf[self.pos..].starts_with(b"FRAME") {
            return Err(Error::invalid("y4m demux: expected FRAME marker"));
        }
        let nl = self.buf[self.pos..]
            .iter()
            .position(|&b| b == b'\n')
            .map(|i| self.pos + i)
            .ok_or_else(|| Error::invalid("y4m demux: unterminated FRAME header"))?;
        let start = nl + 1;
        let end = start + self.frame_len;
        if end > self.buf.len() {
            return Err(Error::invalid("y4m demux: truncated frame data"));
        }
        let mut packet = Packet::from_data(0, self.buf[start..end].to_vec());
        packet.pts = Some(self.next_pts);
        self.next_pts += 1;
        self.pos = end;
        Ok(packet)
    }
}
