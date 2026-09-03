//! Motion JPEG containers: a raw stream of concatenated JPEGs (`mjpeg`) and
//! the `multipart/x-mixed-replace` stream a camera's HTTP server pushes
//! (`mpjpeg`, ffmpeg's name for it) — `rff -i http://device/stream`.
//!
//! Both share one frame splitter, [`find_frame`], which walks the marker
//! segments rather than scanning for `FF D9`: an EXIF thumbnail is a whole
//! JPEG inside an `APP1` segment, and a byte scan would end the frame there.
//! Progressive files (several `SOS`) and restart markers are handled the same
//! way ffmpeg's `mjpeg` parser does, by the segment grammar.
//!
//! Timestamps: a raw stream has none, so packets are counted at
//! [`RAW_FRAMERATE`] (ffmpeg's default for raw MJPEG). A multipart part may
//! carry `X-Timestamp` (microseconds, as `rusty_esp_video` writes it), and
//! when the first part does, `pts` is taken from it on a `1/1_000_000` base.

use std::io::{Read, Write};

use rff_core::{CodecId, Error, Packet, Rational, Result};
use rff_format::{Demuxer, Format, FormatRegistry, Input, MuxCaps, Muxer, Output, Stream};

/// Frames per second assumed for a raw MJPEG stream, which carries no timing.
pub const RAW_FRAMERATE: i32 = 25;
/// Largest single JPEG we will buffer before calling the stream broken.
const MAX_FRAME: usize = 64 << 20;
/// Read granularity from the input.
const CHUNK: usize = 64 * 1024;
/// Boundary the multipart muxer writes.
const MUX_BOUNDARY: &str = "rff-frame";

/// Register `mjpeg` (raw) and `mpjpeg` (multipart) into a [`FormatRegistry`].
pub fn register(registry: &mut FormatRegistry) {
    registry.register(Format {
        name: "mjpeg",
        long_name: "raw MJPEG (concatenated JPEG frames)",
        extensions: &["mjpeg", "mjpg"],
        demuxer: Some(|input| Box::new(RawMjpegDemuxer::new(input))),
        muxer: Some(|output| Box::new(RawMjpegMuxer { out: output })),
        muxer_path: None,
        probe: Some(probe_raw_mjpeg),
        mux_caps: MuxCaps::single(&[CodecId::Jpeg]),
    });
    registry.register(Format {
        name: "mpjpeg",
        long_name: "MJPEG over HTTP (multipart/x-mixed-replace)",
        extensions: &[],
        demuxer: Some(|input| Box::new(MultipartDemuxer::new(input))),
        muxer: Some(|output| Box::new(MultipartMuxer { out: output })),
        muxer_path: None,
        probe: Some(probe_multipart),
        mux_caps: MuxCaps::single(&[CodecId::Jpeg]),
    });
}

/// A raw stream looks exactly like a single JPEG until the second frame, so
/// it only outscores the `jpeg` still-image format when a complete frame and
/// the start of another both fit in the probe window. Otherwise `-f mjpeg`
/// or the `.mjpeg` extension says so.
fn probe_raw_mjpeg(data: &[u8]) -> i32 {
    if !data.starts_with(&[0xFF, 0xD8, 0xFF]) {
        return 0;
    }
    match find_frame(data) {
        Ok(Some(end)) if data[end..].starts_with(&[0xFF, 0xD8, 0xFF]) => 100,
        _ => 0,
    }
}

/// The body of a multipart stream starts with the boundary line; the part
/// headers that follow name `image/jpeg`.
fn probe_multipart(data: &[u8]) -> i32 {
    let head = &data[..data.len().min(1024)];
    let start = head
        .iter()
        .position(|&b| b != b'\r' && b != b'\n')
        .unwrap_or(head.len());
    if !head[start..].starts_with(b"--") {
        return 0;
    }
    if contains_ignore_case(head, b"content-type:") && contains_ignore_case(head, b"image/jpeg") {
        100
    } else {
        0
    }
}

