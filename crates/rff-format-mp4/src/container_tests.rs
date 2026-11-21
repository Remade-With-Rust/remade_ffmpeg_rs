//! Round trips for the QuickTime / ISO features FilmCraft needs (#110):
//! colour, aspect, HDR, timecode, ProRes / DNx, PCM in MOV, VP9 / FLAC /
//! ALAC / AC-3 in MP4, and reading fragmented files.

use std::io::{Cursor, Seek, SeekFrom, Write};
use std::sync::{Arc, Mutex};

use rff_core::{
    CleanAperture, ColorInfo, ColorRange, ContentLightLevel, MasteringDisplay, SampleFormat,
    Timecode,
};

use super::*;

/// A seekable in-memory sink the test can read back after the muxer is done.
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

/// Mux with both muxers of `flavor`; they must write files that demux the same.
fn mux_both(flavor: Flavor, streams: &[Stream], packets: &[Packet]) -> [Vec<u8>; 2] {
    let buffered = Shared::default();
    let mut m = Mp4Muxer::with_flavor(Box::new(buffered.clone()), flavor);
    m.write_header(streams).unwrap();
    for p in packets {
        m.write_packet(p).unwrap();
    }
    m.write_trailer().unwrap();
    drop(m);

    let streamed = Shared::default();
    let mut m = Mp4StreamMuxer::with_flavor(Box::new(streamed.clone()), flavor);
    m.write_header(streams).unwrap();
    for p in packets {
        m.write_packet(p).unwrap();
    }
    m.write_trailer().unwrap();
    drop(m);

    let take = |s: Shared| s.0.lock().unwrap().get_ref().clone();
    [take(buffered), take(streamed)]
}

/// Demux through the buffered and the seekable path; both must agree.
fn demux(file: &[u8]) -> (Vec<Stream>, Vec<Packet>) {
    let run = |mut d: Mp4Demuxer| {
        let streams = d.read_header().unwrap();
        let mut packets = Vec::new();
        loop {
            match d.read_packet() {
                Ok(p) => packets.push(p),
                Err(Error::Eof) => break,
                Err(e) => panic!("{e}"),
            }
        }
        (streams, packets)
    };
    let a = run(Mp4Demuxer::new(Box::new(Cursor::new(file.to_vec()))));
    let b = run(Mp4Demuxer::new_seekable(Box::new(Cursor::new(
        file.to_vec(),
    ))));
    assert_eq!(a.1.len(), b.1.len(), "buffered and seekable packet counts");
    for (x, y) in a.1.iter().zip(&b.1) {
        assert_eq!(
            (x.stream_index, x.pts, x.dts, &x.data),
            (y.stream_index, y.pts, y.dts, &y.data)
        );
    }
    assert_eq!(
        format!("{:?}", a.0),
        format!("{:?}", b.0),
        "buffered and seekable streams"
    );
    a
}

fn packets_of(packets: &[Packet], stream: usize) -> Vec<&Packet> {
    packets
        .iter()
        .filter(|p| p.stream_index == stream)
        .collect()
}

/// A fake ProRes frame: size, `icpf`, a header whose chroma format is `chroma`.
fn prores_frame(n: u8, chroma: u8) -> Vec<u8> {
    let mut f = vec![0u8; 64];
    f[..4].copy_from_slice(&64u32.to_be_bytes());
    f[4..8].copy_from_slice(b"icpf");
    f[20] = chroma << 6;
    f[40] = n;
    f
}

fn pcm_packets(stream: usize, frames: usize, frame_bytes: usize, count: usize) -> Vec<Packet> {
    (0..count)
        .map(|k| {
            let data: Vec<u8> = (0..frames * frame_bytes)
                .map(|i| (i * 7 + k * 13) as u8)
                .collect();
            let mut p = Packet::from_data(stream, data);
            p.pts = Some((k * frames) as i64);
            p.dts = p.pts;
            p.flags.keyframe = true;
            p
        })
        .collect()
}

fn prores_stream() -> Stream {
    let mut v = Stream::new(0, CodecId::ProRes);
    v.width = 1920;
    v.height = 1080;
    v.time_base = Rational::new(1, 24);
    v.codec_tag = Some(*b"apch");
    v.color = ColorInfo {
        primaries: 1,
        transfer: 1,
        matrix: 1,
    };
    v.color_range = ColorRange::Limited;
    v.pixel_aspect = Some((4, 3));
    v.clean_aperture = Some(CleanAperture {
        width: (1888, 1),
        height: (1062, 1),
        horiz_offset: (-2, 1),
        vert_offset: (0, 1),
    });
    v.mastering_display = Some(MasteringDisplay {
        red: (35_400, 14_600),
        green: (8_500, 39_850),
        blue: (6_550, 2_300),
        white_point: (15_635, 16_450),
        max_luminance: 10_000_000,
        min_luminance: 50,
    });
    v.content_light = Some(ContentLightLevel {
        max_cll: 1000,
        max_fall: 400,
    });
    v.timecode = Timecode::parse("01:00:00:00", 24);
    v
}

fn pcm_stream(index: usize, format: SampleFormat, channels: u16, tag: Option<[u8; 4]>) -> Stream {
    let mut a = Stream::new(index, CodecId::Pcm);
    a.sample_rate = 48_000;
    a.channels = channels;
    a.sample_format = Some(format);
    a.time_base = Rational::new(1, 48_000);
    a.codec_tag = tag;
    a
}

fn video_packets(frames: impl Fn(u8) -> Vec<u8>, n: u8) -> Vec<Packet> {
    (0..n)
        .map(|i| {
            let mut p = Packet::from_data(0, frames(i));
            p.pts = Some(i64::from(i));
            p.dts = p.pts;
            p.flags.keyframe = true;
            p
        })
        .collect()
}

/// Interleave video and audio packets by time (as an encoder pipeline would).
fn interleave(video: Vec<Packet>, audio: Vec<Packet>) -> Vec<Packet> {
    let mut all: Vec<Packet> = video.into_iter().chain(audio).collect();
    all.sort_by(|a, b| {
        let t = |p: &Packet| {
            p.pts.unwrap_or(0) as f64
                * if p.stream_index == 0 {
                    1.0 / 24.0
                } else {
                    1.0 / 48_000.0
                }
        };
        t(a).partial_cmp(&t(b)).unwrap()
    });
    all
}

