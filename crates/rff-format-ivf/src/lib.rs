//! IVF container — the minimal native wrapper for VP8/VP9 bitstreams.
//!
//! IVF is what `vpxenc` writes and `vpxdec`/ffmpeg read for raw VP9 testing: a
//! 32-byte file header, then `[size:u32][pts:u64]` + frame data per packet. This
//! is the output side of the VP9 RD campaign — rff encodes `y4m → ivf`, and
//! ffmpeg/libvpx decode the `.ivf` back for PSNR/VMAF.

use std::io::{BufReader, Read, Write};

use rff_core::{CodecId, Error, Packet, Rational, Result};
use rff_format::{Demuxer, Format, FormatRegistry, Input, MuxCaps, Muxer, Output, Stream};

/// Register the IVF format into a [`FormatRegistry`].
pub fn register(registry: &mut FormatRegistry) {
    registry.register(Format {
        name: "ivf",
        long_name: "IVF (raw VP8/VP9)",
        extensions: &["ivf"],
        demuxer: Some(|input| Box::new(IvfDemuxer::new(input))),
        muxer: Some(|output| Box::new(IvfMuxer::new(output))),
        muxer_path: None,
        probe: Some(probe_ivf),
        mux_caps: MuxCaps::single(&[CodecId::Vp9, CodecId::Av2]),
    });
}

/// Sniff IVF: the file begins with the `DKIF` signature.
fn probe_ivf(data: &[u8]) -> i32 {
    if data.len() >= 4 && &data[..4] == b"DKIF" {
        100
    } else {
        0
    }
}

fn rd_u16(b: &[u8], at: usize) -> u16 {
    u16::from_le_bytes([b[at], b[at + 1]])
}
fn rd_u32(b: &[u8], at: usize) -> u32 {
    u32::from_le_bytes([b[at], b[at + 1], b[at + 2], b[at + 3]])
}

// ---------------------------------------------------------------------------
// Muxer
// ---------------------------------------------------------------------------

struct IvfMuxer {
    out: Output,
    ready: bool,
    /// Frame index used as the IVF timestamp. IVF pts is a plain frame counter;
    /// we assign it here rather than trust `packet.pts` (encoders often leave it
    /// unset, which would collapse every frame onto pts 0 and make decoders drop
    /// them as duplicates).
    frame: u64,
}

impl IvfMuxer {
    fn new(out: Output) -> IvfMuxer {
        IvfMuxer {
            out,
            ready: false,
            frame: 0,
        }
    }
}

impl Muxer for IvfMuxer {
    fn write_header(&mut self, streams: &[Stream]) -> Result<()> {
        let s = streams
            .first()
            .filter(|s| fourcc_from_codec(s.codec_id).is_some())
            .ok_or_else(|| {
                Error::unsupported("ivf mux: needs a single `vp9`, `av1` or `av2` stream")
            })?;
        let fourcc = fourcc_from_codec(s.codec_id).expect("filtered above");
        // fps num:den = 1/time_base = time_base.den : time_base.num.
        let (fnum, fden) = (s.time_base.den.max(1) as u32, s.time_base.num.max(1) as u32);
        let mut h = Vec::with_capacity(32);
        h.extend_from_slice(b"DKIF");
        h.extend_from_slice(&0u16.to_le_bytes()); // version
        h.extend_from_slice(&32u16.to_le_bytes()); // header length
        h.extend_from_slice(fourcc); // fourcc
        h.extend_from_slice(&(s.width as u16).to_le_bytes());
        h.extend_from_slice(&(s.height as u16).to_le_bytes());
        h.extend_from_slice(&fnum.to_le_bytes()); // framerate numerator
        h.extend_from_slice(&fden.to_le_bytes()); // framerate denominator
                                                  // Frame count is not authoritative (decoders read to EOF); leave 0.
        h.extend_from_slice(&0u32.to_le_bytes());
        h.extend_from_slice(&0u32.to_le_bytes()); // unused
        self.out.write_all(&h)?;
        self.ready = true;
        Ok(())
    }

