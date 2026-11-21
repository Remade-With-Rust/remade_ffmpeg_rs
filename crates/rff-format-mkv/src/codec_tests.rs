//! #114 round 2: Matroska CodecIDs (V_MJPEG, V_APV, V_MPEG1/2, PCM at
//! every depth and byte order, legacy A_AAC/MPEG{2,4}/..., A_DTS,
//! A_TRUEHD, V_MS/VFW/FOURCC), OutputSamplingFrequency, the raw CodecID
//! string, and muxing DNxHD / APV / MJPEG / HEVC / ProRes / 24-bit PCM, the
//! H.264 avcC from Annex-B extradata and the FLAC STREAMINFO rewrite.

use std::io::{Cursor, Seek, SeekFrom, Write};
use std::sync::{Arc, Mutex};

use rff_core::{CodecId, Error, MediaType, Packet, Rational, SampleFormat};
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

fn mux_both(streams: &[Stream], packets: &[Packet]) -> [Vec<u8>; 2] {
    let run = |m: &mut dyn Muxer| {
        m.write_header(streams).unwrap();
        for p in packets {
            m.write_packet(p).unwrap();
        }
        m.write_trailer().unwrap();
    };
    let a = Shared::default();
    run(&mut MkvMuxer::new(Box::new(a.clone()), false));
    let b = Shared::default();
    run(&mut MkvStreamingMuxer::new(Box::new(b.clone()), false));
    let take = |s: Shared| s.0.lock().unwrap().get_ref().clone();
    [take(a), take(b)]
}

fn open(file: &[u8]) -> (MkvDemuxer, Vec<Stream>) {
    let mut d = MkvDemuxer::new_seekable(Box::new(Cursor::new(file.to_vec())));
    let s = d.read_header().unwrap();
    (d, s)
}

fn drain(d: &mut MkvDemuxer) -> Vec<Packet> {
    let mut out = Vec::new();
    loop {
        match d.read_packet() {
            Ok(p) => out.push(p),
            Err(Error::Eof) => return out,
            Err(e) => panic!("{e}"),
        }
    }
}

/// A one-track file: the TrackEntry's codec children, one SimpleBlock.
fn one_track(track_type: u64, children: &[u8], frame: &[u8]) -> Vec<u8> {
    let mut te = Vec::new();
    put_uint(&mut te, 0xD7, 1);
    put_uint(&mut te, 0x83, track_type);
    te.extend_from_slice(children);
    let mut tracks = Vec::new();
    put_elem(&mut tracks, 0xAE, &te);
    let mut info = Vec::new();
    put_uint(&mut info, 0x2A_D7B1, 1_000_000);
    let mut cl = Vec::new();
    put_uint(&mut cl, 0xE7, 0);
    put_elem(&mut cl, 0xA3, &[&[0x81, 0, 0, 0x80][..], frame].concat());
    let mut body = Vec::new();
    put_elem(&mut body, 0x1549_A966, &info);
    put_elem(&mut body, 0x1654_AE6B, &tracks);
    put_elem(&mut body, 0x1F43_B675, &cl);
    let mut ebml = Vec::new();
    put_string(&mut ebml, 0x4282, "matroska");
    let mut file = Vec::new();
    put_elem(&mut file, 0x1A45_DFA3, &ebml);
    put_elem(&mut file, 0x1853_8067, &body);
    file
}

fn audio(
    codec: &str,
    rate: f64,
    out_rate: Option<f64>,
    channels: u64,
    depth: Option<u64>,
) -> Vec<u8> {
    let mut c = Vec::new();
    put_string(&mut c, 0x86, codec);
    let mut a = Vec::new();
    put_elem(&mut a, 0xB5, &rate.to_be_bytes());
    if let Some(o) = out_rate {
        put_elem(&mut a, 0x78B5, &o.to_be_bytes());
    }
    put_uint(&mut a, 0x9F, channels);
    if let Some(d) = depth {
        put_uint(&mut a, 0x6264, d);
    }
    put_elem(&mut c, 0xE1, &a);
    c
}