fn contains_ignore_case(hay: &[u8], needle: &[u8]) -> bool {
    hay.windows(needle.len())
        .any(|w| w.eq_ignore_ascii_case(needle))
}

/// Find the end of the JPEG that starts at `buf[0]` (which must be `SOI`).
///
/// `Ok(Some(end))` is the offset one past `EOI`; `Ok(None)` means the frame
/// is not complete yet (feed more bytes and try again); `Err` means the bytes
/// do not follow the marker grammar.
pub fn find_frame(buf: &[u8]) -> Result<Option<usize>> {
    if buf.len() < 2 {
        return Ok(None);
    }
    if buf[0] != 0xFF || buf[1] != 0xD8 {
        return Err(Error::invalid("mjpeg: frame does not start with SOI"));
    }
    let mut i = 2;
    loop {
        // Fill bytes before a marker are allowed (B.1.1.2).
        while i < buf.len() && buf[i] == 0xFF && buf.get(i + 1) == Some(&0xFF) {
            i += 1;
        }
        if i + 1 >= buf.len() {
            return Ok(None);
        }
        if buf[i] != 0xFF {
            return Err(Error::invalid("mjpeg: expected a marker"));
        }
        let marker = buf[i + 1];
        match marker {
            0xD9 => return Ok(Some(i + 2)),
            // Standalone markers: TEM, RSTn (outside a scan they are stray but harmless).
            0x01 | 0xD0..=0xD7 => {
                i += 2;
                continue;
            }
            // A new SOI where a segment should be: the previous frame had no EOI.
            0xD8 => return Ok(Some(i)),
            0x00 | 0xFF => return Err(Error::invalid("mjpeg: bad marker")),
            _ => {}
        }
        if i + 4 > buf.len() {
            return Ok(None);
        }
        let len = u16::from_be_bytes([buf[i + 2], buf[i + 3]]) as usize;
        if len < 2 {
            return Err(Error::invalid("mjpeg: bad segment length"));
        }
        i += 2 + len;
        if marker == 0xDA {
            // Entropy-coded data: runs to the next marker that is not stuffing
            // (FF 00) or a restart marker.
            loop {
                let Some(pos) = buf[i.min(buf.len())..].iter().position(|&b| b == 0xFF) else {
                    return Ok(None);
                };
                let at = i + pos;
                let Some(&next) = buf.get(at + 1) else {
                    return Ok(None);
                };
                match next {
                    0x00 | 0xD0..=0xD7 => i = at + 2,
                    0xFF => i = at + 1,
                    _ => {
                        i = at;
                        break;
                    }
                }
            }
        }
    }
}

/// Scan a JPEG's marker segments for a frame header (`SOF0..SOF15`, minus the
/// non-frame `C4`/`C8`/`CC`) and return `(width, height)`.
pub fn jpeg_dimensions(buf: &[u8]) -> Option<(u32, u32)> {
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
        if marker == 0xFF || marker == 0x01 || (0xD0..=0xD9).contains(&marker) {
            i += 2;
            continue;
        }
        if (0xC0..=0xCF).contains(&marker) && marker != 0xC4 && marker != 0xC8 && marker != 0xCC {
            if i + 9 > buf.len() {
                return None;
            }
            let h = u16::from_be_bytes([buf[i + 5], buf[i + 6]]) as u32;
            let w = u16::from_be_bytes([buf[i + 7], buf[i + 8]]) as u32;
            return Some((w, h));
        }
        if i + 4 > buf.len() {
            return None;
        }
        let len = u16::from_be_bytes([buf[i + 2], buf[i + 3]]) as usize;
        i += 2 + len;
    }
    None
}

/// Buffered input with "read more" on demand.
struct Feed {
    input: Input,
    buf: Vec<u8>,
    eof: bool,
}

impl Feed {
    fn new(input: Input) -> Feed {
        Feed {
            input,
            buf: Vec::new(),
            eof: false,
        }
    }

