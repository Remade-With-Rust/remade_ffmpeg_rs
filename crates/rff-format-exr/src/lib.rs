//! OpenEXR single-image container.
//!
//! An EXR file *is* its codec bitstream, so the demuxer hands the whole file
//! to the [`exr`](rff-codec-exr) codec as one packet and the muxer writes a
//! packet straight out. Dimensions for `ffprobe` come from the first part's
//! display window, read with `rusty_exr`'s header parser.

use std::io::{Read, Write};

use rff_core::{CodecId, Error, Packet, Rational, Result};
use rff_format::{Demuxer, Format, FormatRegistry, Input, MuxCaps, Muxer, Output, Stream};

/// The four-byte EXR magic number (20000630, little-endian).
const MAGIC: [u8; 4] = [0x76, 0x2f, 0x31, 0x01];

/// Register the EXR format into a [`FormatRegistry`].
pub fn register(registry: &mut FormatRegistry) {
    registry.register(Format {
        name: "exr_pipe",
        long_name: "OpenEXR image",
        extensions: &["exr"],
        demuxer: Some(|input| Box::new(ExrDemuxer::new(input))),
        muxer: Some(|output| Box::new(ExrMuxer::new(output))),
        muxer_path: None,
        probe: Some(probe_exr),
        mux_caps: MuxCaps::single(&[CodecId::Exr]).image(),
    });
}

fn probe_exr(data: &[u8]) -> i32 {
    if data.len() >= 8 && data[0..4] == MAGIC && data[4] == 2 {
        100
    } else {
        0
    }
}

struct ExrDemuxer {
    input: Option<Input>,
    sample: Option<Vec<u8>>,
}

impl ExrDemuxer {
    fn new(input: Input) -> Self {
        Self {
            input: Some(input),
            sample: None,
        }
    }
}

impl Demuxer for ExrDemuxer {
    fn read_header(&mut self) -> Result<Vec<Stream>> {
        let mut input = self
            .input
            .take()
            .ok_or_else(|| Error::invalid("exr demux: header already read"))?;
        let mut buf = Vec::new();
        input.read_to_end(&mut buf)?;
        if probe_exr(&buf) == 0 {
            return Err(Error::invalid("exr demux: not an EXR file"));
        }
        let headers =
            rusty_exr::read_headers(&buf).map_err(|e| Error::invalid(format!("exr demux: {e}")))?;
        let h = headers
            .first()
            .ok_or_else(|| Error::invalid("exr demux: no parts"))?;
        let win = h
            .display_window()
            .or(h.data_window())
            .ok_or_else(|| Error::invalid("exr demux: no window"))?;
        let mut stream = Stream::new(0, CodecId::Exr);
        stream.width = win.width().clamp(0, i64::from(u32::MAX)) as u32;
        stream.height = win.height().clamp(0, i64::from(u32::MAX)) as u32;
        stream.time_base = Rational::new(1, 1);
        self.sample = Some(buf);
        Ok(vec![stream])
    }

    fn read_packet(&mut self) -> Result<Packet> {
        match self.sample.take() {
            Some(data) => {
                let mut packet = Packet::from_data(0, data);
                packet.flags.keyframe = true;
                packet.pts = Some(0);
                Ok(packet)
            }
            None => Err(Error::Eof),
        }
    }
}

struct ExrMuxer {
    out: Output,
}

impl ExrMuxer {
    fn new(out: Output) -> Self {
        Self { out }
    }
}

impl Muxer for ExrMuxer {
    fn write_header(&mut self, streams: &[Stream]) -> Result<()> {
        match streams.first() {
            Some(s) if s.codec_id == CodecId::Exr => Ok(()),
            Some(_) => Err(Error::unsupported(
                "exr mux: only the `exr` codec is supported",
            )),
            None => Err(Error::invalid("exr mux: no streams")),
        }
    }

    fn write_packet(&mut self, packet: &Packet) -> Result<()> {
        self.out.write_all(&packet.data)?;
        Ok(())
    }

    fn write_trailer(&mut self) -> Result<()> {
        self.out.flush()?;
        Ok(())
    }
}

#[cfg(test)]
mod tests {
    use super::*;
    use std::io::Cursor;

    fn tiny_exr() -> Vec<u8> {
        let img = rusty_exr::Image::new(3, 2)
            .with_channel("R", rusty_exr::Samples::F16(vec![0x3c00; 6]))
            .with_channel("G", rusty_exr::Samples::F16(vec![0; 6]));
        rusty_exr::encode(&img, &rusty_exr::EncodeOptions::default()).unwrap()
    }

    #[test]
    fn sniffs_and_demuxes_exr() {
        let f = tiny_exr();
        assert_eq!(probe_exr(&f), 100);
        assert_eq!(probe_exr(b"not an exr file"), 0);
        let mut d = ExrDemuxer::new(Box::new(Cursor::new(f.clone())));
        let s = d.read_header().unwrap();
        assert_eq!(s[0].codec_id, CodecId::Exr);
        assert_eq!((s[0].width, s[0].height), (3, 2));
        assert_eq!(d.read_packet().unwrap().data, f);
        assert!(matches!(d.read_packet(), Err(Error::Eof)));
    }
}
