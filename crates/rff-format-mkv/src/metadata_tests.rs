//! Colour / HDR metadata, BlockAdditions (WebM alpha), CodecDelay and
//! SeekPreRoll, ContentEncodings and the copy-only codecs (#110).

use std::io::{Cursor, Seek, SeekFrom, Write};
use std::sync::{Arc, Mutex};

use rff_core::{
    BlockAdditional, CodecId, ColorInfo, ColorRange, ContentLightLevel, Error, MasteringDisplay,
    Packet, Rational,
};
use rff_format::{Demuxer, Muxer, Stream};

use crate::{MkvDemuxer, MkvMuxer, MkvStreamingMuxer};

// ---- muxing and demuxing in memory -----------------------------------------

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

fn run(mux: &mut dyn Muxer, streams: &[Stream], packets: &[Packet]) -> rff_core::Result<()> {
    mux.write_header(streams)?;
    for p in packets {
        mux.write_packet(p)?;
    }
    mux.write_trailer()
}

/// The file from the in-memory muxer (`streamed == false`) or the streaming
/// (seekable) one.
fn mux(webm: bool, streamed: bool, streams: &[Stream], packets: &[Packet]) -> Vec<u8> {
    try_mux(webm, streamed, streams, packets).unwrap()
}

fn try_mux(
    webm: bool,
    streamed: bool,
    streams: &[Stream],
    packets: &[Packet],
) -> rff_core::Result<Vec<u8>> {
    let sink = Shared::default();
    if streamed {
        run(
            &mut MkvStreamingMuxer::new(Box::new(sink.clone()), webm),
            streams,
            packets,
        )?;
    } else {
        run(
            &mut MkvMuxer::new(Box::new(sink.clone()), webm),
            streams,
            packets,
        )?;
    }
    let bytes = sink.0.lock().unwrap().get_ref().clone();
    Ok(bytes)
}

fn demux(file: Vec<u8>) -> (Vec<Stream>, Vec<Packet>) {
    let mut dem = MkvDemuxer::new(Box::new(Cursor::new(file)));
    let streams = dem.read_header().unwrap();
    let mut packets = Vec::new();
    loop {
        match dem.read_packet() {
            Ok(p) => packets.push(p),
            Err(Error::Eof) => break,
            Err(e) => panic!("demux: {e}"),
        }
    }
    (streams, packets)
}

/// Demux whatever the bytes give, never panicking: the robustness probe.
fn demux_lossy(file: Vec<u8>) -> usize {
    let len = file.len();
    let mut dem = MkvDemuxer::new(Box::new(Cursor::new(file)));
    if dem.read_header().is_err() {
        return 0;
    }
    let mut n = 0;
    while dem.read_packet().is_ok() {
        n += 1;
        assert!(n <= len, "{n} packets from {len} bytes");
    }
    n
}

fn pkt(stream: usize, pts: i64, key: bool, data: &[u8]) -> Packet {
    let mut p = Packet::from_data(stream, data.to_vec());
    p.pts = Some(pts);
    p.flags.keyframe = key;
    p
}

fn video(codec: CodecId) -> Stream {
    let mut v = Stream::new(0, codec);
    v.width = 320;
    v.height = 240;
    v
}

/// Every (webm, streamed) combination.
const ARMS: [(bool, bool); 4] = [(false, false), (false, true), (true, false), (true, true)];

// ---- hand-built EBML ---------------------------------------------------------

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

/// One track (`entry` = its TrackEntry children) and one Cluster at 0 of
/// `cluster` (already-built children), 1 ms ticks.
fn file(entry: &[u8], cluster: &[u8]) -> Vec<u8> {
    let segment = [
        el(crate::ID_INFO, &uel(crate::ID_TIMESTAMP_SCALE, 1_000_000)),
        el(crate::ID_TRACKS, &el(crate::ID_TRACK_ENTRY, entry)),
        el(
            crate::ID_CLUSTER,
            &[uel(crate::ID_TIMESTAMP, 0), cluster.to_vec()].concat(),
        ),
    ]
    .concat();
    [
        el(0x1A45_DFA3, &el(0x4282, b"matroska")),
        el(crate::ID_SEGMENT, &segment),
    ]
    .concat()
}

/// TrackEntry children for track 1.
fn entry(track_type: u64, codec: &str, private: &[u8], extra: &[u8]) -> Vec<u8> {
    let mut e = [
        uel(crate::ID_TRACK_NUMBER, 1),
        uel(crate::ID_TRACK_TYPE, track_type),
        el(crate::ID_CODEC_ID, codec.as_bytes()),
    ]
    .concat();
    if !private.is_empty() {
        e.extend(el(crate::ID_CODEC_PRIVATE, private));
    }
    e.extend_from_slice(extra);
    e
}

