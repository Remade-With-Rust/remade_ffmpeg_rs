//! Round trips for #114's second list: rotation, `dOps` / `av1C` / `vpcC`,
//! the stored sample table under an edit list, `udta` metadata, the movie
//! time scale, faststart without buffering, sample rates of 65,536 Hz and
//! up, and 64-bit offsets that must not narrow on 32-bit targets.

use std::io::{Cursor, Seek, SeekFrom, Write};
use std::sync::{Arc, Mutex};

use rff_core::{ColorRange, Dictionary};

use super::*;

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

impl Shared {
    fn bytes(&self) -> Vec<u8> {
        self.0.lock().unwrap().get_ref().clone()
    }
}

type Setup<'a> = &'a dyn Fn(&mut dyn Muxer);

fn feed(m: &mut dyn Muxer, setup: Setup, streams: &[Stream], packets: &[Packet]) {
    setup(m);
    m.write_header(streams).unwrap();
    for p in packets {
        m.write_packet(p).unwrap();
    }
    m.write_trailer().unwrap();
}

/// Mux with the buffered (pipe) and the streaming (file) muxer of `flavor`.
fn mux_both(flavor: Flavor, setup: Setup, streams: &[Stream], packets: &[Packet]) -> [Vec<u8>; 2] {
    let a = Shared::default();
    feed(
        &mut Mp4Muxer::with_flavor(Box::new(a.clone()), flavor),
        setup,
        streams,
        packets,
    );
    let b = Shared::default();
    feed(
        &mut Mp4StreamMuxer::with_flavor(Box::new(b.clone()), flavor),
        setup,
        streams,
        packets,
    );
    [a.bytes(), b.bytes()]
}

fn open(file: &[u8]) -> (Mp4Demuxer, Vec<Stream>) {
    let mut d = Mp4Demuxer::new_seekable(Box::new(Cursor::new(file.to_vec())));
    let s = d.read_header().unwrap();
    (d, s)
}

fn drain(d: &mut Mp4Demuxer) -> Vec<Packet> {
    let mut out = Vec::new();
    loop {
        match d.read_packet() {
            Ok(p) => out.push(p),
            Err(Error::Eof) => return out,
            Err(e) => panic!("{e}"),
        }
    }
}

fn top(file: &[u8]) -> Vec<[u8; 4]> {
    boxes_at(file).into_iter().map(|(t, _, _)| t).collect()
}

/// A small H.264 stream (SPS/PPS in band) and its packets at 1/1000.
fn h264(frames: usize) -> (Stream, Vec<Packet>) {
    let mut s = Stream::new(0, CodecId::H264);
    (s.width, s.height) = (64, 48);
    s.time_base = Rational::new(1, 1000);
    let packets = (0..frames)
        .map(|i| {
            let key = i % 10 == 0;
            let mut v = Vec::new();
            if key {
                for nal in [&[0x67u8, 0x42, 0, 0x1E, 0xAA][..], &[0x68, 0xCE, 0xBB]] {
                    v.extend_from_slice(&[0, 0, 0, 1]);
                    v.extend_from_slice(nal);
                }
            }
            v.extend_from_slice(&[0, 0, 0, 1, if key { 0x65 } else { 0x41 }, i as u8, 7]);
            let mut p = Packet::from_data(0, v);
            p.pts = Some(i as i64 * 40);
            p.flags.keyframe = key;
            p
        })
        .collect();
    (s, packets)
}

fn audio_packets(stream: usize, n: usize, step: i64) -> Vec<Packet> {
    (0..n)
        .map(|i| {
            let mut p = Packet::from_data(stream, vec![0x21, i as u8, (i >> 8) as u8, 0x5A]);
            p.pts = Some(i as i64 * step);
            p.dts = p.pts;
            p.duration = step;
            p.flags.keyframe = true;
            p
        })
        .collect()
}

// ---- 1. rotation, language, field order, enabled flag -------------------