    fn write_packet(&mut self, packet: &Packet) -> Result<()> {
        if !self.ready {
            return Err(Error::invalid("ivf mux: write_header not called"));
        }
        let pts = self.frame;
        self.frame += 1;
        self.out
            .write_all(&(packet.data.len() as u32).to_le_bytes())?;
        self.out.write_all(&pts.to_le_bytes())?;
        self.out.write_all(&packet.data)?;
        Ok(())
    }

    fn write_trailer(&mut self) -> Result<()> {
        self.out.flush()?;
        Ok(())
    }
}

// ---------------------------------------------------------------------------
// Demuxer
// ---------------------------------------------------------------------------

/// Bytes in an IVF file header (the `header length` field may declare more).
const FILE_HEADER: usize = 32;
/// Bytes in each frame header: `size:u32le` + `pts:u64le`.
const FRAME_HEADER: usize = 12;

/// Streaming IVF demuxer: the 32-byte file header is read in `read_header`,
/// then each frame's 12-byte header + payload on demand in `read_packet`, so
/// peak memory is one frame, not the file (issue #26). Works on pipes.
struct IvfDemuxer {
    /// The source before `read_header` takes it.
    input: Option<Input>,
    /// The source after the header, positioned at the next frame header.
    reader: Option<BufReader<Input>>,
    /// Sticky failure: once a frame is found truncated, every later call
    /// reports it again (as the whole-buffer demuxer did).
    failed: Option<&'static str>,
}

impl IvfDemuxer {
    fn new(input: Input) -> IvfDemuxer {
        IvfDemuxer {
            input: Some(input),
            reader: None,
            failed: None,
        }
    }
}

/// Fill as much of `buf` as the source holds (short only at end of input).
fn read_full(r: &mut impl Read, buf: &mut [u8]) -> std::io::Result<usize> {
    let mut n = 0;
    while n < buf.len() {
        match r.read(&mut buf[n..]) {
            Ok(0) => break,
            Ok(k) => n += k,
            Err(e) if e.kind() == std::io::ErrorKind::Interrupted => {}
            Err(e) => return Err(e),
        }
    }
    Ok(n)
}

/// Read up to `len` bytes, growing the buffer only as bytes actually arrive
/// (in bounded steps), so a lying size field cannot force a huge allocation.
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

/// Map an IVF fourcc (bytes 8..12 of the header) onto a codec id.
///
/// IVF is a bare frame-length container with no codec description beyond this
/// tag, so it is the only thing distinguishing a VP9 stream from an AV1 or AV2
/// one. Assuming VP9 here silently hands AV2 packets to the VP9 decoder.
fn codec_from_fourcc(tag: &[u8]) -> Result<CodecId> {
    match tag {
        b"VP90" => Ok(CodecId::Vp9),
        b"AV02" => Ok(CodecId::Av2),
        other => Err(Error::unsupported(format!(
            "ivf demux: unsupported fourcc {:?}",
            String::from_utf8_lossy(other)
        ))),
    }
}

/// The IVF fourcc to write for a codec, or `None` if IVF cannot carry it.
fn fourcc_from_codec(id: CodecId) -> Option<&'static [u8; 4]> {
    match id {
        CodecId::Vp9 => Some(b"VP90"),
        CodecId::Av2 => Some(b"AV02"),
        _ => None,
    }
}

impl Demuxer for IvfDemuxer {
    fn read_header(&mut self) -> Result<Vec<Stream>> {
        let input = self
            .input
            .take()
            .ok_or_else(|| Error::invalid("ivf demux: header already read"))?;
        let mut reader = BufReader::new(input);
        let mut h = [0u8; FILE_HEADER];
        if read_full(&mut reader, &mut h)? < FILE_HEADER || probe_ivf(&h) == 0 {
            return Err(Error::invalid("ivf demux: not an IVF file"));
        }
        let width = rd_u16(&h, 12) as u32;
        let height = rd_u16(&h, 14) as u32;
        let fnum = rd_u32(&h, 16).max(1);
        let fden = rd_u32(&h, 20).max(1);
        let codec = codec_from_fourcc(&h[8..12])?;
        // A header longer than 32 bytes: skip the extension without buffering it.
        let extra = (rd_u16(&h, 6) as usize).saturating_sub(FILE_HEADER) as u64;
        std::io::copy(&mut (&mut reader).take(extra), &mut std::io::sink())?;
        self.reader = Some(reader);

        let mut stream = Stream::new(0, codec);
        stream.width = width;
        stream.height = height;
        stream.time_base = Rational::new(fden as i32, fnum as i32);
        Ok(vec![stream])
    }

