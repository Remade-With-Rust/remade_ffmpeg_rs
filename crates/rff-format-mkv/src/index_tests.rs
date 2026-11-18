//! Random access (#114): the sample index against `read_packet`, raw
//! samples, and the codecs the muxer learned with it (HEVC, ProRes, more PCM).

use std::io::{Cursor, Read, Seek, SeekFrom, Write};
use std::sync::atomic::{AtomicU64, Ordering};
use std::sync::{Arc, Mutex};

use rff_core::{BlockAdditional, CodecId, Error, Packet, Rational, SampleFormat};
use rff_format::{Demuxer, FormatRegistry, Muxer, Stream};

use crate::{
    MkvDemuxer, MkvMuxer, MkvStreamingMuxer, ID_AUDIO, ID_BLOCK, ID_BLOCK_ADDITIONAL,
    ID_BLOCK_ADDITIONS, ID_BLOCK_DURATION, ID_BLOCK_GROUP, ID_BLOCK_MORE, ID_CHANNELS, ID_CLUSTER,
    ID_CODEC_DELAY, ID_CODEC_ID, ID_CODEC_PRIVATE, ID_CONTENT_ENCODINGS, ID_DEFAULT_DURATION,
    ID_DISCARD_PADDING, ID_INFO, ID_REFERENCE_BLOCK, ID_SAMPLING_FREQUENCY, ID_SEGMENT,
    ID_SIMPLE_BLOCK, ID_TAGS, ID_TIMESTAMP, ID_TIMESTAMP_SCALE, ID_TRACKS, ID_TRACK_ENTRY,
    ID_TRACK_NUMBER, ID_TRACK_TYPE,
};

// ---- muxing in memory ------------------------------------------------------

#[derive(Clone, Default)]
struct Shared(Arc<Mutex<Cursor<Vec<u8>>>>);
impl Write for Shared {
    fn write(&mut self, buf: &[u8]) -> std::io::Result<usize> {
        self.0.lock().unwrap().write(buf)
    }
    fn flush(&mut self) -> std::io::Result<()> {
        Ok(())
    }
}
impl Seek for Shared {
    fn seek(&mut self, pos: SeekFrom) -> std::io::Result<u64> {
        self.0.lock().unwrap().seek(pos)
    }
}

fn try_mux(streamed: bool, streams: &[Stream], packets: &[Packet]) -> rff_core::Result<Vec<u8>> {
    let sink = Shared::default();
    let mut mux: Box<dyn Muxer> = if streamed {
        Box::new(MkvStreamingMuxer::new(Box::new(sink.clone()), false))
    } else {
        Box::new(MkvMuxer::new(Box::new(sink.clone()), false))
    };
    mux.write_header(streams)?;
    for p in packets {
        mux.write_packet(p)?;
    }
    mux.write_trailer()?;
    drop(mux);
    let file = sink.0.lock().unwrap().get_ref().clone();
    Ok(file)
}

fn mux(streamed: bool, streams: &[Stream], packets: &[Packet]) -> Vec<u8> {
    try_mux(streamed, streams, packets).unwrap()
}

fn pkt(stream: usize, pts: i64, key: bool, data: &[u8]) -> Packet {
    let mut p = Packet::from_data(stream, data.to_vec());
    p.pts = Some(pts);
    p.flags.keyframe = key;
    p
}

struct Lcg(u64);
impl Lcg {
    fn next(&mut self) -> u64 {
        self.0 = self
            .0
            .wrapping_mul(6_364_136_223_846_793_005)
            .wrapping_add(1_442_695_040_888_963_407);
        self.0 >> 33
    }
    /// Never zero, so H.264/HEVC NALs carry no start-code emulation.
    fn bytes(&mut self, n: usize) -> Vec<u8> {
        (0..n).map(|_| (self.next() as u8) | 0x10).collect()
    }
}

fn annexb(nals: &[&[u8]]) -> Vec<u8> {
    let mut out = Vec::new();
    for nal in nals {
        out.extend_from_slice(&[0, 0, 0, 1]);
        out.extend_from_slice(nal);
    }
    out
}

/// Annex-B NALs → 4-byte length-prefixed, as Matroska stores them.
fn length_prefixed(nals: &[&[u8]]) -> Vec<u8> {
    let mut out = Vec::new();
    for nal in nals {
        out.extend_from_slice(&(nal.len() as u32).to_be_bytes());
        out.extend_from_slice(nal);
    }
    out
}

// ---- demuxing --------------------------------------------------------------

fn seekable(file: &[u8]) -> MkvDemuxer {
    MkvDemuxer::new_seekable(Box::new(Cursor::new(file.to_vec())))
}

/// Everything about a packet the index promises to reproduce.
type Key = (
    usize,
    Option<i64>,
    Option<i64>,
    i64,
    bool,
    Vec<u8>,
    Vec<BlockAdditional>,
    u32,
);

fn key(p: &Packet) -> Key {
    (
        p.stream_index,
        p.pts,
        p.dts,
        p.duration,
        p.flags.keyframe,
        p.data.clone(),
        p.additions.clone(),
        p.discard_padding,
    )
}