#[test]
fn mov_carries_prores_pcm_timecode_and_colour() {
    let streams = [
        prores_stream(),
        pcm_stream(1, SampleFormat::S24, 2, Some(*b"in24")),
    ];
    let video = video_packets(|i| prores_frame(i, 2), 6);
    let audio = pcm_packets(1, 2000, 6, 6);
    let packets = interleave(video.clone(), audio.clone());
    for file in mux_both(Flavor::Mov, &streams, &packets) {
        assert_eq!(&file[4..12], b"ftypqt  ", "a QuickTime file");
        // in24 is big-endian on disk: the first frame is stored byte-reversed.
        let first = &audio[0].data[..6];
        let swapped = [first[2], first[1], first[0], first[5], first[4], first[3]];
        assert!(
            file.windows(6).any(|w| w == swapped),
            "in24 samples stored big-endian"
        );

        let (out, got) = demux(&file);
        assert_eq!(
            out.len(),
            2,
            "the timecode track is not a stream of its own"
        );
        let v = &out[0];
        assert_eq!((v.codec_id, v.codec_tag), (CodecId::ProRes, Some(*b"apch")));
        assert_eq!((v.width, v.height), (1920, 1080));
        assert_eq!(v.color, streams[0].color);
        // A known range is written as `nclx` (QuickTime's `nclc` has no
        // range flag), so limited comes back as limited.
        assert_eq!(v.color_range, ColorRange::Limited);
        assert_eq!(v.pixel_aspect, streams[0].pixel_aspect);
        assert_eq!(v.clean_aperture, streams[0].clean_aperture);
        assert_eq!(v.mastering_display, streams[0].mastering_display);
        assert_eq!(v.content_light, streams[0].content_light);
        let tc = v.timecode.as_ref().expect("start timecode");
        assert_eq!((tc.start_frame, tc.frames_per_second), (86_400, 24));
        assert_eq!(tc.to_string(), "01:00:00:00");

        let a = &out[1];
        assert_eq!((a.codec_id, a.codec_tag), (CodecId::Pcm, Some(*b"in24")));
        assert_eq!(a.sample_format, Some(SampleFormat::S24));
        assert_eq!((a.channels, a.sample_rate), (2, 48_000));

        let gv = packets_of(&got, 0);
        assert_eq!(gv.len(), 6);
        for (g, w) in gv.iter().zip(&video) {
            assert_eq!((&g.data, g.pts), (&w.data, w.pts));
        }
        let ga = packets_of(&got, 1);
        assert_eq!(ga.len(), 6, "one PCM packet per chunk");
        for (g, w) in ga.iter().zip(&audio) {
            assert_eq!(g.data, w.data, "PCM back in little-endian");
            assert_eq!((g.pts, g.duration), (w.pts, 2000));
        }
    }
}

#[test]
fn untagged_prores_takes_its_profile_from_the_frame_and_pcm_its_default_entry() {
    let mut v = prores_stream();
    v.codec_tag = None;
    v.timecode = None;
    let streams = [v, pcm_stream(1, SampleFormat::S16, 2, None)];
    let packets = interleave(
        video_packets(|i| prores_frame(i, 3), 2),
        pcm_packets(1, 100, 4, 2),
    );
    for file in mux_both(Flavor::Mov, &streams, &packets) {
        let (out, _) = demux(&file);
        assert_eq!(
            out[0].codec_tag,
            Some(*b"ap4h"),
            "4:4:4 chroma is ProRes 4444"
        );
        assert_eq!(
            out[1].codec_tag,
            Some(*b"sowt"),
            "16-bit PCM defaults to sowt"
        );
        assert!(out[0].timecode.is_none());
    }
    // 24-bit without a tag is lpcm v2, little-endian, and comes back as such.
    let streams = [pcm_stream(0, SampleFormat::S24, 6, None)];
    let packets = pcm_packets(0, 480, 18, 3);
    for file in mux_both(Flavor::Mov, &streams, &packets) {
        let (out, got) = demux(&file);
        assert_eq!(out[0].codec_tag, Some(*b"lpcm"));
        assert_eq!(
            (out[0].sample_format, out[0].channels),
            (Some(SampleFormat::S24), 6)
        );
        assert_eq!(
            got.iter().map(|p| &p.data).collect::<Vec<_>>(),
            packets.iter().map(|p| &p.data).collect::<Vec<_>>()
        );
    }
}

#[test]
fn dnxhd_and_big_endian_pcm_round_trip_in_mov() {
    let mut v = Stream::new(0, CodecId::DnxHd);
    v.width = 1280;
    v.height = 720;
    v.time_base = Rational::new(1, 24);
    v.codec_tag = Some(*b"AVdh");
    v.color_range = ColorRange::Full;
    let streams = [v, pcm_stream(1, SampleFormat::S16, 2, Some(*b"twos"))];
    let packets = interleave(
        video_packets(|i| vec![i; 100], 3),
        pcm_packets(1, 1000, 4, 3),
    );
    for file in mux_both(Flavor::Mov, &streams, &packets) {
        let (out, got) = demux(&file);
        assert_eq!(
            (out[0].codec_id, out[0].codec_tag),
            (CodecId::DnxHd, Some(*b"AVdh"))
        );
        // Full range survives a MOV remux: written as `nclx`.
        assert_eq!(out[0].color_range, ColorRange::Full);
        assert_eq!(out[1].codec_tag, Some(*b"twos"));
        let ga = packets_of(&got, 1);
        assert_eq!(ga[0].data, packets_of(&packets, 1)[0].data);
    }
}

#[test]
fn pcm_whose_stsz_counts_one_byte_per_frame_still_reads_whole_frames() {
    // QuickTime writers often put 1 in `stsz` for uncompressed audio; the frame
    // size comes from the sound description instead.
    let streams = [pcm_stream(0, SampleFormat::S16, 2, Some(*b"twos"))];
    let packets = pcm_packets(0, 500, 4, 4);
    let [mut file, _] = mux_both(Flavor::Mov, &streams, &packets);
    let at = file.windows(4).position(|w| w == b"stsz").unwrap();
    // stsz: type(4) version/flags(4) sample_size(4) — the frame size, 4.
    assert_eq!(be32(&file, at + 8), 4);
    file[at + 8..at + 12].copy_from_slice(&1u32.to_be_bytes());
    let (_, got) = demux(&file);
    assert_eq!(got.len(), 4);
    for (g, w) in got.iter().zip(&packets) {
        assert_eq!(g.data, w.data);
    }
}

