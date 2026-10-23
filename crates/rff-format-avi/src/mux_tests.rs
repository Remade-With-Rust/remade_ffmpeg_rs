//! Muxer tests (issue #28): the streaming muxer (seekable output: payload
//! written as it arrives, header patched at the trailer) and the buffered one
//! (pipes) must both write exactly the bytes the previous whole-file muxer
//! wrote, and the streaming one must keep its sizes right past 4 GiB without
//! anyone writing 4 GiB.
//!
//! The previous muxer is kept below, verbatim, as [`LegacyAviMuxer`]: it is
//! the byte-identity oracle.

use super::*;
use std::collections::BTreeMap;
use std::io::{Cursor, Seek, SeekFrom, Write};
use std::sync::{Arc, Mutex};

// ===========================================================================
// Oracle: the pre-#28 muxer (every packet buffered, file assembled at the end).
// ===========================================================================

struct LegacyAviMuxer {
    out: Output,
    streams: Vec<Stream>,
    packets: Vec<Packet>,
}

impl LegacyAviMuxer {
    fn new(output: Output) -> LegacyAviMuxer {
        LegacyAviMuxer {
            out: output,
            streams: Vec::new(),
            packets: Vec::new(),
        }
    }

    fn suffix_for(&self, stream_index: usize) -> [u8; 2] {
        match self.streams.iter().find(|s| s.index == stream_index) {
            Some(s) if s.media_type == MediaType::Audio => *b"wb",
            _ => *b"dc",
        }
    }

    fn packet_count(&self, stream_index: usize) -> u32 {
        self.packets
            .iter()
            .filter(|p| p.stream_index == stream_index)
            .count() as u32
    }

    fn avih(&self, micros_per_frame: u32, total_frames: u32, width: u32, height: u32) -> Vec<u8> {
        let mut a = vec![0u8; 56];
        put_u32(&mut a, 0, micros_per_frame);
        put_u32(&mut a, 12, 0x10);
        put_u32(&mut a, 16, total_frames);
        put_u32(&mut a, 24, self.streams.len() as u32);
        put_u32(&mut a, 32, width);
        put_u32(&mut a, 36, height);
        a
    }
}

impl Muxer for LegacyAviMuxer {
    fn write_header(&mut self, streams: &[Stream]) -> Result<()> {
        if streams.is_empty() {
            return Err(Error::invalid("avi mux: no streams"));
        }
        self.streams = streams.to_vec();
        Ok(())
    }

    fn write_packet(&mut self, packet: &Packet) -> Result<()> {
        self.packets.push(packet.clone());
        Ok(())
    }

    fn write_trailer(&mut self) -> Result<()> {
        let primary_video = self
            .streams
            .iter()
            .find(|s| s.media_type == MediaType::Video);
        let (vw, vh) = primary_video.map_or((0, 0), |s| (s.width, s.height));
        let micros_per_frame = primary_video.map_or(0, |s| {
            let tb = s.time_base;
            if tb.den != 0 && tb.num > 0 {
                (tb.num as i64 * 1_000_000 / tb.den as i64) as u32
            } else {
                0
            }
        });
        let total_frames = primary_video.map_or(0, |s| self.packet_count(s.index));

        let mut hdrl = Vec::new();
        put_chunk(
            &mut hdrl,
            b"avih",
            &self.avih(micros_per_frame, total_frames, vw, vh),
        );
        for s in &self.streams {
            let length = self.packet_count(s.index);
            let mut strl = Vec::new();
            put_chunk(&mut strl, b"strh", &strh(s, length));
            put_chunk(&mut strl, b"strf", &strf(s)?);
            put_list(&mut hdrl, b"strl", &strl);
        }

        let mut movi = Vec::new();
        let mut idx1 = Vec::new();
        for p in &self.packets {
            let id = chunk_id(p.stream_index, &self.suffix_for(p.stream_index));
            let offset = 4 + movi.len();
            put_chunk(&mut movi, &id, &p.data);

            let flags: u32 = if p.is_keyframe() { 0x10 } else { 0 };
            idx1.extend_from_slice(&id);
            idx1.extend_from_slice(&flags.to_le_bytes());
            idx1.extend_from_slice(&(offset as u32).to_le_bytes());
            idx1.extend_from_slice(&(p.data.len() as u32).to_le_bytes());
        }

        let mut body = Vec::new();
        body.extend_from_slice(b"AVI ");
        put_list(&mut body, b"hdrl", &hdrl);
        put_list(&mut body, b"movi", &movi);
        put_chunk(&mut body, b"idx1", &idx1);

        let mut file = Vec::with_capacity(body.len() + 8);
        file.extend_from_slice(b"RIFF");
        file.extend_from_slice(&(body.len() as u32).to_le_bytes());
        file.extend_from_slice(&body);

        self.out.write_all(&file)?;
        self.out.flush()?;
        Ok(())
    }
}

