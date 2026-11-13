//! Streaming-demuxer tests (issue #26): the demuxer must read only the header
//! in `read_header`, stream `movi` one chunk at a time, match the previous
//! whole-file implementation exactly on well-formed input, and never panic or
//! spin on malformed input.
//!
//! The previous implementation is kept below, verbatim, as [`legacy_demux`]:
//! it is the equivalence oracle.

use super::*;
use std::io::{Cursor, Write};
use std::sync::atomic::{AtomicUsize, Ordering};
use std::sync::{Arc, Mutex};

// ===========================================================================
// Oracle: the pre-#26 whole-file demuxer (read everything, slice it up).
// ===========================================================================

fn legacy_find_list(
    chunks: &[([u8; 4], Range<usize>)],
    buf: &[u8],
    list_type: &[u8; 4],
) -> Option<Range<usize>> {
    chunks
        .iter()
        .find(|(id, r)| id == b"LIST" && r.len() >= 4 && &buf[r.start..r.start + 4] == list_type)
        .map(|(_, r)| r.clone())
}

fn legacy_push_data_chunk(buf: &[u8], id: [u8; 4], data: Range<usize>, out: &mut Vec<Packet>) {
    if !id[0].is_ascii_digit() || !id[1].is_ascii_digit() {
        return;
    }
    let stream_index = ((id[0] - b'0') * 10 + (id[1] - b'0')) as usize;
    out.push(Packet::from_data(stream_index, buf[data].to_vec()));
}

fn legacy_demux(buf: &[u8]) -> Result<(Vec<Stream>, Vec<Packet>)> {
    if buf.len() < 12 || &buf[0..4] != b"RIFF" || &buf[8..12] != b"AVI " {
        return Err(Error::invalid("avi demux: not a RIFF/AVI file"));
    }
    let top = riff_chunks(buf, 12, buf.len());
    let hdrl = legacy_find_list(&top, buf, b"hdrl")
        .ok_or_else(|| Error::invalid("avi demux: no `hdrl` list"))?;
    let hdrl_children = riff_chunks(buf, hdrl.start + 4, hdrl.end);
    let (frame_w, frame_h) = match find(&hdrl_children, b"avih") {
        Some(r) => (rd_u32(buf, r.start + 32), rd_u32(buf, r.start + 36)),
        None => (0, 0),
    };
    let mut streams = Vec::new();
    for (id, r) in &hdrl_children {
        if id != b"LIST" || r.len() < 4 || &buf[r.start..r.start + 4] != b"strl" {
            continue;
        }
        if let Some(stream) = parse_strl(buf, r.start + 4, r.end, streams.len(), frame_w, frame_h) {
            streams.push(stream);
        }
    }
    let mut packets = Vec::new();
    if let Some(movi) = legacy_find_list(&top, buf, b"movi") {
        for (id, r) in riff_chunks(buf, movi.start + 4, movi.end) {
            if id == *b"LIST" && r.len() >= 4 && &buf[r.start..r.start + 4] == b"rec " {
                for (cid, cr) in riff_chunks(buf, r.start + 4, r.end) {
                    legacy_push_data_chunk(buf, cid, cr, &mut packets);
                }
            } else {
                legacy_push_data_chunk(buf, id, r, &mut packets);
            }
        }
    }
    Ok((streams, packets))
}

// ===========================================================================
// Harness
// ===========================================================================

/// Everything observable about one packet.
type PacketRecord = (
    usize,
    Option<i64>,
    Option<i64>,
    i64,
    bool,
    Rational,
    Vec<u8>,
);

fn record_packet(p: &Packet) -> PacketRecord {
    (
        p.stream_index,
        p.pts,
        p.dts,
        p.duration,
        p.flags.keyframe,
        p.time_base,
        p.data.clone(),
    )
}

/// Everything observable about a demux: stream fields (via `Debug`, which
/// covers every field) and every packet.
#[derive(Debug, PartialEq)]
struct Recording {
    streams: String,
    packets: Vec<PacketRecord>,
}