fn video(codec: &str, private: &[u8]) -> Vec<u8> {
    let mut c = Vec::new();
    put_string(&mut c, 0x86, codec);
    if !private.is_empty() {
        put_elem(&mut c, 0x63A2, private);
    }
    let mut v = Vec::new();
    put_uint(&mut v, 0xB0, 64);
    put_uint(&mut v, 0xBA, 48);
    put_elem(&mut c, 0xE0, &v);
    c
}

#[test]
fn video_codec_ids_are_identified() {
    let apvc = [1u8, 2, 3, 4, 5];
    let mut bih = vec![0u8; 40];
    bih[..4].copy_from_slice(&40u32.to_le_bytes());
    bih[16..20].copy_from_slice(b"AVdh");
    let mut mjpg = bih.clone();
    mjpg[16..20].copy_from_slice(b"MJPG");
    let mut other = bih.clone();
    other[16..20].copy_from_slice(b"XVID");
    for (codec, private, want, tag, extradata) in [
        ("V_MJPEG", &[][..], CodecId::Jpeg, None, &[][..]),
        ("V_APV", &apvc[..], CodecId::Apv, None, &apvc[..]),
        ("V_MPEG1", &[], CodecId::Mpeg1Video, None, &[]),
        ("V_MPEG2", &[], CodecId::Mpeg2Video, None, &[]),
        (
            "V_MS/VFW/FOURCC",
            &bih[..],
            CodecId::DnxHd,
            Some(*b"AVdh"),
            &[],
        ),
        (
            "V_MS/VFW/FOURCC",
            &mjpg[..],
            CodecId::Jpeg,
            Some(*b"MJPG"),
            &[],
        ),
        (
            "V_MS/VFW/FOURCC",
            &other[..],
            CodecId::None,
            Some(*b"XVID"),
            &[],
        ),
        ("V_SOMETHING_NEW", &[], CodecId::None, None, &[]),
    ] {
        let file = one_track(1, &video(codec, private), &[9, 8, 7]);
        let (mut d, s) = open(&file);
        assert_eq!(s[0].codec_id, want, "{codec}");
        assert_eq!(s[0].codec_string.as_deref(), Some(codec));
        assert_eq!(s[0].codec_tag, tag, "{codec}");
        assert_eq!(s[0].extradata, extradata, "{codec}");
        assert_eq!(s[0].media_type, MediaType::Video);
        assert_eq!(drain(&mut d)[0].data, [9, 8, 7]);
    }
}

#[test]
fn pcm_at_every_depth_and_byte_order() {
    let frame: Vec<u8> = (1..=24).collect();
    let swapped = |n: usize| -> Vec<u8> {
        let mut v = frame.clone();
        v.chunks_exact_mut(n).for_each(<[u8]>::reverse);
        v
    };
    for (codec, depth, format, data) in [
        ("A_PCM/INT/LIT", 8, SampleFormat::U8, frame.clone()),
        ("A_PCM/INT/LIT", 16, SampleFormat::S16, frame.clone()),
        ("A_PCM/INT/LIT", 24, SampleFormat::S24, frame.clone()),
        ("A_PCM/INT/LIT", 32, SampleFormat::S32, frame.clone()),
        ("A_PCM/FLOAT/IEEE", 32, SampleFormat::F32, frame.clone()),
        ("A_PCM/FLOAT/IEEE", 64, SampleFormat::F64, frame.clone()),
        ("A_PCM/INT/BIG", 8, SampleFormat::U8, frame.clone()),
        ("A_PCM/INT/BIG", 16, SampleFormat::S16, swapped(2)),
        ("A_PCM/INT/BIG", 24, SampleFormat::S24, swapped(3)),
        ("A_PCM/INT/BIG", 32, SampleFormat::S32, swapped(4)),
    ] {
        let file = one_track(2, &audio(codec, 48_000.0, None, 2, Some(depth)), &frame);
        let (mut d, s) = open(&file);
        assert_eq!(
            (s[0].codec_id, s[0].sample_format),
            (CodecId::Pcm, Some(format)),
            "{codec} {depth}"
        );
        assert_eq!(
            drain(&mut d)[0].data,
            data,
            "little-endian out: {codec} {depth}"
        );
        // Raw samples keep the stored byte order.
        let mut d = MkvDemuxer::new_seekable(Box::new(Cursor::new(file.clone())));
        d.read_header().unwrap();
        d.set_raw_samples(true);
        assert_eq!(drain(&mut d)[0].data, frame);
        let idx = rff_format::Demuxer::sample_index(&mut d).unwrap();
        d_check(idx.read_sample(0, 0).unwrap().data, frame.clone());
    }
}

