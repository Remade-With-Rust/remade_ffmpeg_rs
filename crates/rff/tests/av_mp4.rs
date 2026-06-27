//! Default-codec A/V MP4: combine AV1 video (rav1e/rav1d) + Opus audio into one
//! MP4, then transcode that MP4 again — decoding *both* tracks back out of it.
//! Exercises MP4 mux (av01/av1C + Opus/dOps) and demux + the multi-stream
//! decode path, with no C/feature codecs.

use std::fs;
use std::path::{Path, PathBuf};

use rff::core::{AudioFrame, CodecId, Dictionary, Frame, MediaType, Packet, PixelFormat, Rational, SampleFormat, VideoFrame};
use rff::format::Stream;
use rff::transcode::{InputSpec, MapSelector, MapSpec, OutputSpec, StreamCodec, TranscodeSpec};
use rff::Engine;

fn tmp(name: &str, ext: &str) -> PathBuf {
    std::env::temp_dir().join(format!("rff_avmp4_{}_{name}.{ext}", std::process::id()))
}

fn write_avif(engine: &Engine, path: &Path) {
    let (w, h) = (64u32, 64u32);
    let mut y = vec![0u8; (w * h) as usize];
    for (i, p) in y.iter_mut().enumerate() {
        *p = (i % 256) as u8;
    }
    let chroma = vec![128u8; (w / 2 * h / 2) as usize];
    let frame = Frame::Video(VideoFrame {
        width: w,
        height: h,
        format: PixelFormat::Yuv420p,
        planes: vec![y, chroma.clone(), chroma],
        strides: vec![w as usize, (w / 2) as usize, (w / 2) as usize],
        pts: Some(0),
    });
    let mut enc = engine.codecs.find_encoder(CodecId::Avif).unwrap();
    enc.send_frame(&frame).unwrap();
    enc.flush();
    let mut payload = Vec::new();
    while let Ok(p) = enc.receive_packet() {
        payload.extend_from_slice(&p.data);
    }
    let mut mux = engine.formats.open_muxer("avif", Box::new(fs::File::create(path).unwrap())).unwrap();
    let mut s = Stream::new(0, CodecId::Avif);
    s.width = w;
    s.height = h;
    mux.write_header(&[s]).unwrap();
    mux.write_packet(&Packet::from_data(0, payload)).unwrap();
    mux.write_trailer().unwrap();
}

fn write_wav(engine: &Engine, path: &Path) {
    // 0.3 s tone at 8 kHz mono (an Opus-supported rate).
    let pcm: Vec<u8> = (0..2400)
        .flat_map(|i| (((i as f32 * 0.2).sin() * 12000.0) as i16).to_le_bytes())
        .collect();
    let af = AudioFrame {
        sample_rate: 8000,
        channels: 1,
        format: SampleFormat::S16,
        planes: vec![pcm.clone()],
        samples: pcm.len() / 2,
        pts: Some(0),
    };
    let mut enc = engine.codecs.find_encoder(CodecId::Pcm).unwrap();
    enc.send_frame(&Frame::Audio(af)).unwrap();
    enc.flush();
    let packet = enc.receive_packet().unwrap();
    let mut mux = engine.formats.open_muxer("wav", Box::new(fs::File::create(path).unwrap())).unwrap();
    let mut s = Stream::new(0, CodecId::Pcm);
    s.sample_rate = 8000;
    s.channels = 1;
    s.sample_format = Some(SampleFormat::S16);
    mux.write_header(&[s]).unwrap();
    mux.write_packet(&packet).unwrap();
    mux.write_trailer().unwrap();
}

fn av_spec(inputs: Vec<PathBuf>, out: &Path) -> TranscodeSpec {
    TranscodeSpec {
        inputs: inputs.into_iter().map(|p| InputSpec { path: p, format: None }).collect(),
        outputs: vec![OutputSpec {
            path: out.to_path_buf(),
            format: None,
            video_codec: Some(StreamCodec { codec: CodecId::Avif, options: Dictionary::new() }),
            audio_codec: Some(StreamCodec { codec: CodecId::Opus, options: Dictionary::new() }),
            video_filters: None,
            filter_complex: None,
            maps: Vec::new(),
            overwrite: true,
        }],
    }
}