    /// Append up to one chunk; false at end of input.
    fn fill(&mut self) -> Result<bool> {
        if self.eof {
            return Ok(false);
        }
        let old = self.buf.len();
        self.buf.resize(old + CHUNK, 0);
        let n = loop {
            match self.input.read(&mut self.buf[old..]) {
                Ok(n) => break n,
                Err(e) if e.kind() == std::io::ErrorKind::Interrupted => continue,
                Err(e) => {
                    self.buf.truncate(old);
                    return Err(e.into());
                }
            }
        };
        self.buf.truncate(old + n);
        if n == 0 {
            self.eof = true;
        }
        Ok(n > 0)
    }

    fn consume(&mut self, n: usize) {
        self.buf.drain(..n);
    }

    /// Drop bytes up to the next `SOI`; false if none is buffered.
    fn skip_to_soi(&mut self) -> bool {
        match self.buf.windows(3).position(|w| w == [0xFF, 0xD8, 0xFF]) {
            Some(p) => {
                self.consume(p);
                true
            }
            None => {
                // Keep two bytes in case the marker straddles a chunk.
                let keep = self.buf.len().saturating_sub(2);
                self.consume(keep);
                false
            }
        }
    }

    /// The next complete JPEG at the head of the buffer, reading as needed.
    fn next_jpeg(&mut self) -> Result<Option<Vec<u8>>> {
        loop {
            while !self.skip_to_soi() {
                if !self.fill()? {
                    return Ok(None);
                }
            }
            match find_frame(&self.buf)? {
                Some(end) => {
                    let frame = self.buf[..end].to_vec();
                    self.consume(end);
                    return Ok(Some(frame));
                }
                None if self.buf.len() > MAX_FRAME => {
                    return Err(Error::invalid("mjpeg: frame larger than 64 MiB"));
                }
                None => {
                    if !self.fill()? {
                        return Ok(None);
                    }
                }
            }
        }
    }
}

// ---------------------------------------------------------------------------
// raw
// ---------------------------------------------------------------------------

/// Demuxes concatenated JPEGs, one packet per frame.
pub struct RawMjpegDemuxer {
    feed: Feed,
    first: Option<Vec<u8>>,
    index: i64,
}

impl RawMjpegDemuxer {
    /// Wrap an input.
    pub fn new(input: Input) -> RawMjpegDemuxer {
        RawMjpegDemuxer {
            feed: Feed::new(input),
            first: None,
            index: 0,
        }
    }

    fn packet(&mut self, data: Vec<u8>) -> Packet {
        let mut packet = Packet::from_data(0, data);
        packet.time_base = Rational::new(1, RAW_FRAMERATE);
        packet.pts = Some(self.index);
        packet.dts = Some(self.index);
        packet.duration = 1;
        packet.flags.keyframe = true;
        self.index += 1;
        packet
    }
}

impl Demuxer for RawMjpegDemuxer {
    fn read_header(&mut self) -> Result<Vec<Stream>> {
        let frame = self
            .feed
            .next_jpeg()?
            .ok_or_else(|| Error::invalid("mjpeg: no JPEG frame in the input"))?;
        let (width, height) = jpeg_dimensions(&frame)
            .ok_or_else(|| Error::invalid("mjpeg: first frame has no SOF"))?;
        let mut stream = Stream::new(0, CodecId::Jpeg);
        stream.width = width;
        stream.height = height;
        stream.time_base = Rational::new(1, RAW_FRAMERATE);
        self.first = Some(frame);
        Ok(vec![stream])
    }

    fn read_packet(&mut self) -> Result<Packet> {
        let frame = match self.first.take() {
            Some(f) => f,
            None => self.feed.next_jpeg()?.ok_or(Error::Eof)?,
        };
        Ok(self.packet(frame))
    }
}

struct RawMjpegMuxer {
    out: Output,
}

