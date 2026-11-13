//! YUV4MPEG2 (`.y4m`) raw-video container.
//!
//! `.y4m` is the standard interchange for raw video in codec testing (libvpx,
//! aomenc, x264 all consume it): a one-line ASCII header, then `FRAME\n` +
//! tightly-packed planar pixels per frame. This demuxer feeds those frames to
//! the [`rawvideo`](rff-codec-rawvideo) codec, which lets the CLI encode real
//! uncompressed clips (the input path the VP9 RD campaign needs).
//!
//! Supports 8-bit planar `C420*` → yuv420p, `C422` → yuv422p, `C444` → yuv444p.

use std::io::{BufRead, BufReader, Read, Write};

use rff_core::{CodecId, ColorRange, Error, Packet, PixelFormat, Rational, Result};
use rff_format::{Demuxer, Format, FormatRegistry, Input, MuxCaps, Muxer, Output, Stream};

/// Register the y4m format into a [`FormatRegistry`].
pub fn register(registry: &mut FormatRegistry) {
    registry.register(Format {
        name: "yuv4mpegpipe",
        long_name: "YUV4MPEG2 (raw planar video)",
        extensions: &["y4m"],
        demuxer: Some(|input| Box::new(Y4mDemuxer::new(input))),
        muxer: Some(|output| Box::new(Y4mMuxer::new(output))),
        muxer_path: None,
        probe: Some(probe_y4m),
        mux_caps: MuxCaps::single(&[CodecId::RawVideo]),
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

/// Largest frame the demuxer accepts (1 GiB — 16K×16K 4:4:4 is 768 MiB). A
/// header declaring more is rejected instead of trusted.
const MAX_FRAME_BYTES: u64 = 1 << 30;

/// Bytes in one packed frame for `format` at `w`×`h`, overflow-checked and
/// capped at [`MAX_FRAME_BYTES`].
fn frame_bytes(format: PixelFormat, w: usize, h: usize) -> Result<usize> {
    let (w, h) = (w as u64, h as u64);
    let (cw, ch) = (w.div_ceil(2), h.div_ceil(2));
    // luma + 2 * chroma, every step checked.
    let planes = |chroma: Option<u64>| w.checked_mul(h)?.checked_add(chroma?.checked_mul(2)?);
    let total = match format {
        PixelFormat::Yuv420p => planes(cw.checked_mul(ch)),
        PixelFormat::Yuv422p => planes(cw.checked_mul(h)),
        PixelFormat::Yuv444p => planes(w.checked_mul(h)),
        _ => Some(0),
    };
    match total {
        Some(n) if n <= MAX_FRAME_BYTES => Ok(n as usize),
        _ => Err(Error::unsupported(format!(
            "y4m demux: {w}x{h} frame exceeds {MAX_FRAME_BYTES} bytes"
        ))),
    }
}

/// Longest header line accepted. Real headers are well under 100 bytes; the
/// cap keeps `read_header`'s cost bounded on a source with no newline.
const MAX_HEADER_LINE: u64 = 16 * 1024;
/// Longest `FRAME[ params]` line accepted.
const MAX_FRAME_LINE: u64 = 4 * 1024;

/// Read one line (through its `\n`), at most `cap` bytes. Empty at end of input;
/// no trailing `\n` means the line hit the cap or the end of input.
fn read_line(r: &mut BufReader<Input>, cap: u64) -> std::io::Result<Vec<u8>> {
    let mut line = Vec::new();
    r.by_ref().take(cap).read_until(b'\n', &mut line)?;
    Ok(line)
}

/// Read up to `len` bytes, growing the buffer only as bytes actually arrive
/// (in bounded steps), so a frame size from the header cannot force a huge
/// allocation on a short input.
fn read_up_to(r: &mut impl Read, len: usize) -> std::io::Result<Vec<u8>> {
    const STEP: usize = 8 << 20;
    let mut v = Vec::new();
    while v.len() < len {
        let want = (len - v.len()).min(STEP);
        v.reserve_exact(want);
        let got = r.by_ref().take(want as u64).read_to_end(&mut v)?;
        if got < want {
            break; // end of input
        }
    }
    Ok(v)
}

// ---------------------------------------------------------------------------
// Demuxer
// ---------------------------------------------------------------------------

/// Streaming y4m demuxer: the header line is read in `read_header`, then each
/// `FRAME` line + one frame of pixels on demand in `read_packet`, so peak
/// memory is one frame, not the clip (issue #26). Works on pipes.
struct Y4mDemuxer {
    /// The source before `read_header` takes it.
    input: Option<Input>,
    /// The source after the header, positioned at the next `FRAME` line.
    reader: Option<BufReader<Input>>,
    frame_len: usize,
    next_pts: i64,
    /// Sticky failure: a malformed frame is reported again on every later call
    /// (as the whole-buffer demuxer did), never skipped past.
    failed: Option<&'static str>,
}

impl Y4mDemuxer {
    fn new(input: Input) -> Y4mDemuxer {
        Y4mDemuxer {
            input: Some(input),
            reader: None,
            frame_len: 0,
            next_pts: 0,
            failed: None,
        }
    }

    fn fail(&mut self, msg: &'static str) -> Error {
        self.failed = Some(msg);
        Error::invalid(msg)
    }
}

impl Demuxer for Y4mDemuxer {
    fn read_header(&mut self) -> Result<Vec<Stream>> {
        let input = self
            .input
            .take()
            .ok_or_else(|| Error::invalid("y4m demux: header already read"))?;
        let mut reader = BufReader::new(input);
        // Header is the first line, terminated by '\n'.
        let line = read_line(&mut reader, MAX_HEADER_LINE)?;
        if probe_y4m(&line) == 0 {
            return Err(Error::invalid("y4m demux: missing YUV4MPEG2 signature"));
        }
        if line.last() != Some(&b'\n') {
            return Err(Error::invalid(if line.len() as u64 >= MAX_HEADER_LINE {
                "y4m demux: header line too long"
            } else {
                "y4m demux: unterminated header"
            }));
        }
        let header = std::str::from_utf8(&line[MAGIC.len()..line.len() - 1])
            .map_err(|_| Error::invalid("y4m demux: non-UTF8 header"))?;

        let (mut w, mut h) = (0u32, 0u32);
        let (mut fnum, mut fden) = (30i32, 1i32); // default 30 fps
        let mut cs: Option<String> = None;
        let mut range = ColorRange::Unspecified;
        for tok in header.split_whitespace() {
            // Split off the first *character* (a byte split panics on UTF-8).
            let mut chars = tok.chars();
            let tag = chars.next();
            let val = chars.as_str();
            match tag {
                Some('W') => w = val.parse().map_err(|_| Error::invalid("y4m: bad width"))?,
                Some('H') => h = val.parse().map_err(|_| Error::invalid("y4m: bad height"))?,
                Some('F') => {
                    let (n, d) = val
                        .split_once(':')
                        .ok_or_else(|| Error::invalid("y4m: bad framerate"))?;
                    fnum = n.parse().map_err(|_| Error::invalid("y4m: bad fps num"))?;
                    fden = d.parse().map_err(|_| Error::invalid("y4m: bad fps den"))?;
                }
                Some('C') => cs = Some(val.to_string()),
                // X is the extension namespace; XCOLORRANGE carries the value
                // range. Without it a full-range stream reads as limited.
                Some('X') => {
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
        self.frame_len = frame_bytes(format, w as usize, h as usize)?;
        self.reader = Some(reader);

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
        if let Some(msg) = self.failed {
            return Err(Error::invalid(msg));
        }
        let frame_len = self.frame_len;
        let reader = self.reader.as_mut().ok_or(Error::Eof)?;
        // Each frame starts with a `FRAME[ params]\n` line.
        let line = read_line(reader, MAX_FRAME_LINE)?;
        if line.is_empty() {
            return Err(Error::Eof);
        }
        if !line.starts_with(b"FRAME") {
            return Err(self.fail("y4m demux: expected FRAME marker"));
        }
        if line.last() != Some(&b'\n') {
            return Err(self.fail("y4m demux: unterminated FRAME header"));
        }
        let data = read_up_to(reader, frame_len)?;
        if data.len() < frame_len {
            return Err(self.fail("y4m demux: truncated frame data"));
        }
        let mut packet = Packet::from_data(0, data);
        packet.pts = Some(self.next_pts);
        self.next_pts += 1;
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
mod legacy;

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

    // ---- streaming (issue #26): equivalence, header cost, robustness ----

    use std::sync::atomic::{AtomicU64, Ordering};
    use std::sync::Arc;

    /// A reader that counts the bytes pulled from it and hands out at most
    /// `chunk` bytes per `read` (a pipe-like source).
    struct Counting {
        inner: Cursor<Vec<u8>>,
        read: Arc<AtomicU64>,
        chunk: usize,
    }
    impl Read for Counting {
        fn read(&mut self, buf: &mut [u8]) -> std::io::Result<usize> {
            let n = buf.len().min(self.chunk);
            let got = self.inner.read(&mut buf[..n])?;
            self.read.fetch_add(got as u64, Ordering::Relaxed);
            Ok(got)
        }
    }
    fn counting(data: Vec<u8>, chunk: usize) -> (Input, Arc<AtomicU64>) {
        let read = Arc::new(AtomicU64::new(0));
        let r = Counting {
            inner: Cursor::new(data),
            read: read.clone(),
            chunk,
        };
        (Box::new(r), read)
    }

    type Sig = (
        usize,
        Option<i64>,
        Option<i64>,
        i64,
        (i32, i32),
        bool,
        Vec<u8>,
    );
    /// Everything a demuxer reports: the header (or its error), every packet,
    /// and how the packet loop ended. Bounded so a looping demuxer fails.
    fn drive(mut d: Box<dyn Demuxer>) -> (String, Vec<Sig>, String) {
        let head = match d.read_header() {
            Ok(s) => format!("{s:?}"),
            Err(e) => return (format!("Err({e})"), Vec::new(), String::new()),
        };
        let mut packets = Vec::new();
        for _ in 0..1_000_000 {
            match d.read_packet() {
                Ok(p) => packets.push((
                    p.stream_index,
                    p.pts,
                    p.dts,
                    p.duration,
                    (p.time_base.num, p.time_base.den),
                    p.flags.keyframe,
                    p.data,
                )),
                Err(e) => {
                    // The end is sticky: asking again says the same thing.
                    let again = d.read_packet().err().map(|e| e.to_string());
                    assert_eq!(again, Some(e.to_string()), "end not sticky");
                    return (head, packets, e.to_string());
                }
            }
        }
        panic!("demuxer did not terminate");
    }
    fn assert_same_as_legacy(name: &str, file: &[u8], chunks: &[usize]) {
        let old = drive(Box::new(legacy::LegacyY4mDemuxer::new(Box::new(
            Cursor::new(file.to_vec()),
        ))));
        for &chunk in chunks {
            let (input, _) = counting(file.to_vec(), chunk);
            let new = drive(Box::new(Y4mDemuxer::new(input)));
            assert_eq!(new.0, old.0, "{name}: header (chunk {chunk})");
            assert_eq!(new.1.len(), old.1.len(), "{name}: packet count");
            assert!(new.1 == old.1, "{name}: packets differ (chunk {chunk})");
            assert_eq!(new.2, old.2, "{name}: end (chunk {chunk})");
        }
    }

    /// A y4m file: `header` (without newline), `n` frames of `len` bytes each
    /// behind `frame_line`, then `tail`.
    fn y4m(header: &str, frame_line: &str, len: usize, n: usize, tail: &[u8]) -> Vec<u8> {
        let mut f = format!("{header}\n").into_bytes();
        for i in 0..n {
            f.extend_from_slice(frame_line.as_bytes());
            f.push(b'\n');
            f.extend((0..len).map(|j| (i * 7 + j) as u8));
        }
        f.extend_from_slice(tail);
        f
    }

    #[test]
    fn streaming_matches_whole_buffer_demuxer() {
        let h420 = "YUV4MPEG2 W4 H2 F30:1 Ip A1:1 C420mpeg2";
        let cases: Vec<(&str, Vec<u8>)> = vec![
            ("sample", sample_y4m()),
            (
                "422",
                y4m("YUV4MPEG2 W6 H4 F25:1 C422", "FRAME", 48, 3, &[]),
            ),
            (
                "444",
                y4m("YUV4MPEG2 W6 H4 F24000:1001 C444", "FRAME", 72, 2, &[]),
            ),
            (
                "odd dims",
                y4m("YUV4MPEG2 W5 H3 C420jpeg", "FRAME", 15 + 2 * 6, 3, &[]),
            ),
            ("no C", y4m("YUV4MPEG2 W4 H2", "FRAME", 12, 2, &[])),
            (
                "range+params",
                y4m(
                    "YUV4MPEG2 W4 H2 F30:1 C420 XYSCSS=420JPEG XCOLORRANGE=FULL",
                    "FRAME Ip XFOO=1",
                    12,
                    3,
                    &[],
                ),
            ),
            ("header only", y4m(h420, "FRAME", 12, 0, &[])),
            ("partial frame", y4m(h420, "FRAME", 12, 2, b"FRAME\n12345")),
            ("partial marker", y4m(h420, "FRAME", 12, 2, b"FRA")),
            ("unterminated marker", y4m(h420, "FRAME", 12, 1, b"FRAME")),
            ("garbage after", y4m(h420, "FRAME", 12, 1, b"XYZ\n")),
            ("no signature", b"YUV4MPEG W4 H2\nFRAME\n".to_vec()),
            ("unterminated header", b"YUV4MPEG2 W4 H2".to_vec()),
            ("zero W", y4m("YUV4MPEG2 W0 H2", "FRAME", 12, 1, &[])),
            ("bad W", y4m("YUV4MPEG2 Wx H2", "FRAME", 12, 1, &[])),
            ("bad cs", y4m("YUV4MPEG2 W4 H2 C411", "FRAME", 12, 1, &[])),
            (
                "huge dims",
                y4m("YUV4MPEG2 W4000000000 H4000000000", "FRAME", 1, 1, &[]),
            ),
            ("non-utf8", b"YUV4MPEG2 W4 H2 \xff\xfe\nFRAME\n".to_vec()),
            ("empty", Vec::new()),
        ];
        for (name, file) in &cases {
            assert_same_as_legacy(name, file, &[1, 7, usize::MAX]);
        }
        // Real files, when the checkout has them.
        let root = std::path::Path::new(env!("CARGO_MANIFEST_DIR")).join("../..");
        for rel in [
            "video-tests/results/av1/_tmp/mob30.y4m",
            "hevc-vectors/.tmp/ref_t.mp4.y4m",
            "hevc-vectors/.tmp/ours_t10.y4m",
        ] {
            if let Ok(file) = std::fs::read(root.join(rel)) {
                assert_same_as_legacy(rel, &file, &[4093, usize::MAX]);
            }
        }
    }

    #[test]
    fn read_header_reads_only_the_header() {
        // 20 CIF frames, ~3 MB: the header must not pull the clip in.
        let file = y4m(
            "YUV4MPEG2 W352 H288 F30:1 C420jpeg",
            "FRAME",
            152_064,
            20,
            &[],
        );
        let (input, read) = counting(file, usize::MAX);
        let mut dem = Y4mDemuxer::new(input);
        dem.read_header().unwrap();
        assert!(
            read.load(Ordering::Relaxed) < 64 * 1024,
            "header read too much"
        );
        // A packet pulls about one frame.
        assert_eq!(dem.read_packet().unwrap().data.len(), 152_064);
        assert!(read.load(Ordering::Relaxed) < 152_064 + 64 * 1024);
    }

    #[test]
    fn truncated_and_garbage_inputs_fail_cleanly() {
        // A header declaring 384 MiB frames over a tiny input: plausible, so
        // accepted, but the short frame is an error (not a 384 MiB
        // allocation), and the error is sticky.
        let file = y4m("YUV4MPEG2 W16384 H16384 C420", "FRAME", 10, 1, &[]);
        let mut dem = Y4mDemuxer::new(Box::new(Cursor::new(file)));
        dem.read_header().unwrap();
        assert!(matches!(dem.read_packet(), Err(Error::InvalidData(_))));
        assert!(matches!(dem.read_packet(), Err(Error::InvalidData(_))));

        // Absurd dimensions are refused up front, without overflow.
        for hdr in [
            "YUV4MPEG2 W4294967295 H4294967295 C444",
            "YUV4MPEG2 W99999 H99999",
        ] {
            let mut dem = Y4mDemuxer::new(Box::new(Cursor::new(y4m(hdr, "FRAME", 1, 1, &[]))));
            assert!(dem.read_header().is_err(), "{hdr}");
        }
        // A token starting with a multi-byte character used to panic.
        let file = y4m("YUV4MPEG2 W4 H2 \u{e9}x", "FRAME", 12, 1, &[]);
        let (_, packets, end) = drive(Box::new(Y4mDemuxer::new(Box::new(Cursor::new(file)))));
        assert_eq!((packets.len(), end.as_str()), (1, "end of stream"));
        // A header with no newline is bounded, not read to the end.
        let mut file = b"YUV4MPEG2 W4 H2 ".to_vec();
        file.resize(4 << 20, b'A');
        let (input, read) = counting(file, usize::MAX);
        let mut dem = Y4mDemuxer::new(input);
        assert!(dem.read_header().is_err());
        assert!(read.load(Ordering::Relaxed) < 64 * 1024);

        // Every truncation of a valid file, and garbage after a valid header:
        // never a panic, always an end.
        let good = y4m("YUV4MPEG2 W4 H2 C420", "FRAME", 12, 3, &[]);
        for cut in 0..good.len() {
            let _ = drive(Box::new(Y4mDemuxer::new(Box::new(Cursor::new(
                good[..cut].to_vec(),
            )))));
        }
        let mut x = 0x2545_F491_u32;
        for _ in 0..64 {
            let mut g = b"YUV4MPEG2 W4 H2 C420\nFRAME".to_vec();
            for _ in 0..300 {
                x ^= x << 13;
                x ^= x >> 17;
                x ^= x << 5;
                g.push(x as u8);
            }
            let _ = drive(Box::new(Y4mDemuxer::new(Box::new(Cursor::new(g)))));
        }
        // Reading packets before the header is just end-of-stream.
        let mut dem = Y4mDemuxer::new(Box::new(Cursor::new(good)));
        assert!(matches!(dem.read_packet(), Err(Error::Eof)));
    }
}
