//! Audio resampling: a 44.1 kHz WAV transcoded to Opus must be resampled to a
//! rate Opus accepts (48 kHz) — FFmpeg's automatic `aresample`. Exercises the
//! `rff-resample` path wired into the transcode loop.

use std::fs;
use std::path::{Path, PathBuf};

use rff::core::{AudioFrame, CodecId, Dictionary, Frame, MediaType, Rational, SampleFormat};
use rff::format::Stream;
use rff::transcode::{InputSpec, OutputSpec, StreamCodec, TranscodeSpec};
use rff::Engine;

fn tmp(name: &str, ext: &str) -> PathBuf {
    std::env::temp_dir().join(format!("rff_resamp_{}_{name}.{ext}", std::process::id()))
}

fn write_wav_44100(engine: &Engine, path: &Path) {
    // ~0.3 s of a 440 Hz tone at 44.1 kHz mono.
    let pcm: Vec<u8> = (0..13_230)
        .flat_map(|i| {
            let s = (std::f64::consts::TAU * 440.0 * i as f64 / 44_100.0).sin();
            ((s * 12_000.0) as i16).to_le_bytes()
        })
        .collect();
    let af = AudioFrame {
        sample_rate: 44_100,
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
    let mut mux = engine
        .formats
        .open_muxer("wav", Box::new(fs::File::create(path).unwrap()))
        .unwrap();
    let mut s = Stream::new(0, CodecId::Pcm);
    s.sample_rate = 44_100;
    s.channels = 1;
    s.sample_format = Some(SampleFormat::S16);
    mux.write_header(&[s]).unwrap();
    mux.write_packet(&packet).unwrap();
    mux.write_trailer().unwrap();
}

#[test]
fn wav_44100_resamples_to_opus_48000() {
    let engine = Engine::new();
    let wav = tmp("in", "wav");
    let mp4 = tmp("out", "mp4");
    write_wav_44100(&engine, &wav);

    let spec = TranscodeSpec {
        inputs: vec![InputSpec { path: wav.clone(), format: None }],
        outputs: vec![OutputSpec {
            path: mp4.clone(),
            format: None,
            video_codec: None,
            audio_codec: Some(StreamCodec { codec: CodecId::Opus, options: Dictionary::new() }),
            video_filters: None,
            maps: Vec::new(),
            overwrite: true,
        }],
    };
    let report = rff::transcode::run(&engine, &spec).expect("resample transcode");
    assert!(report.frames_decoded >= 1);

    // The Opus track was resampled 44100 → 48000 (nearest accepted rate).
    let info = rff::probe::probe(&engine, &mp4).unwrap();
    let a = info.streams.iter().find(|s| s.media_type == MediaType::Audio).unwrap();
    assert_eq!(a.codec_id, CodecId::Opus);
    assert_eq!(a.sample_rate, 48_000, "input 44.1 kHz should resample to 48 kHz");
    assert_eq!(a.time_base, Rational::new(1, 48_000));

    // PTS advance by a 20 ms Opus frame at 48 kHz = 960 samples.
    let mut dem = engine
        .formats
        .open_demuxer("mp4", Box::new(fs::File::open(&mp4).unwrap()))
        .unwrap();
    let _ = dem.read_header().unwrap();
    let mut pts = Vec::new();
    while let Ok(p) = dem.read_packet() {
        pts.push(p.pts.expect("opus packet pts"));
    }
    assert!(pts.len() >= 2);
    assert_eq!(pts[0], 0);
    assert_eq!(pts[1] - pts[0], 960);

    for p in [wav, mp4] {
        let _ = fs::remove_file(p);
    }
}