#[test]
fn an_unreferenced_timecode_track_times_the_first_video_track() {
    let mut v = prores_stream();
    let mut tc = Timecode::parse("00:59:59;28", 30).unwrap();
    tc.reel_name = Some("A001".into());
    v.timecode = Some(tc.clone());
    v.time_base = Rational::new(1001, 30_000);
    let packets = video_packets(|i| prores_frame(i, 2), 3);
    let [mut file, _] = mux_both(Flavor::Mov, &[v], &packets);
    let (out, _) = demux(&file);
    assert_eq!(
        out[0].timecode.as_ref(),
        Some(&Timecode {
            timescale: 30_000,
            frame_duration: 1001,
            ..tc.clone()
        })
    );
    assert_eq!(out[0].timecode.as_ref().unwrap().to_string(), "00:59:59;28");
    // Break the `tref`: the timecode still lands on the (only) video track.
    let at = file.windows(4).position(|w| w == b"tref").unwrap();
    file[at..at + 4].copy_from_slice(b"free");
    let (out, _) = demux(&file);
    assert_eq!(
        out[0].timecode.as_ref().map(|t| t.start_frame),
        Some(tc.start_frame)
    );
}

/// A VP9 profile 0 keyframe header (8-bit 4:2:0, BT.601, limited range).
fn vp9_keyframe(n: u8) -> Vec<u8> {
    let bits: String = "10 0 0 0 0 1 0 010010011000001101000010 001 0"
        .split_whitespace()
        .collect();
    let mut bytes = vec![0u8; 16];
    for (i, c) in bits.chars().enumerate() {
        if c == '1' {
            bytes[i / 8] |= 0x80 >> (i % 8);
        }
    }
    bytes[15] = n;
    bytes
}

/// An AC-3 frame header: 48 kHz, bit_rate_code 14, 3/2 + LFE.
fn ac3_frame(n: u8) -> Vec<u8> {
    let mut f = vec![0x0B, 0x77, 0, 0, 0b0001_1100, 0b0100_0000, 0b1110_0001, 0];
    f.resize(64, n);
    f
}

#[test]
fn mp4_carries_vp9_flac_alac_ac3() {
    let mut vp9 = Stream::new(0, CodecId::Vp9);
    vp9.width = 1920;
    vp9.height = 1080;
    vp9.time_base = Rational::new(1, 30);
    vp9.color = ColorInfo {
        primaries: 9,
        transfer: 16,
        matrix: 9,
    };
    vp9.color_range = ColorRange::Full;
    let audio = |i: usize, codec: CodecId, extradata: Vec<u8>| {
        let mut a = Stream::new(i, codec);
        a.sample_rate = 48_000;
        a.channels = 2;
        a.time_base = Rational::new(1, 48_000);
        a.extradata = extradata;
        a
    };
    let streaminfo: Vec<u8> = (0..34).collect();
    let cookie: Vec<u8> = (100..124).collect();
    let streams = [
        vp9,
        audio(1, CodecId::Flac, streaminfo.clone()),
        audio(2, CodecId::Alac, cookie.clone()),
        audio(3, CodecId::Ac3, Vec::new()),
    ];
    let mut packets = Vec::new();
    for k in 0..3u8 {
        let mut p = Packet::from_data(0, vp9_keyframe(k));
        p.pts = Some(i64::from(k));
        p.flags.keyframe = true;
        packets.push(p);
        for (s, data) in [(1, vec![0xFF, 0xF8, k]), (2, vec![k; 9]), (3, ac3_frame(k))] {
            let mut p = Packet::from_data(s, data);
            p.pts = Some(i64::from(k) * 1600);
            p.flags.keyframe = true;
            packets.push(p);
        }
    }
    for file in mux_both(Flavor::Mp4, &streams, &packets) {
        assert_eq!(&file[4..12], b"ftypisom");
        let (out, got) = demux(&file);
        assert_eq!(
            out.iter().map(|s| s.codec_id).collect::<Vec<_>>(),
            [CodecId::Vp9, CodecId::Flac, CodecId::Alac, CodecId::Ac3,]
        );
        // nclx colour (written) agrees with the vpcC built from the keyframe.
        assert_eq!(
            (out[0].color, out[0].color_range),
            (streams[0].color, ColorRange::Full)
        );
        assert_eq!(out[1].extradata, streaminfo, "dfLa carries the STREAMINFO");
        assert_eq!(out[2].extradata, cookie, "the ALAC magic cookie");
        assert_eq!(
            out[3].extradata.len(),
            3,
            "dac3 derived from the first frame"
        );
        for s in 0..4 {
            let want: Vec<&Vec<u8>> = packets_of(&packets, s).iter().map(|p| &p.data).collect();
            let have: Vec<&Vec<u8>> = packets_of(&got, s).iter().map(|p| &p.data).collect();
            assert_eq!(have, want, "stream {s} payloads");
        }
    }
}

#[test]
fn the_mp4_muxer_refuses_quicktime_only_codecs_and_points_at_mov() {
    let mut m = Mp4Muxer::with_flavor(Box::new(Vec::new()), Flavor::Mp4);
    let err = m.write_header(&[prores_stream()]).unwrap_err().to_string();
    assert!(err.contains("prores") && err.contains(".mov"), "{err}");
    // PCM too: most MP4 players cannot play it, so `in.wav -> out.mp4` asks
    // for AAC (or `.mov`) instead of stream-copying.
    let mut m = Mp4Muxer::with_flavor(Box::new(Vec::new()), Flavor::Mp4);
    let pcm = pcm_stream(0, SampleFormat::F32, 2, None);
    let err = m.write_header(&[pcm]).unwrap_err().to_string();
    assert!(
        err.contains("pcm") && err.contains("-c:a aac") && err.contains(".mov"),
        "{err}"
    );
}

