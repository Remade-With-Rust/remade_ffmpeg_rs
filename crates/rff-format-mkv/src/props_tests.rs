//! Round trips for the Matroska elements of #114: Name, Language(BCP47),
//! the track flags, FlagInterlaced/FieldOrder, StereoMode, PixelCrop,
//! DisplayWidth/Height, Projection (rotation), Info Title/DateUTC/WritingApp
//! and global Tags, plus av1C / vpcC CodecPrivate from MP4 extradata.

use std::io::{Cursor, Seek, SeekFrom, Write};
use std::sync::{Arc, Mutex};

use rff_core::{CleanAperture, CodecId, Dictionary, Error, MediaType, Packet, Rational};
use rff_format::{Demuxer, Muxer, Stream};

use crate::mux::{put_elem, put_string, put_uint};
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

fn feed(m: &mut dyn Muxer, md: &Dictionary, streams: &[Stream], packets: &[Packet]) {
    m.set_metadata(md);
    m.write_header(streams).unwrap();
    for p in packets {
        m.write_packet(p).unwrap();
    }
    m.write_trailer().unwrap();
}

/// The buffered and the streaming muxer's files.
fn mux_both(md: &Dictionary, streams: &[Stream], packets: &[Packet]) -> [Vec<u8>; 2] {
    let a = Shared::default();
    feed(
        &mut MkvMuxer::new(Box::new(a.clone()), false),
        md,
        streams,
        packets,
    );
    let b = Shared::default();
    feed(
        &mut MkvStreamingMuxer::new(Box::new(b.clone()), false),
        md,
        streams,
        packets,
    );
    let take = |s: Shared| s.0.lock().unwrap().get_ref().clone();
    [take(a), take(b)]
}

fn open(file: &[u8]) -> (MkvDemuxer, Vec<Stream>) {
    let mut d = MkvDemuxer::new_seekable(Box::new(Cursor::new(file.to_vec())));
    let s = d.read_header().unwrap();
    (d, s)
}

fn vp9(width: u32, height: u32) -> (Stream, Vec<Packet>) {
    let mut s = Stream::new(0, CodecId::Vp9);
    s.media_type = MediaType::Video;
    (s.width, s.height) = (width, height);
    s.time_base = Rational::new(1, 1000);
    let packets = (0..5)
        .map(|i| {
            let mut p = Packet::from_data(0, vec![0x82, 0x49, 0x83, 0x42, i as u8]);
            p.pts = Some(i * 40);
            p.flags.keyframe = i == 0;
            p
        })
        .collect();
    (s, packets)
}

#[test]
fn track_properties_round_trip() {
    let (mut v, mut packets) = vp9(1440, 1088);
    v.name = Some("Camera A — main".into());
    v.language = Some("fra".into());
    v.disposition.default = Some(false);
    v.disposition.enabled = Some(false);
    v.disposition.forced = true;
    v.disposition.commentary = true;
    v.field_order = Some(1);
    v.stereo_mode = Some(1);
    // 8 rows cropped off the bottom, 1440 wide shown 4:3 wider (1920).
    v.clean_aperture = Some(CleanAperture {
        width: (1440, 1),
        height: (1080, 1),
        horiz_offset: (0, 2),
        vert_offset: (-8, 2),
    });
    v.pixel_aspect = Some((4, 3));
    v.set_rotation(90.0);
    let mut a = Stream::new(1, CodecId::Opus);
    a.media_type = MediaType::Audio;
    (a.channels, a.sample_rate) = (2, 48_000);
    a.time_base = Rational::new(1, 1000);
    a.language = Some("pt-BR".into());
    a.disposition.hearing_impaired = true;
    a.disposition.visual_impaired = true;
    a.disposition.original = true;
    for i in 0..5 {
        let mut p = Packet::from_data(1, vec![0xFC, i as u8]);
        p.pts = Some(i * 20);
        p.flags.keyframe = true;
        packets.push(p);
    }
    for file in mux_both(&Dictionary::new(), &[v.clone(), a.clone()], &packets) {
        let (_, got) = open(&file);
        let (gv, ga) = (&got[0], &got[1]);
        assert_eq!(gv.name, v.name);
        assert_eq!(gv.language.as_deref(), Some("fra"));
        assert_eq!(gv.disposition, v.disposition);
        assert_eq!((gv.field_order, gv.stereo_mode), (Some(1), Some(1)));
        assert_eq!(gv.clean_aperture, v.clean_aperture);
        assert_eq!(gv.pixel_aspect, Some((4, 3)));
        assert_eq!(gv.rotation(), Some(90.0));
        assert_eq!(gv.display_matrix, v.display_matrix);
        assert_eq!(ga.language.as_deref(), Some("pt-BR"));
        assert_eq!(ga.disposition, a.disposition);
        assert_eq!(ga.name, None);
    }
}