fn read_all(dem: &mut dyn Demuxer) -> Vec<Packet> {
    let mut out = Vec::new();
    loop {
        match dem.read_packet() {
            Ok(p) => out.push(p),
            Err(Error::Eof) => return out,
            Err(e) => panic!("read_packet: {e}"),
        }
    }
}

/// The packets `read_packet` returns, per stream, from a plain (streaming)
/// demuxer.
fn by_stream(file: &[u8], raw: bool) -> (Vec<Stream>, Vec<Vec<Packet>>) {
    let mut dem = MkvDemuxer::new(Box::new(Cursor::new(file.to_vec())));
    let streams = dem.read_header().unwrap();
    dem.set_raw_samples(raw);
    let mut out = vec![Vec::new(); streams.len()];
    for p in read_all(&mut dem) {
        out[p.stream_index].push(p);
    }
    (streams, out)
}

/// The standing check: every stream's index matches what `read_packet`
/// returns — entry by entry and sample by sample, cooked and raw — and the
/// stored bytes sit at `offset`/`size`.
fn check_index(file: &[u8]) {
    for raw in [false, true] {
        let (streams, want) = by_stream(file, raw);
        let mut dem = seekable(file);
        dem.read_header().unwrap();
        assert!(dem.set_raw_samples(raw));
        let idx = dem.sample_index().expect("a seekable input has an index");
        for (s, packets) in want.iter().enumerate() {
            assert_eq!(idx.sample_count(s), packets.len() as u64, "stream {s}");
            for (i, p) in packets.iter().enumerate() {
                let info = idx.sample_info(s, i as u64).unwrap();
                if let Some(pts) = p.pts {
                    assert_eq!((info.pts, info.dts), (pts, pts), "stream {s} sample {i}");
                }
                assert_eq!(info.duration, p.duration, "stream {s} sample {i}");
                assert_eq!(info.sync, p.flags.keyframe, "stream {s} sample {i}");
                let got = idx.read_sample(s, i as u64).unwrap();
                assert_eq!(key(&got), key(p), "stream {s} sample {i} raw={raw}");
                let stored = &file[info.offset as usize..][..info.size as usize];
                // Raw frames of tracks without ContentEncodings or a
                // restored header are the stored bytes themselves.
                if raw && streams[s].codec_id != CodecId::ProRes && !is_encoded(s, &streams) {
                    assert_eq!(stored, &got.data[..], "stream {s} sample {i}");
                }
            }
            assert!(idx.sample_info(s, packets.len() as u64).is_none());
            assert!(idx.read_sample(s, packets.len() as u64).is_err());
        }
        assert_eq!(idx.sample_count(streams.len()), 0);
    }
}

/// Track 4 of [`fx_mixed`] is header-stripped.
fn is_encoded(stream: usize, streams: &[Stream]) -> bool {
    streams.len() == 4 && stream == 3
}

// ---- fixtures --------------------------------------------------------------

/// H.264 with B-frames (decode order I P B B ...; GOPs of 12 at 25 fps),
/// Opus and SubRip, through rff's muxer: the index has to follow the
/// decode order and keep each frame's own presentation time.
fn fx_h264_bframes(streamed: bool) -> Vec<u8> {
    let mut r = Lcg(11);
    let mut v = Stream::new(0, CodecId::H264);
    v.width = 64;
    v.height = 48;
    let mut a = Stream::new(1, CodecId::Opus);
    a.sample_rate = 48_000;
    a.channels = 2;
    a.time_base = Rational::new(1, 48_000);
    let s = Stream::new(2, CodecId::Subrip);
    let sps = [&[0x67u8, 0x42, 0x00, 0x1E][..], &r.bytes(6)].concat();
    let pps = [&[0x68u8][..], &r.bytes(3)].concat();
    let order = [0i64, 3, 1, 2, 6, 4, 5, 9, 7, 8, 11, 10];
    let mut packets = Vec::new();
    for gop in 0..4i64 {
        for (i, &d) in order.iter().enumerate() {
            let key = i == 0;
            let n = 40 + (r.next() % 300) as usize;
            let slice = [&[if key { 0x65u8 } else { 0x41 }][..], &r.bytes(n)].concat();
            let data = if key {
                annexb(&[&sps, &pps, &slice])
            } else {
                annexb(&[&slice])
            };
            let mut p = pkt(0, (gop * 12 + d) * 40, key, &data);
            p.dts = Some((gop * 12 + i as i64) * 40 - 80);
            packets.push(p);
        }
    }
    for i in 0..96i64 {
        let n = 20 + (r.next() % 100) as usize;
        let mut p = pkt(1, i * 960, true, &r.bytes(n));
        p.duration = 960;
        packets.push(p);
    }
    for i in 0..3i64 {
        let mut cue = pkt(2, 300 + i * 500, true, format!("line {i}").as_bytes());
        cue.duration = 250;
        packets.push(cue);
    }
    mux(streamed, &[v, a, s], &packets)
}