impl Muxer for RawMjpegMuxer {
    fn write_header(&mut self, streams: &[Stream]) -> Result<()> {
        check_one_jpeg_stream(streams, "mjpeg")
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

fn check_one_jpeg_stream(streams: &[Stream], name: &str) -> Result<()> {
    match streams {
        [s] if s.codec_id == CodecId::Jpeg => Ok(()),
        [_] => Err(Error::unsupported(format!(
            "{name} mux: only the `mjpeg` codec is supported"
        ))),
        [] => Err(Error::invalid(format!("{name} mux: no streams"))),
        _ => Err(Error::unsupported(format!(
            "{name} mux: one video stream only"
        ))),
    }
}

// ---------------------------------------------------------------------------
// multipart/x-mixed-replace
// ---------------------------------------------------------------------------

/// One part's headers, as far as the demuxer cares.
#[derive(Debug, Default, Clone, Copy, PartialEq, Eq)]
struct PartHeaders {
    content_length: Option<usize>,
    /// `X-Timestamp`, microseconds.
    timestamp: Option<u64>,
}

fn header_value<'a>(line: &'a [u8], name: &[u8]) -> Option<&'a [u8]> {
    if line.len() > name.len() && line[..name.len()].eq_ignore_ascii_case(name) {
        let v = &line[name.len()..];
        let start = v.iter().position(|b| !b.is_ascii_whitespace())?;
        Some(&v[start..])
    } else {
        None
    }
}

fn parse_usize(v: &[u8]) -> Option<u64> {
    let s = std::str::from_utf8(v).ok()?.trim();
    s.parse().ok()
}

/// Demuxes a `multipart/x-mixed-replace` body: one JPEG per part.
pub struct MultipartDemuxer {
    feed: Feed,
    boundary: Vec<u8>,
    first: Option<(Vec<u8>, PartHeaders)>,
    /// Timing mode, decided by the first part.
    timed: bool,
    first_ts: Option<u64>,
    index: i64,
}

impl MultipartDemuxer {
    /// Wrap an input positioned at the body (the HTTP head already consumed).
    pub fn new(input: Input) -> MultipartDemuxer {
        MultipartDemuxer {
            feed: Feed::new(input),
            boundary: Vec::new(),
            first: None,
            timed: false,
            first_ts: None,
            index: 0,
        }
    }

    /// Ensure at least `n` bytes are buffered (or EOF).
    fn want(&mut self, n: usize) -> Result<bool> {
        while self.feed.buf.len() < n {
            if !self.feed.fill()? {
                return Ok(false);
            }
        }
        Ok(true)
    }

    /// Position of `needle` in the buffer, reading more until found or EOF.
    fn find(&mut self, needle: &[u8], from: usize) -> Result<Option<usize>> {
        loop {
            if let Some(p) = self.feed.buf[from.min(self.feed.buf.len())..]
                .windows(needle.len())
                .position(|w| w == needle)
            {
                return Ok(Some(from + p));
            }
            if self.feed.buf.len() > MAX_FRAME || !self.feed.fill()? {
                return Ok(None);
            }
        }
    }

    /// Learn the boundary from the first line of the body.
    fn read_boundary(&mut self) -> Result<()> {
        // Some servers put a CRLF before the first boundary line.
        loop {
            if !self.want(1)? {
                return Err(Error::invalid("mpjpeg: no boundary line"));
            }
            match self.feed.buf[0] {
                b'\r' | b'\n' => self.feed.consume(1),
                _ => break,
            }
        }
        let Some(eol) = self.find(b"\n", 0)? else {
            return Err(Error::invalid("mpjpeg: no boundary line"));
        };
        let line = &self.feed.buf[..eol];
        let line = line.strip_suffix(b"\r").unwrap_or(line);
        let Some(b) = line.strip_prefix(b"--") else {
            return Err(Error::invalid(
                "mpjpeg: body does not start with a boundary",
            ));
        };
        if b.is_empty() || b.len() > 70 {
            return Err(Error::invalid("mpjpeg: bad boundary"));
        }
        self.boundary = [b"--", b].concat();
        self.feed.consume(eol + 1);
        Ok(())
    }