fn record_legacy(file: &[u8]) -> Option<Recording> {
    let (streams, packets) = legacy_demux(file).ok()?;
    Some(Recording {
        streams: format!("{streams:?}"),
        packets: packets.iter().map(record_packet).collect(),
    })
}

/// Demux with the streaming implementation. Panics if the packet loop does not
/// terminate within a bound derived from the input size (every packet consumes
/// at least an 8-byte chunk header).
fn record_new_from(input: Input, input_len: usize) -> Option<Recording> {
    let mut dem = AviDemuxer::new(input);
    let streams = dem.read_header().ok()?;
    let mut packets = Vec::new();
    let bound = input_len / 8 + 2;
    loop {
        match dem.read_packet() {
            Ok(p) => packets.push(record_packet(&p)),
            Err(Error::Eof) => break,
            Err(e) => panic!("unexpected demux error: {e}"),
        }
        assert!(packets.len() <= bound, "read_packet did not terminate");
    }
    // Eof is sticky.
    assert!(matches!(dem.read_packet(), Err(Error::Eof)));
    Some(Recording {
        streams: format!("{streams:?}"),
        packets,
    })
}

fn record_new(file: &[u8]) -> Option<Recording> {
    record_new_from(Box::new(Cursor::new(file.to_vec())), file.len())
}

/// A `Write` sink whose bytes can be recovered after the muxer drops it.
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

/// A reader that hands out at most `step` bytes per call — a pipe.
struct Trickle {
    inner: Cursor<Vec<u8>>,
    step: usize,
}

impl Read for Trickle {
    fn read(&mut self, buf: &mut [u8]) -> std::io::Result<usize> {
        let n = buf.len().min(self.step);
        self.inner.read(&mut buf[..n])
    }
}

/// A reader that counts the bytes pulled out of it.
struct Counting {
    inner: Cursor<Vec<u8>>,
    pulled: Arc<AtomicUsize>,
}

impl Read for Counting {
    fn read(&mut self, buf: &mut [u8]) -> std::io::Result<usize> {
        let n = self.inner.read(buf)?;
        self.pulled.fetch_add(n, Ordering::Relaxed);
        Ok(n)
    }
}

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

fn mux(streams: &[Stream], packets: &[Packet]) -> Vec<u8> {
    let sink = SharedBuf(Arc::new(Mutex::new(Vec::new())));
    {
        let mut m = AviMuxer::new(Box::new(sink.clone()));
        m.write_header(streams).unwrap();
        for p in packets {
            m.write_packet(p).unwrap();
        }
        m.write_trailer().unwrap();
    }
    let out = sink.0.lock().unwrap().clone();
    out
}

fn video_stream(index: usize, codec: CodecId, w: u32, h: u32) -> Stream {
    let mut s = Stream::new(index, codec);
    s.media_type = MediaType::Video;
    s.width = w;
    s.height = h;
    s.time_base = Rational::new(1, 30);
    s
}

fn audio_stream(index: usize, fmt: SampleFormat, rate: u32, channels: u16) -> Stream {
    let mut s = Stream::new(index, CodecId::Pcm);
    s.media_type = MediaType::Audio;
    s.sample_rate = rate;
    s.channels = channels;
    s.sample_format = Some(fmt);
    s
}

/// Video + audio interleaved, a mix of odd and even payload sizes.
fn muxed_av(rng: &mut Rng, frames: usize, max_frame: usize) -> Vec<u8> {
    let streams = [
        video_stream(0, CodecId::H264, 320, 240),
        audio_stream(1, SampleFormat::S16, 48_000, 2),
    ];
    let mut packets = Vec::new();
    for i in 0..frames {
        let vlen = rng.below(max_frame) | (i & 1); // odd on odd frames
        let mut v = Packet::from_data(0, rng.bytes(vlen));
        v.flags.keyframe = i % 5 == 0;
        packets.push(v);
        let alen = 1 + rng.below(3000);
        packets.push(Packet::from_data(1, rng.bytes(alen)));
    }
    mux(&streams, &packets)
}

