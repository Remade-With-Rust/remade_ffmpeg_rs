//! YUV4MPEG2 (`.y4m`) raw-video container.
//!
//! `.y4m` is the standard interchange for raw video in codec testing (libvpx,
//! aomenc, x264 all consume it): a one-line ASCII header, then `FRAME\n` +
//! tightly-packed planar pixels per frame. This demuxer feeds those frames to
//! the [`rawvideo`](rff-codec-rawvideo) codec, which lets the CLI encode real
//! uncompressed clips (the input path the VP9 RD campaign needs).
//!
//! Supports 8-bit planar `C420*` → yuv420p, `C422` → yuv422p, `C444` → yuv444p.

use std::io::{Read, Write};

use rff_core::{CodecId, ColorRange, Error, Packet, PixelFormat, Rational, Result};
use rff_format::{Demuxer, Format, FormatRegistry, Input, Muxer, Output, Stream};

/// Register the y4m format into a [`FormatRegistry`].
pub fn register(registry: &mut FormatRegistry) {
    registry.register(Format {
        name: "yuv4mpegpipe",
        long_name: "YUV4MPEG2 (raw planar video)",
        extensions: &["y4m"],
        demuxer: Some(|input| Box::new(Y4mDemuxer::new(input))),
        muxer: Some(|output| Box::new(Y4mMuxer::new(output))),
        probe: Some(probe_y4m),
    });
}

const MAGIC: &[u8] = b"YUV4MPEG2";

/// Sniff y4m: the file begins with the `YUV4MPEG2` signature.
fn probe_y4m(data: &[u8]) -> i32 {
    if data.len() >= MAGIC.len() && &data[..MAGIC.len()] == MAGIC {
        100
    } else {
        0
    }
}

/// Map a y4m colorspace tag (the `C…` parameter) to a [`PixelFormat`].
/// Absent tag defaults to 4:2:0 (the y4m default).
fn colorspace(tag: Option<&str>) -> Result<PixelFormat> {
    match tag {
        None => Ok(PixelFormat::Yuv420p),
        Some(c) if c.starts_with("420") => Ok(PixelFormat::Yuv420p),
        Some("422") => Ok(PixelFormat::Yuv422p),
        Some("444") => Ok(PixelFormat::Yuv444p),
        Some(other) => Err(Error::unsupported(format!(
            "y4m: colorspace `C{other}` (only 8-bit 420/422/444)"
        ))),
    }
}

/// Bytes in one packed frame for `format` at `w`×`h`.
fn frame_bytes(format: PixelFormat, w: usize, h: usize) -> usize {
    let (cw, ch) = (w.div_ceil(2), h.div_ceil(2));
    match format {
        PixelFormat::Yuv420p => w * h + 2 * cw * ch,
        PixelFormat::Yuv422p => w * h + 2 * cw * h,
        PixelFormat::Yuv444p => 3 * w * h,
        _ => 0,
    }
}

// ---------------------------------------------------------------------------
// Demuxer
// ---------------------------------------------------------------------------

struct Y4mDemuxer {
    input: Option<Input>,
    buf: Vec<u8>,
    pos: usize,
    frame_len: usize,
    next_pts: i64,
}

impl Y4mDemuxer {
    fn new(input: Input) -> Y4mDemuxer {
        Y4mDemuxer {
            input: Some(input),
            buf: Vec::new(),
            pos: 0,
            frame_len: 0,
            next_pts: 0,
        }
    }
}