#[test]
fn float_pcm_goes_into_mov_as_lpcm() {
    let streams = [pcm_stream(0, SampleFormat::F32, 2, None)];
    let packets = pcm_packets(0, 1600, 8, 3);
    for file in mux_both(Flavor::Mov, &streams, &packets) {
        let (out, got) = demux(&file);
        assert_eq!(
            (out[0].codec_tag, out[0].sample_format),
            (Some(*b"lpcm"), Some(SampleFormat::F32))
        );
        let have: Vec<&Vec<u8>> = got.iter().map(|p| &p.data).collect();
        let want: Vec<&Vec<u8>> = packets.iter().map(|p| &p.data).collect();
        assert_eq!(have, want);
    }
}

#[test]
fn registry_routes_mov_to_the_quicktime_muxer() {
    let mut registry = FormatRegistry::new();
    register(&mut registry);
    assert_eq!(registry.by_extension("mov").map(|f| f.name), Some("mov"));
    assert_eq!(registry.by_extension("mp4").map(|f| f.name), Some("mp4"));
    assert_eq!(
        registry
            .probe(b"\0\0\0\x14ftypqt  \0\0\x02\0qt  ")
            .map(|f| f.name),
        Some("mov")
    );
    assert_eq!(
        registry
            .probe(b"\0\0\0\x14ftypisom\0\0\x02\0isom")
            .map(|f| f.name),
        Some("mp4")
    );
    let sink = Shared::default();
    let mut m = registry
        .open_muxer_seekable("mov", Box::new(sink.clone()))
        .unwrap();
    m.write_header(&[pcm_stream(0, SampleFormat::S16, 1, None)])
        .unwrap();
    m.write_trailer().unwrap();
    drop(m);
    assert_eq!(&sink.0.lock().unwrap().get_ref()[4..12], b"ftypqt  ");
}

#[test]
fn fragmented_files_are_read_from_their_moofs() {
    let mut s = Stream::new(0, CodecId::Avif);
    s.width = 64;
    s.height = 64;
    let track = TrackOut::new(&s, *b"av01");
    let mut file = fmp4::init_segment(&track, 1, 90_000, 0);
    let frame = |i: u8, key: bool| fmp4::FragSample {
        data: vec![i; 10 + i as usize],
        keyframe: key,
        duration: 3000,
    };
    let first: Vec<_> = (0..3).map(|i| frame(i, i == 0)).collect();
    let second: Vec<_> = (3..6).map(|i| frame(i, i == 3)).collect();
    file.extend(fmp4::media_segment(1, 1, 0, &first));
    file.extend(fmp4::media_segment_with_offsets(
        2,
        1,
        9000,
        &second,
        Some(&[3000, -3000, 0]),
    ));
    let (streams, got) = demux(&file);
    assert_eq!(streams[0].nb_frames, Some(6));
    assert_eq!(got.len(), 6);
    for (i, p) in got.iter().enumerate() {
        assert_eq!(p.data, vec![i as u8; 10 + i]);
        assert_eq!(p.dts, Some(3000 * i as i64));
        assert_eq!(p.duration, 3000);
        assert_eq!(p.flags.keyframe, i % 3 == 0, "frame {i}");
    }
    let pts: Vec<i64> = got.iter().map(|p| p.pts.unwrap()).collect();
    assert_eq!(
        pts,
        [0, 3000, 6000, 12_000, 9000, 15_000],
        "trun composition offsets"
    );
}

#[test]
fn malformed_new_boxes_do_not_panic() {
    // Truncate every interesting box of a feature-rich file at every length.
    let streams = [
        prores_stream(),
        pcm_stream(1, SampleFormat::S24, 2, Some(*b"in24")),
    ];
    let packets = interleave(
        video_packets(|i| prores_frame(i, 2), 2),
        pcm_packets(1, 100, 6, 2),
    );
    let [file, _] = mux_both(Flavor::Mov, &streams, &packets);
    for tag in [
        &b"colr"[..],
        b"clap",
        b"mdcv",
        b"clli",
        b"pasp",
        b"tmcd",
        b"in24",
        b"tref",
    ] {
        let at = file.windows(4).position(|w| w == tag).unwrap();
        for cut in 0..48 {
            let mut f = file.clone();
            // Shrink the box's declared size: it now ends inside its own fields.
            let size_at = at - 4;
            let size = be32(&f, size_at);
            let new = (size as usize).saturating_sub(cut).max(8) as u32;
            f[size_at..size_at + 4].copy_from_slice(&new.to_be_bytes());
            let mut d = Mp4Demuxer::new(Box::new(Cursor::new(f)));
            if d.read_header().is_ok() {
                while !matches!(d.read_packet(), Err(Error::Eof) | Err(_)) {}
            }
        }
    }
}

// ---------------------------------------------------------------------------
// Random access (#114): SampleIndex, raw samples, extradata, edit lists.
// ---------------------------------------------------------------------------

/// Every packet of `file` in order, plus a fresh demuxer (header read), for
/// the buffered and the seekable source.
fn open_both(file: &[u8]) -> Vec<(Mp4Demuxer, Vec<Packet>)> {
    let mk = |seekable: bool| {
        let mut d = if seekable {
            Mp4Demuxer::new_seekable(Box::new(Cursor::new(file.to_vec())))
        } else {
            Mp4Demuxer::new(Box::new(Cursor::new(file.to_vec())))
        };
        d.read_header().unwrap();
        d
    };
    [false, true]
        .into_iter()
        .map(|seekable| {
            let mut d = mk(seekable);
            let mut packets = Vec::new();
            while let Ok(p) = d.read_packet() {
                packets.push(p);
            }
            (mk(seekable), packets)
        })
        .collect()
}

/// The stored form of a packet's first bytes: PCM `in24` is big-endian on
/// disk, ProRes is stored as is.
fn stored_head(data: &[u8], stream: usize) -> Vec<u8> {
    let mut d = data[..6.min(data.len())].to_vec();
    if stream == 1 {
        d.chunks_exact_mut(3).for_each(<[u8]>::reverse);
    }
    d
}