fn simple_block(rel: i16, flags: u8, payload: &[u8]) -> Vec<u8> {
    el(
        crate::ID_SIMPLE_BLOCK,
        &[&[0x81][..], &rel.to_be_bytes(), &[flags], payload].concat(),
    )
}

/// A ContentEncodings element of one ContentEncoding.
fn content_encodings(encodings: &[Vec<u8>]) -> Vec<u8> {
    el(crate::ID_CONTENT_ENCODINGS, &encodings.concat())
}

/// A compression ContentEncoding: (order, scope, algo, settings).
fn compression(order: u64, scope: u64, algo: u64, settings: &[u8]) -> Vec<u8> {
    let mut comp = uel(0x4254, algo);
    if !settings.is_empty() {
        comp.extend(el(0x4255, settings));
    }
    el(
        0x6240,
        &[
            uel(0x5031, order),
            uel(0x5032, scope),
            uel(0x5033, 0),
            el(0x5034, &comp),
        ]
        .concat(),
    )
}

// ---- Colour --------------------------------------------------------------------

fn hdr_stream(codec: CodecId) -> Stream {
    let mut v = video(codec);
    v.color = ColorInfo {
        primaries: 9,
        transfer: 16,
        matrix: 9,
    };
    v.color_range = ColorRange::Limited;
    v.mastering_display = Some(MasteringDisplay {
        red: (35_400, 14_600),
        green: (8_500, 39_850),
        blue: (6_550, 2_300),
        white_point: (15_635, 16_450),
        max_luminance: 10_000_000, // 1000 cd/m²
        min_luminance: 50,         // 0.005 cd/m²
    });
    v.content_light = Some(ContentLightLevel {
        max_cll: 1000,
        max_fall: 400,
    });
    v
}

#[test]
fn colour_mastering_and_light_levels_round_trip() {
    let src = hdr_stream(CodecId::Vp9);
    for (webm, streamed) in ARMS {
        let f = mux(
            webm,
            streamed,
            std::slice::from_ref(&src),
            &[pkt(0, 0, true, &[0x11; 8])],
        );
        let (s, _) = demux(f.clone());
        assert_eq!(s[0].color, src.color, "webm={webm} streamed={streamed}");
        assert_eq!(s[0].color_range, ColorRange::Limited);
        assert_eq!(s[0].mastering_display, src.mastering_display);
        assert_eq!(s[0].content_light, src.content_light);
        // And again through a remux of what came out.
        let (s2, _) = demux(mux(webm, streamed, &s, &[pkt(0, 0, true, &[1])]));
        assert_eq!(
            (
                s2[0].color,
                s2[0].color_range,
                s2[0].mastering_display,
                s2[0].content_light
            ),
            (
                s[0].color,
                s[0].color_range,
                s[0].mastering_display,
                s[0].content_light
            )
        );
    }
}

#[test]
fn colour_writes_only_what_is_specified() {
    // Nothing specified: no Colour element at all (the payloads are 0x11s,
    // so the ID's bytes can only come from an element).
    let plain = mux(
        false,
        false,
        &[video(CodecId::Vp9)],
        &[pkt(0, 0, true, &[0x11; 8])],
    );
    assert!(!plain.windows(2).any(|w| w == [0x55, 0xB0]));
    let (s, _) = demux(plain);
    assert!(s[0].color.is_unspecified());
    assert_eq!(s[0].color_range, ColorRange::Unspecified);
    assert_eq!((s[0].mastering_display, s[0].content_light), (None, None));

    // Only the range: the rest stays unspecified / absent.
    let mut v = video(CodecId::Vp9);
    v.color_range = ColorRange::Full;
    let f = mux(false, false, &[v], &[pkt(0, 0, true, &[0x11; 8])]);
    let colour = f
        .windows(2)
        .position(|w| w == [0x55, 0xB0])
        .expect("Colour");
    // Colour { Range 2 } and nothing else: ID, 1-byte size 4, Range (2+1+1).
    assert_eq!(
        &f[colour..colour + 7],
        &[0x55, 0xB0, 0x84, 0x55, 0xB9, 0x81, 2]
    );
    let (s, _) = demux(f);
    assert_eq!(s[0].color_range, ColorRange::Full);
    assert!(s[0].color.is_unspecified());
    assert_eq!((s[0].mastering_display, s[0].content_light), (None, None));
}