fn d_check(a: Vec<u8>, b: Vec<u8>) {
    assert_eq!(a, b);
}

#[test]
fn legacy_aac_ids_dts_truehd_and_output_sampling_frequency() {
    // HE-AAC: core 22.05 kHz, SBR output 44.1 kHz.
    let file = one_track(
        2,
        &audio("A_AAC/MPEG4/LC/SBR", 22_050.0, Some(44_100.0), 2, None),
        &[1],
    );
    let (_, s) = open(&file);
    assert_eq!(s[0].codec_id, CodecId::Aac);
    assert_eq!(s[0].sample_rate, 44_100, "the output rate");
    // As matroskadec: LC, index 7 (22050), 2 ch, then SBR at index 4.
    assert_eq!(s[0].extradata, [0x13, 0x90, 0x56, 0xE5, 0x80 | (4 << 3)]);
    assert_eq!(rff_format::aac::asc_sbr_rate(&s[0].extradata), Some(44_100));
    for (codec, first) in [
        ("A_AAC/MPEG2/MAIN", 1u8 << 3),
        ("A_AAC/MPEG4/LC", 2 << 3),
        ("A_AAC/MPEG2/SSR", 3 << 3),
        ("A_AAC/MPEG4/LTP", 4 << 3),
    ] {
        let (_, s) = open(&one_track(2, &audio(codec, 48_000.0, None, 2, None), &[1]));
        // index 3 (48000) straddles the two bytes: 3 >> 1 = 1, low bit 1.
        assert_eq!(s[0].extradata, [first | 1, 0x80 | 2 << 3], "{codec}");
        assert_eq!(s[0].sample_rate, 48_000);
    }
    for (codec, want) in [
        ("A_DTS", CodecId::Dts),
        ("A_DTS/LOSSLESS", CodecId::Dts),
        ("A_TRUEHD", CodecId::TrueHd),
    ] {
        let (mut d, s) = open(&one_track(
            2,
            &audio(codec, 48_000.0, None, 6, None),
            &[7, 7],
        ));
        assert_eq!(
            (s[0].codec_id, s[0].media_type),
            (want, MediaType::Audio),
            "{codec}"
        );
        assert_eq!(s[0].codec_string.as_deref(), Some(codec));
        assert_eq!(drain(&mut d)[0].data, [7, 7]);
    }
}

#[test]
fn he_aac_keeps_both_rates_through_a_remux() {
    let mut a = Stream::new(0, CodecId::Aac);
    a.media_type = MediaType::Audio;
    (a.channels, a.sample_rate) = (2, 44_100);
    a.time_base = Rational::new(1, 1000);
    a.extradata = vec![0x13, 0x90, 0x56, 0xE5, 0x80 | (4 << 3)];
    let packets: Vec<Packet> = (0..3)
        .map(|i| {
            let mut p = Packet::from_data(0, vec![0x21, i as u8]);
            p.pts = Some(i * 46);
            p
        })
        .collect();
    for file in mux_both(&[a.clone()], &packets) {
        // SamplingFrequency 22050 (core) and OutputSamplingFrequency 44100.
        assert!(file
            .windows(10)
            .any(|w| w == [0xB5, 0x88, 0x40, 0xD5, 0x88, 0x80, 0, 0, 0, 0]));
        assert!(file.windows(3).any(|w| w == [0x78, 0xB5, 0x88]));
        let (_, s) = open(&file);
        assert_eq!((s[0].sample_rate, &s[0].extradata), (44_100, &a.extradata));
    }
}

fn frames(n: i64, f: impl Fn(i64) -> Vec<u8>) -> Vec<Packet> {
    (0..n)
        .map(|i| {
            let mut p = Packet::from_data(0, f(i));
            p.pts = Some(i * 40);
            p.flags.keyframe = i == 0;
            p
        })
        .collect()
}