    /// The next part: its JPEG and headers. `None` at the closing boundary or EOF.
    fn next_part(&mut self) -> Result<Option<(Vec<u8>, PartHeaders)>> {
        // Headers up to the blank line.
        let Some(end) = self.find(b"\r\n\r\n", 0)? else {
            return Ok(None);
        };
        let mut headers = PartHeaders::default();
        for line in self.feed.buf[..end].split(|&b| b == b'\n') {
            let line = line.strip_suffix(b"\r").unwrap_or(line);
            if let Some(v) = header_value(line, b"content-length:") {
                headers.content_length = parse_usize(v).map(|n| n as usize);
            } else if let Some(v) = header_value(line, b"x-timestamp:") {
                headers.timestamp = parse_usize(v);
            }
        }
        self.feed.consume(end + 4);
        let body = match headers.content_length {
            Some(n) => {
                if n > MAX_FRAME {
                    return Err(Error::invalid("mpjpeg: part larger than 64 MiB"));
                }
                if !self.want(n)? {
                    return Ok(None);
                }
                let body = self.feed.buf[..n].to_vec();
                self.feed.consume(n);
                body
            }
            None => {
                // No length: the JPEG grammar delimits the part.
                let Some(frame) = self.feed.next_jpeg()? else {
                    return Ok(None);
                };
                frame
            }
        };
        // Skip to just past the next boundary line; a `--` suffix ends the stream.
        let boundary = self.boundary.clone();
        let Some(p) = self.find(&boundary, 0)? else {
            self.feed.consume(self.feed.buf.len());
            return Ok(Some((body, headers)));
        };
        let after = p + boundary.len();
        if !self.want(after + 2)? {
            self.feed.consume(self.feed.buf.len());
            return Ok(Some((body, headers)));
        }
        if &self.feed.buf[after..after + 2] == b"--" {
            self.feed.consume(self.feed.buf.len());
            self.feed.eof = true;
            return Ok(Some((body, headers)));
        }
        let eol = match self.find(b"\n", after)? {
            Some(e) => e + 1,
            None => self.feed.buf.len(),
        };
        self.feed.consume(eol);
        Ok(Some((body, headers)))
    }

    fn packet(&mut self, data: Vec<u8>, headers: PartHeaders) -> Packet {
        let mut packet = Packet::from_data(0, data);
        packet.flags.keyframe = true;
        if self.timed {
            packet.time_base = Rational::new(1, 1_000_000);
            if let Some(ts) = headers.timestamp {
                let first = *self.first_ts.get_or_insert(ts);
                let pts = ts.saturating_sub(first) as i64;
                packet.pts = Some(pts);
                packet.dts = Some(pts);
            }
        } else {
            packet.time_base = Rational::new(1, RAW_FRAMERATE);
            packet.pts = Some(self.index);
            packet.dts = Some(self.index);
            packet.duration = 1;
        }
        self.index += 1;
        packet
    }
}

impl Demuxer for MultipartDemuxer {
    fn read_header(&mut self) -> Result<Vec<Stream>> {
        self.read_boundary()?;
        let (frame, headers) = self
            .next_part()?
            .ok_or_else(|| Error::invalid("mpjpeg: no parts in the stream"))?;
        let (width, height) = jpeg_dimensions(&frame)
            .ok_or_else(|| Error::invalid("mpjpeg: first part is not a JPEG"))?;
        self.timed = headers.timestamp.is_some();
        let mut stream = Stream::new(0, CodecId::Jpeg);
        stream.width = width;
        stream.height = height;
        stream.time_base = if self.timed {
            Rational::new(1, 1_000_000)
        } else {
            Rational::new(1, RAW_FRAMERATE)
        };
        self.first = Some((frame, headers));
        Ok(vec![stream])
    }

