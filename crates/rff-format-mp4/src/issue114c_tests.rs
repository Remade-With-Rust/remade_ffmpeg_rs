//! #114 phase 2: chapters (QuickTime chapter track and Nero `chpl`), timecode
//! tracks as streams, sample groups.

use std::io::{Cursor, Seek, SeekFrom, Write};
use std::sync::{Arc, Mutex};

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

fn run(m: &mut dyn Muxer, streams: &[Stream], packets: &[Packet], setup: Setup) {
    setup(m);
    m.write_header(streams).unwrap();
    for p in packets {
        m.write_packet(p).unwrap();
    }
    m.write_trailer().unwrap();
}

/// The same movie from the pipe muxer, the streaming muxer and the streaming
/// muxer with faststart.
fn mux_all(flavor: Flavor, streams: &[Stream], packets: &[Packet], setup: Setup) -> [Vec<u8>; 3] {
    let a = Shared::default();
    let mut m = Mp4Muxer::with_flavor(Box::new(a.clone()), flavor);
    run(&mut m, streams, packets, setup);
    let b = Shared::default();
    let mut m = Mp4StreamMuxer::with_flavor(Box::new(b.clone()), flavor);
    run(&mut m, streams, packets, setup);
    let c = Shared::default();
    let mut m = Mp4StreamMuxer::with_flavor(Box::new(c.clone()), flavor);
    m.set_option("movflags", "+faststart");
    run(&mut m, streams, packets, setup);
    [a.bytes(), b.bytes(), c.bytes()]
}

fn open(file: &[u8]) -> (Mp4Demuxer, Vec<Stream>) {
    let mut d = Mp4Demuxer::new_seekable(Box::new(Cursor::new(file.to_vec())));
    let s = d.read_header().unwrap();
    (d, s)
}

fn drain(d: &mut dyn Demuxer) -> Vec<Packet> {
    let mut out = Vec::new();
    while let Ok(p) = d.read_packet() {
        out.push(p);
    }
    out
}

/// A 10 s video track of 25 fps frames (any bytes do).
fn video(index: usize) -> (Stream, Vec<Packet>) {
    let mut s = Stream::new(index, CodecId::Jpeg);
    (s.width, s.height) = (64, 48);
    s.time_base = Rational::new(1, 1000);
    let packets = (0..250i64)
        .map(|i| {
            let mut p = Packet::from_data(index, vec![i as u8, index as u8, 7]);
            p.pts = Some(i * 40);
            p.dts = Some(i * 40);
            p.duration = 40;
            p.flags.keyframe = true;
            p
        })
        .collect();
    (s, packets)
}

fn ms(id: u64, start: i64, end: i64, title: &str) -> Chapter {
    Chapter::new(id, Rational::new(1, 1000), start, end).with_title(title)
}

fn spans(list: &[Chapter]) -> Vec<(i64, i64, String)> {
    list.iter()
        .map(|c| {
            (
                c.start_in(1000),
                c.end_in(1000),
                c.title().unwrap_or("-").to_string(),
            )
        })
        .collect()
}

const UNICODE: &str = "Intro é 日本語 😀";

fn listed() -> Vec<Chapter> {
    vec![
        ms(0, 1500, 2500, UNICODE),
        ms(1, 2500, 2500, "Zero"),
        ms(2, 2500, 7000, ""),
        ms(3, 7000, 10_000, "Last"),
    ]
}

#[test]
fn chapters_round_trip_through_every_muxer_and_both_readers() {
    let (s, packets) = video(0);
    let list = listed();
    for flavor in [Flavor::Mp4, Flavor::Mov] {
        let files = mux_all(flavor, std::slice::from_ref(&s), &packets, &|m| {
            assert!(m.set_chapters(&list));
        });
        for file in files {
            let (mut d, streams) = open(&file);
            assert_eq!(spans(d.chapters()), spans(&list));
            assert_eq!(d.chapters()[0].time_base, Rational::new(1, 1000));
            // The media is as it was; the chapter track is a data stream.
            assert_eq!(streams.len(), 2);
            assert_eq!(streams[1].media_type, MediaType::Data);
            assert_eq!(streams[1].disposition.enabled, Some(false));
            let refs = d.track_references(0).to_vec();
            assert_eq!(refs, [TrackReference::new(*b"chap", vec![1])]);
            let got = drain(&mut d);
            let media: Vec<_> = got.iter().filter(|p| p.stream_index == 0).collect();
            assert_eq!(media.len(), packets.len());
            assert!(media.iter().zip(&packets).all(|(a, b)| a.data == b.data));
            // A pipe reads the same.
            let mut d = Mp4Demuxer::new(Box::new(Cursor::new(file)));
            d.read_header().unwrap();
            assert_eq!(spans(d.chapters()), spans(&list));
        }
    }
}