#[test]
fn hand_built_colour_reads_floats_and_defaults() {
    let mastering = [
        el(crate::ID_PRIMARY_R_X, &0.708f64.to_be_bytes()),
        el(crate::ID_PRIMARY_R_Y, &0.292f32.to_be_bytes()), // 4-byte float
        el(crate::ID_LUMINANCE_MAX, &4000.0f64.to_be_bytes()),
        el(crate::ID_LUMINANCE_MIN, &0.0001f64.to_be_bytes()),
    ]
    .concat();
    let colour = [
        uel(crate::ID_MATRIX_COEFFICIENTS, 1),
        uel(crate::ID_RANGE, 3), // "defined by matrix/transfer": unspecified for us
        uel(crate::ID_PRIMARIES, 70_000), // not an H.273 code point
        uel(crate::ID_MAX_FALL, 1 << 20), // past u16: saturates
        el(crate::ID_MASTERING_METADATA, &mastering),
    ]
    .concat();
    let vid = [
        uel(crate::ID_PIXEL_WIDTH, 16),
        uel(crate::ID_PIXEL_HEIGHT, 16),
        el(crate::ID_COLOUR, &colour),
    ]
    .concat();
    let f = file(
        &entry(1, "V_VP9", &[], &el(crate::ID_VIDEO, &vid)),
        &simple_block(0, 0x80, b"k"),
    );
    let (s, _) = demux(f);
    assert_eq!(
        s[0].color,
        ColorInfo {
            primaries: 2,
            transfer: 2,
            matrix: 1
        }
    );
    assert_eq!(s[0].color_range, ColorRange::Unspecified);
    assert_eq!(
        s[0].content_light,
        Some(ContentLightLevel {
            max_cll: 0,
            max_fall: u16::MAX
        })
    );
    let m = s[0].mastering_display.unwrap();
    assert_eq!(m.red, (35_400, 14_600));
    assert_eq!((m.green, m.blue, m.white_point), ((0, 0), (0, 0), (0, 0)));
    assert_eq!((m.max_luminance, m.min_luminance), (40_000_000, 1));
}

// ---- BlockAdditions / alpha ------------------------------------------------------

fn alpha(data: &[u8]) -> Vec<BlockAdditional> {
    vec![BlockAdditional {
        id: BlockAdditional::ALPHA,
        data: data.to_vec(),
    }]
}

fn alpha_packets() -> Vec<Packet> {
    let mut p = vec![
        pkt(0, 0, true, &[0x21; 40]),
        pkt(0, 40, false, &[0x22; 20]),
        pkt(0, 80, false, &[0x23; 21]), // no alpha: a SimpleBlock
        pkt(0, 120, false, &[0x24; 22]),
        pkt(0, 160, true, &[0x25; 30]),
        pkt(0, 200, false, &[0x26; 23]),
    ];
    p[0].additions = alpha(&[0xA0; 12]);
    p[1].additions = alpha(&[0xA1; 6]);
    p[3].additions = alpha(&[0xA3; 7]);
    p[4].additions = alpha(&[0xA4; 9]);
    // Another BlockAddID, and two BlockMores on one block.
    p[5].additions = vec![
        BlockAdditional {
            id: 1,
            data: vec![0xA5; 3],
        },
        BlockAdditional {
            id: 4,
            data: vec![0xB5; 2],
        },
    ];
    p
}

#[test]
fn vp8_and_vp9_alpha_additions_round_trip_with_keyframes() {
    let want = alpha_packets();
    for codec in [CodecId::Vp9, CodecId::Vp8] {
        let mut v = video(codec);
        v.alpha_mode = true;
        for (webm, streamed) in ARMS {
            let f = mux(webm, streamed, &[v.clone()], &want);
            let (s, got) = demux(f);
            let arm = format!("{codec} webm={webm} streamed={streamed}");
            assert_eq!(s[0].codec_id, codec, "{arm}");
            assert!(s[0].alpha_mode, "{arm}");
            assert_eq!(got.len(), want.len(), "{arm}");
            for (g, w) in got.iter().zip(&want) {
                assert_eq!(g.data, w.data, "{arm}");
                assert_eq!(g.pts, w.pts, "{arm}");
                assert_eq!(g.flags.keyframe, w.flags.keyframe, "{arm} pts {:?}", w.pts);
                assert_eq!(g.additions, w.additions, "{arm} pts {:?}", w.pts);
            }
        }
    }
    // Without the flag, no AlphaMode; additions still travel.
    let f = mux(true, false, &[video(CodecId::Vp9)], &want);
    let (s, got) = demux(f);
    assert!(!s[0].alpha_mode);
    assert_eq!(got[1].additions, want[1].additions);
}