// ===========================================================================
// Sinks
// ===========================================================================

/// A plain `Write` sink (no seek: the pipe path) whose bytes outlive the muxer.
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

/// A seekable `Cursor<Vec<u8>>` sink (the file path) whose bytes outlive the
/// muxer.
#[derive(Clone)]
struct SharedCursor(Arc<Mutex<Cursor<Vec<u8>>>>);

impl Write for SharedCursor {
    fn write(&mut self, buf: &[u8]) -> std::io::Result<usize> {
        self.0.lock().unwrap().write(buf)
    }
    fn flush(&mut self) -> std::io::Result<()> {
        Ok(())
    }
}

impl Seek for SharedCursor {
    fn seek(&mut self, pos: SeekFrom) -> std::io::Result<u64> {
        self.0.lock().unwrap().seek(pos)
    }
}

fn drive(mut m: Box<dyn Muxer>, streams: &[Stream], packets: &[Packet]) -> Result<()> {
    m.write_header(streams)?;
    for p in packets {
        m.write_packet(p)?;
    }
    m.write_trailer()
}

fn mux_legacy(streams: &[Stream], packets: &[Packet]) -> Result<Vec<u8>> {
    let sink = SharedBuf(Arc::new(Mutex::new(Vec::new())));
    drive(
        Box::new(LegacyAviMuxer::new(Box::new(sink.clone()))),
        streams,
        packets,
    )?;
    let out = sink.0.lock().unwrap().clone();
    Ok(out)
}

/// Through the registered plain factory (non-seekable output).
fn mux_plain(streams: &[Stream], packets: &[Packet]) -> Result<Vec<u8>> {
    let mut reg = FormatRegistry::new();
    register(&mut reg);
    let sink = SharedBuf(Arc::new(Mutex::new(Vec::new())));
    drive(
        reg.open_muxer("avi", Box::new(sink.clone()))?,
        streams,
        packets,
    )?;
    let out = sink.0.lock().unwrap().clone();
    Ok(out)
}

/// Through the registered seekable factory, into a `Cursor<Vec<u8>>` that
/// already holds `prefix` (the header patch must land relative to where the
/// muxer started, not at offset 0). Returns the bytes after the prefix.
fn mux_seekable(streams: &[Stream], packets: &[Packet], prefix: &[u8]) -> Result<Vec<u8>> {
    let mut reg = FormatRegistry::new();
    register(&mut reg);
    let mut cur = Cursor::new(prefix.to_vec());
    cur.seek(SeekFrom::End(0)).unwrap();
    let sink = SharedCursor(Arc::new(Mutex::new(cur)));
    drive(
        reg.open_muxer_seekable("avi", Box::new(sink.clone()))?,
        streams,
        packets,
    )?;
    let cur = sink.0.lock().unwrap();
    // The muxer must leave the cursor at the end of what it wrote.
    assert_eq!(cur.position(), cur.get_ref().len() as u64, "not at the end");
    assert_eq!(&cur.get_ref()[..prefix.len()], prefix, "prefix clobbered");
    Ok(cur.get_ref()[prefix.len()..].to_vec())
}

// ===========================================================================
// Inputs
// ===========================================================================

/// Tiny deterministic PRNG (xorshift64*).
struct Rng(u64);