// A tiny EBML writer (8-byte sizes) for hand-built files.
fn put_id(out: &mut Vec<u8>, id: u32) {
    let n = 4 - (id.leading_zeros() / 8) as usize;
    for i in (0..n).rev() {
        out.push((id >> (8 * i)) as u8);
    }
}
fn elem(out: &mut Vec<u8>, id: u32, body: &[u8]) {
    put_id(out, id);
    out.push(0x01);
    out.extend_from_slice(&(body.len() as u64).to_be_bytes()[1..]);
    out.extend_from_slice(body);
}
fn el(id: u32, body: &[u8]) -> Vec<u8> {
    let mut out = Vec::new();
    elem(&mut out, id, body);
    out
}
fn uint(out: &mut Vec<u8>, id: u32, v: u64) {
    elem(out, id, &v.to_be_bytes());
}
fn open_unknown(out: &mut Vec<u8>, id: u32) {
    put_id(out, id);
    out.extend_from_slice(&[0x01, 0xFF, 0xFF, 0xFF, 0xFF, 0xFF, 0xFF, 0xFF]);
}
fn block(track: u8, rel: i16, flags: u8, payload: &[u8]) -> Vec<u8> {
    [&[0x80 | track][..], &rel.to_be_bytes(), &[flags], payload].concat()
}
fn track(number: u64, ttype: u64, codec: &str, private: &[u8], extra: &[u8]) -> Vec<u8> {
    let mut te = Vec::new();
    uint(&mut te, ID_TRACK_NUMBER, number);
    uint(&mut te, ID_TRACK_TYPE, ttype);
    elem(&mut te, ID_CODEC_ID, codec.as_bytes());
    if !private.is_empty() {
        elem(&mut te, ID_CODEC_PRIVATE, private);
    }
    te.extend_from_slice(extra);
    el(ID_TRACK_ENTRY, &te)
}
fn audio(rate: f64) -> Vec<u8> {
    let mut a = Vec::new();
    elem(&mut a, ID_SAMPLING_FREQUENCY, &rate.to_be_bytes());
    uint(&mut a, ID_CHANNELS, 2);
    el(ID_AUDIO, &a)
}
fn frame(n: u8, len: usize) -> Vec<u8> {
    vec![n; len]
}