fn le32(v: u32) -> [u8; 4] {
    v.to_le_bytes()
}

fn chunk(id: &[u8; 4], data: &[u8]) -> Vec<u8> {
    let mut o = Vec::new();
    o.extend_from_slice(id);
    o.extend_from_slice(&le32(data.len() as u32));
    o.extend_from_slice(data);
    if data.len() % 2 == 1 {
        o.push(0);
    }
    o
}

fn list(list_type: &[u8; 4], children: &[u8]) -> Vec<u8> {
    let mut body = list_type.to_vec();
    body.extend_from_slice(children);
    chunk(b"LIST", &body)
}

fn riff(form: &[u8; 4], children: &[u8]) -> Vec<u8> {
    let mut body = form.to_vec();
    body.extend_from_slice(children);
    let mut o = b"RIFF".to_vec();
    o.extend_from_slice(&le32(body.len() as u32));
    o.extend_from_slice(&body);
    o
}

fn avih(w: u32, h: u32, streams: u32) -> Vec<u8> {
    let mut a = vec![0u8; 56];
    a[24..28].copy_from_slice(&le32(streams));
    a[32..36].copy_from_slice(&le32(w));
    a[36..40].copy_from_slice(&le32(h));
    a
}

fn strl(fcc_type: &[u8; 4], handler: &[u8; 4], scale: u32, rate: u32, strf: &[u8]) -> Vec<u8> {
    let mut h = vec![0u8; 56];
    h[0..4].copy_from_slice(fcc_type);
    h[4..8].copy_from_slice(handler);
    h[20..24].copy_from_slice(&le32(scale));
    h[24..28].copy_from_slice(&le32(rate));
    let mut c = chunk(b"strh", &h);
    c.extend_from_slice(&chunk(b"strf", strf));
    list(b"strl", &c)
}

fn bih(fourcc: &[u8; 4]) -> Vec<u8> {
    let mut b = vec![0u8; 40];
    b[0..4].copy_from_slice(&le32(40));
    b[16..20].copy_from_slice(fourcc);
    b
}

fn wfx(tag: u16, channels: u16, rate: u32, bits: u16) -> Vec<u8> {
    let mut w = vec![0u8; 18];
    w[0..2].copy_from_slice(&tag.to_le_bytes());
    w[2..4].copy_from_slice(&channels.to_le_bytes());
    w[4..8].copy_from_slice(&le32(rate));
    w[14..16].copy_from_slice(&bits.to_le_bytes());
    w
}

/// A standard three-stream `hdrl` (vids, a skipped `txts`, auds) with a JUNK
/// child and an unknown LIST, as real-world muxers leave around.
fn rich_hdrl() -> Vec<u8> {
    let mut c = chunk(b"avih", &avih(640, 360, 3));
    c.extend_from_slice(&chunk(b"JUNK", &[0xAA; 7]));
    c.extend_from_slice(&strl(b"vids", b"MJPG", 1001, 30000, &bih(b"MJPG")));
    c.extend_from_slice(&strl(b"txts", b"\0\0\0\0", 1, 1000, &[]));
    c.extend_from_slice(&list(b"odml", &chunk(b"dmlh", &[0; 248])));
    c.extend_from_slice(&strl(b"auds", b"\0\0\0\0", 1, 44100, &wfx(1, 1, 44100, 16)));
    list(b"hdrl", &c)
}