#[test]
fn dnxhd_apv_and_mjpeg_mux_into_matroska() {
    let mut dnx = Stream::new(0, CodecId::DnxHd);
    dnx.media_type = MediaType::Video;
    (dnx.width, dnx.height) = (1920, 1080);
    dnx.time_base = Rational::new(1, 1000);
    dnx.codec_tag = Some(*b"AVdh");
    let mut apv = dnx.clone();
    apv.codec_id = CodecId::Apv;
    apv.codec_tag = None;
    apv.extradata = vec![1, 0x21, 0x99, 0x10, 0x0A];
    let mut jpeg = dnx.clone();
    jpeg.codec_id = CodecId::Jpeg;
    jpeg.codec_tag = None;
    for (s, id) in [
        (&dnx, "V_MS/VFW/FOURCC"),
        (&apv, "V_APV"),
        (&jpeg, "V_MJPEG"),
    ] {
        let p = frames(3, |i| vec![0, 0, 2, 0x80, i as u8, 0xAA]);
        for file in mux_both(std::slice::from_ref(s), &p) {
            let (mut d, got) = open(&file);
            assert_eq!(got[0].codec_id, s.codec_id, "{id}");
            assert_eq!(got[0].codec_string.as_deref(), Some(id));
            assert_eq!(got[0].codec_tag, s.codec_tag);
            assert_eq!(got[0].extradata, s.extradata);
            let back: Vec<Vec<u8>> = drain(&mut d).into_iter().map(|p| p.data).collect();
            assert_eq!(back, p.iter().map(|p| p.data.clone()).collect::<Vec<_>>());
        }
    }
    // APV without its configuration record is refused, not written broken.
    let mut bare = apv.clone();
    bare.extradata.clear();
    let mut m = MkvMuxer::new(Box::new(Vec::new()), false);
    m.write_header(&[bare]).unwrap();
    m.write_packet(&frames(1, |_| vec![1])[0]).unwrap();
    assert!(m.write_trailer().is_err());
    // Not WebM codecs.
    assert!(MkvMuxer::new(Box::new(Vec::new()), true)
        .write_header(&[dnx])
        .is_err());
}

#[test]
fn h264_from_annexb_extradata_and_24_bit_pcm() {
    let sps: &[u8] = &[0x67, 0x64, 0x00, 0x1F, 0xAE];
    let pps: &[u8] = &[0x68, 0xCE, 0xBB];
    let mut v = Stream::new(0, CodecId::H264);
    v.media_type = MediaType::Video;
    (v.width, v.height) = (64, 48);
    v.time_base = Rational::new(1, 1000);
    v.extradata = [&[0, 0, 0, 1][..], sps, &[0, 0, 1], pps].concat();
    let p = frames(3, |i| {
        vec![0, 0, 0, 1, if i == 0 { 0x65 } else { 0x41 }, i as u8]
    });
    for file in mux_both(std::slice::from_ref(&v), &p) {
        let (mut d, got) = open(&file);
        let avcc = &got[0].extradata;
        assert_eq!(&avcc[..5], &[1, 0x64, 0, 0x1F, 0xFF]);
        assert_eq!(&avcc[avcc.len() - 4..], &[0xFD, 0xF8, 0xF8, 0]);
        let first = &drain(&mut d)[0].data;
        assert_eq!(
            first,
            &[
                &[0, 0, 0, 1][..],
                sps,
                &[0, 0, 0, 1],
                pps,
                &[0, 0, 0, 1, 0x65, 0]
            ]
            .concat()
        );
    }
    let mut a = Stream::new(0, CodecId::Pcm);
    a.media_type = MediaType::Audio;
    (a.channels, a.sample_rate) = (2, 48_000);
    a.time_base = Rational::new(1, 1000);
    a.sample_format = Some(SampleFormat::S24);
    let data: Vec<u8> = (0..60).collect();
    let p = frames(2, |_| data.clone());
    for file in mux_both(std::slice::from_ref(&a), &p) {
        let (mut d, got) = open(&file);
        assert_eq!(got[0].sample_format, Some(SampleFormat::S24));
        assert_eq!(drain(&mut d)[0].data, data);
    }
}