/// Hand-built, no Cues: an unknown-size Segment; VP9 + laced AAC + ASS +
/// header-stripped Opus with a CodecDelay; a known-size Cluster, then
/// unknown-size ones ended by Tags and by end of input; SimpleBlocks,
/// BlockGroups with ReferenceBlock, BlockDuration, DiscardPadding and
/// BlockAdditions (before the Block); Xiph, EBML and fixed lacing; a Void and
/// a block on no track inside Clusters.
fn fx_mixed() -> Vec<u8> {
    let mut f = el(0x1A45_DFA3, &el(0x4282, b"matroska"));
    open_unknown(&mut f, ID_SEGMENT);
    let mut info = Vec::new();
    uint(&mut info, ID_TIMESTAMP_SCALE, 1_000_000);
    elem(&mut f, ID_INFO, &info);
    let mut tracks = Vec::new();
    let mut dd = Vec::new();
    uint(&mut dd, ID_DEFAULT_DURATION, 40_000_000);
    tracks.extend(track(1, 1, "V_VP9", &[], &dd));
    let mut aac = audio(48_000.0);
    uint(&mut aac, ID_DEFAULT_DURATION, 21_333_333);
    tracks.extend(track(2, 2, "A_AAC", &[0x11, 0x90], &aac));
    tracks.extend(track(3, 17, "S_TEXT/ASS", b"[Script Info]\n", &[]));
    // ContentEncoding: header stripping (algo 3) of [0xFC, 0xAA], frames.
    let mut comp = Vec::new();
    uint(&mut comp, 0x4254, 3);
    elem(&mut comp, 0x4255, &[0xFC, 0xAA]);
    let mut enc = Vec::new();
    uint(&mut enc, 0x5032, 1);
    elem(&mut enc, 0x5034, &comp);
    let mut opus = audio(48_000.0);
    elem(&mut opus, ID_CONTENT_ENCODINGS, &el(0x6240, &enc));
    uint(&mut opus, ID_CODEC_DELAY, 6_500_000);
    tracks.extend(track(4, 2, "A_OPUS", b"OpusHead\x01\x02", &opus));
    elem(&mut f, ID_TRACKS, &tracks);

    let xiph = [
        &[2, 255, 45, 7][..],
        &frame(1, 300),
        &frame(2, 7),
        &frame(3, 20),
    ]
    .concat();
    let sizes = [3, 0x80 | 100, 0x80 | (63 - 10), 0x80 | (63 + 40)];
    let ebml = [
        &sizes[..],
        &frame(4, 100),
        &frame(5, 90),
        &frame(6, 130),
        &frame(7, 5),
    ]
    .concat();
    let fixed = [&[1][..], &frame(8, 16), &frame(9, 16)].concat();

    // Cluster 1: known size.
    let mut c = Vec::new();
    uint(&mut c, ID_TIMESTAMP, 0);
    elem(
        &mut c,
        ID_SIMPLE_BLOCK,
        &block(1, 0, 0x80, &frame(0x21, 500)),
    );
    elem(&mut c, ID_SIMPLE_BLOCK, &block(2, 0, 0x82, &xiph));
    let mut g = Vec::new();
    elem(&mut g, ID_BLOCK, &block(1, 80, 0, &frame(0x22, 200)));
    elem(&mut g, ID_REFERENCE_BLOCK, &[0xD8]); // -40
    elem(&mut c, ID_BLOCK_GROUP, &g);
    elem(
        &mut c,
        ID_SIMPLE_BLOCK,
        &block(1, 40, 0x00, &frame(0x23, 150)),
    );
    let mut g = Vec::new();
    uint(&mut g, ID_BLOCK_DURATION, 700);
    elem(
        &mut g,
        ID_BLOCK,
        &block(3, 100, 0, b"0,0,Default,,0,0,0,,Hello, index"),
    );
    elem(&mut c, ID_BLOCK_GROUP, &g);
    elem(
        &mut c,
        ID_SIMPLE_BLOCK,
        &block(4, 0, 0x80, &frame(0x31, 30)),
    );
    let mut g = Vec::new();
    elem(&mut g, ID_BLOCK, &block(4, 20, 0, &frame(0x32, 31)));
    elem(&mut g, ID_DISCARD_PADDING, &6_500_000u32.to_be_bytes());
    elem(&mut c, ID_BLOCK_GROUP, &g);
    elem(
        &mut c,
        ID_SIMPLE_BLOCK,
        &block(9, 0, 0x80, b"no such track"),
    );
    elem(&mut f, ID_CLUSTER, &c);

    // Cluster 2: unknown size, ended by Tags.
    open_unknown(&mut f, ID_CLUSTER);
    uint(&mut f, ID_TIMESTAMP, 1000);
    elem(
        &mut f,
        ID_SIMPLE_BLOCK,
        &block(1, 0, 0x80, &frame(0x24, 400)),
    );
    elem(&mut f, ID_SIMPLE_BLOCK, &block(2, 5, 0x06, &ebml));
    let mut more = Vec::new();
    elem(&mut more, ID_BLOCK_ADDITIONAL, b"alpha plane");
    let mut g = Vec::new();
    elem(&mut g, ID_BLOCK_ADDITIONS, &el(ID_BLOCK_MORE, &more));
    elem(&mut g, ID_BLOCK, &block(1, 40, 0, &frame(0x25, 120)));
    elem(&mut g, ID_REFERENCE_BLOCK, &[0xD8]);
    elem(&mut f, ID_BLOCK_GROUP, &g);
    elem(&mut f, 0xEC, &[0; 6]); // Void
    elem(&mut f, ID_SIMPLE_BLOCK, &block(2, 90, 0x84, &fixed));
    elem(&mut f, ID_TAGS, b"tags");

    // Cluster 3: unknown size, to end of input.
    open_unknown(&mut f, ID_CLUSTER);
    uint(&mut f, ID_TIMESTAMP, 2000);
    elem(
        &mut f,
        ID_SIMPLE_BLOCK,
        &block(1, 0, 0x80, &frame(0x26, 300)),
    );
    elem(
        &mut f,
        ID_SIMPLE_BLOCK,
        &block(4, 3, 0x80, &frame(0x33, 25)),
    );
    elem(
        &mut f,
        ID_SIMPLE_BLOCK,
        &block(1, 40, 0x00, &frame(0x27, 90)),
    );
    f
}

// ---- the index -------------------------------------------------------------

#[test]
fn index_matches_read_packet_on_muxed_files_with_cues() {
    for streamed in [false, true] {
        let file = fx_h264_bframes(streamed);
        check_index(&file);
        let mut dem = seekable(&file);
        dem.read_header().unwrap();
        let idx = dem.sample_index().unwrap();
        assert_eq!(idx.sample_count(0), 48);
        assert_eq!(idx.sample_count(1), 96);
        assert_eq!(idx.sample_count(2), 3);
        // Decode order, each frame its own pts: I0 P3 B1 B2 ...
        let pts: Vec<i64> = (0..5).map(|i| idx.sample_info(0, i).unwrap().pts).collect();
        assert_eq!(pts, [0, 120, 40, 80, 240]);
    }
}

#[test]
fn index_matches_read_packet_without_cues_and_with_unknown_size_clusters() {
    let file = fx_mixed();
    check_index(&file);
    let mut dem = seekable(&file);
    dem.read_header().unwrap();
    let idx = dem.sample_index().unwrap();
    // Laced blocks: one entry per frame (3 Xiph + 4 EBML + 2 fixed).
    assert_eq!(idx.sample_count(1), 9);
    let second = idx.sample_info(1, 1).unwrap();
    assert_eq!((second.size, second.pts), (7, 21));
    assert_eq!(&file[second.offset as usize..][..7], &frame(2, 7)[..]);
    // The block on no track is not indexed; the alpha addition rides along.
    assert_eq!(idx.sample_count(0), 7);
    let alpha = idx.read_sample(0, 4).unwrap();
    assert_eq!(alpha.additions[0].data, b"alpha plane");
    assert!(!alpha.flags.keyframe);
    // Header stripping is undone; the stored bytes are the stripped ones.
    let opus = idx.read_sample(3, 0).unwrap();
    assert_eq!(&opus.data[..3], &[0xFC, 0xAA, 0x31]);
    assert_eq!(idx.sample_info(3, 0).unwrap().size, 30);
    assert_eq!(opus.pts, Some(-7)); // CodecDelay 6.5 ms rounds to 7
    assert_eq!(idx.read_sample(3, 1).unwrap().discard_padding, 312);
}