impl Rng {
    fn next(&mut self) -> u64 {
        self.0 ^= self.0 >> 12;
        self.0 ^= self.0 << 25;
        self.0 ^= self.0 >> 27;
        self.0.wrapping_mul(0x2545_F491_4F6C_DD1D)
    }
    fn below(&mut self, n: usize) -> usize {
        (self.next() % n as u64) as usize
    }
    fn bytes(&mut self, n: usize) -> Vec<u8> {
        (0..n).map(|_| self.next() as u8).collect()
    }
}

fn video(index: usize, codec: CodecId, w: u32, h: u32, fps: i32) -> Stream {
    let mut s = Stream::new(index, codec);
    s.media_type = MediaType::Video;
    s.width = w;
    s.height = h;
    s.time_base = Rational::new(1, fps);
    s
}

fn audio(index: usize, fmt: Option<SampleFormat>, rate: u32, channels: u16) -> Stream {
    let mut s = Stream::new(index, CodecId::Pcm);
    s.media_type = MediaType::Audio;
    s.sample_rate = rate;
    s.channels = channels;
    s.sample_format = fmt;
    s
}

fn raw(index: usize, pix: PixelFormat, w: u32, h: u32) -> Stream {
    let mut s = video(index, CodecId::RawVideo, w, h, 15);
    s.pixel_format = Some(pix);
    s
}

/// Interleaved packets over `streams` (by position), random sizes with odd
/// and empty ones, every fifth packet a keyframe.
fn packets(rng: &mut Rng, streams: &[usize], n: usize, max: usize) -> Vec<Packet> {
    (0..n)
        .map(|i| {
            let len = match i % 7 {
                3 => 0,
                5 => 1,
                _ => rng.below(max),
            };
            let mut p = Packet::from_data(streams[i % streams.len()], rng.bytes(len));
            p.flags.keyframe = i % 5 == 0;
            p
        })
        .collect()
}