#[test]
fn unknown_language_progressive_and_other_rotations() {
    for (deg, order) in [(270.0, 0u8), (180.0, 6)] {
        let (mut v, packets) = vp9(64, 48);
        v.set_rotation(deg);
        v.field_order = Some(order);
        for file in mux_both(&Dictionary::new(), std::slice::from_ref(&v), &packets) {
            let (_, got) = open(&file);
            assert_eq!(got[0].rotation(), Some(deg));
            assert_eq!(got[0].field_order, Some(order));
            // None was written as "und" and reads back as unknown.
            assert_eq!(got[0].language, None);
            assert_eq!(got[0].disposition, Default::default());
        }
    }
    // A missing Language element means its default, English.
    assert_eq!(
        crate::props::Lang::default().resolve().as_deref(),
        Some("eng")
    );
}

#[test]
fn info_and_tags_round_trip() {
    let mut md = Dictionary::new();
    md.set("title", "Freehold cut");
    md.set("creation_time", "2026-11-20T10:20:30.250000Z");
    md.set("encoder", "FilmCraft 1.2");
    md.set("artist", "ArtCraft");
    md.set("comment", "two\nlines");
    let (v, packets) = vp9(64, 48);
    for file in mux_both(&md, std::slice::from_ref(&v), &packets) {
        let (d, _) = open(&file);
        assert_eq!(d.metadata(), Some(&md));
    }
    // Plain (non-seekable) input reads header Tags too.
    let [file, _] = mux_both(&md, std::slice::from_ref(&v), &packets);
    let mut d = MkvDemuxer::new(Box::new(Cursor::new(file)));
    d.read_header().unwrap();
    assert_eq!(d.metadata().unwrap().get("artist"), Some("ArtCraft"));
    // Packets still come out after the Tags element.
    let mut n = 0;
    loop {
        match d.read_packet() {
            Ok(_) => n += 1,
            Err(Error::Eof) => break,
            Err(e) => panic!("{e}"),
        }
    }
    assert_eq!(n, 5);
}