#[test]
fn reference_block_points_at_the_previous_block() {
    let mut v = video(CodecId::Vp9);
    v.alpha_mode = true;
    let f = mux(true, false, &[v], &alpha_packets());
    // The block at 40 refers back 40 ms (FFmpeg's value); signed, one byte.
    let at = f
        .windows(3)
        .position(|w| w == [crate::ID_REFERENCE_BLOCK as u8, 0x81, (-40i8) as u8]);
    assert!(at.is_some(), "ReferenceBlock -40 written");
    // AlphaMode 1 and MaxBlockAdditionID 1.
    assert!(f.windows(4).any(|w| w == [0x53, 0xC0, 0x81, 1]));
    assert!(f.windows(4).any(|w| w == [0x55, 0xEE, 0x81, 1]));
}

#[test]
fn block_groups_are_keyframes_unless_referenced() {
    let block = |rel: i16| {
        el(
            crate::ID_BLOCK,
            &[&[0x81][..], &rel.to_be_bytes(), &[0], b"f"].concat(),
        )
    };
    let more = el(
        crate::ID_BLOCK_ADDITIONS,
        &el(
            crate::ID_BLOCK_MORE,
            &el(crate::ID_BLOCK_ADDITIONAL, b"alpha"),
        ),
    );
    let cluster = [
        el(crate::ID_BLOCK_GROUP, &block(0)),
        // ReferenceBlock before the Block, BlockAdditions after it.
        el(
            crate::ID_BLOCK_GROUP,
            &[el(crate::ID_REFERENCE_BLOCK, &[0xD8]), block(40), more].concat(),
        ),
    ]
    .concat();
    let vid = el(crate::ID_VIDEO, &el(crate::ID_ALPHA_MODE, &[1]));
    let (s, got) = demux(file(&entry(1, "V_VP8", &[], &vid), &cluster));
    assert!(s[0].alpha_mode);
    assert_eq!(s[0].codec_id, CodecId::Vp8);
    assert!(got[0].flags.keyframe && got[0].additions.is_empty());
    assert!(!got[1].flags.keyframe);
    assert_eq!(got[1].additions, alpha(b"alpha"));
}

#[test]
fn laced_block_additions_go_on_every_frame() {
    // Xiph lacing, 3 frames.
    let laced = [&[2u8, 2, 3][..], b"aa", b"bbb", b"c"].concat();
    let group = el(
        crate::ID_BLOCK_GROUP,
        &[
            el(
                crate::ID_BLOCK,
                &[&[0x81][..], &0i16.to_be_bytes(), &[0x02], &laced].concat(),
            ),
            el(
                crate::ID_BLOCK_ADDITIONS,
                &el(
                    crate::ID_BLOCK_MORE,
                    &[
                        uel(crate::ID_BLOCK_ADD_ID, 7),
                        el(crate::ID_BLOCK_ADDITIONAL, b"x"),
                    ]
                    .concat(),
                ),
            ),
        ]
        .concat(),
    );
    let (_, got) = demux(file(&entry(2, "A_AAC", &[], &[]), &group));
    assert_eq!(got.len(), 3);
    for p in &got {
        assert_eq!(
            p.additions,
            vec![BlockAdditional {
                id: 7,
                data: b"x".to_vec()
            }]
        );
    }
}

// ---- CodecDelay / SeekPreRoll -----------------------------------------------------

fn opus(extradata: Vec<u8>) -> Stream {
    let mut a = Stream::new(0, CodecId::Opus);
    a.sample_rate = 48_000;
    a.channels = 2;
    a.extradata = extradata;
    a
}

fn opus_head(pre_skip: u16) -> Vec<u8> {
    let mut v = b"OpusHead".to_vec();
    v.extend_from_slice(&[1, 2]);
    v.extend_from_slice(&pre_skip.to_le_bytes());
    v.extend_from_slice(&48_000u32.to_le_bytes());
    v.extend_from_slice(&[0, 0, 0]);
    v
}

