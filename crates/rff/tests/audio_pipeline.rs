//! First audio path through the engine: write a WAV, probe it, then transcode
//! `wav → wav` (decode PCM → re-encode PCM) and confirm the samples are
//! bit-identical (PCM is lossless). Exercises the new codec-parameters plumbing
//! end to end — the PCM decoder is configured from the demuxed stream.

use std::fs;
use std::path::{Path, PathBuf};

use rff::core::{AudioFrame, CodecId, Dictionary, Frame, SampleFormat};
use rff::format::Stream;
use rff::transcode::{InputSpec, OutputSpec, StreamCodec, TranscodeSpec};
use rff::Engine;

fn tmp(name: &str) -> PathBuf {
    std::env::temp_dir().join(format!("rff_audio_{}_{name}.wav", std::process::id()))
}

fn pcm_s16(samples: usize) -> Vec<u8> {
    let mut v = Vec::with_capacity(samples * 2);
    for i in 0..samples {
        let s = ((i as f32 * 0.2).sin() * 12000.0) as i16;
        v.extend_from_slice(&s.to_le_bytes());
    }
    v
}

fn write_wav(engine: &Engine, path: &Path, pcm: &[u8], sr: u32, ch: u16) {
    let af = AudioFrame {
        sample_rate: sr,
        channels: ch,
        format: SampleFormat::S16,
        planes: vec![pcm.to_vec()],
        samples: pcm.len() / (2 * ch as usize),
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
    s.channels = ch;
    s.sample_format = Some(SampleFormat::S16);
    mux.write_header(&[s]).unwrap();
    mux.write_packet(&packet).unwrap();
    mux.write_trailer().unwrap();
}

fn read_wav_pcm(engine: &Engine, path: &Path) -> Vec<u8> {
    let mut dem = engine
        .formats
        .open_demuxer("wav", Box::new(fs::File::open(path).unwrap()))
        .unwrap();
    let _ = dem.read_header().unwrap();
    dem.read_packet().unwrap().data
}

#[test]
fn wav_transcode_preserves_pcm() {
    let engine = Engine::new();
    let pcm = pcm_s16(256);
    let in_path = tmp("in");
    let out_path = tmp("out");
    write_wav(&engine, &in_path, &pcm, 8000, 1);

    // Content sniffing + audio stream parameters surface through probe.
    let info = rff::probe::probe(&engine, &in_path).unwrap();
    assert_eq!(info.format_name, "wav");
    assert_eq!(info.streams[0].codec_id, CodecId::Pcm);
    assert_eq!(info.streams[0].sample_rate, 8000);
    assert_eq!(info.streams[0].channels, 1);

    // wav → wav: demux → PCM decode (configured from the stream) → PCM encode → mux.
    let spec = TranscodeSpec {
        inputs: vec![InputSpec {
            path: in_path.clone(),
            format: None,
        }],
        outputs: vec![OutputSpec {
            path: out_path.clone(),
            format: None,
            video_codec: None,
            audio_codec: Some(StreamCodec {
                codec: CodecId::Pcm,
                options: Dictionary::new(),
            }),
            video_filters: None,
            filter_complex: None,
            maps: Vec::new(),
            overwrite: true,
        }],
    };
    let report = rff::transcode::run(&engine, &spec).expect("audio transcode");
    assert!(report.frames_decoded >= 1, "no audio decoded");

    // PCM is lossless — the samples must come back identical.
    assert_eq!(read_wav_pcm(&engine, &out_path), pcm);

    for p in [in_path, out_path] {
        let _ = fs::remove_file(p);
    }
}