#[test]
fn unordered_chapters_are_written_in_time_order() {
    let (s, packets) = video(0);
    let list = [ms(0, 5000, 10_000, "B"), ms(1, 0, 5000, "A")];
    let files = mux_all(Flavor::Mov, &[s], &packets, &|m| {
        m.set_chapters(&list);
    });
    for file in files {
        let (d, _) = open(&file);
        let want = [(0, 5000, "A".into()), (5000, 10_000, "B".into())];
        assert_eq!(spans(d.chapters()), want);
    }
}

#[test]
fn a_chapter_tracks_time_base_is_kept_where_it_is_exact() {
    let (s, packets) = video(0);
    // 1/90000 ticks that are not whole milliseconds.
    let tb = Rational::new(1, 90_000);
    let list = [
        Chapter::new(0, tb, 0, 90_001).with_title("a"),
        Chapter::new(1, tb, 90_001, 900_000).with_title("b"),
    ];
    let [file, ..] = mux_all(Flavor::Mov, std::slice::from_ref(&s), &packets, &|m| {
        m.set_chapters(&list);
    });
    let (d, _) = open(&file);
    assert_eq!(d.chapters(), list);
    // Matroska's nanoseconds on whole milliseconds.
    let ns = Rational::new(1, 1_000_000_000);
    let list = [Chapter::new(5, ns, 250_000_000, 9_000_000_000).with_title("n")];
    let [file, ..] = mux_all(Flavor::Mov, &[s], &packets, &|m| {
        m.set_chapters(&list);
    });
    let (d, _) = open(&file);
    assert_eq!(spans(d.chapters()), [(250, 9000, "n".into())]);
}

#[test]
fn nero_chapters_stand_in_without_a_chapter_track() {
    let (s, packets) = video(0);
    let list = listed();
    let [file, ..] = mux_all(Flavor::Mov, &[s], &packets, &|m| {
        m.set_chapters(&list);
    });
    // Make the references dangle: only `chpl` is left.
    let at = file.windows(4).position(|w| w == b"chap").unwrap();
    let mut only_chpl = file.clone();
    only_chpl[at..at + 4].copy_from_slice(b"xxxx");
    let (d, _) = open(&only_chpl);
    assert_eq!(d.chapters()[0].time_base, Rational::new(1, 10_000_000));
    let want = [
        (1500, 2500, UNICODE.to_string()),
        // FFmpeg's rule for an open end whose next start is no later.
        (2500, 10_000, "Zero".into()),
        (2500, 7000, "".into()),
        (7000, 10_000, "Last".into()),
    ];
    assert_eq!(spans(d.chapters()), want);
    // `movflags=+disable_chpl` writes the track alone.
    let (s, packets) = video(0);
    let [file, ..] = mux_all(Flavor::Mov, &[s], &packets, &|m| {
        m.set_option("movflags", "+disable_chpl");
        m.set_chapters(&list);
    });
    assert!(!file.windows(4).any(|w| w == b"chpl"));
    assert_eq!(spans(open(&file).0.chapters()), spans(&list));
}

#[test]
fn chapters_follow_a_movie_that_starts_late() {
    // Packets from 10 s: the movie's own timeline starts there (#115).
    let (s, mut packets) = video(0);
    for p in &mut packets {
        p.pts = p.pts.map(|t| t + 10_000);
        p.dts = p.dts.map(|t| t + 10_000);
    }
    let list = [ms(0, 10_000, 12_000, "x"), ms(1, 12_000, 20_000, "y")];
    for file in mux_all(Flavor::Mov, &[s], &packets, &|m| {
        m.set_chapters(&list);
    }) {
        let (d, _) = open(&file);
        let want = [(0, 2000, "x".into()), (2000, 10_000, "y".into())];
        assert_eq!(spans(d.chapters()), want);
    }
}

