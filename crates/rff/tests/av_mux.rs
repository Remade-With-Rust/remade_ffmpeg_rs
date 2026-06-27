//! A/V muxing: combine a *video* input and a separate *audio* input into one
//! container (`ffmpeg -i video -i audio out.avi`). Exercises multi-input in the
//! transcode loop + the AVI muxer's multi-stream support.

use std::fs;
use std::path::{Path, PathBuf};

use rff::core::{AudioFrame, CodecId, Frame, MediaType, Packet, PixelFormat, SampleFormat, VideoFrame};
use rff::format::Stream;
use rff::transcode::{InputSpec, OutputSpec, TranscodeSpec};
use rff::Engine;

fn tmp(name: &str, ext: &str) -> PathBuf {
    std::env::temp_dir().join(format!("rff_avmux_{}_{name}.{ext}", std::process::id()))
}

fn write_avif(engine: &Engine, path: &Path) {
    let (w, h) = (64u32, 64u32);
    let y = vec![128u8; (w * h) as usize];
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
    let mut mux = engine
        .formats
        .open_muxer("avif", Box::new(fs::File::create(path).unwrap()))
        .unwrap();
    let mut s = Stream::new(0, CodecId::Avif);
    s.width = w;
    s.height = h;
    mux.write_header(&[s]).unwrap();
    mux.write_packet(&Packet::from_data(0, payload)).unwrap();
    mux.write_trailer().unwrap();
}

fn write_wav(engine: &Engine, path: &Path, pcm: &[u8]) {
    let af = AudioFrame {
        sample_rate: 8000,
        channels: 1,
        format: SampleFormat::S16,
        planes: vec![pcm.to_vec()],
        samples: pcm.len() / 2,
        pts: Some(0),
    };
    let mut enc = engine.codecs.find_encoder(CodecId::Pcm).unwrap();
    enc.send_frame(&Frame::Audio(af)).unwrap();
    enc.flush();
    let packet = enc.receive_packet().unwrap();
    let mut mux = engine
        .formats
        .open_muxer("wav", Box::new(fs::File::create(path).unwrap()))
        .unwrap();
    let mut s = Stream::new(0, CodecId::Pcm);
    s.sample_rate = 8000;
    s.channels = 1;
    s.sample_format = Some(SampleFormat::S16);
    mux.write_header(&[s]).unwrap();
    mux.write_packet(&packet).unwrap();
    mux.write_trailer().unwrap();
}

#[test]
fn muxes_video_and_audio_into_one_avi() {
    let engine = Engine::new();
    let avif = tmp("v", "avif");
    let wav = tmp("a", "wav");
    let out = tmp("out", "avi");
    let pcm: Vec<u8> = (0..400u16).flat_map(|i| (i as i16).to_le_bytes()).collect();
    write_avif(&engine, &avif);
    write_wav(&engine, &wav, &pcm);

    // `ffmpeg -i v.avif -i a.wav -c:v copy -c:a copy -y out.avi`
    let spec = TranscodeSpec {
        inputs: vec![
            InputSpec { path: avif.clone(), format: None },
            InputSpec { path: wav.clone(), format: None },
        ],
        outputs: vec![OutputSpec {
            path: out.clone(),
            format: None,
            video_codec: None, // copy
            audio_codec: None, // copy
            video_filters: None,
            maps: Vec::new(),
            overwrite: true,
        }],
    };
    let report = rff::transcode::run(&engine, &spec).expect("a/v mux");
    assert!(report.packets_written >= 2, "expected video + audio packets");

    // The AVI now carries both streams.
    let info = rff::probe::probe(&engine, &out).expect("probe avi");
    assert_eq!(info.format_name, "avi");
    assert_eq!(info.streams.len(), 2);
    let media: Vec<MediaType> = info.streams.iter().map(|s| s.media_type).collect();
    assert!(media.contains(&MediaType::Video));
    assert!(media.contains(&MediaType::Audio));

    // Demux and confirm a packet for each stream; copy preserved the audio PCM.
    let mut dem = engine
        .formats
        .open_demuxer("avi", Box::new(fs::File::open(&out).unwrap()))
        .unwrap();
    let _ = dem.read_header().unwrap();
    let mut by_stream: std::collections::BTreeMap<usize, Vec<u8>> = Default::default();
    while let Ok(p) = dem.read_packet() {
        by_stream.entry(p.stream_index).or_default().extend_from_slice(&p.data);
    }
    assert_eq!(by_stream.len(), 2, "both streams should have packets");
    assert!(by_stream.values().any(|d| *d == pcm), "audio PCM survived the copy");

    for p in [avif, wav, out] {
        let _ = fs::remove_file(p);
    }
}
