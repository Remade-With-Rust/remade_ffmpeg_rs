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
        // QuickTime's `nclc` has no range flag; limited is its default.
        assert_eq!(v.color_range, ColorRange::Unspecified);
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