#[test]
fn codec_delay_and_seek_preroll_round_trip_with_stable_timestamps() {
    let mut a = opus(opus_head(312));
    a.codec_delay_ns = 6_500_000;
    a.seek_preroll_ns = 80_000_000;
    // As the demuxer hands them out: the first frame is before zero.
    let packets: Vec<Packet> = (0..6)
        .map(|i| pkt(0, i * 20 - 7, true, &[i as u8 + 1; 9]))
        .collect();
    for (webm, streamed) in ARMS {
        let (s1, p1) = demux(mux(webm, streamed, &[a.clone()], &packets));
        assert_eq!(
            (s1[0].codec_delay_ns, s1[0].seek_preroll_ns),
            (6_500_000, 80_000_000)
        );
        let pts = |p: &[Packet]| p.iter().map(|p| p.pts).collect::<Vec<_>>();
        assert_eq!(pts(&p1), pts(&packets), "webm={webm} streamed={streamed}");
        // Remux what came out: nothing moves.
        let (s2, p2) = demux(mux(webm, streamed, &s1, &p1));
        assert_eq!(
            (s2[0].codec_delay_ns, s2[0].seek_preroll_ns),
            (6_500_000, 80_000_000)
        );
        assert_eq!(pts(&p2), pts(&p1));
        assert_eq!(s2[0].duration, s1[0].duration);
    }
}

#[test]
fn opus_falls_back_to_pre_skip_and_80ms_preroll() {
    // Stream fields unset: CodecDelay from the OpusHead pre-skip (312 samples
    // = 6.5 ms), SeekPreRoll 80 ms, and the timestamps still survive.
    let packets: Vec<Packet> = (0..3).map(|i| pkt(0, i * 20, true, &[7; 5])).collect();
    let (s, p) = demux(mux(true, false, &[opus(opus_head(312))], &packets));
    assert_eq!(
        (s[0].codec_delay_ns, s[0].seek_preroll_ns),
        (6_500_000, 80_000_000)
    );
    assert_eq!(
        p.iter().map(|p| p.pts).collect::<Vec<_>>(),
        [Some(0), Some(20), Some(40)]
    );
    // No pre-skip (a synthesized OpusHead): no CodecDelay, still the preroll.
    let (s, _) = demux(mux(true, false, &[opus(Vec::new())], &packets));
    assert_eq!((s[0].codec_delay_ns, s[0].seek_preroll_ns), (0, 80_000_000));
    // Not Opus: nothing invented, but a given delay is written and undone.
    let mut aac = Stream::new(0, CodecId::Aac);
    aac.sample_rate = 48_000;
    aac.channels = 2;
    let (s, p) = demux(mux(false, true, &[aac.clone()], &packets));
    assert_eq!((s[0].codec_delay_ns, s[0].seek_preroll_ns), (0, 0));
    assert_eq!(p[1].pts, Some(20));
    aac.codec_delay_ns = 21_333_333;
    let (s, p) = demux(mux(false, true, &[aac], &packets));
    assert_eq!(s[0].codec_delay_ns, 21_333_333);
    assert_eq!(
        p.iter().map(|p| p.pts).collect::<Vec<_>>(),
        [Some(0), Some(20), Some(40)]
    );
}

#[test]
fn hand_built_codec_delay_and_preroll_reach_the_stream() {
    let extra = [
        uel(crate::ID_CODEC_DELAY, 6_500_000),
        uel(crate::ID_SEEK_PREROLL, 80_000_000),
    ]
    .concat();
    let (s, p) = demux(file(
        &entry(2, "A_OPUS", &opus_head(312), &extra),
        &simple_block(20, 0x80, b"o"),
    ));
    assert_eq!(
        (s[0].codec_delay_ns, s[0].seek_preroll_ns),
        (6_500_000, 80_000_000)
    );
    assert_eq!(p[0].pts, Some(13));
}

// ---- ContentEncodings ---------------------------------------------------------------

#[test]
fn header_stripping_scope_1_prefixes_every_frame() {
    let enc = content_encodings(&[compression(0, 1, 3, &[0xAA, 0xBB])]);
    let laced = [&[1u8, 2][..], b"xy", b"z"].concat(); // Xiph: 2 frames
    let cluster = [
        simple_block(0, 0x80, &[1, 2, 3]),
        simple_block(10, 0x82, &laced),
        simple_block(20, 0x80, &[]),
    ]
    .concat();
    let (s, p) = demux(file(&entry(2, "A_AAC", b"\x12\x10", &enc), &cluster));
    assert_eq!(
        s[0].extradata, b"\x12\x10",
        "scope 1 leaves CodecPrivate alone"
    );
    let data: Vec<Vec<u8>> = p.into_iter().map(|p| p.data).collect();
    assert_eq!(
        data,
        [
            vec![0xAA, 0xBB, 1, 2, 3],
            vec![0xAA, 0xBB, b'x', b'y'],
            vec![0xAA, 0xBB, b'z'],
            vec![0xAA, 0xBB],
        ]
    );
}