#[test]
fn sync_at_or_before_finds_the_keyframe_that_shows_a_time() {
    let file = fx_h264_bframes(true);
    let mut dem = seekable(&file);
    dem.read_header().unwrap();
    let idx = dem.sample_index().unwrap();
    // Keyframes at pts 0, 480, 960, 1440 are decode-order samples 0, 12, 24, 36.
    for (pts, want) in [
        (0, Some(0)),
        (300, Some(0)),
        (479, Some(0)),
        (480, Some(12)),
        (700, Some(12)),
        (1000, Some(24)),
        (99_999, Some(36)),
        (-1, None),
    ] {
        assert_eq!(idx.sync_at_or_before(0, pts), want, "pts {pts}");
    }
    // Every audio frame is a sync frame; an unknown stream has none.
    assert_eq!(idx.sync_at_or_before(1, 39), Some(1)); // frames every 20 ms
    assert_eq!(idx.sync_at_or_before(7, 0), None);
    // The hand-built file: VP9 keyframes at 0, 1000, 2000.
    let file = fx_mixed();
    let mut dem = seekable(&file);
    dem.read_header().unwrap();
    let idx = dem.sample_index().unwrap();
    assert_eq!(idx.sync_at_or_before(0, 1500), Some(3));
    assert_eq!(idx.sync_at_or_before(0, 2039), Some(5));
}

#[test]
fn reading_through_the_index_leaves_read_packet_in_place() {
    for file in [fx_h264_bframes(true), fx_mixed()] {
        let mut plain = MkvDemuxer::new(Box::new(Cursor::new(file.clone())));
        plain.read_header().unwrap();
        let want: Vec<Key> = read_all(&mut plain).iter().map(key).collect();

        let mut dem = seekable(&file);
        dem.read_header().unwrap();
        let mut got = Vec::new();
        for n in 0.. {
            // Index work between packets, including before the first one and
            // in the middle of a laced block.
            if n % 3 == 0 {
                let idx = dem.sample_index().unwrap();
                let last = idx.sample_count(0) - 1;
                idx.read_sample(0, last).unwrap();
                idx.read_sample(1, n % 5).unwrap();
            }
            match dem.read_packet() {
                Ok(p) => got.push(key(&p)),
                Err(Error::Eof) => break,
                Err(e) => panic!("{e}"),
            }
        }
        assert_eq!(got, want);
    }
}

#[test]
fn a_plain_input_has_no_index_and_the_registry_opens_files_seekable() {
    let file = fx_mixed();
    let mut plain = MkvDemuxer::new(Box::new(Cursor::new(file.clone())));
    plain.read_header().unwrap();
    assert!(plain.sample_index().is_none());

    let mut reg = FormatRegistry::new();
    crate::register(&mut reg);
    for name in ["matroska", "webm"] {
        let mut dem = reg
            .open_demuxer_seekable(name, Box::new(Cursor::new(file.clone())))
            .unwrap();
        // Before read_header, sample_index reads the header itself.
        let idx = dem.sample_index().expect("seekable factory");
        assert_eq!(idx.sample_count(0), 7);
        assert_eq!(dem.read_header().unwrap().len(), 4);
    }
}

/// Counts the bytes pulled from a seekable input.
struct Counting {
    inner: Cursor<Vec<u8>>,
    read: Arc<AtomicU64>,
}
impl Read for Counting {
    fn read(&mut self, buf: &mut [u8]) -> std::io::Result<usize> {
        let n = self.inner.read(buf)?;
        self.read.fetch_add(n as u64, Ordering::Relaxed);
        Ok(n)
    }
}
impl Seek for Counting {
    fn seek(&mut self, pos: SeekFrom) -> std::io::Result<u64> {
        self.inner.seek(pos)
    }
}