#[test]
fn the_sample_index_agrees_with_read_packet() {
    let streams = [
        prores_stream(),
        pcm_stream(1, SampleFormat::S24, 2, Some(*b"in24")),
    ];
    let packets = interleave(
        video_packets(|i| prores_frame(i, 2), 8),
        pcm_packets(1, 1500, 6, 8),
    );
    for file in mux_both(Flavor::Mov, &streams, &packets) {
        for (mut d, got) in open_both(&file) {
            let idx = d.sample_index().expect("an MP4 has an index");
            for s in 0..2 {
                let want: Vec<&Packet> = got.iter().filter(|p| p.stream_index == s).collect();
                assert_eq!(idx.sample_count(s), want.len() as u64);
                for (i, p) in want.iter().enumerate() {
                    let info = idx.sample_info(s, i as u64).unwrap();
                    assert_eq!(
                        (Some(info.dts), Some(info.pts), info.duration, info.sync),
                        (p.dts, p.pts, p.duration, p.flags.keyframe)
                    );
                    let again = idx.read_sample(s, i as u64).unwrap();
                    assert_eq!(again.data, p.data, "stream {s} sample {i}");
                    let head = stored_head(&again.data, s);
                    assert_eq!(&file[info.offset as usize..][..head.len()], &head[..]);
                }
                assert!(idx.sample_info(s, want.len() as u64).is_none());
            }
            assert_eq!(idx.sample_count(9), 0);
        }
    }
}

#[test]
fn reading_through_the_index_does_not_move_read_packet() {
    let streams = [prores_stream()];
    let packets = video_packets(|i| prores_frame(i, 2), 6);
    let [file, _] = mux_both(Flavor::Mov, &streams, &packets);
    let mut d = Mp4Demuxer::new_seekable(Box::new(Cursor::new(file)));
    d.read_header().unwrap();
    assert_eq!(d.read_packet().unwrap().data, packets[0].data);
    assert_eq!(
        d.sample_index().unwrap().read_sample(0, 5).unwrap().data,
        packets[5].data
    );
    assert_eq!(
        d.read_packet().unwrap().data,
        packets[1].data,
        "the read position is kept"
    );
}

#[test]
fn sync_at_or_before_finds_the_keyframe_to_start_from() {
    let mut v = Stream::new(0, CodecId::Avif);
    v.width = 8;
    v.height = 8;
    v.time_base = Rational::new(1, 25);
    let keys = [true, false, false, true, false, false, false, true, false];
    let packets: Vec<Packet> = keys
        .iter()
        .enumerate()
        .map(|(i, &k)| {
            let mut data = if i == 0 {
                vec![0x0A, 0x03, 0, 0, 0]
            } else {
                Vec::new()
            };
            data.push(i as u8);
            let mut p = Packet::from_data(0, data);
            p.pts = Some(i as i64);
            p.dts = p.pts;
            p.flags.keyframe = k;
            p
        })
        .collect();
    let [file, _] = mux_both(Flavor::Mp4, &[v], &packets);
    let mut d = Mp4Demuxer::new_seekable(Box::new(Cursor::new(file)));
    d.read_header().unwrap();
    let idx = d.sample_index().unwrap();
    let at = |pts| idx.sync_at_or_before(0, pts);
    assert_eq!(
        [at(0), at(2), at(3), at(6), at(7), at(100)],
        [Some(0), Some(0), Some(3), Some(3), Some(7), Some(7)]
    );
    assert_eq!(at(-1), None, "nothing is shown that early");
}

/// A minimal H.264 keyframe and P frame, Annex-B, with SPS/PPS in band.
fn h264_frames() -> Vec<Packet> {
    let sps: &[u8] = &[0x67, 0x42, 0x00, 0x1E, 0xAB];
    let pps: &[u8] = &[0x68, 0xCE, 0x38, 0x80];
    let slice: &[u8] = &[0x65, 0x88, 0x84, 0x21];
    let mut key = Vec::new();
    for nal in [sps, pps, slice] {
        key.extend_from_slice(&[0, 0, 0, 1]);
        key.extend_from_slice(nal);
    }
    let p = vec![0, 0, 0, 1, 0x41, 0x9A, 0x00, 0x10];
    [key, p]
        .into_iter()
        .enumerate()
        .map(|(i, data)| {
            let mut pk = Packet::from_data(0, data);
            pk.pts = Some(i as i64);
            pk.dts = pk.pts;
            pk.flags.keyframe = i == 0;
            pk
        })
        .collect()
}

#[test]
fn h264_extradata_is_the_avcc_record_and_raw_samples_stay_length_prefixed() {
    let mut v = Stream::new(0, CodecId::H264);
    v.width = 16;
    v.height = 16;
    v.time_base = Rational::new(1, 25);
    let [file, _] = mux_both(Flavor::Mp4, &[v], &h264_frames());
    let mut d = Mp4Demuxer::new(Box::new(Cursor::new(file.clone())));
    let streams = d.read_header().unwrap();
    let rec = &streams[0].extradata;
    assert_eq!(rec[0], 1, "an AVCDecoderConfigurationRecord");
    assert!(
        rec.windows(5).any(|w| w == [0x67, 0x42, 0x00, 0x1E, 0xAB]),
        "carries the SPS"
    );
    // Default: Annex-B with the parameter sets on the keyframe.
    let key = d.read_packet().unwrap();
    assert_eq!(&key.data[..5], &[0, 0, 0, 1, 0x67]);
    // Raw: the stored 4-byte length prefixes, slice only.
    let mut d = Mp4Demuxer::new(Box::new(Cursor::new(file)));
    d.read_header().unwrap();
    assert!(d.set_raw_samples(true));
    assert_eq!(
        d.read_packet().unwrap().data,
        [0, 0, 0, 4, 0x65, 0x88, 0x84, 0x21]
    );
    assert_eq!(
        d.read_packet().unwrap().data,
        [0, 0, 0, 4, 0x41, 0x9A, 0x00, 0x10]
    );
    assert_eq!(
        d.sample_index().unwrap().read_sample(0, 0).unwrap().data,
        [0, 0, 0, 4, 0x65, 0x88, 0x84, 0x21]
    );
}

#[test]
fn raw_pcm_keeps_the_stored_byte_order() {
    let streams = [pcm_stream(0, SampleFormat::S16, 2, Some(*b"twos"))];
    let packets = pcm_packets(0, 100, 4, 2);
    let [file, _] = mux_both(Flavor::Mov, &streams, &packets);
    let mut d = Mp4Demuxer::new(Box::new(Cursor::new(file)));
    d.read_header().unwrap();
    d.set_raw_samples(true);
    let got = d.read_packet().unwrap().data;
    let mut want = packets[0].data.clone();
    want.chunks_exact_mut(2).for_each(<[u8]>::reverse);
    assert_eq!(got, want, "twos stays big-endian");
}