/// Hand-built files exercising every `movi` construct the demuxer handles.
fn crafted_files() -> Vec<(&'static str, Vec<u8>)> {
    let mut files = Vec::new();

    // JUNK + INFO around hdrl; movi with ix##, JUNK, `rec ` groups, a tiny
    // LIST, a non-`rec ` LIST, odd sizes, a data chunk for a skipped stream;
    // then idx1 and an OpenDML `RIFF AVIX` extension (ignored).
    {
        let mut movi = chunk(b"00dc", b"odd-frame");
        movi.extend_from_slice(&chunk(b"ix00", &[1; 24]));
        movi.extend_from_slice(&chunk(b"JUNK", &[0; 3]));
        let mut rec = chunk(b"00dc", b"in-rec-video!");
        rec.extend_from_slice(&chunk(b"01wb", b"aud"));
        rec.extend_from_slice(&chunk(b"02wb", b""));
        movi.extend_from_slice(&list(b"rec ", &rec));
        movi.extend_from_slice(&chunk(b"LIST", b"xy")); // LIST too small for a type
        movi.extend_from_slice(&list(b"abcd", &chunk(b"00dc", b"hidden")));
        movi.extend_from_slice(&chunk(b"01wb", b"after-lists"));
        movi.extend_from_slice(&chunk(b"02tx", b"text for the skipped txts stream"));
        movi.extend_from_slice(&chunk(b"1xdc", b"not two digits"));
        movi.extend_from_slice(&chunk(b"00db", b"uncompressed-tag"));
        let mut body = chunk(b"JUNK", &[0; 11]);
        body.extend_from_slice(&rich_hdrl());
        body.extend_from_slice(&list(b"INFO", &chunk(b"ISFT", b"rff\0")));
        body.extend_from_slice(&chunk(b"JUNK", &[0; 4]));
        body.extend_from_slice(&list(b"movi", &movi));
        body.extend_from_slice(&chunk(b"idx1", &[0; 32]));
        let mut f = riff(b"AVI ", &body);
        f.extend_from_slice(&riff(
            b"AVIX",
            &list(b"movi", &chunk(b"00dc", b"avix-frame")),
        ));
        files.push(("rich", f));
    }

    // A `rec ` whose child overruns the group: the group ends there and the
    // walk resumes after the group.
    {
        let mut rec = chunk(b"00dc", b"ok");
        rec.extend_from_slice(b"01wb");
        rec.extend_from_slice(&le32(1000)); // claims more than the group holds
        rec.extend_from_slice(b"short");
        let mut movi = list(b"rec ", &rec);
        movi.extend_from_slice(&chunk(b"00dc", b"after-bad-rec"));
        let mut body = rich_hdrl();
        body.extend_from_slice(&list(b"movi", &movi));
        files.push(("rec-overrun", riff(b"AVI ", &body)));
    }

    // A `rec ` with an odd last child whose pad byte the group's size does
    // not cover (a sloppy writer): the walk resumes at the group's end.
    {
        let mut rec = chunk(b"00dc", b"evn");
        rec.pop(); // drop the pad: group size excludes it
        let mut grp = b"LIST".to_vec();
        grp.extend_from_slice(&le32(4 + rec.len() as u32));
        grp.extend_from_slice(b"rec ");
        grp.extend_from_slice(&rec);
        let mut movi = grp;
        movi.extend_from_slice(&chunk(b"01wb", b"next"));
        movi.extend_from_slice(&chunk(b"00dc", b"last"));
        let mut body = rich_hdrl();
        body.extend_from_slice(&list(b"movi", &movi));
        files.push(("rec-odd-unpadded", riff(b"AVI ", &body)));
    }

    // A `rec ` with an odd declared size (pad byte after the group).
    {
        let rec = chunk(b"01wb", b"\x01");
        let mut grp = b"LIST".to_vec();
        grp.extend_from_slice(&le32(4 + rec.len() as u32 - 1)); // odd
        grp.extend_from_slice(b"rec ");
        grp.extend_from_slice(&rec[..rec.len() - 1]);
        grp.push(0); // the group's own pad
        let mut movi = grp;
        movi.extend_from_slice(&chunk(b"00dc", b"after-odd-rec"));
        let mut body = rich_hdrl();
        body.extend_from_slice(&list(b"movi", &movi));
        files.push(("rec-odd-size", riff(b"AVI ", &body)));
    }

    // A movi child that overruns the movi list: the walk stops there.
    {
        let mut movi = chunk(b"00dc", b"first");
        movi.extend_from_slice(b"00dc");
        movi.extend_from_slice(&le32(0xFFFF_FFF0));
        movi.extend_from_slice(b"tail");
        let mut body = rich_hdrl();
        body.extend_from_slice(&list(b"movi", &movi));
        body.extend_from_slice(&chunk(b"idx1", &[0; 16]));
        files.push(("movi-overrun", riff(b"AVI ", &body)));
    }

    // Trailing bytes inside movi too short for a chunk header.
    {
        let mut movi = chunk(b"00dc", b"frame");
        movi.extend_from_slice(b"\0\0\0");
        let mut body = rich_hdrl();
        body.extend_from_slice(&list(b"movi", &movi));
        files.push(("movi-trailing", riff(b"AVI ", &body)));
    }

    // Header only, no movi; empty movi; two movi lists (first wins); a
    // second hdrl (first wins).
    {
        files.push(("no-movi", riff(b"AVI ", &rich_hdrl())));

        let mut body = rich_hdrl();
        body.extend_from_slice(&list(b"movi", &[]));
        files.push(("empty-movi", riff(b"AVI ", &body)));

        let mut body = rich_hdrl();
        body.extend_from_slice(&list(b"movi", &chunk(b"00dc", b"one")));
        body.extend_from_slice(&list(b"movi", &chunk(b"00dc", b"two")));
        files.push(("two-movi", riff(b"AVI ", &body)));

        let other = list(b"hdrl", &strl(b"vids", b"H264", 1, 25, &bih(b"H264")));
        let mut body = rich_hdrl();
        body.extend_from_slice(&other);
        body.extend_from_slice(&list(b"movi", &chunk(b"00dc", b"x")));
        files.push(("two-hdrl", riff(b"AVI ", &body)));
    }

    // A RIFF size field that lies (ignored, as before).
    {
        let mut body = rich_hdrl();
        body.extend_from_slice(&list(b"movi", &chunk(b"01wb", b"pcm!")));
        let mut f = riff(b"AVI ", &body);
        f[4..8].copy_from_slice(&le32(4));
        files.push(("riff-size-lies", f));
    }

    // Not AVI at all / too short: errors on both sides.
    files.push(("wave", riff(b"WAVE", &chunk(b"fmt ", &[0; 16]))));
    files.push(("short", b"RIFF\0\0".to_vec()));
    files.push(("empty", Vec::new()));
    files.push(("no-hdrl", riff(b"AVI ", &chunk(b"JUNK", &[0; 8]))));

    files
}