#[test]
fn long_titles_are_cut_at_a_character() {
    let (s, packets) = video(0);
    let long = "é".repeat(40_000);
    let list = [ms(0, 0, 1000, &long)];
    let [file, ..] = mux_all(Flavor::Mov, &[s], &packets, &|m| {
        m.set_chapters(&list);
    });
    let (d, _) = open(&file);
    assert_eq!(d.chapters()[0].title().unwrap(), "é".repeat(32_767));
}

#[test]
fn more_chapters_than_chpl_holds() {
    let (s, packets) = video(0);
    let list: Vec<Chapter> = (0..300)
        .map(|i| ms(i as u64, i * 30, (i + 1) * 30, &format!("c{i}")))
        .collect();
    let [file, ..] = mux_all(Flavor::Mov, &[s], &packets, &|m| {
        m.set_chapters(&list);
    });
    assert_eq!(spans(open(&file).0.chapters()), spans(&list));
}

#[test]
fn titles_in_utf16_and_short_samples() {
    assert_eq!(
        chapters::sample_title(b"\0\x06\xFE\xFF\0A\x30\x42").as_deref(),
        Some("A\u{3042}")
    );
    assert_eq!(
        chapters::sample_title(b"\0\x04\xFF\xFEA\0").as_deref(),
        Some("A")
    );
    assert_eq!(
        chapters::sample_title(b"\0\x05ab\0cd").as_deref(),
        Some("ab")
    );
    assert_eq!(chapters::sample_title(b"\0\0").as_deref(), Some(""));
    assert_eq!(chapters::sample_title(b"\0\x05ab"), None);
    assert_eq!(chapters::sample_title(b"\0"), None);
}

// ---- timecode streams -------------------------------------------------------

fn timecode(text: &str, fps: u8) -> Timecode {
    let mut tc = Timecode::parse(text, fps).unwrap();
    tc.reel_name = Some("A001".into());
    tc.flags = 2;
    tc
}

/// Two video tracks, each with a timecode of its own (one drop-frame).
fn two_timecodes() -> ([Vec<u8>; 3], Vec<Packet>) {
    let (mut a, mut packets) = video(0);
    let (mut b, pb) = video(1);
    a.timecode = Some(timecode("00:01:00;02", 30));
    b.timecode = Some(timecode("10:00:00:05", 24));
    packets.extend(pb);
    (mux_all(Flavor::Mov, &[a, b], &packets, &|_| {}), packets)
}

#[test]
fn timecode_tracks_are_streams_only_when_asked_for() {
    let (files, _) = two_timecodes();
    for file in files {
        let (d, plain) = open(&file);
        assert_eq!(plain.len(), 2);
        assert!(d.track_references(0).is_empty());

        let mut d = Mp4Demuxer::new_seekable(Box::new(Cursor::new(file.clone())));
        assert!(d.set_timecode_streams(true));
        let s = d.read_header().unwrap();
        assert_eq!(s.len(), 4);
        for i in 0..2 {
            assert_eq!(s[i].timecode, plain[i].timecode);
            assert!(s[i].timecode.is_some());
            let refs = d.track_references(i).to_vec();
            assert_eq!(refs, [TrackReference::new(*b"tmcd", vec![2 + i])]);
            let t = &s[2 + i];
            assert_eq!(
                (t.index, t.codec_id, t.media_type, t.codec_tag),
                (2 + i, CodecId::None, MediaType::Data, Some(*b"tmcd"))
            );
            assert_eq!(t.timecode, s[i].timecode);
            let idx = d.sample_index().unwrap();
            assert_eq!(idx.sample_count(2 + i), 1);
            let p = idx.read_sample(2 + i, 0).unwrap();
            let start = s[i].timecode.as_ref().unwrap().start_frame;
            assert_eq!(p.data, start.to_be_bytes());
        }
        let text = |i: usize| s[i].timecode.as_ref().unwrap().to_string();
        assert_eq!(
            (text(2), text(3)),
            ("00:01:00;02".into(), "10:00:00:05".into())
        );
    }
}