#[test]
fn header_stripping_scope_2_restores_codec_private_only() {
    let enc = content_encodings(&[compression(0, 2, 3, b"Opus")]);
    let head = opus_head(0);
    let (s, p) = demux(file(
        &entry(2, "A_OPUS", &head[4..], &enc),
        &simple_block(0, 0x80, b"frame"),
    ));
    assert_eq!(s[0].extradata, head);
    assert_eq!(p[0].data, b"frame");
}

#[test]
fn header_stripping_comes_off_before_the_avcc_conversion() {
    // mkvmerge-style: the first two bytes of every AVCC frame (the top of
    // the 4-byte NAL length) are stripped.
    let sps = [0x67u8, 0x64, 0x00, 0x1F, 0xAC];
    let pps = [0x68u8, 0xEE, 0x3C];
    let avcc = rff_format::avc::build_avcc_record(&sps, &pps);
    let enc = content_encodings(&[compression(0, 1, 3, &[0, 0])]);
    let nal = [0x65u8, 1, 2, 3];
    let stored = [&[0u8, 4][..], &nal].concat(); // length 0x00000004, less two bytes
    let (_, p) = demux(file(
        &entry(1, "V_MPEG4/ISO/AVC", &avcc, &enc),
        &simple_block(0, 0x80, &stored),
    ));
    let mut want = Vec::new();
    for n in [&sps[..], &pps, &nal] {
        want.extend_from_slice(&[0, 0, 0, 1]);
        want.extend_from_slice(n);
    }
    assert_eq!(p[0].data, want);
}

#[test]
fn zlib_frames_and_codec_private_inflate() {
    let z = |d: &[u8]| miniz_oxide::deflate::compress_to_vec_zlib(d, 6);
    let head = opus_head(312);
    // Scope 3: frames and CodecPrivate. Chained after a header strip of
    // order 1, which was applied last, so comes off first.
    let enc = content_encodings(&[compression(0, 3, 0, &[]), compression(1, 1, 3, &[0x78])]);
    let frame = vec![9u8; 500];
    let zf = z(&frame);
    assert_eq!(zf[0], 0x78); // the zlib header byte the strip removes
    let cluster = [
        simple_block(0, 0x80, &zf[1..]),
        simple_block(20, 0x80, &[0xDE, 0xAD]), // corrupt: dropped
        simple_block(40, 0x80, &z(b"last")[1..]),
    ]
    .concat();
    // The strip has scope 1, so CodecPrivate is zlib only.
    let (s, p) = demux(file(&entry(2, "A_OPUS", &z(&head), &enc), &cluster));
    assert_eq!(s[0].extradata, head);
    let got: Vec<_> = p.iter().map(|p| (p.data.clone(), p.pts)).collect();
    assert_eq!(got, [(frame, Some(0)), (b"last".to_vec(), Some(40))]);
}

#[test]
fn encrypted_and_unsupported_tracks_are_refused_at_the_header() {
    let encrypted = el(
        0x6240,
        &[
            uel(0x5033, 1),
            el(0x5035, &uel(0x47E1, 5)), // ContentEncryption { ContentEncAlgo AES }
        ]
        .concat(),
    );
    // A ContentEncryption element without ContentEncodingType 1 counts too.
    let sneaky = el(0x6240, &el(0x5035, &[]));
    for (enc, what) in [
        (encrypted, "encrypted"),
        (sneaky, "encrypted"),
        (compression(0, 1, 1, &[]), "bzlib"),
        (compression(0, 1, 2, &[]), "LZO"),
        (compression(0, 1, 9, &[]), "ContentCompAlgo 9"),
    ] {
        let f = file(
            &entry(1, "V_VP9", &[], &content_encodings(&[enc])),
            &simple_block(0, 0x80, b"ciphertext"),
        );
        let mut dem = MkvDemuxer::new(Box::new(Cursor::new(f)));
        let err = dem.read_header().unwrap_err();
        assert!(matches!(err, Error::Unsupported(_)), "{err}");
        assert!(err.to_string().contains(what), "{err}");
        assert!(err.to_string().contains("track 1"), "{err}");
        // And stays refused: no frames come out.
        assert!(dem.read_header().is_err());
        assert!(dem.read_packet().is_err());
    }
    // A CodecPrivate that does not inflate is refused too.
    let enc = content_encodings(&[compression(0, 2, 0, &[])]);
    let f = file(
        &entry(2, "A_OPUS", b"not zlib", &enc),
        &simple_block(0, 0x80, b"o"),
    );
    let err = MkvDemuxer::new(Box::new(Cursor::new(f)))
        .read_header()
        .unwrap_err();
    assert!(err.to_string().contains("CodecPrivate"), "{err}");
}