#[test]
fn the_edit_list_is_exposed_with_the_movie_timescale() {
    let mut v = prores_stream();
    v.timecode = None;
    v.time_base = Rational::new(1, 1000);
    let mut packets = Vec::new();
    for i in 0..4i64 {
        let mut p = Packet::from_data(0, prores_frame(i as u8, 2));
        p.pts = Some(500 + 40 * i);
        p.dts = p.pts;
        p.flags.keyframe = true;
        packets.push(p);
    }
    let a = pcm_stream(1, SampleFormat::S16, 1, None);
    packets.extend(pcm_packets(1, 4800, 2, 3));
    let [file, _] = mux_both(Flavor::Mov, &[v, a], &packets);
    let mut d = Mp4Demuxer::new(Box::new(Cursor::new(file)));
    d.read_header().unwrap();
    let idx = d.sample_index().unwrap();
    assert_eq!(idx.movie_timescale(), Some(1000));
    let e = idx.edit_list(0);
    assert_eq!(e.len(), 2, "{e:?}");
    assert_eq!(
        (e[0].segment_duration, e[0].media_time),
        (500, -1),
        "the empty edit"
    );
    assert_eq!((e[1].media_time, e[1].media_rate), (0, 0x10000));
    assert!(idx.edit_list(1).is_empty());
}

#[test]
fn discarded_streams_are_skipped_but_stay_in_the_index() {
    let streams = [prores_stream(), pcm_stream(1, SampleFormat::S16, 2, None)];
    let packets = interleave(
        video_packets(|i| prores_frame(i, 2), 4),
        pcm_packets(1, 1000, 4, 4),
    );
    let [file, _] = mux_both(Flavor::Mov, &streams, &packets);
    let mut d = Mp4Demuxer::new_seekable(Box::new(Cursor::new(file)));
    d.read_header().unwrap();
    d.set_discard(&[false, true]);
    let mut got = Vec::new();
    while let Ok(p) = d.read_packet() {
        got.push(p.stream_index);
    }
    assert_eq!(got, [0, 0, 0, 0]);
    let idx = d.sample_index().unwrap();
    assert_eq!(idx.sample_count(1), 4);
    assert_eq!(
        idx.read_sample(1, 2).unwrap().data,
        packets_of(&packets, 1)[2].data
    );
}

// ---------------------------------------------------------------------------
// Mux coverage (#114): HEVC, MJPEG, MP3.
// ---------------------------------------------------------------------------

/// The VPS/SPS/PPS of the JCT-VC `IPRED_A_docomo_2` stream (832x480 Main),
/// NAL headers included; a slice NAL of type 19 (IDR) and 1 (TRAIL_R).
const HEVC_VPS: &[u8] = &[
    0x40, 0x01, 0x0c, 0x01, 0xff, 0xff, 0x01, 0x60, 0x00, 0x00, 0x03, 0x00, 0x00, 0x03, 0x00, 0x00,
    0x03, 0x00, 0x00, 0x03, 0x00, 0x99, 0xf0, 0x24,
];
const HEVC_SPS: &[u8] = &[
    0x42, 0x01, 0x01, 0x01, 0x60, 0x00, 0x00, 0x03, 0x00, 0x00, 0x03, 0x00, 0x00, 0x03, 0x00, 0x00,
    0x03, 0x00, 0x99, 0xa0, 0x06, 0x82, 0x01, 0xe1, 0xfe, 0x5f, 0x92, 0x46, 0xd9, 0x6c, 0x80,
];
const HEVC_PPS: &[u8] = &[0x44, 0x01, 0xc1, 0x90, 0x95, 0x81, 0x12];

fn annexb(nals: &[&[u8]]) -> Vec<u8> {
    let mut out = Vec::new();
    for n in nals {
        out.extend_from_slice(&[0, 0, 0, 1]);
        out.extend_from_slice(n);
    }
    out
}

#[test]
fn hevc_goes_into_mp4_and_mov_with_an_hvcc() {
    let idr: &[u8] = &[0x26, 0x01, 0xAF, 0x09, 0x40];
    let trail: &[u8] = &[0x02, 0x01, 0xD0, 0x11];
    for flavor in [Flavor::Mp4, Flavor::Mov] {
        let mut v = Stream::new(0, CodecId::Hevc);
        v.width = 832;
        v.height = 480;
        v.time_base = Rational::new(1, 25);
        let frames = [
            annexb(&[HEVC_VPS, HEVC_SPS, HEVC_PPS, idr]),
            annexb(&[trail]),
        ];
        let packets: Vec<Packet> = frames
            .iter()
            .enumerate()
            .map(|(i, f)| {
                let mut p = Packet::from_data(0, f.clone());
                p.pts = Some(i as i64);
                p.dts = p.pts;
                p.flags.keyframe = i == 0;
                p
            })
            .collect();
        for file in mux_both(flavor, &[v.clone()], &packets) {
            assert!(file.windows(4).any(|w| w == b"hvc1") && file.windows(4).any(|w| w == b"hvcC"));
            let (out, got) = demux(&file);
            assert_eq!(out[0].codec_id, CodecId::Hevc);
            assert_eq!(out[0].extradata[0], 1, "the hvcC record is the extradata");
            // Default demux: Annex-B with the parameter sets on the keyframe.
            assert_eq!(got[0].data, frames[0], "{flavor:?}");
            assert_eq!(got[1].data, frames[1]);
        }
        // A remux whose parameter sets are only in extradata (an hvcC).
        let [file, _] = mux_both(flavor, &[v.clone()], &packets);
        let (out, _) = demux(&file);
        let mut copy = v.clone();
        copy.extradata = out[0].extradata.clone();
        let bare: Vec<Packet> = packets
            .iter()
            .enumerate()
            .map(|(i, p)| Packet {
                data: if i == 0 {
                    annexb(&[idr])
                } else {
                    p.data.clone()
                },
                ..p.clone()
            })
            .collect();
        let [again, _] = mux_both(flavor, &[copy], &bare);
        let (_, got) = demux(&again);
        assert_eq!(got[0].data, frames[0], "hvcC rebuilt from extradata");
    }
}