#[test]
fn rotation_language_and_field_order_round_trip() {
    for flavor in [Flavor::Mp4, Flavor::Mov] {
        for deg in [90.0, 180.0, 270.0, 45.0] {
            let (mut s, packets) = h264(12);
            s.set_rotation(deg);
            s.language = Some("fra".into());
            s.field_order = Some(6);
            s.disposition.enabled = Some(false);
            let want = s.display_matrix;
            for file in mux_both(flavor, &|_| {}, std::slice::from_ref(&s), &packets) {
                let (_, got) = open(&file);
                assert_eq!(got[0].display_matrix, want, "{flavor:?} {deg}");
                assert_eq!(got[0].rotation(), Some(deg));
                assert_eq!(got[0].language.as_deref(), Some("fra"));
                assert_eq!(got[0].field_order, Some(6));
                assert_eq!(got[0].disposition.enabled, Some(false));
            }
        }
    }
    // No rotation writes the identity, which reads back as none.
    let (s, packets) = h264(3);
    let [file, _] = mux_both(Flavor::Mp4, &|_| {}, &[s], &packets);
    let (_, got) = open(&file);
    assert_eq!(
        (got[0].display_matrix, got[0].language.clone()),
        (None, None)
    );
    assert_eq!(got[0].field_order, None);
}

#[test]
fn an_iphone_portrait_matrix_reads_as_90_degrees() {
    let (mut s, packets) = h264(3);
    s.display_matrix = Some([0, 0x10000, 0, -0x10000, 0, 0, 48 << 16, 0, 1 << 30]);
    let [file, _] = mux_both(Flavor::Mov, &|_| {}, &[s], &packets);
    // The matrix is in tkhd verbatim.
    let at = file.windows(4).position(|w| w == b"tkhd").unwrap() + 4 + 40;
    assert_eq!(be32(&file, at + 4), 0x10000);
    assert_eq!(be32(&file, at + 12) as i32, -0x10000);
    let (_, got) = open(&file);
    assert_eq!(got[0].rotation(), Some(90.0));
}

// ---- 2. dOps, av1C, vpcC -----------------------------------------------

#[test]
fn dops_carries_the_whole_opus_head() {
    // Family 1, 6 channels: pre-skip 312, 44.1 kHz input, gain -256,
    // 4 streams, 2 coupled, mapping 0 4 1 2 3 5.
    let mut head = b"OpusHead".to_vec();
    head.extend_from_slice(&[1, 6]);
    head.extend_from_slice(&312u16.to_le_bytes());
    head.extend_from_slice(&44_100u32.to_le_bytes());
    head.extend_from_slice(&(-256i16).to_le_bytes());
    head.extend_from_slice(&[1, 4, 2, 0, 4, 1, 2, 3, 5]);
    let mut s = Stream::new(0, CodecId::Opus);
    (s.channels, s.sample_rate) = (6, 48_000);
    s.time_base = Rational::new(1, 48_000);
    s.extradata = head.clone();
    for flavor in [Flavor::Mp4, Flavor::Mov] {
        for file in mux_both(flavor, &|_| {}, &[s.clone()], &audio_packets(0, 5, 960)) {
            let (_, got) = open(&file);
            assert_eq!(got[0].extradata, head);
            assert_eq!(got[0].sample_rate, 48_000);
            let dops = file.windows(4).position(|w| w == b"dOps").unwrap() + 4;
            assert_eq!(&file[dops..dops + 4], &[0, 6, 0x01, 0x38]); // v0, 6 ch, BE 312
        }
    }
    // No OpusHead: the codec delay becomes the pre-skip.
    let mut s = Stream::new(0, CodecId::Opus);
    (s.channels, s.sample_rate) = (2, 48_000);
    s.time_base = Rational::new(1, 48_000);
    s.codec_delay_ns = 6_500_000; // 312 samples
    let [file, _] = mux_both(Flavor::Mp4, &|_| {}, &[s], &audio_packets(0, 3, 960));
    let (_, got) = open(&file);
    assert_eq!(&got[0].extradata[10..12], &312u16.to_le_bytes());
}