#[test]
fn timecode_streams_write_back_unchanged() {
    let (files, _) = two_timecodes();
    let mut d = Mp4Demuxer::new_seekable(Box::new(Cursor::new(files[1].clone())));
    d.set_timecode_streams(true);
    let streams = d.read_header().unwrap();
    let refs: Vec<Vec<TrackReference>> = (0..4).map(|i| d.track_references(i).to_vec()).collect();
    let packets = drain(&mut d);
    for flavor in [Flavor::Mp4, Flavor::Mov] {
        let copies = mux_all(flavor, &streams, &packets, &|m| {
            for (i, r) in refs.iter().enumerate() {
                assert!(m.set_track_references(i, r));
            }
        });
        for file in copies {
            // Two timecode tracks, not two plus two made from `timecode`.
            assert_eq!(traks(&file).len(), 4);
            let mut d = Mp4Demuxer::new_seekable(Box::new(Cursor::new(file)));
            d.set_timecode_streams(true);
            let s = d.read_header().unwrap();
            for i in 0..4 {
                assert_eq!(s[i].extradata, streams[i].extradata);
                assert_eq!(s[i].timecode, streams[i].timecode);
                assert_eq!(s[i].time_base, streams[i].time_base);
                assert_eq!(s[i].duration, streams[i].duration);
                assert_eq!(d.track_references(i), refs[i]);
            }
            let got = drain(&mut d);
            let of = |ps: &[Packet], i: usize| -> Vec<_> {
                ps.iter()
                    .filter(|p| p.stream_index == i)
                    .map(|p| (p.pts, p.duration, p.data.clone()))
                    .collect()
            };
            for i in 0..4 {
                assert_eq!(of(&got, i), of(&packets, i), "stream {i}");
            }
        }
    }
}

#[test]
fn unreferenced_timecode_streams_time_the_first_video() {
    let (files, _) = two_timecodes();
    let mut d = Mp4Demuxer::new_seekable(Box::new(Cursor::new(files[0].clone())));
    d.set_timecode_streams(true);
    let streams = d.read_header().unwrap();
    // The first video and its timecode track, no references given.
    let mut pick = vec![streams[0].clone(), streams[2].clone()];
    pick[1].index = 1;
    let packets: Vec<Packet> = drain(&mut d)
        .into_iter()
        .filter(|p| p.stream_index % 2 == 0)
        .map(|mut p| {
            p.stream_index /= 2;
            p
        })
        .collect();
    let [file, ..] = mux_all(Flavor::Mov, &pick, &packets, &|_| {});
    assert_eq!(traks(&file).len(), 2);
    let (_, s) = open(&file);
    assert_eq!(s[0].timecode, streams[0].timecode);
    // A description is required.
    pick[1].extradata.truncate(17);
    let mut m = Mp4Muxer::with_flavor(Box::new(Shared::default()), Flavor::Mp4);
    assert!(m.write_header(&pick).is_err());
}

fn traks(file: &[u8]) -> Vec<Vec<u8>> {
    let top = child_boxes(file);
    child_boxes(find(&top, b"moov").unwrap())
        .iter()
        .filter(|(t, _)| t == b"trak")
        .map(|(_, p)| p.to_vec())
        .collect()
}

#[test]
fn chapters_and_timecodes_share_a_movie() {
    let (mut s, packets) = video(0);
    s.timecode = Some(timecode("01:00:00:00", 25));
    let list = listed();
    for file in mux_all(Flavor::Mov, std::slice::from_ref(&s), &packets, &|m| {
        m.set_chapters(&list);
    }) {
        let (d, got) = open(&file);
        assert_eq!(got[0].timecode, s.timecode);
        assert_eq!(spans(d.chapters()), spans(&list));
    }
}

// ---- sample groups ----------------------------------------------------------