    fn read_packet(&mut self) -> Result<Packet> {
        let (frame, headers) = match self.first.take() {
            Some(f) => f,
            None => self.next_part()?.ok_or(Error::Eof)?,
        };
        Ok(self.packet(frame, headers))
    }
}

/// Writes parts the way a camera's HTTP server does (the HTTP response head
/// is the server's, not the muxer's). `X-Timestamp` is written when the
/// packet has a `pts`, in microseconds.
struct MultipartMuxer {
    out: Output,
}

impl Muxer for MultipartMuxer {
    fn write_header(&mut self, streams: &[Stream]) -> Result<()> {
        check_one_jpeg_stream(streams, "mpjpeg")
    }

    fn write_packet(&mut self, packet: &Packet) -> Result<()> {
        write!(
            self.out,
            "--{MUX_BOUNDARY}\r\nContent-Type: image/jpeg\r\nContent-Length: {}\r\n",
            packet.data.len()
        )?;
        if let Some(pts) = packet.pts {
            let tb = packet.time_base;
            if tb.den > 0 {
                let us = (pts as i128 * tb.num as i128 * 1_000_000 / tb.den as i128).max(0);
                write!(self.out, "X-Timestamp: {us}\r\n")?;
            }
        }
        self.out.write_all(b"\r\n")?;
        self.out.write_all(&packet.data)?;
        self.out.write_all(b"\r\n")?;
        Ok(())
    }

    fn write_trailer(&mut self) -> Result<()> {
        write!(self.out, "--{MUX_BOUNDARY}--\r\n")?;
        self.out.flush()?;
        Ok(())
    }
}

#[cfg(test)]
mod tests {
    use super::*;
    use std::io::Cursor;

    fn jpeg(w: u16, h: u16, seed: u8) -> Vec<u8> {
        let rgb: Vec<u8> = (0..usize::from(w) * usize::from(h) * 3)
            .map(|i| (i as u32 * 7 + u32::from(seed) * 31) as u8)
            .collect();
        let mut out = Vec::new();
        rusty_jpeg::encode::Encoder::new(&mut out, 80)
            .encode(&rgb, w, h, rusty_jpeg::encode::ColorType::Rgb)
            .unwrap();
        out
    }

    /// A JPEG with an APP1 segment holding a whole thumbnail JPEG, the way
    /// EXIF does: a byte scan for `FF D9` would end the frame at the thumbnail.
    fn jpeg_with_thumbnail(w: u16, h: u16) -> Vec<u8> {
        let main = jpeg(w, h, 1);
        let thumb = jpeg(8, 8, 2);
        let mut out = vec![0xFF, 0xD8, 0xFF, 0xE1];
        out.extend_from_slice(&((2 + 6 + thumb.len()) as u16).to_be_bytes());
        out.extend_from_slice(b"Exif\0\0");
        out.extend_from_slice(&thumb);
        out.extend_from_slice(&main[2..]);
        out
    }

    /// A `Box<dyn Write + Send>` sink the test can read back after the muxer
    /// is dropped (the `Output` alias is `'static`, so no borrowed `Vec`).
    #[derive(Clone, Default)]
    struct Shared(std::sync::Arc<std::sync::Mutex<Vec<u8>>>);

    impl Write for Shared {
        fn write(&mut self, buf: &[u8]) -> std::io::Result<usize> {
            self.0.lock().unwrap().extend_from_slice(buf);
            Ok(buf.len())
        }
        fn flush(&mut self) -> std::io::Result<()> {
            Ok(())
        }
    }

    impl Shared {
        fn bytes(&self) -> Vec<u8> {
            self.0.lock().unwrap().clone()
        }
    }

    fn drain(dem: &mut dyn Demuxer) -> Vec<Packet> {
        let mut v = Vec::new();
        loop {
            match dem.read_packet() {
                Ok(p) => v.push(p),
                Err(Error::Eof) => return v,
                Err(e) => panic!("{e}"),
            }
        }
    }