#[test]
fn the_index_scan_seeks_past_frames_and_starts_mid_file() {
    let mut r = Lcg(9);
    let mut v = Stream::new(0, CodecId::Vp9);
    v.width = 640;
    v.height = 360;
    let packets: Vec<Packet> = (0..200i64)
        .map(|i| pkt(0, i * 40, i % 50 == 0, &r.bytes(20_000)))
        .collect();
    let file = mux(true, &[v], &packets);
    // The file sits 1000 bytes into the input: offsets are input offsets.
    let mut input = vec![0xEEu8; 1000];
    input.extend_from_slice(&file);
    let mut cursor = Cursor::new(input.clone());
    cursor.seek(SeekFrom::Start(1000)).unwrap();
    let read = Arc::new(AtomicU64::new(0));
    let mut dem = MkvDemuxer::new_seekable(Box::new(Counting {
        inner: cursor,
        read: read.clone(),
    }));
    dem.read_header().unwrap();
    let before = read.load(Ordering::Relaxed);
    let idx = dem.sample_index().unwrap();
    let scanned = read.load(Ordering::Relaxed) - before;
    assert!(
        scanned < file.len() as u64 / 3,
        "the scan read {scanned} of {} bytes",
        file.len()
    );
    assert_eq!(idx.sample_count(0), 200);
    let info = idx.sample_info(0, 123).unwrap();
    assert_eq!(
        &input[info.offset as usize..][..20_000],
        &packets[123].data[..]
    );
    assert_eq!(idx.read_sample(0, 123).unwrap().data, packets[123].data);
    assert_eq!(idx.sync_at_or_before(0, 5_000), Some(100));
    // read_packet still starts at the first frame.
    assert_eq!(dem.read_packet().unwrap().data, packets[0].data);
}

#[test]
fn truncated_and_garbage_files_index_without_panicking() {
    // Every prefix: the index is exactly what read_packet gets from it.
    let check_prefix = |f: &[u8]| {
        let mut dem = seekable(f);
        if dem.read_header().is_err() {
            assert!(dem.sample_index().is_none());
            return;
        }
        let (_, want) = by_stream(f, false);
        let idx = dem.sample_index().expect("index of a parsed header");
        for (s, packets) in want.iter().enumerate() {
            assert_eq!(idx.sample_count(s), packets.len() as u64, "len {}", f.len());
            for (i, p) in packets.iter().enumerate() {
                let got = idx.read_sample(s, i as u64).unwrap();
                assert_eq!(key(&got), key(p), "len {}", f.len());
            }
        }
    };
    let mixed = fx_mixed();
    for cut in 0..=mixed.len() {
        check_prefix(&mixed[..cut]);
    }
    let muxed = fx_h264_bframes(false);
    for cut in (0..muxed.len()).step_by(41) {
        check_prefix(&muxed[..cut]);
    }
    // Byte flips and garbage: never a panic, whatever comes back.
    let mut r = Lcg(5);
    let poke = |f: Vec<u8>| {
        let mut dem = seekable(&f);
        if dem.read_header().is_err() {
            return;
        }
        let streams = dem.read_header().unwrap().len();
        if let Some(idx) = dem.sample_index() {
            for s in 0..streams {
                for i in 0..idx.sample_count(s) {
                    let _ = idx.sample_info(s, i);
                    let _ = idx.read_sample(s, i);
                }
                let _ = idx.sync_at_or_before(s, 1000);
            }
        }
        while dem.read_packet().is_ok() {}
    };
    for base in [mixed.clone(), muxed.clone()] {
        for _ in 0..400 {
            let mut f = base.clone();
            for _ in 0..1 + r.next() % 4 {
                let at = (r.next() as usize) % f.len();
                f[at] = r.next() as u8;
            }
            poke(f);
        }
    }
    let prefix_end = mixed
        .windows(4)
        .position(|w| w == [0x1F, 0x43, 0xB6, 0x75])
        .unwrap();
    for i in 0..300usize {
        let mut g: Vec<u8> = (0..i * 7 % 1500).map(|_| r.next() as u8).collect();
        poke(g.clone());
        let mut f = mixed[..prefix_end].to_vec();
        open_unknown(&mut f, ID_CLUSTER);
        f.append(&mut g);
        poke(f);
    }
}

// ---- raw samples -----------------------------------------------------------

#[test]
fn raw_samples_keep_h264_length_prefixed_and_extradata_is_the_avcc() {
    let sps = [0x67u8, 0x42, 0x00, 0x1E, 0xAB];
    let pps = [0x68u8, 0xCE];
    let idr = [0x65u8, 1, 2, 3, 4];
    let p_slice = [0x41u8, 9, 8, 7];
    let mut v = Stream::new(0, CodecId::H264);
    v.width = 64;
    v.height = 48;
    let packets = [
        pkt(0, 0, true, &annexb(&[&sps, &pps, &idr])),
        pkt(0, 40, false, &annexb(&[&p_slice])),
    ];
    let file = mux(true, &[v], &packets);
    let avcc = rff_format::avc::build_avcc_record(&sps, &pps);

    // Cooked: Annex-B with the parameter sets on the keyframe.
    let mut dem = seekable(&file);
    let streams = dem.read_header().unwrap();
    assert_eq!(streams[0].extradata, avcc);
    assert_eq!(read_all(&mut dem)[0].data, packets[0].data);

    // Raw: as stored — AVCC, the parameter sets only in the record.
    let mut dem = seekable(&file);
    let streams = dem.read_header().unwrap();
    assert_eq!(streams[0].extradata, avcc);
    assert!(dem.set_raw_samples(true));
    let got = read_all(&mut dem);
    assert_eq!(got[0].data, length_prefixed(&[&idr]));
    assert_eq!(got[1].data, length_prefixed(&[&p_slice]));
    assert!(got[0].flags.keyframe);
    let idx = dem.sample_index().unwrap();
    assert_eq!(
        idx.read_sample(0, 0).unwrap().data,
        length_prefixed(&[&idr])
    );
    check_index(&file);
}