#[test]
fn av1c_and_vpcc_records_round_trip_as_extradata() {
    // av1C: profile 1, 10-bit, 4:4:4, a sequence-header configOBU.
    let av1c = vec![
        0x81, 0x28, 0x40, 0x00, 0x0A, 0x05, 0x00, 0x00, 0x00, 0x01, 0x02,
    ];
    let mut a = Stream::new(0, CodecId::Avif);
    (a.width, a.height) = (64, 48);
    a.time_base = Rational::new(1, 1000);
    a.extradata = av1c.clone();
    let frames: Vec<Packet> = (0..4)
        .map(|i| {
            let mut p = Packet::from_data(0, vec![0x32, 0x00, i]);
            p.pts = Some(i64::from(i) * 40);
            p.flags.keyframe = i == 0;
            p
        })
        .collect();
    for flavor in [Flavor::Mp4, Flavor::Mov] {
        for file in mux_both(flavor, &|_| {}, &[a.clone()], &frames) {
            let (_, got) = open(&file);
            assert_eq!(got[0].extradata, av1c, "{flavor:?}");
        }
    }

    // vpcC v1: profile 2, level 41, 10-bit 4:2:0 colocated, full range,
    // BT.2020 / PQ / BT.2020nc.
    let vpcc = vec![1, 0, 0, 0, 2, 41, (10 << 4) | (1 << 1) | 1, 9, 16, 9, 0, 0];
    let mut v = Stream::new(0, CodecId::Vp9);
    (v.width, v.height) = (64, 48);
    v.time_base = Rational::new(1, 1000);
    v.extradata = vpcc.clone();
    let frames: Vec<Packet> = (0..3)
        .map(|i| {
            let mut p = Packet::from_data(0, vec![0x80, 0x49, 0x83, 0x42, i]);
            p.pts = Some(i64::from(i) * 40);
            p.flags.keyframe = i == 0;
            p
        })
        .collect();
    let [file, streamed] = mux_both(Flavor::Mp4, &|_| {}, &[v.clone()], &frames);
    for f in [&file, &streamed] {
        let (_, got) = open(f);
        assert_eq!(got[0].extradata, vpcc);
        assert_eq!((got[0].color.primaries, got[0].color.transfer), (9, 16));
        assert_eq!(got[0].color_range, ColorRange::Full);
    }
    // Matroska's V_VP9 feature list (profile 1, level 31, 8-bit, 4:4:4)
    // becomes a vpcC with those fields.
    v.extradata = vec![1, 1, 1, 2, 1, 31, 3, 1, 8, 4, 1, 3];
    let [file, _] = mux_both(Flavor::Mp4, &|_| {}, &[v], &frames);
    let (_, got) = open(&file);
    assert_eq!(&got[0].extradata[4..7], &[1, 31, (8 << 4) | (3 << 1)]);
}

// ---- 3 + 6. the stored table under an edit list, movie time scale ------

/// An AAC track at 48 kHz: `n` frames of 1024 samples from 0.
fn aac(n: usize) -> (Stream, Vec<Packet>) {
    let mut s = Stream::new(0, CodecId::Aac);
    (s.channels, s.sample_rate) = (2, 48_000);
    s.time_base = Rational::new(1, 48_000);
    s.extradata = rff_format::aac::audio_specific_config(48_000, 2);
    let mut packets = audio_packets(0, n, 1024);
    for (i, p) in packets.iter_mut().enumerate() {
        p.data = vec![0xDE, i as u8, (i >> 8) as u8, 0xAD, 0xBE];
    }
    (s, packets)
}