    #[test]
    fn find_frame_walks_segments_not_bytes() {
        let f = jpeg_with_thumbnail(32, 24);
        assert_eq!(find_frame(&f).unwrap(), Some(f.len()));
        assert_eq!(find_frame(&f[..f.len() - 1]).unwrap(), None, "no EOI yet");
        assert!(find_frame(b"\x12\x34").is_err());
        assert_eq!(jpeg_dimensions(&f), Some((32, 24)));
        // Progressive + restart intervals: several SOS and RST markers in the scan.
        let mut prog = Vec::new();
        let rgb = vec![90u8; 48 * 40 * 3];
        let mut enc = rusty_jpeg::encode::Encoder::new(&mut prog, 85);
        enc.set_progressive(true);
        enc.set_restart_interval(2);
        enc.encode(&rgb, 48, 40, rusty_jpeg::encode::ColorType::Rgb)
            .unwrap();
        assert_eq!(find_frame(&prog).unwrap(), Some(prog.len()));
    }

    #[test]
    fn raw_stream_demuxes_every_frame_at_25_fps() {
        let frames: Vec<Vec<u8>> = (0..5).map(|i| jpeg(40, 32, i)).collect();
        let mut stream: Vec<u8> = frames.concat();
        stream.extend_from_slice(b"junk"); // trailing garbage is ignored
        assert_eq!(probe_raw_mjpeg(&stream), 100);
        assert_eq!(probe_raw_mjpeg(&frames[0]), 0, "a lone JPEG stays `jpeg`");

        let mut dem = RawMjpegDemuxer::new(Box::new(Cursor::new(stream)));
        let streams = dem.read_header().unwrap();
        assert_eq!((streams[0].width, streams[0].height), (40, 32));
        let got = drain(&mut dem);
        assert_eq!(got.len(), 5);
        for (i, p) in got.iter().enumerate() {
            assert_eq!(p.data, frames[i]);
            assert_eq!(p.pts, Some(i as i64));
            assert_eq!(p.time_base, Rational::new(1, 25));
        }
    }

    #[test]
    fn raw_mux_then_demux_round_trips() {
        let frames: Vec<Vec<u8>> = (0..3).map(|i| jpeg(16, 16, i)).collect();
        let sink = Shared::default();
        {
            let mut mux = RawMjpegMuxer {
                out: Box::new(sink.clone()),
            };
            let s = Stream::new(0, CodecId::Jpeg);
            mux.write_header(std::slice::from_ref(&s)).unwrap();
            for f in &frames {
                mux.write_packet(&Packet::from_data(0, f.clone())).unwrap();
            }
            mux.write_trailer().unwrap();
        }
        let mut dem = RawMjpegDemuxer::new(Box::new(Cursor::new(sink.bytes())));
        dem.read_header().unwrap();
        let got: Vec<Vec<u8>> = drain(&mut dem).into_iter().map(|p| p.data).collect();
        assert_eq!(got, frames);
    }

    /// The exact shape `rusty_esp_video::mjpeg_http` writes (and a browser
    /// reads): `--boundary`, Content-Type, Content-Length, X-Timestamp.
    fn janus_stream(frames: &[Vec<u8>], with_len: bool, with_ts: bool) -> Vec<u8> {
        let mut s = Vec::new();
        for (i, f) in frames.iter().enumerate() {
            s.extend_from_slice(b"--janus-frame\r\nContent-Type: image/jpeg\r\n");
            if with_len {
                s.extend_from_slice(format!("Content-Length: {}\r\n", f.len()).as_bytes());
            }
            if with_ts {
                s.extend_from_slice(
                    format!("X-Timestamp: {}\r\n", 1_000_000 + i * 100_000).as_bytes(),
                );
            }
            s.extend_from_slice(b"\r\n");
            s.extend_from_slice(f);
            s.extend_from_slice(b"\r\n");
        }
        s.extend_from_slice(b"--janus-frame--\r\n");
        s
    }