#[test]
fn raw_samples_keep_ass_events_whole_and_still_undo_encodings() {
    let (_, cooked) = by_stream(&fx_mixed(), false);
    let (_, raw) = by_stream(&fx_mixed(), true);
    assert_eq!(cooked[2][0].data, b"Hello, index");
    assert_eq!(raw[2][0].data, b"0,0,Default,,0,0,0,,Hello, index");
    // Header stripping is a Matroska storage detail: undone either way.
    assert_eq!(raw[3][0].data, cooked[3][0].data);
    assert_eq!(&raw[3][0].data[..2], &[0xFC, 0xAA]);
}

// ---- muxer codec coverage --------------------------------------------------

/// The VPS/SPS/PPS of the JCT-VC `IPRED_A_docomo_2` stream (832x480 Main).
const VPS: &[u8] = &[
    0x40, 0x01, 0x0c, 0x01, 0xff, 0xff, 0x01, 0x60, 0x00, 0x00, 0x03, 0x00, 0x00, 0x03, 0x00, 0x00,
    0x03, 0x00, 0x00, 0x03, 0x00, 0x99, 0xf0, 0x24,
];
const SPS: &[u8] = &[
    0x42, 0x01, 0x01, 0x01, 0x60, 0x00, 0x00, 0x03, 0x00, 0x00, 0x03, 0x00, 0x00, 0x03, 0x00, 0x00,
    0x03, 0x00, 0x99, 0xa0, 0x06, 0x82, 0x01, 0xe1, 0xfe, 0x5f, 0x92, 0x46, 0xd9, 0x6c, 0x80,
];
const PPS: &[u8] = &[0x44, 0x01, 0xc1, 0x90, 0x95, 0x81, 0x12];

fn hevc_stream() -> Stream {
    let mut v = Stream::new(0, CodecId::Hevc);
    v.width = 832;
    v.height = 480;
    v
}

#[test]
fn hevc_round_trips_with_an_hvcc_codec_private() {
    let idr = [0x26u8, 0x01, 0xAF, 0x10, 0x20]; // IDR_W_RADL
    let trail = [0x02u8, 0x01, 0xD0, 0x30]; // TRAIL_R
    let packets = [
        pkt(0, 0, true, &annexb(&[VPS, SPS, PPS, &idr])),
        pkt(0, 40, false, &annexb(&[&trail])),
        pkt(0, 80, true, &annexb(&[VPS, SPS, PPS, &idr])),
    ];
    let hvcc = rff_format::hvc::build_hvcc_record(VPS, SPS, PPS).unwrap();
    for streamed in [false, true] {
        let file = mux(streamed, &[hevc_stream()], &packets);
        let mut dem = seekable(&file);
        let streams = dem.read_header().unwrap();
        assert_eq!(streams[0].codec_id, CodecId::Hevc);
        assert_eq!(streams[0].extradata, hvcc, "CodecPrivate is the hvcC");
        // Cooked: exactly the Annex-B packets that went in.
        let got = read_all(&mut dem);
        assert_eq!(got.len(), 3);
        for (g, p) in got.iter().zip(&packets) {
            assert_eq!((&g.data, g.flags.keyframe), (&p.data, p.flags.keyframe));
        }
        // Stored: 4-byte length prefixes, parameter sets hoisted out.
        let (_, raw) = by_stream(&file, true);
        assert_eq!(raw[0][0].data, length_prefixed(&[&idr]));
        assert_eq!(raw[0][1].data, length_prefixed(&[&trail]));
        check_index(&file);
    }

    // An hvcC as extradata stands in for in-band parameter sets.
    let mut s = hevc_stream();
    s.extradata = hvcc.clone();
    let bare = [
        pkt(0, 0, true, &annexb(&[&idr])),
        pkt(0, 40, false, &annexb(&[&trail])),
    ];
    for streamed in [false, true] {
        let (streams, got) = by_stream(&mux(streamed, &[s.clone()], &bare), false);
        assert_eq!(streams[0].extradata, hvcc);
        assert_eq!(got[0][0].data, annexb(&[VPS, SPS, PPS, &idr]));
        assert_eq!(got[0][1].data, annexb(&[&trail]));
    }
    // ...and one declaring 2-byte NAL lengths is rebuilt for 4-byte blocks.
    let mut two = hvcc.clone();
    two[21] = (two[21] & !3) | 1;
    s.extradata = two;
    let (streams, got) = by_stream(&mux(true, &[s], &bare), false);
    assert_eq!(streams[0].extradata, hvcc);
    assert_eq!(got[0][0].data, annexb(&[VPS, SPS, PPS, &idr]));

    // Neither: refused, not written undecodable.
    for streamed in [false, true] {
        let err = try_mux(streamed, &[hevc_stream()], &bare).unwrap_err();
        assert!(err.to_string().contains("HEVC"), "{err}");
    }
}