#[test]
fn flac_streaminfo_is_rewritten_at_the_trailer() {
    // 4096-sample frames, then a 1000-sample one.
    let sizes = [900usize, 1300, 700, 333];
    let frames_: Vec<Vec<u8>> = sizes
        .iter()
        .enumerate()
        .map(|(i, &len)| {
            let mut f = if i == 3 {
                vec![0xFF, 0xF8, 0x79, 0x08, i as u8, 0x03, 0xE7]
            } else {
                vec![0xFF, 0xF8, 0xC9, 0x08, i as u8]
            };
            f.resize(len, 0x55);
            f
        })
        .collect();
    let mut si = vec![0x10, 0x00, 0x10, 0x00, 0, 0, 0, 0, 0, 0];
    si.extend_from_slice(&(48_000u64 << 44 | 1 << 41 | 15 << 36).to_be_bytes());
    si.extend_from_slice(&[0xEE; 16]);
    let mut a = Stream::new(0, CodecId::Flac);
    a.media_type = MediaType::Audio;
    (a.channels, a.sample_rate) = (2, 48_000);
    a.time_base = Rational::new(1, 1000);
    a.extradata = si.clone();
    let packets: Vec<Packet> = frames_
        .iter()
        .enumerate()
        .map(|(i, f)| {
            let mut p = Packet::from_data(0, f.clone());
            p.pts = Some(i as i64 * 85);
            p.flags.keyframe = true;
            p
        })
        .collect();
    for file in mux_both(std::slice::from_ref(&a), &packets) {
        let (mut d, got) = open(&file);
        let private = &got[0].extradata;
        assert!(private.starts_with(b"fLaC"));
        let s = &private[8..42];
        assert_eq!(&s[4..7], &[0, 0x01, 0x4D], "min frame 333");
        assert_eq!(&s[7..10], &[0, 0x05, 0x14], "max frame 1300");
        assert_eq!(
            u32::from_be_bytes([s[14], s[15], s[16], s[17]]),
            3 * 4096 + 1000
        );
        assert_eq!(&s[18..], &[0; 16]);
        assert_eq!(drain(&mut d).len(), 4);
    }
}

#[test]
fn sample_sources_read_interleaved_and_across_threads() {
    let mut v = Stream::new(0, CodecId::Vp9);
    v.media_type = MediaType::Video;
    (v.width, v.height) = (64, 48);
    v.time_base = Rational::new(1, 1000);
    let p = frames(40, |i| vec![0x82, 0x49, 0x83, 0x42, i as u8, (i * 7) as u8]);
    let [file, _] = mux_both(std::slice::from_ref(&v), &p);
    let mut d = MkvDemuxer::new_seekable(Box::new(Cursor::new(file)));
    d.read_header().unwrap();
    let idx = rff_format::Demuxer::sample_index(&mut d).unwrap();
    let n = idx.sample_count(0);
    assert_eq!(n, 40);
    let a = idx.sample_source().unwrap();
    let b = idx.sample_source().unwrap();
    for i in 0..n {
        assert_eq!(a.read_sample(0, i).unwrap().data, p[i as usize].data);
        assert_eq!(
            b.read_sample(0, n - 1 - i).unwrap().data,
            p[(n - 1 - i) as usize].data
        );
        // The packet walk is untouched by the sources.
        let walked = d.read_packet().unwrap();
        assert_eq!(walked.data, p[i as usize].data);
    }
    assert_eq!(a.sync_at_or_before(0, 1000), Some(0));
    let c = Arc::clone(&b);
    let want: Vec<Vec<u8>> = p.iter().map(|x| x.data.clone()).collect();
    let t = std::thread::spawn(move || {
        (0..n).all(|i| c.read_sample(0, i).unwrap().data == want[i as usize])
    });
    for i in (0..n).rev() {
        assert_eq!(a.read_sample(0, i).unwrap().data, p[i as usize].data);
    }
    assert!(t.join().unwrap());
    // A piped demuxer has no index, so no source.
    let mut piped = MkvDemuxer::new(Box::new(Cursor::new(Vec::new())));
    assert!(rff_format::Demuxer::sample_index(&mut piped).is_none());
}