#[test]
fn sample_exact_aac_trims_survive_a_round_trip() {
    const N: usize = 50;
    const PRIMING: i64 = 2112;
    const PADDING: i64 = 577;
    let trim = EditEntry {
        segment_duration: N as i64 * 1024 - PRIMING - PADDING,
        media_time: PRIMING,
        media_rate: 0x10000,
    };
    let (s, packets) = aac(N);
    let setup = |m: &mut dyn Muxer| {
        assert!(m.set_option("movie_timescale", "48000"));
        assert!(m.set_edit_list(0, &[trim]));
    };
    for flavor in [Flavor::Mp4, Flavor::Mov] {
        for file in mux_both(flavor, &setup, std::slice::from_ref(&s), &packets) {
            let (mut d, streams) = open(&file);
            let idx = d.sample_index().unwrap();
            assert_eq!(idx.movie_timescale(), Some(48_000));
            assert_eq!(idx.edit_list(0), &[trim]);
            assert_eq!(idx.raw_sample_count(0), N as u64);
            for i in 0..N as u64 {
                let info = idx.raw_sample_info(0, i).unwrap();
                assert_eq!(
                    (info.dts, info.pts, info.duration, info.size, info.sync),
                    (i as i64 * 1024, i as i64 * 1024, 1024, 5, true)
                );
                assert_eq!(
                    idx.read_raw_sample(0, i).unwrap().data,
                    packets[i as usize].data
                );
            }
            // The edited index (read_packet's timing) times the priming
            // before zero, so the first shown sample is at 0.
            let edited: Vec<i64> = (0..idx.sample_count(0))
                .map(|i| idx.sample_info(0, i).unwrap().pts)
                .collect();
            assert!(edited.contains(&-PRIMING));

            // Remux from the stored table + edit list: the same file tables.
            let raw: Vec<Packet> = (0..N as u64)
                .map(|i| {
                    let info = idx.raw_sample_info(0, i).unwrap();
                    let mut p = idx.read_raw_sample(0, i).unwrap();
                    p.duration = info.duration;
                    p
                })
                .collect();
            let edits = idx.edit_list(0).to_vec();
            let again = mux_both(
                flavor,
                &|m: &mut dyn Muxer| {
                    m.set_option("movie_timescale", "48000");
                    m.set_edit_list(0, &edits);
                },
                &streams,
                &raw,
            );
            for f2 in again {
                let (mut d2, _) = open(&f2);
                let i2 = d2.sample_index().unwrap();
                assert_eq!(i2.edit_list(0), &[trim]);
                for i in 0..N as u64 {
                    assert_eq!(
                        i2.raw_sample_info(0, i)
                            .map(|x| (x.dts, x.duration, x.size)),
                        Some((i as i64 * 1024, 1024, 5))
                    );
                }
            }
        }
    }
}

#[test]
fn the_movie_timescale_is_an_option_and_times_derived_edits() {
    // B-frames: a derived edit list whose segment is in the movie timescale.
    let (s, mut packets) = h264(10);
    let order = [0i64, 3, 1, 2, 6, 4, 5, 9, 7, 8];
    for (p, &o) in packets.iter_mut().zip(&order) {
        p.dts = p.pts.map(|d| d - 40);
        p.pts = Some(o * 40);
    }
    for ts in [1000u32, 600, 90_000] {
        let setup = |m: &mut dyn Muxer| assert!(m.set_option("movie_timescale", &ts.to_string()));
        for file in mux_both(Flavor::Mp4, &setup, std::slice::from_ref(&s), &packets) {
            let (mut d, _) = open(&file);
            let idx = d.sample_index().unwrap();
            assert_eq!(idx.movie_timescale(), Some(ts));
            let e = idx.edit_list(0);
            assert_eq!(e.len(), 1);
            // 400 ms shown, in the movie timescale.
            assert_eq!(e[0].segment_duration, 400 * i64::from(ts) / 1000);
        }
    }
    let mut m = Mp4Muxer::new(Box::new(Vec::new()));
    assert!(!m.set_option("movie_timescale", "0"));
    assert!(!m.set_option("movie_timescale", "abc"));
}

