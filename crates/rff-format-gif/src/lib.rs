//! GIF single-image container. A GIF file is its own codec stream, so the
//! demuxer hands the whole file to the [`gif`](rff-codec-gif) codec as one
//! packet; dimensions come from the logical screen descriptor.

use std::io::{Read, Write};

use rff_core::{CodecId, Error, Packet, Rational, Result};
use rff_format::{Demuxer, Format, FormatRegistry, Input, Muxer, Output, Stream};

/// Register the GIF format into a [`FormatRegistry`].
pub fn register(registry: &mut FormatRegistry) {
    registry.register(Format {
        name: "gif",
        long_name: "GIF (Graphics Interchange Format) image",
        extensions: &["gif"],
        demuxer: Some(|input| Box::new(GifDemuxer::new(input))),
        muxer: Some(|output| Box::new(GifMuxer::new(output))),
        probe: Some(probe_gif),
    });
}

/// Sniff GIF by its `GIF87a` / `GIF89a` signature.
fn probe_gif(data: &[u8]) -> i32 {
    if data.len() >= 6 && &data[0..4] == b"GIF8" && (data[4] == b'7' || data[4] == b'9') {
        100
    } else {
        0
    }
}

struct GifDemuxer {
    input: Option<Input>,
    sample: Option<Vec<u8>>,
}

impl GifDemuxer {
    fn new(input: Input) -> GifDemuxer {
        GifDemuxer {
            input: Some(input),
            sample: None,
        }
    }
}

impl Demuxer for GifDemuxer {
    fn read_header(&mut self) -> Result<Vec<Stream>> {
        let mut input = self
            .input
            .take()
            .ok_or_else(|| Error::invalid("gif demux: header already read"))?;
        let mut buf = Vec::new();
        input.read_to_end(&mut buf)?;
        if probe_gif(&buf) == 0 || buf.len() < 10 {
            return Err(Error::invalid("gif demux: not a GIF file"));
        }
        // Logical screen descriptor: width @6, height @8 (little-endian).
        let width = u16::from_le_bytes([buf[6], buf[7]]) as u32;
        let height = u16::from_le_bytes([buf[8], buf[9]]) as u32;

        self.sample = Some(buf);
        let mut stream = Stream::new(0, CodecId::Gif);
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

struct GifMuxer {
    out: Output,
}

impl GifMuxer {
    fn new(out: Output) -> GifMuxer {
        GifMuxer { out }
    }
}

impl Muxer for GifMuxer {
    fn write_header(&mut self, streams: &[Stream]) -> Result<()> {
        match streams.first() {
            Some(s) if s.codec_id == CodecId::Gif => Ok(()),
            Some(_) => Err(Error::unsupported("gif mux: only the `gif` codec is supported")),
            None => Err(Error::invalid("gif mux: no streams")),
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

    /// Minimal GIF header: `GIF89a` + logical screen descriptor declaring 6×4.
    fn gif_header_6x4() -> Vec<u8> {
        let mut v = b"GIF89a".to_vec();
        v.extend_from_slice(&6u16.to_le_bytes()); // width
        v.extend_from_slice(&4u16.to_le_bytes()); // height
        v.extend_from_slice(&[0x00, 0x00, 0x00]); // packed, bg, aspect
        v
    }

    #[test]
    fn sniffs_and_reads_gif_dimensions() {
        let gif = gif_header_6x4();
        assert_eq!(probe_gif(&gif), 100);
        assert_eq!(probe_gif(b"NOTAGIF!"), 0);

        let mut dem = GifDemuxer::new(Box::new(Cursor::new(gif.clone())));
        let streams = dem.read_header().unwrap();
        assert_eq!(streams[0].codec_id, CodecId::Gif);
        assert_eq!((streams[0].width, streams[0].height), (6, 4));
        assert_eq!(dem.read_packet().unwrap().data, gif);
        assert!(matches!(dem.read_packet(), Err(Error::Eof)));
    }
}