/// `file` with the bytes of `boxes` put at the end of its first `stbl`, every
/// enclosing box grown (the `moov` is last, so no offset moves).
fn add_to_stbl(file: &[u8], boxes: &[u8]) -> Vec<u8> {
    fn grow(buf: &[u8], path: &[&[u8; 4]], boxes: &[u8]) -> Vec<u8> {
        let Some((want, rest)) = path.split_first() else {
            return [buf, boxes].concat();
        };
        let mut out = Vec::new();
        let mut done = false;
        for (t, at, p) in boxes_at(buf) {
            if &t == *want && !done {
                done = true;
                out.extend_from_slice(&bx(&t, &grow(p, rest, boxes)));
            } else {
                let end = p.as_ptr() as usize - buf.as_ptr() as usize + p.len();
                out.extend_from_slice(&buf[at..end]);
            }
        }
        assert!(done);
        out
    }
    grow(file, &[b"moov", b"trak", b"mdia", b"minf", b"stbl"], boxes)
}

#[test]
fn sample_groups_of_a_movie() {
    let (s, packets) = video(0);
    let [_, file, _] = mux_all(Flavor::Mov, &[s], &packets, &|_| {});
    let mut sgpd = b"roll\0\0\0\x02\0\0\0\x02".to_vec();
    sgpd.extend_from_slice(&[0xFF, 0xFF, 0, 3]);
    let mut sbgp = b"roll\0\0\0\x03".to_vec();
    for (n, i) in [(10u32, 1u32), (200, 0), (5, 2)] {
        sbgp.extend_from_slice(&n.to_be_bytes());
        sbgp.extend_from_slice(&i.to_be_bytes());
    }
    let with = add_to_stbl(
        &file,
        &[fbx(b"sgpd", 1, 0, &sgpd), fbx(b"sbgp", 0, 0, &sbgp)].concat(),
    );
    let (mut d, _) = open(&with);
    let idx = d.sample_index().unwrap();
    let [g] = idx.sample_groups(0) else { panic!() };
    assert_eq!(&g.grouping_type, b"roll");
    assert_eq!(g.description_of(9), Some(&[0xFF, 0xFF][..]));
    assert_eq!(g.description_of(10), None);
    assert_eq!(g.description_of(214), Some(&[0, 3][..]));
    assert_eq!(g.description_of(215), None);
    assert!(idx.sample_groups(1).is_empty());
    let src = idx.sample_source().unwrap();
    assert_eq!(src.sample_groups(0), [g_of(&with)]);
    // Nothing without the boxes.
    let (mut d, _) = open(&file);
    assert!(d.sample_index().unwrap().sample_groups(0).is_empty());
}

fn g_of(file: &[u8]) -> SampleGrouping {
    let (mut d, _) = open(file);
    d.sample_index().unwrap().sample_groups(0)[0].clone()
}

#[test]
fn sample_groups_of_fragments() {
    // A one-track movie whose samples are all in two fragments, the second
    // with a description of its own.
    let (s, packets) = video(0);
    let [_, file, _] = mux_all(Flavor::Mov, &[s], &packets, &|_| {});
    let description = |v: u8| {
        fbx(
            b"sgpd",
            1,
            0,
            &[&b"tscl\0\0\0\x01\0\0\0\x01"[..], &[v]].concat(),
        )
    };
    let fragment = |groups: &[u8]| {
        let mut tfhd = 1u32.to_be_bytes().to_vec(); // track 1
        pu32(&mut tfhd, 40); // default duration
        pu32(&mut tfhd, 3); // default size
        let mut trun = 4u32.to_be_bytes().to_vec(); // 4 samples
        pu32(&mut trun, 0); // data offset
        let traf = [
            fbx(b"tfhd", 0, 0x18, &tfhd),
            fbx(b"trun", 0, 1, &trun),
            groups.to_vec(),
        ]
        .concat();
        bx(b"moof", &bx(b"traf", &traf))
    };
    let assign = |runs: &[(u32, u32)]| {
        let mut b = b"tscl".to_vec();
        pu32(&mut b, runs.len() as u32);
        for &(n, i) in runs {
            pu32(&mut b, n);
            pu32(&mut b, i);
        }
        fbx(b"sbgp", 0, 0, &b)
    };
    let mut whole = add_to_stbl(&file, &description(1));
    whole.extend(fragment(&assign(&[(1, 1), (3, 0)])));
    whole.extend(fragment(
        &[description(9), assign(&[(2, 0x1_0001), (2, 1)])].concat(),
    ));
    let (mut d, _) = open(&whole);
    let idx = d.sample_index().unwrap();
    assert_eq!(idx.raw_sample_count(0), 258);
    let g = &idx.sample_groups(0)[0];
    assert_eq!(g.descriptions, [[1], [9]]);
    let of: Vec<_> = (249..258).map(|i| g.group_of(i)).collect();
    let want = [
        None,
        Some(0),
        None,
        None,
        None,
        Some(1),
        Some(1),
        Some(0),
        Some(0),
    ];
    assert_eq!(of, want);
}