// ---- 5. udta metadata ----------------------------------------------------

#[test]
fn set_metadata_writes_udta_and_reads_back() {
    let mut md = Dictionary::new();
    md.set("title", "Freehold — cut 3");
    md.set("artist", "ArtCraft");
    md.set("comment", "multi\nline");
    md.set("date", "2026");
    md.set("encoder", "rff test");
    md.set("copyright", "(c) 2026");
    md.set("description", "a test clip");
    md.set("creation_time", "2026-11-20T10:20:30.000000Z");
    let (s, packets) = h264(4);
    for flavor in [Flavor::Mp4, Flavor::Mov] {
        for file in mux_both(
            flavor,
            &|m| m.set_metadata(&md),
            std::slice::from_ref(&s),
            &packets,
        ) {
            let (d, _) = open(&file);
            assert_eq!(d.metadata(), Some(&md), "{flavor:?}");
            assert!(file.windows(4).any(|w| w == b"udta"));
            let ilst = file.windows(4).any(|w| w == b"ilst");
            assert_eq!(
                ilst,
                flavor == Flavor::Mp4,
                "MP4 writes ilst, MOV text atoms"
            );
        }
    }
    // No metadata: no udta, and nothing read.
    let [file, _] = mux_both(Flavor::Mp4, &|_| {}, &[s], &packets);
    assert!(!file.windows(4).any(|w| w == b"udta"));
    let (d, _) = open(&file);
    assert!(d.metadata().unwrap().is_empty());
}

#[test]
fn mdta_keys_and_moov_meta_are_read() {
    // QuickTime `moov/meta` with `keys` (an iPhone's com.apple.quicktime.*).
    let key = |name: &str| bx(b"mdta", name.as_bytes());
    let mut keys = vec![0, 0, 0, 0, 0, 0, 0, 2];
    keys.extend_from_slice(&key("com.apple.quicktime.make"));
    keys.extend_from_slice(&key("com.apple.quicktime.model"));
    let item = |i: u32, text: &str| {
        let data = [&1u32.to_be_bytes()[..], &[0; 4], text.as_bytes()].concat();
        bx(&i.to_be_bytes(), &bx(b"data", &data))
    };
    let mut hdlr = vec![0u8; 8];
    hdlr.extend_from_slice(b"mdta");
    hdlr.extend_from_slice(&[0; 13]);
    let meta = bx(
        b"meta",
        &[
            bx(b"hdlr", &hdlr),
            bx(b"keys", &keys),
            bx(b"ilst", &[item(1, "Apple"), item(2, "iPhone 15")].concat()),
        ]
        .concat(),
    );
    let moov = [build_mvhd(1000, 0, 1, 0), meta].concat();
    let md = meta::read_movie_metadata(&child_boxes(&moov));
    assert_eq!(md.get("com.apple.quicktime.make"), Some("Apple"));
    assert_eq!(md.get("com.apple.quicktime.model"), Some("iPhone 15"));
}

// ---- 7. faststart --------------------------------------------------------

