//! `Chapters` (#114 phase 2): both muxers, both doctypes, hand-built editions,
//! hostile input.

use std::io::{Cursor, Seek, SeekFrom, Write};
use std::sync::{Arc, Mutex};

use rff_core::{CodecId, Packet, Rational};
use rff_format::{Chapter, Demuxer, Muxer, Stream};

use crate::{MkvDemuxer, MkvMuxer, MkvStreamingMuxer};

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

fn mux(webm: bool, streamed: bool, chapters: &[Chapter]) -> Vec<u8> {
    let out = Shared::default();
    let mut m: Box<dyn Muxer> = if streamed {
        Box::new(MkvStreamingMuxer::new(Box::new(out.clone()), webm))
    } else {
        Box::new(MkvMuxer::new(Box::new(out.clone()), webm))
    };
    let mut s = Stream::new(0, CodecId::Vp9);
    (s.width, s.height) = (32, 32);
    assert!(m.set_chapters(chapters));
    m.write_header(&[s]).unwrap();
    for i in 0..10i64 {
        let mut p = Packet::from_data(0, vec![i as u8; 5]);
        p.pts = Some(i * 1000);
        p.duration = 1000;
        p.flags.keyframe = i == 0;
        m.write_packet(&p).unwrap();
    }
    m.write_trailer().unwrap();
    let bytes = out.0.lock().unwrap().get_ref().clone();
    bytes
}

fn read(file: &[u8]) -> Vec<Chapter> {
    let mut d = MkvDemuxer::new_seekable(Box::new(Cursor::new(file.to_vec())));
    d.read_header().unwrap();
    d.chapters().to_vec()
}

fn ms(id: u64, start: i64, end: i64, title: &str) -> Chapter {
    Chapter::new(id, Rational::new(1, 1000), start, end).with_title(title)
}

type Span = (u64, i64, i64, Option<String>);

fn spans(list: &[Chapter]) -> Vec<Span> {
    list.iter()
        .map(|c| {
            let title = c.title().map(str::to_string);
            (c.id, c.start_in(1000), c.end_in(1000), title)
        })
        .collect()
}

#[test]
fn chapters_round_trip_in_matroska_and_webm() {
    let mut tagged = ms(9, 2500, 2500, "Zero");
    tagged.metadata.set("artist", "someone");
    let list = [
        ms(7, 0, 2500, "Intro é 日本語 😀"),
        tagged,
        ms(3, 2500, 6000, ""),
        Chapter::new(1, Rational::new(1, 1000), 7000, 10_000),
    ];
    for webm in [false, true] {
        for streamed in [false, true] {
            let file = mux(webm, streamed, &list);
            let got = read(&file);
            assert_eq!(spans(&got), spans(&list));
            assert_eq!(got[1].metadata.get("artist"), Some("someone"));
            assert!(got[0].metadata.get("artist").is_none());
            // A pipe reads the same, and the frames are all there.
            let mut d = MkvDemuxer::new(Box::new(Cursor::new(file)));
            d.read_header().unwrap();
            assert_eq!(d.chapters(), got);
            let mut n = 0;
            while d.read_packet().is_ok() {
                n += 1;
            }
            assert_eq!(n, 10);
        }
    }
}

#[test]
fn chapters_are_written_in_time_order_under_valid_uids() {
    let list = [ms(1, 5000, 9000, "B"), ms(0, 0, 5000, "A")];
    let got = read(&mux(false, true, &list));
    let want = [
        (1, 0, 5000, Some("A".into())),
        (2, 5000, 9000, Some("B".into())),
    ];
    assert_eq!(spans(&got), want);
    let ids = |list: &[Chapter]| -> Vec<u64> {
        read(&mux(false, true, list)).iter().map(|c| c.id).collect()
    };
    assert_eq!(ids(&[ms(4, 0, 1, ""), ms(4, 1, 2, "")]), [1, 2]);
    assert_eq!(ids(&[ms(0, 0, 1, ""), ms(u64::MAX, 1, 2, "")]), [1, 2]);
    assert_eq!(ids(&[ms(u64::MAX, 0, 1, "")]), [u64::MAX]);
    // Before the start of time, and backwards.
    let list = [ms(1, -500, 100, "x"), ms(2, 300, 200, "y")];
    let got = read(&mux(false, true, &list));
    let got: Vec<_> = spans(&got).into_iter().map(|s| (s.1, s.2)).collect();
    assert_eq!(got, [(0, 100), (300, 300)]);
}

#[test]
fn nothing_is_written_for_no_chapters() {
    let file = mux(false, true, &[]);
    assert!(!file.windows(4).any(|w| w == [0x10, 0x43, 0xA7, 0x70]));
    assert!(read(&file).is_empty());
}

// ---- hand-built ---------------------------------------------------------------

fn el(id: u32, body: &[u8]) -> Vec<u8> {
    let mut out: Vec<u8> = id
        .to_be_bytes()
        .into_iter()
        .skip_while(|&b| b == 0)
        .collect();
    out.push(0x01);
    out.extend_from_slice(&(body.len() as u64).to_be_bytes()[1..]);
    out.extend_from_slice(body);
    out
}