    #[test]
    fn multipart_with_lengths_and_timestamps() {
        let frames: Vec<Vec<u8>> = (0..4).map(|i| jpeg(24, 16, i)).collect();
        let stream = janus_stream(&frames, true, true);
        assert_eq!(probe_multipart(&stream), 100);
        assert_eq!(probe_multipart(&frames[0]), 0);

        let mut dem = MultipartDemuxer::new(Box::new(Cursor::new(stream)));
        let streams = dem.read_header().unwrap();
        assert_eq!((streams[0].width, streams[0].height), (24, 16));
        assert_eq!(streams[0].time_base, Rational::new(1, 1_000_000));
        let got = drain(&mut dem);
        assert_eq!(got.len(), 4);
        for (i, p) in got.iter().enumerate() {
            assert_eq!(p.data, frames[i]);
            assert_eq!(p.pts, Some(i as i64 * 100_000), "rebased on the first part");
        }
    }

    #[test]
    fn multipart_without_lengths_uses_the_jpeg_grammar() {
        let frames = vec![jpeg_with_thumbnail(24, 16), jpeg(24, 16, 3)];
        // Leading CRLF before the first boundary, as some servers send.
        let mut stream = b"\r\n".to_vec();
        stream.extend_from_slice(&janus_stream(&frames, false, false));
        let mut dem = MultipartDemuxer::new(Box::new(Cursor::new(stream)));
        let streams = dem.read_header().unwrap();
        assert_eq!(
            streams[0].time_base,
            Rational::new(1, 25),
            "untimed: counted"
        );
        let got = drain(&mut dem);
        assert_eq!(got.len(), 2);
        assert_eq!(
            got[0].data, frames[0],
            "the thumbnail did not end the frame"
        );
        assert_eq!(got[1].data, frames[1]);
        assert_eq!(got[1].pts, Some(1));
    }

    #[test]
    fn multipart_mux_then_demux_round_trips_with_timestamps() {
        let frames: Vec<Vec<u8>> = (0..3).map(|i| jpeg(16, 16, i)).collect();
        let sink = Shared::default();
        {
            let mut mux = MultipartMuxer {
                out: Box::new(sink.clone()),
            };
            let s = Stream::new(0, CodecId::Jpeg);
            mux.write_header(std::slice::from_ref(&s)).unwrap();
            for (i, f) in frames.iter().enumerate() {
                let mut p = Packet::from_data(0, f.clone());
                p.time_base = Rational::new(1, 25);
                p.pts = Some(i as i64);
                mux.write_packet(&p).unwrap();
            }
            mux.write_trailer().unwrap();
        }
        let buf = sink.bytes();
        assert!(buf.starts_with(b"--rff-frame\r\nContent-Type: image/jpeg\r\n"));
        let mut dem = MultipartDemuxer::new(Box::new(Cursor::new(buf)));
        dem.read_header().unwrap();
        let got = drain(&mut dem);
        assert_eq!(got.len(), 3);
        assert_eq!(got[2].data, frames[2]);
        assert_eq!(got[2].pts, Some(80_000), "2 frames at 25 fps = 80 ms");
    }

    #[test]
    fn hostile_input_errors_instead_of_panicking() {
        for bytes in [
            &b""[..],
            b"--x\r\n",
            b"--x\r\nContent-Length: 99999999999\r\n\r\n",
            b"\xFF\xD8\xFF\xE1\x00\x01",
            b"\xFF\xD8\xFF\xDA\x00\x02\xFF",
        ] {
            let mut d = MultipartDemuxer::new(Box::new(Cursor::new(bytes.to_vec())));
            let _ = d.read_header();
            let mut d = RawMjpegDemuxer::new(Box::new(Cursor::new(bytes.to_vec())));
            let _ = d.read_header();
        }
    }
}