#[test]
fn prores_is_stored_without_its_frame_header_and_gets_it_back() {
    let prores_frame = |n: u8, len: usize| {
        let mut f = ((len + 8) as u32).to_be_bytes().to_vec();
        f.extend_from_slice(b"icpf");
        f.extend_from_slice(&[0x00, 0x94, 0x00, 0x00]); // frame header start
        f.extend_from_slice(b"apl0");
        f.resize(len + 8, n);
        f
    };
    let packets: Vec<Packet> = (0..4u8)
        .map(|i| {
            pkt(
                0,
                i as i64 * 40,
                true,
                &prores_frame(i + 1, 3000 + i as usize),
            )
        })
        .collect();
    for (tag, want) in [
        (Some(*b"apch"), *b"apch"),
        (None, *b"apcn"),
        (Some(*b"avc1"), *b"apcn"),
    ] {
        let mut s = Stream::new(0, CodecId::ProRes);
        s.width = 1920;
        s.height = 1080;
        s.codec_tag = tag;
        for streamed in [false, true] {
            let file = mux(streamed, &[s.clone()], &packets);
            assert!(
                !file.windows(4).any(|w| w == b"icpf"),
                "the icpf atom header is not stored"
            );
            let mut dem = seekable(&file);
            let streams = dem.read_header().unwrap();
            assert_eq!(streams[0].codec_id, CodecId::ProRes);
            assert_eq!(streams[0].codec_tag, Some(want));
            assert!(streams[0].extradata.is_empty());
            let got = read_all(&mut dem);
            assert_eq!(got.len(), 4);
            for (g, p) in got.iter().zip(&packets) {
                assert_eq!(g.data, p.data, "header restored byte for byte");
            }
            // The index locates the stored (headerless) frame; raw or not,
            // the frame comes back whole.
            let idx = dem.sample_index().unwrap();
            let info = idx.sample_info(0, 2).unwrap();
            assert_eq!(info.size as usize, packets[2].data.len() - 8);
            assert_eq!(&file[info.offset as usize..][..8], &packets[2].data[8..16]);
            assert_eq!(idx.read_sample(0, 2).unwrap().data, packets[2].data);
            check_index(&file);
        }
    }
}

#[test]
fn pcm_round_trips_at_every_interleaved_layout() {
    for (format, codec, depth, bytes) in [
        (SampleFormat::U8, "A_PCM/INT/LIT", 8u8, 1usize),
        (SampleFormat::S16, "A_PCM/INT/LIT", 16, 2),
        (SampleFormat::S24, "A_PCM/INT/LIT", 24, 3),
        (SampleFormat::S32, "A_PCM/INT/LIT", 32, 4),
        (SampleFormat::F32, "A_PCM/FLOAT/IEEE", 32, 4),
        (SampleFormat::F64, "A_PCM/FLOAT/IEEE", 64, 8),
    ] {
        let mut a = Stream::new(0, CodecId::Pcm);
        a.sample_rate = 48_000;
        a.channels = 2;
        a.sample_format = Some(format);
        a.time_base = Rational::new(1, 48_000);
        let mut r = Lcg(depth as u64);
        let packets: Vec<Packet> = (0..5i64)
            .map(|i| pkt(0, i * 480, true, &r.bytes(480 * 2 * bytes)))
            .collect();
        for streamed in [false, true] {
            let file = mux(streamed, &[a.clone()], &packets);
            assert!(file.windows(codec.len()).any(|w| w == codec.as_bytes()));
            // Audio > BitDepth (0x62 0x64), 1-byte size, the depth.
            assert!(file.windows(4).any(|w| w == [0x62, 0x64, 0x81, depth]));
            let (streams, got) = by_stream(&file, false);
            assert_eq!(streams[0].sample_format, Some(format), "{format:?}");
            let data: Vec<_> = got[0].iter().map(|p| p.data.clone()).collect();
            let want: Vec<_> = packets.iter().map(|p| p.data.clone()).collect();
            assert_eq!(data, want, "{format:?}");
        }
    }
    let mut planar = Stream::new(0, CodecId::Pcm);
    planar.sample_format = Some(SampleFormat::F32Planar);
    let err = try_mux(false, &[planar], &[]).unwrap_err();
    assert!(err.to_string().contains("fltp"), "{err}");
}

#[test]
fn webm_still_refuses_the_matroska_only_codecs() {
    for codec in [CodecId::Hevc, CodecId::ProRes] {
        let mut mux = MkvMuxer::new(Box::new(Vec::new()), true);
        let err = mux.write_header(&[Stream::new(0, codec)]).unwrap_err();
        assert!(err.to_string().contains("webm"), "{err}");
    }
    // DNxHD and APV have no Matroska codec ID.
    for codec in [CodecId::DnxHd, CodecId::Apv] {
        let mut mux = MkvMuxer::new(Box::new(Vec::new()), false);
        assert!(mux.write_header(&[Stream::new(0, codec)]).is_err());
    }
}