fn uel(id: u32, v: u64) -> Vec<u8> {
    el(id, &v.to_be_bytes())
}

fn atom(uid: u64, start: Option<u64>, end: Option<u64>, extra: &[u8]) -> Vec<u8> {
    let mut b = uel(0x73C4, uid);
    b.extend(start.map(|v| uel(0x91, v)).unwrap_or_default());
    b.extend(end.map(|v| uel(0x92, v)).unwrap_or_default());
    b.extend_from_slice(extra);
    el(0xB6, &b)
}

fn display(title: &str) -> Vec<u8> {
    el(0x80, &el(0x85, title.as_bytes()))
}

/// `file` (from [`mux`] without chapters) with `chapters` put after its
/// Clusters, reachable only through a SeekHead in front.
fn after_clusters(body: &[u8]) -> Vec<u8> {
    let track = [
        uel(0xD7, 1),
        uel(0x83, 1),
        el(0x86, b"V_VP9"),
        el(0xE0, &[uel(0xB0, 8), uel(0xBA, 8)].concat()),
    ]
    .concat();
    let info = [
        uel(0x2AD7B1, 1_000_000),
        el(0x4489, &10_000f64.to_be_bytes()),
    ]
    .concat();
    let head = [
        el(0x1549_A966, &info),
        el(0x1654_AE6B, &el(0xAE, &track)),
        el(
            0x1F43_B675,
            &[uel(0xE7, 0), el(0xA3, &[0x81, 0, 0, 0x80, 9])].concat(),
        ),
    ]
    .concat();
    let seek = |pos: u64| {
        el(
            0x114D_9B74,
            &el(
                0x4DBB,
                &[el(0x53AB, &[0x10, 0x43, 0xA7, 0x70]), uel(0x53AC, pos)].concat(),
            ),
        )
    };
    let at = (seek(0).len() + head.len()) as u64;
    let seg = [seek(at), head, el(0x1043_A770, body)].concat();
    [el(0x1A45_DFA3, &[]), el(0x1853_8067, &seg)].concat()
}

const S: u64 = 1_000_000_000;

#[test]
fn chapters_after_the_clusters_and_open_ends() {
    let edition = [
        atom(1, Some(0), None, &display("a")),
        atom(2, Some(4 * S), None, &[display("x"), display("b")].concat()),
        atom(3, None, None, &display("no start")),
        atom(4, Some(u64::MAX), None, &[]),
        // A nested chapter is not listed.
        atom(
            5,
            Some(6 * S),
            Some(7 * S),
            &atom(6, Some(6 * S), None, &[]),
        ),
    ]
    .concat();
    let file = after_clusters(&el(0x45B9, &edition));
    let want = [
        (1, 0, 4000, Some("a".into())),
        (2, 4000, 6000, Some("b".into())),
        (5, 6000, 7000, None),
    ];
    assert_eq!(spans(&read(&file)), want);
    // A pipe cannot go there; the media still reads.
    let mut d = MkvDemuxer::new(Box::new(Cursor::new(file)));
    d.read_header().unwrap();
    assert!(d.chapters().is_empty());
    assert!(d.read_packet().is_ok());
}

#[test]
fn the_default_edition_is_the_one_read() {
    let one = el(0x45B9, &atom(1, Some(0), None, &display("first")));
    let two = el(
        0x45B9,
        &[uel(0x45DB, 1), atom(2, Some(0), None, &display("default"))].concat(),
    );
    let title = |body: &[u8]| read(&after_clusters(body))[0].title().map(str::to_string);
    assert_eq!(
        title(&[one.clone(), two.clone()].concat()).unwrap(),
        "default"
    );
    assert_eq!(title(&one).unwrap(), "first");
    // The last chapter's open end is the end of the file.
    assert_eq!(read(&after_clusters(&one))[0].end_in(1000), 10_000);
}

fn exercise(file: &[u8]) {
    for pipe in [false, true] {
        let input = Cursor::new(file.to_vec());
        let mut d = if pipe {
            MkvDemuxer::new(Box::new(input))
        } else {
            MkvDemuxer::new_seekable(Box::new(input))
        };
        if d.read_header().is_err() {
            continue;
        }
        for c in d.chapters() {
            assert!(c.start >= 0 && c.end >= c.start);
        }
        while d.read_packet().is_ok() {}
    }
}

#[test]
fn truncated_and_flipped_chapters_do_not_panic() {
    let mut tagged = ms(1, 0, 10, "t");
    tagged.metadata.set("artist", "someone");
    let edition = el(0x45B9, &atom(1, Some(0), Some(S), &display("first")));
    for file in [
        mux(false, true, &[tagged, ms(2, 10, 20, "u")]),
        after_clusters(&edition),
    ] {
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
}

#[test]
fn the_number_of_chapters_is_bounded() {
    let many: Vec<u8> = (0..70_000u64)
        .flat_map(|i| atom(i + 1, Some(i), None, &[]))
        .collect();
    let got = read(&after_clusters(&el(0x45B9, &many)));
    assert_eq!(got.len(), rff_format::chapter::MAX_CHAPTERS);
}