/// `(name, streams, packets)` covering every stream kind the muxer writes.
type Case = (&'static str, Vec<Stream>, Vec<Packet>);

fn cases() -> Vec<Case> {
    let mut rng = Rng(0x9E37_79B9_7F4A_7C15);
    let mut cases: Vec<Case> = Vec::new();
    let h264 = video(0, CodecId::H264, 640, 360, 25);
    cases.push(("h264 video only", vec![h264.clone()], {
        packets(&mut rng, &[0], 40, 5000)
    }));
    cases.push(("h264 no packets", vec![h264.clone()], Vec::new()));
    cases.push((
        "mjpeg + s16 interleaved",
        vec![
            video(0, CodecId::Jpeg, 320, 240, 30),
            audio(1, Some(SampleFormat::S16), 48_000, 2),
        ],
        packets(&mut rng, &[0, 1, 1], 90, 3000),
    ));
    cases.push((
        "audio first, f32 + avif",
        vec![
            audio(0, Some(SampleFormat::F32), 44_100, 6),
            video(1, CodecId::Avif, 200, 100, 24),
        ],
        packets(&mut rng, &[0, 1], 30, 2000),
    ));
    cases.push((
        "audio only, no sample format",
        vec![audio(0, None, 8_000, 1)],
        packets(&mut rng, &[0], 17, 999),
    ));
    for (name, pix) in [
        ("rawvideo yuv420p", PixelFormat::Yuv420p),
        ("rawvideo yuv422p", PixelFormat::Yuv422p),
        ("rawvideo yuv444p", PixelFormat::Yuv444p),
    ] {
        let s = raw(0, pix, 17, 9);
        cases.push((name, vec![s], packets(&mut rng, &[0], 6, 800)));
    }
    let mut many = Vec::new();
    for i in 0..12 {
        if i % 3 == 2 {
            many.push(audio(i, Some(SampleFormat::S16), 8000, 1));
        } else {
            many.push(video(i, CodecId::H264, 16 + i as u32, 16, 25 + i as i32));
        }
    }
    let all: Vec<usize> = (0..12).collect();
    cases.push(("twelve streams", many, packets(&mut rng, &all, 100, 99)));
    // Oddities the counts must treat exactly as before: two streams sharing
    // an index, a packet for a stream that does not exist, a zero time base.
    let mut zero_tb = video(0, CodecId::H264, 8, 8, 1);
    zero_tb.time_base = Rational::new(0, 0);
    cases.push((
        "duplicate index, stray packet, zero time base",
        vec![zero_tb, audio(0, Some(SampleFormat::S16), 8000, 1)],
        packets(&mut rng, &[0, 0, 3], 12, 50),
    ));
    cases
}

// ===========================================================================
// Tests
// ===========================================================================

/// Both muxer paths write exactly the legacy muxer's bytes.
#[test]
fn both_paths_are_byte_identical_to_the_legacy_muxer() {
    for (name, streams, packets) in cases() {
        let want = mux_legacy(&streams, &packets).unwrap();
        let plain = mux_plain(&streams, &packets).unwrap();
        assert!(plain == want, "{name}: plain path differs");
        let seek = mux_seekable(&streams, &packets, &[]).unwrap();
        assert!(seek == want, "{name}: seekable path differs");
        let seek = mux_seekable(&streams, &packets, b"prefix!").unwrap();
        assert!(seek == want, "{name}: seekable path after a prefix differs");
    }
}

/// Our demuxer reads the streamed file back with the packets that went in.
#[test]
fn seekable_output_demuxes_back() {
    for (name, streams, packets) in cases() {
        let file = mux_seekable(&streams, &packets, &[]).unwrap();
        let mut dem = AviDemuxer::new(Box::new(Cursor::new(file)));
        let got_streams = dem.read_header().unwrap();
        assert_eq!(got_streams.len(), streams.len(), "{name}");
        // The demuxer numbers streams by position; packets keep their index.
        for p in packets.iter().filter(|p| p.stream_index < 100) {
            let got = dem.read_packet().unwrap();
            assert_eq!(got.stream_index, p.stream_index, "{name}");
            assert!(got.data == p.data, "{name}: payload differs");
        }
        assert!(matches!(dem.read_packet(), Err(Error::Eof)), "{name}");
    }
}

/// An unmappable stream is refused at `write_header` on both paths (the
/// legacy muxer only noticed in the trailer, after buffering everything).
#[test]
fn unsupported_stream_fails_at_header() {
    let s = [raw(0, PixelFormat::Rgb24, 4, 4)];
    assert!(mux_legacy(&s, &[]).is_err());
    let mut reg = FormatRegistry::new();
    register(&mut reg);
    let mut m = reg
        .open_muxer_seekable("avi", Box::new(Cursor::new(Vec::new())))
        .unwrap();
    assert!(m.write_header(&s).is_err());
    let mut m = reg.open_muxer("avi", Box::new(Vec::new())).unwrap();
    assert!(m.write_header(&s).is_err());
    assert!(m.write_header(&[]).is_err());
}

/// A seekable sink that keeps only the bytes of small writes (headers, chunk
/// headers, the index) and just advances the position over big ones, so a
/// multi-GiB file can be "written" in a unit test.
#[derive(Default)]
struct Sparse {
    pos: u64,
    len: u64,
    kept: BTreeMap<u64, Vec<u8>>,
}

impl Sparse {
    /// Bytes `[at, at + n)`, which must have come from kept writes.
    fn read(&self, at: u64, n: usize) -> Vec<u8> {
        let mut out = vec![None; n];
        for (&start, bytes) in &self.kept {
            for (i, &b) in bytes.iter().enumerate() {
                let p = start + i as u64;
                if p >= at && p < at + n as u64 {
                    out[(p - at) as usize] = Some(b);
                }
            }
        }
        out.into_iter()
            .map(|b| b.expect("byte was never written"))
            .collect()
    }
    fn u32_at(&self, at: u64) -> u32 {
        u32::from_le_bytes(self.read(at, 4).try_into().unwrap())
    }
}

#[derive(Clone)]
struct SharedSparse(Arc<Mutex<Sparse>>);

impl Write for SharedSparse {
    fn write(&mut self, buf: &[u8]) -> std::io::Result<usize> {
        let mut s = self.0.lock().unwrap();
        if buf.len() <= 1 << 16 {
            let pos = s.pos;
            s.kept.insert(pos, buf.to_vec());
        }
        s.pos += buf.len() as u64;
        s.len = s.len.max(s.pos);
        Ok(buf.len())
    }
    fn flush(&mut self) -> std::io::Result<()> {
        Ok(())
    }
}

impl Seek for SharedSparse {
    fn seek(&mut self, pos: SeekFrom) -> std::io::Result<u64> {
        let mut s = self.0.lock().unwrap();
        s.pos = match pos {
            SeekFrom::Start(p) => p,
            SeekFrom::End(d) => (s.len as i64 + d) as u64,
            SeekFrom::Current(d) => (s.pos as i64 + d) as u64,
        };
        Ok(s.pos)
    }
}

/// Past 4 GiB the streaming muxer writes what the legacy muxer would have
/// (it never wrote OpenDML, so the 32-bit RIFF/`movi` sizes and `idx1`
/// offsets wrap modulo 2^32) -- checked field by field against sizes computed
/// here, on a ~4.3 GiB file that is never actually stored.
#[test]
fn sizes_past_4_gib_match_the_legacy_arithmetic() {
    let streams = [
        video(0, CodecId::Jpeg, 1920, 1080, 24),
        audio(1, Some(SampleFormat::S16), 48_000, 2),
    ];
    let big = Packet::from_data(0, vec![0x5A; (64 << 20) + 1]); // odd: padded
    let small = Packet::from_data(1, vec![1, 2, 3, 4]);
    let n_big = 66u64; // 66 x 64 MiB > 4 GiB of movi
    let sink = SharedSparse(Arc::new(Mutex::new(Sparse::default())));
    let mut reg = FormatRegistry::new();
    register(&mut reg);
    let mut m = reg
        .open_muxer_seekable("avi", Box::new(sink.clone()))
        .unwrap();
    m.write_header(&streams).unwrap();
    for _ in 0..n_big {
        m.write_packet(&big).unwrap();
        m.write_packet(&small).unwrap();
    }
    m.write_trailer().unwrap();
    let s = sink.0.lock().unwrap();
    assert_eq!(s.pos, s.len, "not left at the end");

    // The header is the same length as for any file with these streams.
    let header_len = mux_legacy(&streams, &[]).unwrap().len() as u64 - 8; // minus idx1 header
    let big_chunk = 8 + big.data.len() as u64 + 1;
    let small_chunk = 8 + 4;
    let movi_len = n_big * (big_chunk + small_chunk);
    let idx1_len = 16 * 2 * n_big;
    let file_len = header_len + movi_len + 8 + idx1_len;
    assert_eq!(s.len, file_len);
    assert!(file_len > 1 << 32);

    assert_eq!(s.read(0, 4), b"RIFF");
    assert_eq!(s.u32_at(4), (file_len - 8) as u32);
    let movi_at = header_len - 12;
    assert_eq!(s.read(movi_at, 4), b"LIST");
    assert_eq!(s.u32_at(movi_at + 4), (4 + movi_len) as u32);
    assert_eq!(s.read(movi_at + 8, 4), b"movi");
    // avih dwTotalFrames and each strh dwLength.
    let avih = 12 + 12 + 8;
    assert_eq!(s.u32_at(avih + 16), n_big as u32);
    let strh0 = avih + 56 + 12 + 8;
    assert_eq!(s.u32_at(strh0 + 32), n_big as u32);

    // idx1: the last entry's offset wraps exactly as `offset as u32` did.
    let idx1_at = header_len + movi_len;
    assert_eq!(s.read(idx1_at, 4), b"idx1");
    assert_eq!(s.u32_at(idx1_at + 4), idx1_len as u32);
    let last = idx1_at + 8 + idx1_len - 16;
    assert_eq!(s.read(last, 4), b"01wb");
    let last_offset = 4 + movi_len - small_chunk;
    assert_eq!(s.u32_at(last + 8), last_offset as u32);
    assert_eq!(s.u32_at(last + 12), 4);
    // A chunk header deep past 4 GiB carries the true payload size.
    let deep = header_len + (n_big - 1) * (big_chunk + small_chunk);
    assert_eq!(s.read(deep, 4), b"00dc");
    assert_eq!(s.u32_at(deep + 4), big.data.len() as u32);
}
