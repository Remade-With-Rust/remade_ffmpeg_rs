//! WebP single-image container. A WebP file (RIFF/`WEBP`) is its own codec
//! stream, so the demuxer hands the whole file to the [`webp`](rff-codec-webp)
//! codec as one packet; dimensions are read from the header via `image-webp`.

use std::io::{Cursor, Read, Write};

use image_webp::WebPDecoder;
use rff_core::{CodecId, Error, Packet, Rational, Result};
use rff_format::{Demuxer, Format, FormatRegistry, Input, Muxer, Output, Stream};

/// Register the WebP format into a [`FormatRegistry`].
pub fn register(registry: &mut FormatRegistry) {
    registry.register(Format {
        name: "webp",
        long_name: "WebP image",
        extensions: &["webp"],
        demuxer: Some(|input| Box::new(WebpDemuxer::new(input))),
        muxer: Some(|output| Box::new(WebpMuxer::new(output))),
        probe: Some(probe_webp),
    });
}

/// Sniff WebP: a RIFF file whose form type is `WEBP`.
fn probe_webp(data: &[u8]) -> i32 {
    if data.len() >= 12 && &data[0..4] == b"RIFF" && &data[8..12] == b"WEBP" {
        100
    } else {
        0
    }
}

struct WebpDemuxer {
    input: Option<Input>,
    sample: Option<Vec<u8>>,
}

impl WebpDemuxer {
    fn new(input: Input) -> WebpDemuxer {
        WebpDemuxer {
            input: Some(input),
            sample: None,
        }
    }
}

impl Demuxer for WebpDemuxer {
    fn read_header(&mut self) -> Result<Vec<Stream>> {
        let mut input = self
            .input
            .take()
            .ok_or_else(|| Error::invalid("webp demux: header already read"))?;
        let mut buf = Vec::new();
        input.read_to_end(&mut buf)?;
        if probe_webp(&buf) == 0 {
            return Err(Error::invalid("webp demux: not a WebP file"));
        }
        let (width, height) = WebPDecoder::new(Cursor::new(&buf))
            .map(|d| d.dimensions())
            .map_err(|e| Error::invalid(format!("webp demux: {e}")))?;

        self.sample = Some(buf);
        let mut stream = Stream::new(0, CodecId::Webp);
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

struct WebpMuxer {
    out: Output,
}

impl WebpMuxer {
    fn new(out: Output) -> WebpMuxer {
        WebpMuxer { out }
    }
}

impl Muxer for WebpMuxer {
    fn write_header(&mut self, streams: &[Stream]) -> Result<()> {
        match streams.first() {
            Some(s) if s.codec_id == CodecId::Webp => Ok(()),
            Some(_) => Err(Error::unsupported("webp mux: only the `webp` codec is supported")),
            None => Err(Error::invalid("webp mux: no streams")),
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
    fn probe_recognizes_webp() {
        let mut riff = b"RIFF".to_vec();
        riff.extend_from_slice(&[0, 0, 0, 0]);
        riff.extend_from_slice(b"WEBP");
        assert_eq!(probe_webp(&riff), 100);
        assert_eq!(probe_webp(b"RIFF____AVI "), 0);
        assert_eq!(probe_webp(b"short"), 0);
    }
}
