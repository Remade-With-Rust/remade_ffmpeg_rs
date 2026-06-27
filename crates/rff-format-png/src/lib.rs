//! PNG single-image container.
//!
//! A PNG file *is* its codec bitstream — there's no wrapping container — so the
//! demuxer hands the whole file to the [`png`](rff-codec-png) codec as one
//! packet, and the muxer writes a codec packet straight out. Image dimensions
//! are read cheaply from the `IHDR` chunk for `ffprobe`.

use std::io::{Read, Write};

use rff_core::{CodecId, Error, Packet, Rational, Result};
use rff_format::{Demuxer, Format, FormatRegistry, Input, Muxer, Output, Stream};

/// The 8-byte PNG signature.
const SIGNATURE: [u8; 8] = [0x89, b'P', b'N', b'G', b'\r', b'\n', 0x1a, b'\n'];

/// Register the PNG format into a [`FormatRegistry`].
pub fn register(registry: &mut FormatRegistry) {
    registry.register(Format {
        name: "png",
        long_name: "PNG (Portable Network Graphics) image",
        extensions: &["png"],
        demuxer: Some(|input| Box::new(PngDemuxer::new(input))),
        muxer: Some(|output| Box::new(PngMuxer::new(output))),
        probe: Some(probe_png),
    });
}

/// Sniff PNG by its fixed 8-byte signature.
fn probe_png(data: &[u8]) -> i32 {
    if data.len() >= 8 && data[0..8] == SIGNATURE {
        100
    } else {
        0
    }
}

// ---------------------------------------------------------------------------
// Demuxer
// ---------------------------------------------------------------------------

struct PngDemuxer {
    input: Option<Input>,
    sample: Option<Vec<u8>>,
}

impl PngDemuxer {
    fn new(input: Input) -> PngDemuxer {
        PngDemuxer {
            input: Some(input),
            sample: None,
        }
    }
}

impl Demuxer for PngDemuxer {
    fn read_header(&mut self) -> Result<Vec<Stream>> {
        let mut input = self
            .input
            .take()
            .ok_or_else(|| Error::invalid("png demux: header already read"))?;
        let mut buf = Vec::new();
        input.read_to_end(&mut buf)?;

        if buf.len() < 24 || buf[0..8] != SIGNATURE || &buf[12..16] != b"IHDR" {
            return Err(Error::invalid("png demux: not a PNG file"));
        }
        // IHDR: width @16, height @20 (big-endian), right after the 8-byte
        // signature and the `len`+`IHDR` chunk header.
        let width = u32::from_be_bytes([buf[16], buf[17], buf[18], buf[19]]);
        let height = u32::from_be_bytes([buf[20], buf[21], buf[22], buf[23]]);

        self.sample = Some(buf);
        let mut stream = Stream::new(0, CodecId::Png);
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

// ---------------------------------------------------------------------------
// Muxer
// ---------------------------------------------------------------------------

struct PngMuxer {
    out: Output,
}

impl PngMuxer {
    fn new(out: Output) -> PngMuxer {
        PngMuxer { out }
    }
}

impl Muxer for PngMuxer {
    fn write_header(&mut self, streams: &[Stream]) -> Result<()> {
        match streams.first() {
            Some(s) if s.codec_id == CodecId::Png => Ok(()),
            Some(_) => Err(Error::unsupported("png mux: only the `png` codec is supported")),
            None => Err(Error::invalid("png mux: no streams")),
        }
    }

    fn write_packet(&mut self, packet: &Packet) -> Result<()> {
        // The packet already is a complete PNG file; write it through.
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
    use std::sync::{Arc, Mutex};

    #[derive(Clone)]
    struct SharedBuf(Arc<Mutex<Vec<u8>>>);
    impl Write for SharedBuf {
        fn write(&mut self, buf: &[u8]) -> std::io::Result<usize> {
            self.0.lock().unwrap().extend_from_slice(buf);
            Ok(buf.len())
        }
        fn flush(&mut self) -> std::io::Result<()> {
            Ok(())
        }
    }

    /// A tiny but valid PNG (2×1, RGB) built with the `png` crate.
    fn tiny_png() -> Vec<u8> {
        let mut out = Vec::new();
        let mut enc = png::Encoder::new(&mut out, 2, 1);
        enc.set_color(png::ColorType::Rgb);
        enc.set_depth(png::BitDepth::Eight);
        let mut w = enc.write_header().unwrap();
        w.write_image_data(&[1, 2, 3, 4, 5, 6]).unwrap();
        drop(w);
        out
    }

    #[test]
    fn sniffs_and_demuxes_png() {
        let png = tiny_png();
        assert_eq!(probe_png(&png), 100);
        assert_eq!(probe_png(b"not a png file!!"), 0);

        let mut dem = PngDemuxer::new(Box::new(Cursor::new(png.clone())));
        let streams = dem.read_header().unwrap();
        assert_eq!(streams[0].codec_id, CodecId::Png);
        assert_eq!((streams[0].width, streams[0].height), (2, 1));
        assert_eq!(dem.read_packet().unwrap().data, png); // whole file as one packet
        assert!(matches!(dem.read_packet(), Err(Error::Eof)));
    }

    #[test]
    fn muxer_writes_packet_through() {
        let png = tiny_png();
        let sink = SharedBuf(Arc::new(Mutex::new(Vec::new())));
        {
            let mut mux = PngMuxer::new(Box::new(sink.clone()));
            mux.write_header(&[Stream::new(0, CodecId::Png)]).unwrap();
            mux.write_packet(&Packet::from_data(0, png.clone())).unwrap();
            mux.write_trailer().unwrap();
        }
        assert_eq!(*sink.0.lock().unwrap(), png);
    }
}