/// Muxer-produced files: interleaved A/V with odd sizes, single-stream MJPEG
/// with an empty packet, float audio, and a 12-stream file (two-digit ids).
fn muxed_files() -> Vec<(&'static str, Vec<u8>)> {
    let mut rng = Rng(0x9E37_79B9_7F4A_7C15);
    let mut files = vec![
        ("av-small", muxed_av(&mut rng, 12, 400)),
        ("av-medium", muxed_av(&mut rng, 60, 20_000)),
    ];

    let mj = [video_stream(0, CodecId::Jpeg, 64, 48)];
    let pk = [
        Packet::from_data(0, rng.bytes(777)),
        Packet::from_data(0, Vec::new()),
        Packet::from_data(0, rng.bytes(2)),
    ];
    files.push(("mjpeg", mux(&mj, &pk)));

    let fa = [audio_stream(0, SampleFormat::F32, 22_050, 1)];
    let pk: Vec<Packet> = (0..9)
        .map(|i| Packet::from_data(0, rng.bytes(4 * i + 1)))
        .collect();
    files.push(("f32-audio", mux(&fa, &pk)));

    let mut many = Vec::new();
    for i in 0..12 {
        if i % 3 == 2 {
            many.push(audio_stream(i, SampleFormat::S16, 8000, 1));
        } else {
            let mut v = video_stream(i, CodecId::H264, 16 + i as u32, 16);
            v.time_base = Rational::new(1, 25 + i as i32);
            many.push(v);
        }
    }
    let pk: Vec<Packet> = (0..48)
        .map(|i| {
            let n = 1 + rng.below(99);
            Packet::from_data(i % 12, rng.bytes(n))
        })
        .collect();
    files.push(("twelve-streams", mux(&many, &pk)));

    files
}