    fn read_packet(&mut self) -> Result<Packet> {
        if let Some(msg) = self.failed {
            return Err(Error::invalid(msg));
        }
        let reader = self.reader.as_mut().ok_or(Error::Eof)?;
        let mut h = [0u8; FRAME_HEADER];
        if read_full(reader, &mut h)? < FRAME_HEADER {
            return Err(Error::Eof); // clean end, or a partial trailing frame header
        }
        let size = rd_u32(&h, 0) as usize;
        let pts = u64::from_le_bytes(h[4..12].try_into().expect("8 bytes"));
        let data = read_up_to(reader, size)?;
        if data.len() < size {
            let msg = "ivf demux: truncated frame";
            self.failed = Some(msg);
            return Err(Error::invalid(msg));
        }
        let mut packet = Packet::from_data(0, data);
        packet.pts = Some(pts as i64);
        Ok(packet)
    }
}

#[cfg(test)]
mod legacy;

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

    #[test]
    fn mux_then_demux_roundtrips() {
        let sink = SharedBuf(Arc::new(Mutex::new(Vec::new())));
        {
            let mut mux = IvfMuxer::new(Box::new(sink.clone()));
            let mut s = Stream::new(0, CodecId::Vp9);
            s.width = 352;
            s.height = 288;
            s.time_base = Rational::new(1, 30);
            mux.write_header(&[s]).unwrap();
            let mut p = Packet::from_data(0, vec![1, 2, 3, 4, 5]);
            p.pts = Some(0);
            mux.write_packet(&p).unwrap();
            mux.write_trailer().unwrap();
        }
        let file = sink.0.lock().unwrap().clone();
        assert_eq!(&file[0..4], b"DKIF");
        assert_eq!(probe_ivf(&file), 100);

        let mut dem = IvfDemuxer::new(Box::new(Cursor::new(file)));
        let streams = dem.read_header().unwrap();
        assert_eq!(streams[0].codec_id, CodecId::Vp9);
        assert_eq!((streams[0].width, streams[0].height), (352, 288));
        assert_eq!(dem.read_packet().unwrap().data, vec![1, 2, 3, 4, 5]);
        assert!(matches!(dem.read_packet(), Err(Error::Eof)));
    }

    // ---- streaming (issue #26): equivalence, header cost, robustness ----

    use std::sync::atomic::{AtomicU64, Ordering};

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
    fn assert_same_as_legacy(name: &str, file: &[u8]) {
        let old = drive(Box::new(legacy::LegacyIvfDemuxer::new(Box::new(
            Cursor::new(file.to_vec()),
        ))));
        for chunk in [1, 7, usize::MAX] {
            let (input, _) = counting(file.to_vec(), chunk);
            let new = drive(Box::new(IvfDemuxer::new(input)));
            assert_eq!(new.0, old.0, "{name}: header (chunk {chunk})");
            assert_eq!(new.1.len(), old.1.len(), "{name}: packet count");
            assert!(new.1 == old.1, "{name}: packets differ (chunk {chunk})");
            assert_eq!(new.2, old.2, "{name}: end (chunk {chunk})");
        }
    }

    /// IVF with `frames` payload sizes, a header-length field and trailing bytes.
    fn ivf(fourcc: &[u8; 4], hlen: u16, frames: &[usize], tail: &[u8]) -> Vec<u8> {
        let mut f = b"DKIF".to_vec();
        f.extend_from_slice(&0u16.to_le_bytes());
        f.extend_from_slice(&hlen.to_le_bytes());
        f.extend_from_slice(fourcc);
        f.extend_from_slice(&176u16.to_le_bytes());
        f.extend_from_slice(&144u16.to_le_bytes());
        f.extend_from_slice(&30u32.to_le_bytes());
        f.extend_from_slice(&1u32.to_le_bytes());
        f.extend_from_slice(&[0u8; 8]);
        f.resize(f.len().max(hlen as usize), 0xEE);
        for (i, &n) in frames.iter().enumerate() {
            f.extend_from_slice(&(n as u32).to_le_bytes());
            f.extend_from_slice(&(i as u64 * 3).to_le_bytes());
            f.extend((0..n).map(|j| (i * 31 + j) as u8));
        }
        f.extend_from_slice(tail);
        f
    }

    #[test]
    fn streaming_matches_whole_buffer_demuxer() {
        let cases: Vec<(&str, Vec<u8>)> = vec![
            ("one frame", ivf(b"VP90", 32, &[5], &[])),
            (
                "many frames",
                ivf(b"VP90", 32, &[1, 0, 900, 70_000, 3], &[]),
            ),
            ("av2", ivf(b"AV02", 32, &[100, 200], &[])),
            ("header ext", ivf(b"VP90", 48, &[10, 20], &[])),
            ("short hlen", ivf(b"VP90", 8, &[10], &[])),
            (
                "partial frame header",
                ivf(b"VP90", 32, &[10], &[1, 2, 3, 4, 5]),
            ),
            ("header only", ivf(b"VP90", 32, &[], &[])),
            ("bad fourcc", ivf(b"VP80", 32, &[10], &[])),
            ("not ivf", b"RIFF0000WAVEfmt nothing to see here".to_vec()),
            ("empty", Vec::new()),
        ];
        for (name, file) in &cases {
            assert_same_as_legacy(name, file);
        }
        // Real files, when the checkout has them.
        let root = std::path::Path::new(env!("CARGO_MANIFEST_DIR")).join("../..");
        for rel in [
            "crates/rff/testdata/av2_424x240.ivf",
            "crates/rff-codec-vp9/benches/data/vp9_720p.ivf",
            "video-tests/results/av1/_tmp/akiyo_qcif.ours.ivf",
        ] {
            if let Ok(file) = std::fs::read(root.join(rel)) {
                assert_same_as_legacy(rel, &file);
            }
        }
    }

    #[test]
    fn read_header_reads_only_the_header() {
        // ~4 MB of frames: the header must not pull the file in.
        let file = ivf(b"VP90", 32, &[64 * 1024; 64], &[]);
        let (input, read) = counting(file, usize::MAX);
        let mut dem = IvfDemuxer::new(input);
        dem.read_header().unwrap();
        assert!(
            read.load(Ordering::Relaxed) < 64 * 1024,
            "header read too much"
        );
        // A packet pulls about one frame.
        dem.read_packet().unwrap();
        assert!(read.load(Ordering::Relaxed) < 3 * 64 * 1024);
    }

    #[test]
    fn truncated_and_garbage_inputs_fail_cleanly() {
        // A frame that declares 4 GB but holds 10 bytes: an error, not a 4 GB
        // allocation, and the error is sticky.
        let mut file = ivf(b"VP90", 32, &[], &[]);
        file.extend_from_slice(&u32::MAX.to_le_bytes());
        file.extend_from_slice(&0u64.to_le_bytes());
        file.extend_from_slice(&[0u8; 10]);
        let mut dem = IvfDemuxer::new(Box::new(Cursor::new(file)));
        dem.read_header().unwrap();
        assert!(matches!(dem.read_packet(), Err(Error::InvalidData(_))));
        assert!(matches!(dem.read_packet(), Err(Error::InvalidData(_))));

        // Every truncation of a valid file, and pseudo-random garbage behind a
        // valid header: never a panic, always an end.
        let good = ivf(b"VP90", 32, &[40, 3, 17], &[]);
        for cut in 0..good.len() {
            let _ = drive(Box::new(IvfDemuxer::new(Box::new(Cursor::new(
                good[..cut].to_vec(),
            )))));
        }
        let mut x = 0x2545_F491_u32;
        for _ in 0..64 {
            let mut g = good[..32].to_vec();
            for _ in 0..300 {
                x ^= x << 13;
                x ^= x >> 17;
                x ^= x << 5;
                g.push(x as u8);
            }
            let _ = drive(Box::new(IvfDemuxer::new(Box::new(Cursor::new(g)))));
        }
        // Reading packets before the header is just end-of-stream.
        let mut dem = IvfDemuxer::new(Box::new(Cursor::new(good)));
        assert!(matches!(dem.read_packet(), Err(Error::Eof)));
    }
}