#[test]
fn tags_after_the_clusters_are_found_through_the_seekhead() {
    // EBML header, then a Segment of SeekHead (Tags only), Info, Tracks, one
    // Cluster, Tags -- the layout FFmpeg writes.
    let mut ebml = Vec::new();
    put_string(&mut ebml, 0x4282, "matroska");
    let mut file = Vec::new();
    put_elem(&mut file, 0x1A45_DFA3, &ebml);
    let mut info = Vec::new();
    put_uint(&mut info, 0x2A_D7B1, 1_000_000);
    put_string(&mut info, 0x7BA9, "late tags");
    let mut info_e = Vec::new();
    put_elem(&mut info_e, 0x1549_A966, &info);
    let mut te = Vec::new();
    put_uint(&mut te, 0xD7, 1);
    put_uint(&mut te, 0x83, 2);
    put_string(&mut te, 0x86, "A_PCM/INT/LIT");
    let mut au = Vec::new();
    put_elem(&mut au, 0xB5, &48_000f64.to_be_bytes());
    put_uint(&mut au, 0x9F, 1);
    put_uint(&mut au, 0x6264, 16);
    put_elem(&mut te, 0xE1, &au);
    let mut tracks = Vec::new();
    put_elem(&mut tracks, 0xAE, &te);
    let mut tracks_e = Vec::new();
    put_elem(&mut tracks_e, 0x1654_AE6B, &tracks);
    let mut cl = Vec::new();
    put_uint(&mut cl, 0xE7, 0);
    put_elem(&mut cl, 0xA3, &[0x81, 0, 0, 0x80, 1, 2, 3, 4]);
    let mut cluster = Vec::new();
    put_elem(&mut cluster, 0x1F43_B675, &cl);
    let mut st = Vec::new();
    put_string(&mut st, 0x45A3, "ARTIST");
    put_string(&mut st, 0x4487, "Somebody");
    let mut tag = Vec::new();
    put_elem(&mut tag, 0x67C8, &st);
    // A track-targeted tag is not global metadata.
    let mut tagged = Vec::new();
    let mut targets = Vec::new();
    put_uint(&mut targets, 0x63C5, 1);
    put_elem(&mut tagged, 0x63C0, &targets);
    let mut st2 = Vec::new();
    put_string(&mut st2, 0x45A3, "LANGUAGE");
    put_string(&mut st2, 0x4487, "x");
    put_elem(&mut tagged, 0x67C8, &st2);
    let mut tags = Vec::new();
    put_elem(&mut tags, 0x7373, &tag);
    put_elem(&mut tags, 0x7373, &tagged);
    let mut tags_e = Vec::new();
    put_elem(&mut tags_e, 0x1254_C367, &tags);
    let seekhead = |pos: u64| {
        let mut seek = Vec::new();
        put_elem(&mut seek, 0x53AB, &[0x12, 0x54, 0xC3, 0x67]);
        put_elem(&mut seek, 0x53AC, &pos.to_be_bytes());
        let mut body = Vec::new();
        put_elem(&mut body, 0x4DBB, &seek);
        let mut out = Vec::new();
        put_elem(&mut out, 0x114D_9B74, &body);
        out
    };
    let head_len = seekhead(0).len();
    let tags_pos = (head_len + info_e.len() + tracks_e.len() + cluster.len()) as u64;
    let body = [seekhead(tags_pos), info_e, tracks_e, cluster, tags_e].concat();
    put_elem(&mut file, 0x1853_8067, &body);
    let (mut d, s) = open(&file);
    assert_eq!(s.len(), 1);
    let md = d.metadata().unwrap();
    assert_eq!(md.get("title"), Some("late tags"));
    assert_eq!(md.get("artist"), Some("Somebody"));
    assert_eq!(md.get("language"), None);
    // The jump to the Tags did not move the packet walk.
    let p = d.read_packet().unwrap();
    assert_eq!(p.data, [1, 2, 3, 4]);
}

#[test]
fn mp4_codec_records_become_codec_private() {
    // An MP4 vpcC record (profile 2, level 41, 10-bit, 4:2:0 colocated).
    let (mut v, packets) = vp9(64, 48);
    v.extradata = vec![1, 0, 0, 0, 2, 41, (10 << 4) | (1 << 1), 9, 16, 9, 0, 0];
    for file in mux_both(&Dictionary::new(), std::slice::from_ref(&v), &packets) {
        let (_, got) = open(&file);
        assert_eq!(got[0].extradata, [1, 1, 2, 2, 1, 41, 3, 1, 10, 4, 1, 1]);
    }
    // An av1C record is the V_AV1 CodecPrivate as it is.
    let av1c = vec![
        0x81, 0x08, 0x0C, 0x00, 0x0A, 0x05, 0x00, 0x00, 0x00, 0x01, 0x02,
    ];
    let mut a = Stream::new(0, CodecId::Avif);
    a.media_type = MediaType::Video;
    (a.width, a.height) = (64, 48);
    a.time_base = Rational::new(1, 1000);
    a.extradata = av1c.clone();
    let frames: Vec<Packet> = (0..3)
        .map(|i| {
            let mut p = Packet::from_data(0, vec![0x32, 0x00, i as u8]);
            p.pts = Some(i * 40);
            p.flags.keyframe = i == 0;
            p
        })
        .collect();
    for file in mux_both(&Dictionary::new(), std::slice::from_ref(&a), &frames) {
        let (_, got) = open(&file);
        assert_eq!(got[0].extradata, av1c);
    }
}