// ---- robustness -------------------------------------------------------------------

/// A file using every new element: Colour with MasteringMetadata, AlphaMode,
/// CodecDelay, ContentEncodings (strip + zlib), BlockGroups with
/// BlockAdditions and ReferenceBlocks.
fn kitchen_sink() -> Vec<u8> {
    let mastering: Vec<u8> = (0x55D1..=0x55DA)
        .flat_map(|id| el(id, &0.25f64.to_be_bytes()))
        .collect();
    let colour = [
        uel(crate::ID_MATRIX_COEFFICIENTS, 9),
        uel(crate::ID_RANGE, 1),
        uel(crate::ID_MAX_CLL, 1000),
        el(crate::ID_MASTERING_METADATA, &mastering),
    ]
    .concat();
    let vid = [
        uel(crate::ID_PIXEL_WIDTH, 16),
        uel(crate::ID_ALPHA_MODE, 1),
        el(crate::ID_COLOUR, &colour),
    ]
    .concat();
    let enc = content_encodings(&[compression(1, 1, 3, &[0x78]), compression(0, 1, 0, &[])]);
    let extra = [
        el(crate::ID_VIDEO, &vid),
        uel(crate::ID_CODEC_DELAY, 1_000_000),
        enc,
    ]
    .concat();
    let z = |d: &[u8]| miniz_oxide::deflate::compress_to_vec_zlib(d, 6)[1..].to_vec();
    let group = |rel: i16, refer: bool, payload: &[u8]| {
        let mut g = el(
            crate::ID_BLOCK,
            &[&[0x81][..], &rel.to_be_bytes(), &[0], &z(payload)].concat(),
        );
        if refer {
            g.extend(el(crate::ID_REFERENCE_BLOCK, &[0xFF]));
        }
        g.extend(el(
            crate::ID_BLOCK_ADDITIONS,
            &el(
                crate::ID_BLOCK_MORE,
                &[
                    uel(crate::ID_BLOCK_ADD_ID, 1),
                    el(crate::ID_BLOCK_ADDITIONAL, b"alpha"),
                ]
                .concat(),
            ),
        ));
        el(crate::ID_BLOCK_GROUP, &g)
    };
    let cluster = [
        group(0, false, b"key frame"),
        group(40, true, b"inter frame"),
        simple_block(80, 0x00, &z(b"plain")),
    ]
    .concat();
    file(&entry(1, "V_VP9", &[], &extra), &cluster)
}

#[test]
fn kitchen_sink_demuxes() {
    let (s, p) = demux(kitchen_sink());
    assert!(s[0].alpha_mode);
    assert_eq!(s[0].color.matrix, 9);
    assert_eq!(s[0].content_light.unwrap().max_cll, 1000);
    assert_eq!(s[0].mastering_display.unwrap().max_luminance, 2_500);
    let got: Vec<_> = p
        .iter()
        .map(|p| (p.data.clone(), p.pts, p.flags.keyframe, p.additions.len()))
        .collect();
    assert_eq!(
        got,
        [
            (b"key frame".to_vec(), Some(-1), true, 1),
            (b"inter frame".to_vec(), Some(39), false, 1),
            (b"plain".to_vec(), Some(79), false, 0),
        ]
    );
}