#[test]
fn streaming_faststart_spools_and_puts_moov_first() {
    let (v, mut packets) = h264(40);
    let (mut a, ap) = aac(60);
    a.index = 1;
    packets.extend(ap.into_iter().map(|mut p| {
        p.stream_index = 1;
        p
    }));
    let streams = [v, a];
    let plain = Shared::default();
    feed(
        &mut Mp4StreamMuxer::new(Box::new(plain.clone())),
        &|_| {},
        &streams,
        &packets,
    );
    let fast = Shared::default();
    let mut m = Mp4StreamMuxer::new(Box::new(fast.clone()));
    assert!(m.set_option("movflags", "+faststart"));
    m.write_header(&streams).unwrap();
    // The samples go to a temporary file, not memory.
    assert!(m.spool.as_ref().unwrap().on_disk());
    for p in &packets {
        m.write_packet(p).unwrap();
    }
    m.write_trailer().unwrap();
    drop(m);
    let (plain, fast) = (plain.bytes(), fast.bytes());
    assert_eq!(top(&fast), [*b"ftyp", *b"moov", *b"mdat"]);
    assert_eq!(top(&plain), [*b"ftyp", *b"free", *b"mdat", *b"moov"]);
    let strip = |v: Vec<Packet>| -> Vec<_> {
        v.into_iter()
            .map(|p| (p.stream_index, p.pts, p.dts, p.data))
            .collect()
    };
    assert_eq!(
        strip(drain(&mut open(&plain).0)),
        strip(drain(&mut open(&fast).0))
    );
    // The buffered muxer's faststart agrees.
    let buffered = Shared::default();
    let mut m = Mp4Muxer::new(Box::new(buffered.clone()));
    feed(
        &mut m,
        &|m| assert!(m.set_option("movflags", "+faststart")),
        &streams,
        &packets,
    );
    assert_eq!(top(&buffered.bytes()), [*b"ftyp", *b"moov", *b"mdat"]);
}

#[test]
fn the_memory_spool_copies_what_it_holds() {
    let mut s = faststart::Spool::memory();
    assert!(!s.on_disk());
    s.write_all(b"abc").unwrap();
    s.write_all(b"de").unwrap();
    let mut out = Vec::new();
    s.copy_to(&mut out).unwrap();
    assert_eq!((s.len(), out.as_slice()), (5, &b"abcde"[..]));
    let mut f = faststart::Spool::new();
    f.write_all(&[7u8; 3 << 20]).unwrap();
    let mut out = Vec::new();
    f.copy_to(&mut out).unwrap();
    assert_eq!(out.len(), 3 << 20);
}

#[test]
fn faststart_in_place_moves_moov_and_keeps_every_sample() {
    let (v, mut packets) = h264(30);
    let (mut a, ap) = aac(40);
    a.index = 1;
    packets.extend(ap.into_iter().map(|mut p| {
        p.stream_index = 1;
        p
    }));
    let mut tc = v.clone();
    tc.timecode = Some(rff_core::Timecode {
        start_frame: 1234,
        frames_per_second: 25,
        drop_frame: false,
        timescale: 25,
        frame_duration: 1,
        flags: 0,
        reel_name: None,
    });
    let streams = [tc, a];
    let sink = Shared::default();
    feed(
        &mut Mp4StreamMuxer::new(Box::new(sink.clone())),
        &|_| {},
        &streams,
        &packets,
    );
    let before = sink.bytes();
    let mut file = Cursor::new(before.clone());
    assert!(faststart_in_place(&mut file).unwrap());
    let after = file.into_inner();
    assert_eq!(top(&after), [*b"ftyp", *b"free", *b"moov", *b"mdat"]);
    assert_eq!(after.len(), before.len());
    let (mut d0, s0) = open(&before);
    let (mut d1, s1) = open(&after);
    assert_eq!(s1[0].timecode, s0[0].timecode, "the tmcd sample moved too");
    let strip = |v: Vec<Packet>| -> Vec<_> {
        v.into_iter()
            .map(|p| (p.stream_index, p.pts, p.data))
            .collect()
    };
    assert_eq!(strip(drain(&mut d0)), strip(drain(&mut d1)));
    // Already faststart: nothing to do.
    let mut again = Cursor::new(after.clone());
    assert!(!faststart_in_place(&mut again).unwrap());
    assert_eq!(again.into_inner(), after);
}