#[test]
fn mjpeg_goes_into_mov_and_mp4_as_jpeg() {
    let mut v = Stream::new(0, CodecId::Jpeg);
    v.width = 64;
    v.height = 48;
    v.time_base = Rational::new(1, 30);
    let packets = video_packets(|i| vec![0xFF, 0xD8, 0xFF, 0xE0, i, 0xFF, 0xD9], 3);
    for flavor in [Flavor::Mov, Flavor::Mp4] {
        for file in mux_both(flavor, &[v.clone()], &packets) {
            let (out, got) = demux(&file);
            assert_eq!(
                (out[0].codec_id, out[0].codec_tag),
                (CodecId::Jpeg, Some(*b"jpeg"))
            );
            assert_eq!(
                got.iter().map(|p| &p.data).collect::<Vec<_>>(),
                packets.iter().map(|p| &p.data).collect::<Vec<_>>()
            );
        }
    }
}

#[test]
fn mp3_goes_into_mp4_as_mp4a_object_type_0x6b() {
    let mut a = Stream::new(0, CodecId::Mp3);
    a.sample_rate = 44_100;
    a.channels = 2;
    a.time_base = Rational::new(1, 44_100);
    let packets: Vec<Packet> = (0..4)
        .map(|i| {
            let mut p = Packet::from_data(0, vec![0xFF, 0xFB, 0x90, 0x64, i]);
            p.pts = Some(i64::from(i) * 1152);
            p.dts = p.pts;
            p.flags.keyframe = true;
            p
        })
        .collect();
    for file in mux_both(Flavor::Mp4, &[a], &packets) {
        let (out, got) = demux(&file);
        assert_eq!(
            out[0].codec_id,
            CodecId::Mp3,
            "mp4a + esds 0x6B reads back as MP3"
        );
        assert_eq!(got.len(), 4);
        assert_eq!(got[3].data, packets[3].data);
        assert_eq!(got[1].pts, Some(1152));
    }
}

// ---------------------------------------------------------------------------
// Faststart and caller edit lists (#114).
// ---------------------------------------------------------------------------

/// [`mux_both`] with `setup` applied to each muxer before its header.
fn mux_both_with(
    flavor: Flavor,
    streams: &[Stream],
    packets: &[Packet],
    setup: &dyn Fn(&mut dyn Muxer),
) -> [Vec<u8>; 2] {
    let run = |m: &mut dyn Muxer| {
        setup(m);
        m.write_header(streams).unwrap();
        for p in packets {
            m.write_packet(p).unwrap();
        }
        m.write_trailer().unwrap();
    };
    let buffered = Shared::default();
    run(&mut Mp4Muxer::with_flavor(
        Box::new(buffered.clone()),
        flavor,
    ));
    let streamed = Shared::default();
    run(&mut Mp4StreamMuxer::with_flavor(
        Box::new(streamed.clone()),
        flavor,
    ));
    let take = |s: Shared| s.0.lock().unwrap().get_ref().clone();
    [take(buffered), take(streamed)]
}

/// The top-level box types of `file`, in order.
fn top_level(file: &[u8]) -> Vec<[u8; 4]> {
    let mut at = 0usize;
    let mut out = Vec::new();
    while at + 8 <= file.len() {
        let mut size = u32::from_be_bytes(file[at..at + 4].try_into().unwrap()) as u64;
        let kind: [u8; 4] = file[at + 4..at + 8].try_into().unwrap();
        if size == 1 {
            size = u64::from_be_bytes(file[at + 8..at + 16].try_into().unwrap());
        }
        out.push(kind);
        at += size as usize;
    }
    out
}

#[test]
fn faststart_puts_moov_before_mdat_and_reads_back_the_same() {
    let streams = [
        prores_stream(),
        pcm_stream(1, SampleFormat::S24, 2, Some(*b"in24")),
    ];
    let packets = interleave(
        video_packets(|i| prores_frame(i, 2), 6),
        pcm_packets(1, 1500, 6, 6),
    );
    let plain = mux_both(Flavor::Mov, &streams, &packets);
    let (_, want) = demux(&plain[0]);
    for flags in ["+faststart", "faststart", "+frag_keyframe+faststart"] {
        let fast = mux_both_with(Flavor::Mov, &streams, &packets, &|m| {
            assert!(m.set_option("movflags", flags));
        });
        for file in &fast {
            assert_eq!(top_level(file), [*b"ftyp", *b"moov", *b"mdat"], "{flags}");
            let (_, got) = demux(file);
            assert_eq!(got.len(), want.len());
            for (g, w) in got.iter().zip(&want) {
                assert_eq!(
                    (g.stream_index, g.pts, g.dts, &g.data),
                    (w.stream_index, w.pts, w.dts, &w.data)
                );
            }
            // The index's offsets point past `moov`, at the stored samples.
            let mut d = Mp4Demuxer::new(Box::new(Cursor::new(file.clone())));
            d.read_header().unwrap();
            let idx = d.sample_index().unwrap();
            let info = idx.sample_info(0, 3).unwrap();
            let raw = idx.read_sample(0, 3).unwrap().data;
            assert_eq!(&file[info.offset as usize..][..raw.len()], &raw[..]);
        }
        // Same bytes from both muxers, and as large as the plain file.
        assert_eq!(fast[0], fast[1]);
        assert_eq!(fast[0].len(), plain[0].len());
    }
}

#[test]
fn a_cut_short_faststart_file_reads_its_samples_then_ends() {
    // A download or copy cut short: `moov` (up front) still lists every
    // sample, but the tail of `mdat` is missing. The samples that are there
    // come out, in order, and the read ends in end-of-stream rather than an
    // error (`demux` panics on any error, through both readers).
    let streams = [
        prores_stream(),
        pcm_stream(1, SampleFormat::S16, 2, Some(*b"sowt")),
    ];
    let packets = interleave(
        video_packets(|i| prores_frame(i, 2), 8),
        pcm_packets(1, 1500, 8, 4),
    );
    let [fast, _] = mux_both_with(Flavor::Mov, &streams, &packets, &|m| {
        assert!(m.set_option("movflags", "+faststart"));
    });
    let (_, want) = demux(&fast);
    for pct in [40, 60, 95] {
        let (_, got) = demux(&fast[..fast.len() * pct / 100]);
        assert!(
            !got.is_empty() && got.len() < want.len(),
            "{pct}%: {}",
            got.len()
        );
        for (g, w) in got.iter().zip(&want) {
            assert_eq!(
                (g.stream_index, g.pts, &g.data),
                (w.stream_index, w.pts, &w.data)
            );
        }
    }
}