fn all_files() -> Vec<(&'static str, Vec<u8>)> {
    let mut f = muxed_files();
    f.extend(crafted_files());
    f
}

fn is_prefix(short: &[PacketRecord], long: &[PacketRecord]) -> bool {
    short.len() <= long.len() && short == &long[..short.len()]
}

// ===========================================================================
// Tests
// ===========================================================================

/// The streaming demuxer reproduces the whole-file demuxer exactly — stream
/// fields and every packet field — on muxer-produced and hand-built files.
#[test]
fn matches_whole_file_demuxer() {
    let (mut with_packets, mut total_packets) = (0, 0);
    for (name, file) in all_files() {
        let old = record_legacy(&file);
        let new = record_new(&file);
        assert_eq!(new, old, "{name}: streaming output differs");
        if let Some(rec) = &new {
            with_packets += usize::from(!rec.packets.is_empty());
            total_packets += rec.packets.len();
        }
    }
    // Guard against a vacuous comparison.
    assert!(
        with_packets >= 12,
        "only {with_packets} files produced packets"
    );
    assert!(
        total_packets >= 200,
        "only {total_packets} packets compared"
    );
}

/// Same, through a pipe that returns at most a few bytes per `read`.
#[test]
fn matches_whole_file_demuxer_over_a_trickling_pipe() {
    for (name, file) in all_files() {
        let old = record_legacy(&file);
        for step in [1, 3, 7, 4096] {
            let input = Box::new(Trickle {
                inner: Cursor::new(file.clone()),
                step,
            });
            let new = record_new_from(input, file.len());
            assert_eq!(new, old, "{name} (step {step}): streaming output differs");
        }
    }
}

/// Opening a file costs memory/IO proportional to its header, not its size.
#[test]
fn read_header_reads_only_a_small_prefix() {
    let mut rng = Rng(7);
    let file = muxed_av(&mut rng, 120, 100_000);
    assert!(file.len() > 4 << 20, "test file should be several MB");

    let pulled = Arc::new(AtomicUsize::new(0));
    let input = Box::new(Counting {
        inner: Cursor::new(file.clone()),
        pulled: pulled.clone(),
    });
    let mut dem = AviDemuxer::new(input);
    let streams = dem.read_header().unwrap();
    assert_eq!(streams.len(), 2);
    let after_header = pulled.load(Ordering::Relaxed);
    assert!(
        after_header < 64 << 10,
        "read_header pulled {after_header} bytes of a {}-byte file",
        file.len()
    );

    // And packets stream: after one packet we are still far from the end.
    let first = dem.read_packet().unwrap();
    let after_one = pulled.load(Ordering::Relaxed);
    assert!(after_one < first.data.len() + (64 << 10));

    let mut n = 1;
    while dem.read_packet().is_ok() {
        n += 1;
    }
    assert_eq!(n, 240);
}

/// Truncating a file at any byte never panics or hangs. Everything before the
/// cut that the whole-file demuxer produced is still produced; a file cut
/// inside `movi` now yields its complete chunks (the old demuxer, which needed
/// the whole `movi` list in memory, yielded none).
#[test]
fn truncated_inputs_terminate_and_yield_a_prefix() {
    let mut rng = Rng(11);
    let mut cases = vec![("av", muxed_av(&mut rng, 8, 300))];
    cases.extend(crafted_files());
    for (name, file) in cases {
        let full = record_new(&file);
        for cut in 0..file.len() {
            let part = &file[..cut];
            let old = record_legacy(part);
            let new = record_new(part);
            assert_eq!(
                new.is_some(),
                old.is_some(),
                "{name} cut {cut}: header result"
            );
            if let (Some(old), Some(new)) = (old, new) {
                assert_eq!(new.streams, old.streams, "{name} cut {cut}");
                assert!(is_prefix(&old.packets, &new.packets), "{name} cut {cut}");
                let full = full.as_ref().unwrap();
                assert!(is_prefix(&new.packets, &full.packets), "{name} cut {cut}");
            }
        }
    }
}