impl Demuxer for Y4mDemuxer {
    fn read_header(&mut self) -> Result<Vec<Stream>> {
        let mut input = self
            .input
            .take()
            .ok_or_else(|| Error::invalid("y4m demux: header already read"))?;
        input.read_to_end(&mut self.buf)?;
        if probe_y4m(&self.buf) == 0 {
            return Err(Error::invalid("y4m demux: missing YUV4MPEG2 signature"));
        }
        // Header is the first line, terminated by '\n'.
        let nl = self
            .buf
            .iter()
            .position(|&b| b == b'\n')
            .ok_or_else(|| Error::invalid("y4m demux: unterminated header"))?;
        let header = std::str::from_utf8(&self.buf[MAGIC.len()..nl])
            .map_err(|_| Error::invalid("y4m demux: non-UTF8 header"))?;

        let (mut w, mut h) = (0u32, 0u32);
        let (mut fnum, mut fden) = (30i32, 1i32); // default 30 fps
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
                // X is the extension namespace; XCOLORRANGE carries the value
                // range. Without it a full-range stream reads as limited.
                "X" => {
                    if let Some(v) = val.strip_prefix("COLORRANGE=") {
                        range = ColorRange::from_y4m_tag(v);
                    }
                }
                _ => {} // I (interlace), A (aspect), other X comments — ignored
            }
        }
        if w == 0 || h == 0 {
            return Err(Error::invalid("y4m demux: missing/zero W or H"));
        }
        let format = colorspace(cs.as_deref())?;
        self.pos = nl + 1;
        self.frame_len = frame_bytes(format, w as usize, h as usize);

        let mut stream = Stream::new(0, CodecId::RawVideo);
        stream.width = w;
        stream.height = h;
        stream.pixel_format = Some(format);
        stream.color_range = range;
        // time_base = seconds per tick = fden/fnum; pts counts frames.
        stream.time_base = Rational::new(fden.max(1), fnum.max(1));
        Ok(vec![stream])
    }

    fn read_packet(&mut self) -> Result<Packet> {
        if self.pos >= self.buf.len() {
            return Err(Error::Eof);
        }
        // Each frame starts with a `FRAME[ params]\n` line.
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

// ---------------------------------------------------------------------------
// Muxer (rff decode → .y4m, useful for producing references)
// ---------------------------------------------------------------------------

struct Y4mMuxer {
    out: Output,
    header_written: bool,
}

impl Y4mMuxer {
    fn new(out: Output) -> Y4mMuxer {
        Y4mMuxer {
            out,
            header_written: false,
        }
    }
}

impl Muxer for Y4mMuxer {
    fn write_header(&mut self, streams: &[Stream]) -> Result<()> {
        let s = streams
            .first()
            .filter(|s| s.codec_id == CodecId::RawVideo)
            .ok_or_else(|| Error::unsupported("y4m mux: needs a single `rawvideo` stream"))?;
        // A compressed source (e.g. VP9) leaves the stream's pixel_format unset; the
        // decoded frames are 8-bit planar 4:2:0, so default to that.
        let format = s.pixel_format.unwrap_or(PixelFormat::Yuv420p);
        let cs = match format {
            PixelFormat::Yuv420p => "420mpeg2",
            PixelFormat::Yuv422p => "422",
            PixelFormat::Yuv444p => "444",
            other => {
                return Err(Error::unsupported(format!(
                    "y4m mux: pixel format `{}`",
                    other.name()
                )))
            }
        };
        // fps num:den = 1/time_base = time_base.den : time_base.num.
        let (fnum, fden) = (s.time_base.den.max(1), s.time_base.num.max(1));
        // Label the value range when we know it. Omitting this on full-range
        // samples is not neutral — readers default to limited range, which
        // rescales the data. Measured on a JPEG decode: the same byte-identical
        // payload scored 41.3 dB compared as raw planes and 29.0 dB compared
        // through an untagged y4m, i.e. the missing tag alone invented 12 dB of
        // error. FFmpeg writes `XCOLORRANGE=FULL` here for JPEG sources.
        let range = match s.color_range.y4m_tag() {
            Some(tag) => format!(" XCOLORRANGE={tag}"),
            None => String::new(),
        };
        let header = format!(
            "YUV4MPEG2 W{} H{} F{}:{} Ip A1:1 C{}{}\n",
            s.width, s.height, fnum, fden, cs, range
        );
        self.out.write_all(header.as_bytes())?;
        self.header_written = true;
        Ok(())
    }

    fn write_packet(&mut self, packet: &Packet) -> Result<()> {
        if !self.header_written {
            return Err(Error::invalid("y4m mux: write_header not called"));
        }
        self.out.write_all(b"FRAME\n")?;
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

    fn sample_y4m() -> Vec<u8> {
        // 4x2 yuv420p: frame = 8 + 2 + 2 = 12 bytes.
        let mut f = b"YUV4MPEG2 W4 H2 F30:1 Ip A1:1 C420mpeg2\n".to_vec();
        f.extend_from_slice(b"FRAME\n");
        f.extend_from_slice(&(0u8..12).collect::<Vec<u8>>());
        f.extend_from_slice(b"FRAME\n");
        f.extend_from_slice(&(12u8..24).collect::<Vec<u8>>());
        f
    }

    #[test]
    fn demux_parses_header_and_frames() {
        let mut dem = Y4mDemuxer::new(Box::new(Cursor::new(sample_y4m())));
        let streams = dem.read_header().unwrap();
        assert_eq!(streams[0].codec_id, CodecId::RawVideo);
        assert_eq!((streams[0].width, streams[0].height), (4, 2));
        assert_eq!(streams[0].pixel_format, Some(PixelFormat::Yuv420p));
        let p0 = dem.read_packet().unwrap();
        assert_eq!(p0.data, (0u8..12).collect::<Vec<u8>>());
        assert_eq!(p0.pts, Some(0));
        let p1 = dem.read_packet().unwrap();
        assert_eq!(p1.data, (12u8..24).collect::<Vec<u8>>());
        assert_eq!(p1.pts, Some(1));
        assert!(matches!(dem.read_packet(), Err(Error::Eof)));
    }

    #[test]
    fn probe_detects_signature() {
        assert_eq!(probe_y4m(b"YUV4MPEG2 W1 H1"), 100);
        assert_eq!(probe_y4m(b"RIFFxxxx"), 0);
    }

    /// An untagged header must not be *assumed* full-range.
    #[test]
    fn demux_defaults_color_range_to_unspecified() {
        let mut dem = Y4mDemuxer::new(Box::new(Cursor::new(sample_y4m())));
        let streams = dem.read_header().unwrap();
        assert_eq!(streams[0].color_range, ColorRange::Unspecified);
    }

    #[test]
    fn demux_reads_xcolorrange() {
        for (tag, want) in [("FULL", ColorRange::Full), ("LIMITED", ColorRange::Limited)] {
            let mut f =
                format!("YUV4MPEG2 W4 H2 F30:1 Ip A1:1 C420mpeg2 XCOLORRANGE={tag}\n").into_bytes();
            f.extend_from_slice(b"FRAME\n");
            f.extend_from_slice(&(0u8..12).collect::<Vec<u8>>());
            let mut dem = Y4mDemuxer::new(Box::new(Cursor::new(f)));
            let streams = dem.read_header().unwrap();
            assert_eq!(streams[0].color_range, want, "XCOLORRANGE={tag}");
            // The tag must not disturb frame parsing.
            assert_eq!(dem.read_packet().unwrap().data.len(), 12);
        }
    }

    /// Regression: full-range samples written without `XCOLORRANGE=FULL` are
    /// read back as limited-range and rescaled. On a real 1080p JPEG decode that
    /// turned a byte-identical payload into a 12 dB PSNR deficit (41.26 dB
    /// compared as raw planes vs 29.04 dB through the untagged container).
    #[test]
    fn mux_labels_the_color_range() {
        for (range, want) in [
            (ColorRange::Full, Some("XCOLORRANGE=FULL")),
            (ColorRange::Limited, Some("XCOLORRANGE=LIMITED")),
            (ColorRange::Unspecified, None),
        ] {
            let mut s = Stream::new(0, CodecId::RawVideo);
            s.width = 4;
            s.height = 2;
            s.pixel_format = Some(PixelFormat::Yuv420p);
            s.color_range = range;

            // `Output` is a boxed trait object, so share the buffer to read it back.
            let buf = std::sync::Arc::new(std::sync::Mutex::new(Vec::<u8>::new()));
            struct Shared(std::sync::Arc<std::sync::Mutex<Vec<u8>>>);
            impl Write for Shared {
                fn write(&mut self, b: &[u8]) -> std::io::Result<usize> {
                    self.0.lock().unwrap().extend_from_slice(b);
                    Ok(b.len())
                }
                fn flush(&mut self) -> std::io::Result<()> {
                    Ok(())
                }
            }
            let mut mux = Y4mMuxer::new(Box::new(Shared(buf.clone())));
            mux.write_header(&[s]).unwrap();
            let header = String::from_utf8(buf.lock().unwrap().clone()).unwrap();

            match want {
                Some(tag) => assert!(header.contains(tag), "{range:?} -> {header:?}"),
                None => assert!(!header.contains("XCOLORRANGE"), "{range:?} -> {header:?}"),
            }
        }
    }
}