#[test]
fn faststart_patch_promotes_stco_to_co64_past_4_gib() {
    // An stco whose last offset passes 4 GiB once shifted.
    let stco = fbx(
        b"stco",
        0,
        0,
        &[2u32, 100, u32::MAX - 10]
            .iter()
            .flat_map(|v| v.to_be_bytes())
            .collect::<Vec<_>>(),
    );
    let moov = bx(b"trak", &bx(b"mdia", &bx(b"minf", &bx(b"stbl", &stco))));
    let out = faststart::tests_patch(&moov, 1000);
    let co64 = out.windows(4).position(|w| w == b"co64").unwrap() + 4;
    assert_eq!(be32(&out, co64 + 4), 2);
    assert_eq!(be64(&out, co64 + 8), 1100);
    assert_eq!(be64(&out, co64 + 16), u64::from(u32::MAX) - 10 + 1000);
}

// ---- 8. sample rates of 65,536 Hz and up ----------------------------------

fn audio_stream(codec: CodecId, rate: u32) -> Stream {
    let mut s = Stream::new(0, codec);
    (s.channels, s.sample_rate) = (2, rate);
    s.time_base = Rational::new(1, rate as i32);
    match codec {
        CodecId::Aac => s.extradata = rff_format::aac::audio_specific_config(rate, 2),
        CodecId::Flac => {
            // STREAMINFO: block sizes, then rate(20) ch-1(3) bps-1(5).
            let mut si = vec![0x10, 0x00, 0x10, 0x00, 0, 0, 0, 0, 0, 0];
            let packed: u64 = u64::from(rate) << 44 | 1 << 41 | 23 << 36;
            si.extend_from_slice(&packed.to_be_bytes());
            si.extend_from_slice(&[0; 16]);
            s.extradata = si;
        }
        CodecId::Alac => {
            let mut c = vec![0u8; 24];
            c[..4].copy_from_slice(&4096u32.to_be_bytes());
            c[5] = 24;
            c[9] = 2;
            c[20..24].copy_from_slice(&rate.to_be_bytes());
            s.extradata = c;
        }
        CodecId::Ac3 => s.extradata = vec![0x10, 0x3D, 0xC0],
        CodecId::Eac3 => s.extradata = vec![0x06, 0x00, 0x20, 0x0F, 0x00],
        CodecId::Pcm => s.sample_format = Some(rff_core::SampleFormat::S24),
        _ => {}
    }
    s
}

#[test]
fn high_sample_rates_do_not_wrap_and_read_back_exactly() {
    let codecs = [
        CodecId::Aac,
        CodecId::Flac,
        CodecId::Alac,
        CodecId::Opus,
        CodecId::Ac3,
        CodecId::Eac3,
        CodecId::Mp3,
    ];
    for rate in [88_200u32, 96_000, 192_000, 44_100] {
        for flavor in [Flavor::Mp4, Flavor::Mov] {
            let mut list = codecs.to_vec();
            if flavor == Flavor::Mov {
                list.push(CodecId::Pcm);
            }
            for &codec in &list {
                let s = audio_stream(codec, rate);
                let packets = if codec == CodecId::Pcm {
                    (0..4)
                        .map(|i| {
                            let mut p = Packet::from_data(0, vec![i as u8; 6 * 100]);
                            p.pts = Some(i * 100);
                            p.flags.keyframe = true;
                            p
                        })
                        .collect()
                } else {
                    audio_packets(0, 4, 1024)
                };
                for file in mux_both(flavor, &|_| {}, std::slice::from_ref(&s), &packets) {
                    let (_, got) = open(&file);
                    let what = format!("{codec:?} {rate} {flavor:?}");
                    if codec == CodecId::Opus {
                        // Opus entries say 48 kHz; the input rate is in dOps.
                        assert_eq!(got[0].sample_rate, 48_000, "{what}");
                        let head = &got[0].extradata;
                        assert_eq!(&head[12..16], &rate.to_le_bytes(), "{what}");
                        continue;
                    }
                    assert_eq!(got[0].sample_rate, rate, "{what}");
                    let has_srat = file.windows(4).any(|w| w == b"srat");
                    let wide = rate > 0xFFFF && codec != CodecId::Pcm;
                    assert_eq!(has_srat, wide && flavor == Flavor::Mp4, "{what}");
                }
            }
        }
    }
}