#[test]
fn malformed_colour_additions_and_encodings_never_panic() {
    struct Lcg(u64);
    impl Lcg {
        fn next(&mut self) -> u64 {
            self.0 = self
                .0
                .wrapping_mul(6_364_136_223_846_793_005)
                .wrapping_add(1_442_695_040_888_963_407);
            self.0 >> 33
        }
    }
    let base = kitchen_sink();
    // Every truncation.
    for cut in 0..=base.len() {
        demux_lossy(base[..cut].to_vec());
    }
    // Byte flips and size-field corruption anywhere.
    let mut r = Lcg(110);
    for _ in 0..3000 {
        let mut f = base.clone();
        for _ in 0..1 + r.next() % 4 {
            let at = (r.next() as usize) % f.len();
            f[at] = r.next() as u8;
        }
        demux_lossy(f);
    }
    // Garbage bodies inside each new element.
    for i in 0..400usize {
        let junk: Vec<u8> = (0..i % 40).map(|_| r.next() as u8).collect();
        let vid = [el(crate::ID_COLOUR, &junk), el(crate::ID_ALPHA_MODE, &junk)].concat();
        let extra = [
            el(crate::ID_VIDEO, &vid),
            el(crate::ID_CONTENT_ENCODINGS, &junk),
        ]
        .concat();
        let group = el(
            crate::ID_BLOCK_GROUP,
            &[
                el(crate::ID_BLOCK, &[0x81, 0, 0, 0, 1]),
                el(crate::ID_BLOCK_ADDITIONS, &junk),
                el(crate::ID_REFERENCE_BLOCK, &junk),
            ]
            .concat(),
        );
        demux_lossy(file(&entry(1, "V_VP9", &[], &extra), &group));
        let nested = [
            el(crate::ID_MASTERING_METADATA, &junk),
            el(crate::ID_BLOCK_MORE, &junk),
            el(0x6240, &junk),
        ];
        for n in &nested {
            let vid = el(crate::ID_COLOUR, n);
            let extra = [
                el(crate::ID_VIDEO, &vid),
                el(crate::ID_CONTENT_ENCODINGS, n),
            ]
            .concat();
            let group = el(
                crate::ID_BLOCK_GROUP,
                &[
                    el(crate::ID_BLOCK, &[0x81, 0, 0, 0, 1]),
                    el(crate::ID_BLOCK_ADDITIONS, n),
                ]
                .concat(),
            );
            demux_lossy(file(&entry(1, "V_VP9", &[], &extra), &group));
        }
    }
}

// ---- copy-only codecs ----------------------------------------------------------------

#[test]
fn copy_only_codecs_map_both_ways() {
    let cookie: Vec<u8> = (0..24).collect();
    let mut alac = Stream::new(0, CodecId::Alac);
    alac.sample_rate = 44_100;
    alac.channels = 2;
    alac.extradata = cookie.clone();
    for codec in [CodecId::Ac3, CodecId::Eac3] {
        let mut a = Stream::new(0, codec);
        a.sample_rate = 48_000;
        a.channels = 6;
        let (s, p) = demux(mux(false, false, &[a.clone()], &[pkt(0, 0, true, b"sync")]));
        assert_eq!((s[0].codec_id, s[0].channels), (codec, 6));
        assert_eq!(p[0].data, b"sync");
        let err = try_mux(true, false, &[a], &[]).unwrap_err();
        assert!(err.to_string().contains("webm"), "{err}");
    }
    let (s, _) = demux(mux(false, true, &[alac.clone()], &[pkt(0, 0, true, b"a")]));
    assert_eq!((s[0].codec_id, &s[0].extradata), (CodecId::Alac, &cookie));
    // FFmpeg-style extradata: the 12-byte QuickTime atom header comes off.
    let mut atom = 36u32.to_be_bytes().to_vec();
    atom.extend_from_slice(b"alac");
    atom.extend_from_slice(&[0; 4]);
    atom.extend_from_slice(&cookie);
    alac.extradata = atom;
    let (s, _) = demux(mux(false, false, &[alac.clone()], &[pkt(0, 0, true, b"a")]));
    assert_eq!(s[0].extradata, cookie);
    // No cookie: the in-memory muxer accepts the header (MuxCaps) and refuses
    // the file; the streaming one refuses as it writes the header.
    alac.extradata.clear();
    for streamed in [false, true] {
        let err = try_mux(false, streamed, &[alac.clone()], &[]).unwrap_err();
        assert!(err.to_string().contains("magic cookie"), "{err}");
    }
    let mut m = MkvMuxer::new(Box::new(Shared::default()), false);
    assert!(m.write_header(&[alac]).is_ok());
    // VP8 in WebM.
    let (s, _) = demux(mux(
        true,
        true,
        &[video(CodecId::Vp8)],
        &[pkt(0, 0, true, b"v")],
    ));
    assert_eq!(s[0].codec_id, CodecId::Vp8);
}

#[test]
fn audio_time_base_with_codec_delay_keeps_its_rounding() {
    // Samples in, ms on disk: the delay is added after the rescale, so a
    // remux at 1/48000 lands on the same ms ticks as without a delay, +7.
    let mut a = opus(opus_head(312));
    a.time_base = Rational::new(1, 48_000);
    let packets: Vec<Packet> = (0..4).map(|i| pkt(0, i * 960, true, &[1; 4])).collect();
    let (_, p) = demux(mux(false, false, &[a], &packets));
    assert_eq!(
        p.iter().map(|p| p.pts).collect::<Vec<_>>(),
        [Some(0), Some(20), Some(40), Some(60)]
    );
}