// ---- hostile input ----------------------------------------------------------

/// Open `file` every way and read everything; only a panic or a hang fails.
fn exercise(file: &[u8]) {
    for pipe in [false, true] {
        let input = Cursor::new(file.to_vec());
        let mut d = if pipe {
            Mp4Demuxer::new(Box::new(input))
        } else {
            Mp4Demuxer::new_seekable(Box::new(input))
        };
        d.set_timecode_streams(true);
        let Ok(streams) = d.read_header() else {
            continue;
        };
        for c in d.chapters() {
            assert!(c.end >= c.start && c.start >= 0);
        }
        for i in 0..streams.len() {
            for r in d.track_references(i) {
                assert!(r.streams.iter().all(|&t| t < streams.len()));
            }
            if let Some(idx) = d.sample_index() {
                for g in idx.sample_groups(i) {
                    let _ = (g.group_of(0), g.description_of(u64::MAX));
                }
            }
        }
        drain(&mut d);
    }
}

/// A small file with everything in it, its `moov` last.
fn kitchen_sink() -> Vec<u8> {
    let (mut s, packets) = video(0);
    s.timecode = Some(timecode("01:00:00:00", 25));
    let list = listed();
    let [_, file, _] = mux_all(Flavor::Mov, &[s], &packets[..6], &|m| {
        m.set_chapters(&list);
    });
    let sgpd = fbx(b"sgpd", 1, 0, b"roll\0\0\0\x02\0\0\0\x01\xFF\xFF");
    let sbgp = fbx(b"sbgp", 0, 0, b"roll\0\0\0\x01\0\0\0\x06\0\0\0\x01");
    add_to_stbl(&file, &[sgpd, sbgp].concat())
}

#[test]
fn truncated_and_flipped_files_do_not_panic() {
    let file = kitchen_sink();
    for cut in 0..file.len() {
        exercise(&file[..cut]);
    }
    let mut bad = file.clone();
    for at in 0..file.len() {
        for flip in [0xFF, 0x80, 0x01] {
            bad[at] = file[at] ^ flip;
            exercise(&bad);
        }
        bad[at] = file[at];
    }
}

#[test]
fn hostile_counts_stay_bounded() {
    let file = kitchen_sink();
    // Every 32-bit field of the chapter track, `chpl`, `sgpd` and `sbgp`
    // boxes, one at a time, at its largest.
    let moov = file.windows(4).position(|w| w == b"moov").unwrap();
    let mut bad = file.clone();
    for at in (moov + 4)..file.len() - 4 {
        bad[at..at + 4].copy_from_slice(&[0xFF; 4]);
        exercise(&bad);
        bad[at..at + 4].copy_from_slice(&file[at..at + 4]);
    }
}

#[test]
fn chapter_samples_over_the_same_bytes_are_bounded() {
    // 100 000 samples of 4 GiB, all at the start of a 64 KiB title.
    let mut asked = 0u64;
    let samples = (0..100_000i64).map(|i| chapters::TextSample {
        offset: 0,
        size: u64::from(u32::MAX),
        start: i,
        end: i + 1,
    });
    let (list, _) = chapters::read_track(samples, Rational::new(1, 1000), |_, n| {
        asked += n;
        let mut b = vec![b'x'; n as usize];
        b[..2].copy_from_slice(&[0xFF, 0xFF]);
        Some(b)
    });
    assert!(asked <= chapters::MAX_TITLE_BYTES);
    assert!(list.len() <= 256);
}