#[test]
fn av1_plus_opus_mp4_roundtrip() {
    let engine = Engine::new();
    let avif = tmp("v", "avif");
    let wav = tmp("a", "wav");
    let mp4 = tmp("av", "mp4");
    let mp4b = tmp("av2", "mp4");
    write_avif(&engine, &avif);
    write_wav(&engine, &wav);

    // Two inputs → one MP4 with AV1 video + Opus audio.
    rff::transcode::run(&engine, &av_spec(vec![avif.clone(), wav.clone()], &mp4)).expect("mux");
    let info = rff::probe::probe(&engine, &mp4).unwrap();
    assert_eq!(info.format_name, "mp4");
    assert_eq!(info.streams.len(), 2);
    let v = info.streams.iter().find(|s| s.media_type == MediaType::Video).unwrap();
    assert_eq!(v.codec_id, CodecId::Avif);
    assert_eq!((v.width, v.height), (64, 64));
    let a = info.streams.iter().find(|s| s.media_type == MediaType::Audio).unwrap();
    assert_eq!(a.codec_id, CodecId::Opus);

    // Audio timing is real, not nominal: an 8 kHz timescale with 20 ms
    // (160-sample) Opus frames, and strictly increasing per-frame PTS.
    assert_eq!(a.time_base, Rational::new(1, 8000));
    let audio_idx = a.index;
    let mut dem = engine
        .formats
        .open_demuxer("mp4", Box::new(fs::File::open(&mp4).unwrap()))
        .unwrap();
    let _ = dem.read_header().unwrap();
    let mut audio_pts = Vec::new();
    while let Ok(p) = dem.read_packet() {
        if p.stream_index == audio_idx {
            audio_pts.push(p.pts.expect("audio packet has pts"));
        }
    }
    assert!(audio_pts.len() >= 2, "expected several Opus frames");
    assert_eq!(audio_pts[0], 0);
    assert_eq!(audio_pts[1] - audio_pts[0], 160, "20 ms @ 8 kHz = 160 samples");
    assert!(audio_pts.windows(2).all(|w| w[1] > w[0]), "PTS must increase");

    // Re-transcode the MP4: this decodes BOTH tracks back out of it.
    let report = rff::transcode::run(&engine, &av_spec(vec![mp4.clone()], &mp4b)).expect("re-mux");
    assert!(report.frames_decoded >= 2, "expected video + audio frames decoded, got {}", report.frames_decoded);
    assert_eq!(rff::probe::probe(&engine, &mp4b).unwrap().streams.len(), 2);

    for p in [avif, wav, mp4, mp4b] {
        let _ = fs::remove_file(p);
    }
}

#[test]
fn map_selects_individual_streams() {
    let engine = Engine::new();
    let avif = tmp("mv", "avif");
    let wav = tmp("ma", "wav");
    let mp4 = tmp("mav", "mp4");
    write_avif(&engine, &avif);
    write_wav(&engine, &wav);
    rff::transcode::run(&engine, &av_spec(vec![avif.clone(), wav.clone()], &mp4)).expect("mux");

    // `-map 0:a` → an audio-only MP4 (the video track is dropped).
    let audio_only = tmp("aonly", "mp4");
    let mut a_spec = av_spec(vec![mp4.clone()], &audio_only);
    a_spec.outputs[0].maps = vec![MapSpec {
        input: 0,
        selector: MapSelector::Kind(MediaType::Audio),
    }];
    rff::transcode::run(&engine, &a_spec).expect("map audio");
    let ai = rff::probe::probe(&engine, &audio_only).unwrap();
    assert_eq!(ai.streams.len(), 1);
    assert_eq!(ai.streams[0].media_type, MediaType::Audio);
    assert_eq!(ai.streams[0].codec_id, CodecId::Opus);

    // `-map 0:0` → just the first (video) stream by index.
    let video_only = tmp("vonly", "mp4");
    let mut v_spec = av_spec(vec![mp4.clone()], &video_only);
    v_spec.outputs[0].maps = vec![MapSpec { input: 0, selector: MapSelector::Index(0) }];
    rff::transcode::run(&engine, &v_spec).expect("map video");
    let vi = rff::probe::probe(&engine, &video_only).unwrap();
    assert_eq!(vi.streams.len(), 1);
    assert_eq!(vi.streams[0].media_type, MediaType::Video);
    assert_eq!(vi.streams[0].codec_id, CodecId::Avif);

    for p in [avif, wav, mp4, audio_only, video_only] {
        let _ = fs::remove_file(p);
    }
}
