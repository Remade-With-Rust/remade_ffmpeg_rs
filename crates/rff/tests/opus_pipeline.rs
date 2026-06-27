//! Full Opus path through the engine: a WAV tone → `opus` (Ogg) → WAV, proving
//! the pure-Rust Opus codec (opus-rs) and the Ogg container work end to end,
//! with the codec parameters (channels/rate) flowing demuxer → decoder.

use std::fs;
use std::path::{Path, PathBuf};

use rff::core::{AudioFrame, CodecId, Dictionary, Frame, SampleFormat};
use rff::format::Stream;
use rff::transcode::{InputSpec, OutputSpec, StreamCodec, TranscodeSpec};
use rff::Engine;

fn tmp(name: &str, ext: &str) -> PathBuf {
    std::env::temp_dir().join(format!("rff_opus_{}_{name}.{ext}", std::process::id()))
}

/// Write a mono s16 WAV from raw little-endian samples.
fn write_wav(engine: &Engine, path: &Path, pcm: &[u8], sr: u32) {
    let af = AudioFrame {
        sample_rate: sr,
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
    s.sample_rate = sr;
    s.channels = 1;
    s.sample_format = Some(SampleFormat::S16);
    mux.write_header(&[s]).unwrap();
    mux.write_packet(&packet).unwrap();
    mux.write_trailer().unwrap();
}

fn transcode(engine: &Engine, input: &Path, output: &Path, codec: CodecId) {
    let spec = TranscodeSpec {
        inputs: vec![InputSpec {
            path: input.to_path_buf(),
            format: None,
        }],
        outputs: vec![OutputSpec {
            path: output.to_path_buf(),
            format: None,
            video_codec: None,
            audio_codec: Some(StreamCodec {
                codec,
                options: Dictionary::new(),
            }),
            video_filters: None,
            maps: Vec::new(),
            overwrite: true,
        }],
    };
    rff::transcode::run(engine, &spec).expect("transcode");
}

/// Demux a WAV and return (sample count, RMS) interpreting the data as f32.
fn read_wav_f32(engine: &Engine, path: &Path) -> (usize, f64) {
    let mut dem = engine
        .formats
        .open_demuxer("wav", Box::new(fs::File::open(path).unwrap()))
        .unwrap();
    let _ = dem.read_header().unwrap();
    let data = dem.read_packet().unwrap().data;
    let samples: Vec<f32> = data
        .chunks_exact(4)
        .map(|b| f32::from_le_bytes([b[0], b[1], b[2], b[3]]))
        .collect();
    let energy: f64 = samples.iter().map(|s| (*s as f64) * (*s as f64)).sum();
    let rms = (energy / samples.len().max(1) as f64).sqrt();
    (samples.len(), rms)
}

#[test]
fn wav_to_opus_and_back() {
    let engine = Engine::new();
    let sr = 48_000u32;

    // 0.2 s of a loud mono tone (10 × 20 ms Opus frames).
    let n = 9600usize;
    let mut pcm = Vec::with_capacity(n * 2);
    for i in 0..n {
        let s = ((i as f32 * 0.12).sin() * 16000.0) as i16;
        pcm.extend_from_slice(&s.to_le_bytes());
    }

    let wav_in = tmp("in", "wav");
    let opus = tmp("mid", "opus");
    let wav_out = tmp("out", "wav");
    write_wav(&engine, &wav_in, &pcm, sr);

    // wav → opus (Ogg): PCM decode → Opus encode → Ogg mux.
    transcode(&engine, &wav_in, &opus, CodecId::Opus);
    let info = rff::probe::probe(&engine, &opus).unwrap();
    assert_eq!(info.format_name, "ogg");
    assert_eq!(info.streams[0].codec_id, CodecId::Opus);
    assert_eq!(info.streams[0].channels, 1);
    assert_eq!(info.streams[0].sample_rate, sr);

    // opus → wav: Ogg demux → Opus decode (configured from OpusHead) → PCM → WAV.
    transcode(&engine, &opus, &wav_out, CodecId::Pcm);
    let (count, rms) = read_wav_f32(&engine, &wav_out);

    // Opus is lossy and adds a little latency/padding, but the duration and
    // energy must survive: ~9600 samples of a loud tone.
    assert!(count >= 8000, "decoded too few samples: {count}");
    assert!(rms > 0.05, "decoded audio is essentially silent: rms {rms:.4}");

    for p in [wav_in, opus, wav_out] {
        let _ = fs::remove_file(p);
    }
}
