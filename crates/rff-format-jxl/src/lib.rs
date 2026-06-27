//! JPEG XL single-image container.
//!
//! A `.jxl` file (raw codestream or ISOBMFF-wrapped) is its own codec stream, so
//! the demuxer hands the whole file to the [`jpegxl`](rff-codec-jxl) codec as one
//! packet; dimensions for `ffprobe` come from the header via `jxl-oxide`.

use std::io::{Cursor, Read, Write};

use jxl_oxide::JxlImage;
use rff_core::{CodecId, Error, Packet, Rational, Result};
use rff_format::{Demuxer, Format, FormatRegistry, Input, Muxer, Output, Stream};

/// ISOBMFF-wrapped JPEG XL signature box.
const CONTAINER_SIG: [u8; 12] = [
    0x00, 0x00, 0x00, 0x0C, b'J', b'X', b'L', b' ', 0x0D, 0x0A, 0x87, 0x0A,
];

/// Register the JPEG XL format into a [`FormatRegistry`].
pub fn register(registry: &mut FormatRegistry) {
    registry.register(Format {
        name: "jpegxl",
        long_name: "JPEG XL image",
        extensions: &["jxl"],
        demuxer: Some(|input| Box::new(JxlDemuxer::new(input))),
        muxer: Some(|output| Box::new(JxlMuxer::new(output))),
        probe: Some(probe_jxl),
    });
}

/// Sniff JPEG XL: the raw codestream marker `FF 0A`, or the container box.
fn probe_jxl(data: &[u8]) -> i32 {
    if data.len() >= 2 && data[0] == 0xFF && data[1] == 0x0A {
        100
    } else if data.len() >= 12 && data[0..12] == CONTAINER_SIG {
        100
    } else {
        0
    }
}

struct JxlDemuxer {
    input: Option<Input>,
    sample: Option<Vec<u8>>,
}

impl JxlDemuxer {
    fn new(input: Input) -> JxlDemuxer {
        JxlDemuxer {
            input: Some(input),
            sample: None,
        }
    }
}

impl Demuxer for JxlDemuxer {
    fn read_header(&mut self) -> Result<Vec<Stream>> {
        let mut input = self
            .input
            .take()
            .ok_or_else(|| Error::invalid("jxl demux: header already read"))?;
        let mut buf = Vec::new();
        input.read_to_end(&mut buf)?;
        if probe_jxl(&buf) == 0 {
            return Err(Error::invalid("jxl demux: not a JPEG XL file"));
        }
        // Header parse only (no full render) for dimensions.
        let image = JxlImage::builder()
            .read(Cursor::new(&buf))
            .map_err(|e| Error::invalid(format!("jxl demux: {e}")))?;
        let (width, height) = (image.width(), image.height());

        self.sample = Some(buf);
        let mut stream = Stream::new(0, CodecId::Jxl);
        stream.width = width;
        stream.height = height;
        stream.time_base = Rational::new(1, 1);
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

struct JxlMuxer {
    out: Output,
}

impl JxlMuxer {
    fn new(out: Output) -> JxlMuxer {
        JxlMuxer { out }
    }
}

impl Muxer for JxlMuxer {
    fn write_header(&mut self, streams: &[Stream]) -> Result<()> {
        match streams.first() {
            Some(s) if s.codec_id == CodecId::Jxl => Ok(()),
            Some(_) => Err(Error::unsupported("jxl mux: only the `jpegxl` codec is supported")),
            None => Err(Error::invalid("jxl mux: no streams")),
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

    #[test]
    fn probe_recognizes_jxl() {
        assert_eq!(probe_jxl(&[0xFF, 0x0A, 0x00]), 100); // raw codestream
        assert_eq!(probe_jxl(&CONTAINER_SIG), 100); // container box
        assert_eq!(probe_jxl(b"not jxl at all!!"), 0);
    }
}