/// Random garbage and random corruption of valid files never panic and always
/// terminate.
#[test]
fn garbage_and_corrupt_inputs_terminate() {
    let mut rng = Rng(0xDEAD_BEEF);
    for _ in 0..300 {
        let n = rng.below(600);
        let mut g = rng.bytes(n);
        record_new(&g);
        if g.len() >= 12 {
            g[0..4].copy_from_slice(b"RIFF");
            g[8..12].copy_from_slice(b"AVI ");
            record_new(&g);
        }
    }

    let mut seeds: Vec<Vec<u8>> = crafted_files().into_iter().map(|(_, f)| f).collect();
    seeds.push(muxed_av(&mut rng, 6, 200));
    for seed in &seeds {
        if seed.len() < 16 {
            continue;
        }
        for _ in 0..400 {
            let mut f = seed.clone();
            for _ in 0..1 + rng.below(6) {
                let at = 12 + rng.below(f.len() - 12);
                f[at] = rng.next() as u8;
            }
            // Bias toward size fields: overwrite an aligned u32 sometimes.
            if rng.below(2) == 0 {
                let at = 12 + rng.below((f.len() - 16) / 4) * 4;
                let v = [0, 1, 3, 4, 7, 8, 0x7FFF_FFFF, 0xFFFF_FFFF][rng.below(8)];
                f[at..at + 4].copy_from_slice(&le32(v));
            }
            record_new(&f);
        }
    }
}

/// Declared sizes far beyond the data present are bounded by what is read:
/// no huge allocation, prompt termination.
#[test]
fn huge_declared_sizes_are_harmless() {
    // hdrl claiming 4 GiB: over the header cap, rejected.
    let mut f = b"RIFF\0\0\0\0AVI LIST".to_vec();
    f.extend_from_slice(&le32(0xFFFF_FFFF));
    f.extend_from_slice(b"hdrl");
    assert!(record_new(&f).is_none());

    // hdrl claiming 15 MiB with only a few bytes present: truncated header.
    let mut f = b"RIFF\0\0\0\0AVI LIST".to_vec();
    f.extend_from_slice(&le32(15 << 20));
    f.extend_from_slice(b"hdrl");
    f.extend_from_slice(&rich_hdrl());
    assert!(record_new(&f).is_none());

    // JUNK claiming 4 GiB before hdrl: skipped by streaming, hits EOF.
    let mut f = b"RIFF\0\0\0\0AVI JUNK".to_vec();
    f.extend_from_slice(&le32(0xFFFF_FFFF));
    assert!(record_new(&f).is_none());

    // movi and a data chunk claiming 4 GiB with a few bytes present.
    let mut f = b"RIFF\0\0\0\0AVI ".to_vec();
    f.extend_from_slice(&rich_hdrl());
    f.extend_from_slice(b"LIST");
    f.extend_from_slice(&le32(0xFFFF_FFFF));
    f.extend_from_slice(b"movi");
    f.extend_from_slice(&chunk(b"00dc", b"real"));
    f.extend_from_slice(b"00dc");
    f.extend_from_slice(&le32(0xFFFF_FF00));
    f.extend_from_slice(b"only a little data");
    let rec = record_new(&f).unwrap();
    assert_eq!(rec.packets.len(), 1);
    assert_eq!(rec.packets[0].6, b"real");
}

#[test]
fn header_twice_and_packet_before_header() {
    let mut rng = Rng(3);
    let file = muxed_av(&mut rng, 2, 50);

    let mut dem = AviDemuxer::new(Box::new(Cursor::new(file.clone())));
    assert!(matches!(dem.read_packet(), Err(Error::Eof)));

    let mut dem = AviDemuxer::new(Box::new(Cursor::new(file)));
    dem.read_header().unwrap();
    assert!(dem.read_header().is_err());
}