#[test]
fn a_wrapped_rate_from_an_older_writer_is_repaired_from_the_asc() {
    // What rff wrote before #114 for 96 kHz AAC: 96000 << 16 wrapped to
    // 30464 Hz in the 16.16 field, beside an ASC that says 96000.
    let s = audio_stream(CodecId::Aac, 96_000);
    let [mut file, _] = mux_both(Flavor::Mp4, &|_| {}, &[s], &audio_packets(0, 3, 1024));
    let mp4a = file.windows(4).position(|w| w == b"mp4a").unwrap() + 4;
    assert_eq!(
        be32(&file, mp4a + 24),
        48_000 << 16,
        "a division in the field"
    );
    // Rewrite it the old way: a v0 entry, a wrapped field, no srat.
    file[mp4a + 8..mp4a + 10].copy_from_slice(&[0, 0]);
    file[mp4a + 24..mp4a + 28].copy_from_slice(&(96_000u32 << 16).to_be_bytes());
    let srat = file.windows(4).position(|w| w == b"srat").unwrap();
    file[srat..srat + 4].copy_from_slice(b"free");
    assert_eq!(be32(&file, mp4a + 24) >> 16, 30_464);
    let (_, got) = open(&file);
    assert_eq!(got[0].sample_rate, 96_000);
}

// ---- 9. 64-bit offsets ---------------------------------------------------

#[test]
fn co64_offsets_past_4_gib_index_exactly() {
    // A track whose chunks sit at 5 GiB and 1 TiB: offsets are u64 all the
    // way to SampleInfo, never a `usize` (which is 32 bits on wasm32).
    let five = 5u64 << 30;
    let tib = 1u64 << 40;
    let mut co64 = vec![0u8, 0, 0, 0, 0, 0, 0, 2];
    co64.extend_from_slice(&five.to_be_bytes());
    co64.extend_from_slice(&tib.to_be_bytes());
    let table = |typ: &[u8; 4], rows: &[u32]| {
        let body: Vec<u8> = std::iter::once(0u32)
            .chain(rows.iter().copied())
            .flat_map(|v| v.to_be_bytes())
            .collect();
        bx(typ, &body)
    };
    let stbl = [
        table(b"stsz", &[0, 3, 100, 200, 300]),
        table(b"stsc", &[1, 1, 2, 1]),
        table(b"stts", &[1, 3, 10]),
        bx(b"co64", &co64),
    ]
    .concat();
    let boxes = child_boxes(&stbl);
    let timing = TrackTiming {
        audio: false,
        video: true,
        vorbis: false,
        timescale: 1000,
        movie_timescale: 1000,
        edits: Vec::new(),
        advanced_editlist: true,
    };
    let (samples, stored) = build_samples(&boxes, 0, &timing, None).unwrap();
    let offsets: Vec<u64> = samples.iter().map(|s| s.offset).collect();
    assert_eq!(offsets, [five, five + 100, tib]);
    assert_eq!(stored.iter().map(|s| s.offset).collect::<Vec<_>>(), offsets);
    // The seekable reader asks for exactly those offsets.
    let mut d = Mp4Demuxer::with_source(Source::Done);
    d.stored = vec![stored];
    d.by_track = vec![vec![]];
    assert_eq!(d.raw_sample_info(0, 2).unwrap().offset, tib);
}

#[test]
fn a_largesize_past_the_buffer_stops_the_walk() {
    // A 64-bit box size of 4 GiB + 16 in a 32-byte buffer: on a 32-bit
    // target `as usize` made it 16 and parsed garbage as a box.
    let mut b = vec![0, 0, 0, 1];
    b.extend_from_slice(b"mdat");
    b.extend_from_slice(&((1u64 << 32) + 16).to_be_bytes());
    b.extend_from_slice(&[0u8; 16]);
    assert!(boxes_at(&b).is_empty());
}
