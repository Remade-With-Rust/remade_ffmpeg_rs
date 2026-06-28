//! JPEG single-image container.
//!
//! A JPEG file *is* its codec bitstream, so the demuxer hands the whole file to
//! the [`mjpeg`](rff-codec-jpeg) codec as one packet and the muxer writes a
//! codec packet straight out. Dimensions for `ffprobe` come from a light scan
//! of the JPEG marker segments for the frame header (`SOFn`).

use std::io::{Read, Write};

use rff_core::{CodecId, Error, Packet, Rational, Result};
use rff_format::{Demuxer, Format, FormatRegistry, Input, Muxer, Output, Stream};

/// Register the JPEG format into a [`FormatRegistry`].
pub fn register(registry: &mut FormatRegistry) {
    registry.register(Format {
        name: "jpeg",
        long_name: "JPEG (JFIF) image",
        extensions: &["jpg", "jpeg"],
        demuxer: Some(|input| Box::new(JpegDemuxer::new(input))),
        muxer: Some(|output| Box::new(JpegMuxer::new(output))),
        probe: Some(probe_jpeg),
    });
}

/// Sniff JPEG by its start-of-image marker (`FF D8 FF`).
fn probe_jpeg(data: &[u8]) -> i32 {
    if data.len() >= 3 && data[0] == 0xFF && data[1] == 0xD8 && data[2] == 0xFF {
        100
    } else {
        0
    }
}

/// Scan JPEG marker segments for a frame header (`SOF0..SOF15`, minus the
/// non-frame `C4`/`C8`/`CC`) and return `(width, height)`.
fn jpeg_dimensions(buf: &[u8]) -> Option<(u32, u32)> {
    if buf.len() < 2 || buf[0] != 0xFF || buf[1] != 0xD8 {
        return None;
    }
    let mut i = 2;
    while i + 1 < buf.len() {
        if buf[i] != 0xFF {
            i += 1;
            continue;
        }
        let marker = buf[i + 1];
        // Standalone markers (no length): padding, SOI/EOI, RSTn, TEM.
        if marker == 0xFF || marker == 0x01 || (0xD0..=0xD9).contains(&marker) {
            i += 2;
            continue;
        }
        // SOFn frame header: FF, marker, len(2), precision(1), height(2), width(2).
        if (0xC0..=0xCF).contains(&marker) && marker != 0xC4 && marker != 0xC8 && marker != 0xCC {
            if i + 9 > buf.len() {
                return None;
            }
            let h = u16::from_be_bytes([buf[i + 5], buf[i + 6]]) as u32;
            let w = u16::from_be_bytes([buf[i + 7], buf[i + 8]]) as u32;
            return Some((w, h));
        }
        // Any other marker carries a 2-byte length we skip over.
        if i + 4 > buf.len() {
            return None;
        }
        let len = u16::from_be_bytes([buf[i + 2], buf[i + 3]]) as usize;
        i += 2 + len;
    }
    None
}

struct JpegDemuxer {
    input: Option<Input>,
    sample: Option<Vec<u8>>,
}

impl JpegDemuxer {
    fn new(input: Input) -> JpegDemuxer {
        JpegDemuxer {
            input: Some(input),
            sample: None,
        }
    }
}

impl Demuxer for JpegDemuxer {
    fn read_header(&mut self) -> Result<Vec<Stream>> {
        let mut input = self
            .input
            .take()
            .ok_or_else(|| Error::invalid("jpeg demux: header already read"))?;
        let mut buf = Vec::new();
        input.read_to_end(&mut buf)?;

        let (width, height) =
            jpeg_dimensions(&buf).ok_or_else(|| Error::invalid("jpeg demux: not a JPEG file"))?;

        self.sample = Some(buf);
        let mut stream = Stream::new(0, CodecId::Jpeg);
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

struct JpegMuxer {
    out: Output,
}

impl JpegMuxer {
    fn new(out: Output) -> JpegMuxer {
        JpegMuxer { out }
    }
}

impl Muxer for JpegMuxer {
    fn write_header(&mut self, streams: &[Stream]) -> Result<()> {
        match streams.first() {
            Some(s) if s.codec_id == CodecId::Jpeg => Ok(()),
            Some(_) => Err(Error::unsupported(
                "jpeg mux: only the `mjpeg` codec is supported",
            )),
            None => Err(Error::invalid("jpeg mux: no streams")),
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

    /// A minimal baseline-JPEG header: SOI + a SOF0 declaring 5×7.
    fn jpeg_header_5x7() -> Vec<u8> {
        let mut v = vec![0xFF, 0xD8]; // SOI
                                      // SOF0: marker, len=17, precision=8, height=7, width=5, 3 components...
        v.extend_from_slice(&[0xFF, 0xC0, 0x00, 0x11, 0x08]);
        v.extend_from_slice(&7u16.to_be_bytes());
        v.extend_from_slice(&5u16.to_be_bytes());
        v.extend_from_slice(&[3, 1, 0x22, 0, 2, 0x11, 0, 3, 0x11, 0]); // components
        v
    }

    #[test]
    fn sniffs_and_reads_jpeg_dimensions() {
        let jpg = jpeg_header_5x7();
        assert_eq!(probe_jpeg(&jpg), 100);
        assert_eq!(probe_jpeg(b"\xFF\xD8\xEEnope"), 0);

        let mut dem = JpegDemuxer::new(Box::new(Cursor::new(jpg.clone())));
        let streams = dem.read_header().unwrap();
        assert_eq!(streams[0].codec_id, CodecId::Jpeg);
        assert_eq!((streams[0].width, streams[0].height), (5, 7));
        assert_eq!(dem.read_packet().unwrap().data, jpg);
        assert!(matches!(dem.read_packet(), Err(Error::Eof)));
    }
}