#[test]
fn faststart_carries_the_timecode_sample() {
    // `prores_stream` has a timecode, so a `tmcd` track and its 4-byte sample.
    let streams = [prores_stream()];
    let packets = video_packets(|i| prores_frame(i, 2), 3);
    let [plain, _] = mux_both(Flavor::Mov, &streams, &packets);
    let [fast, _] = mux_both_with(Flavor::Mov, &streams, &packets, &|m| {
        m.set_option("movflags", "+faststart");
    });
    let read = |file: &[u8]| {
        let mut d = Mp4Demuxer::new(Box::new(Cursor::new(file.to_vec())));
        d.read_header().unwrap()[0].timecode.clone()
    };
    assert!(read(&plain).is_some());
    assert_eq!(read(&fast), read(&plain));
}

#[test]
fn other_movflags_and_unknown_options_leave_the_layout_alone() {
    let streams = [prores_stream()];
    let packets = video_packets(|i| prores_frame(i, 2), 2);
    let plain = mux_both(Flavor::Mov, &streams, &packets);
    let got = mux_both_with(Flavor::Mov, &streams, &packets, &|m| {
        assert!(
            m.set_option("movflags", "+frag_keyframe"),
            "movflags is known"
        );
        assert!(!m.set_option("brand", "qt  "), "an option the muxer lacks");
    });
    assert_eq!(got, plain);
    assert_eq!(top_level(&got[0]).last(), Some(b"moov"));
}

#[test]
fn faststart_is_refused_once_the_seekable_header_is_out() {
    let mut m = Mp4StreamMuxer::with_flavor(Box::new(Shared::default()), Flavor::Mov);
    m.write_header(&[prores_stream()]).unwrap();
    assert!(
        !m.set_option("movflags", "+faststart"),
        "mdat is already open"
    );
}

#[test]
fn a_caller_edit_list_replaces_the_derived_one() {
    let mut v = prores_stream();
    v.timecode = None;
    v.time_base = Rational::new(1, 1000);
    let packets: Vec<Packet> = (0..4i64)
        .map(|i| {
            let mut p = Packet::from_data(0, prores_frame(i as u8, 2));
            p.pts = Some(40 * i);
            p.dts = p.pts;
            p.flags.keyframe = true;
            p
        })
        .collect();
    // A 250 ms empty edit, then the media from 40 ms (the second frame) for
    // 120 ms: what a caller trimming the head and delaying the start writes.
    let edits = [
        EditEntry {
            segment_duration: 250,
            media_time: -1,
            media_rate: 0x10000,
        },
        EditEntry {
            segment_duration: 120,
            media_time: 40,
            media_rate: 0x10000,
        },
    ];
    for file in mux_both_with(Flavor::Mov, &[v.clone()], &packets, &|m| {
        assert!(m.set_edit_list(0, &edits));
    }) {
        let mut d = Mp4Demuxer::new(Box::new(Cursor::new(file.clone())));
        d.read_header().unwrap();
        let idx = d.sample_index().unwrap();
        let ts = idx.movie_timescale().unwrap() as i64;
        let back = idx.edit_list(0).to_vec();
        assert_eq!(back.len(), 2, "{back:?}");
        assert_eq!(
            (back[0].segment_duration * 1000 / ts, back[0].media_time),
            (250, -1)
        );
        assert_eq!(back[1].segment_duration * 1000 / ts, 120);
        // Media time is in the track's time scale: 40 ms of it, not zero.
        assert!(back[1].media_time > 0, "{back:?}");
        assert_eq!(back[1].media_rate, 0x10000);
        // read_packet applies it: frame 0 is before the edit (and frame 1 is a
        // keyframe), so it is dropped; frames 1-3 fill the 120 ms from 250 ms.
        let (_, got) = demux(&file);
        let shown: Vec<_> = got.iter().map(|p| p.pts).collect();
        assert_eq!(shown, [Some(250), Some(290), Some(330)]);
        assert_eq!(got[0].data, packets[1].data);
    }
}

#[test]
fn a_wide_caller_edit_list_is_written_as_version_1() {
    let mut v = prores_stream();
    v.timecode = None;
    let packets = video_packets(|i| prores_frame(i, 2), 2);
    let long = i64::from(u32::MAX) + 10;
    let edits = [EditEntry {
        segment_duration: long,
        media_time: 0,
        media_rate: 0x10000,
    }];
    let [file, _] = mux_both_with(Flavor::Mov, &[v], &packets, &|m| {
        m.set_edit_list(0, &edits);
    });
    let mut d = Mp4Demuxer::new(Box::new(Cursor::new(file)));
    d.read_header().unwrap();
    let back = d.sample_index().unwrap().edit_list(0).to_vec();
    assert_eq!(back.len(), 1);
    assert_eq!(back[0].segment_duration, long, "64-bit segment kept");
}

#[test]
fn an_empty_caller_edit_list_writes_no_edts() {
    let mut v = prores_stream();
    v.timecode = None;
    v.time_base = Rational::new(1, 1000);
    let packets: Vec<Packet> = (0..2i64)
        .map(|i| {
            let mut p = Packet::from_data(0, prores_frame(i as u8, 2));
            p.pts = Some(500 + 40 * i);
            p.dts = p.pts;
            p.flags.keyframe = true;
            p
        })
        .collect();
    // Derived: with an audio track starting at 0, the video's 500 ms start
    // is an empty edit. A caller's empty list overrides it with none.
    let mut packets = packets;
    packets.extend(pcm_packets(1, 4800, 2, 3));
    let streams = [v, pcm_stream(1, SampleFormat::S16, 1, None)];
    let [derived, _] = mux_both(Flavor::Mov, &streams, &packets);
    assert!(derived.windows(4).any(|w| w == b"edts"));
    let [file, _] = mux_both_with(Flavor::Mov, &streams, &packets, &|m| {
        m.set_edit_list(0, &[]);
    });
    assert!(!file.windows(4).any(|w| w == b"edts"));
}
