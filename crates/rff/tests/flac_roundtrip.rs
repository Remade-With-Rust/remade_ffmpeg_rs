//! Full FLAC path through the engine: WAV → flac → WAV via the real transcode
//! pipeline (demux → decode → encode → mux). FLAC is lossless, so the samples
//! must survive **bit-exact** — a stronger end-to-end gate than the lossy codecs.

use std::fs;
use std::path::{Path, PathBuf};

use rff::core::{AudioFrame, CodecId, Dictionary, Frame, SampleFormat};
use rff::format::Stream;
use rff::transcode::{InputSpec, OutputSpec, StreamCodec, TranscodeSpec};
use rff::Engine;

fn tmp(name: &str, ext: &str) -> PathBuf {
    std::env::temp_dir().join(format!("rff_flacrt_{}_{name}.{ext}", std::process::id()))
}

/// Write an interleaved s16 WAV through the engine.
fn write_wav(engine: &Engine, path: &Path, pcm: &[u8], sr: u32, ch: u16) {
    let af = AudioFrame {
        sample_rate: sr,
        channels: ch,
        format: SampleFormat::S16,
        planes: vec![pcm.to_vec()],
        samples: pcm.len() / 2 / ch as usize,
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
            filter_complex: None,
            maps: Vec::new(),
            overwrite: true,
        }],
    };
    rff::transcode::run(engine, &spec).expect("transcode");
}

fn read_wav_f32(engine: &Engine, path: &Path) -> Vec<f32> {
    let mut dem = engine
        .formats
        .open_demuxer("wav", Box::new(fs::File::open(path).unwrap()))
        .unwrap();
    let _ = dem.read_header().unwrap();
    let data = dem.read_packet().unwrap().data;
    data.chunks_exact(4)
        .map(|b| f32::from_le_bytes([b[0], b[1], b[2], b[3]]))
        .collect()
}

#[test]
fn wav_to_flac_and_back_is_lossless() {
    let engine = Engine::new();
    let sr = 44_100u32;
    let ch = 2u16;
    let n = 8000usize; // per channel

    // Stereo: a tone on L, a correlated R (exercises stereo decorrelation).
    let mut pcm = Vec::with_capacity(n * 2 * 2);
    let mut orig: Vec<i32> = Vec::with_capacity(n * 2);
    let mut prev = 0i32;
    for i in 0..n {
        let l = ((i as f32 * 0.05).sin() * 15000.0) as i32;
        let r = (l - (l - prev) / 4).clamp(-32768, 32767);
        prev = l;
        pcm.extend_from_slice(&(l as i16).to_le_bytes());
        pcm.extend_from_slice(&(r as i16).to_le_bytes());
        orig.push(l);
        orig.push(r);
    }

    let wav_in = tmp("in", "wav");
    let flac = tmp("mid", "flac");
    let wav_out = tmp("out", "wav");
    write_wav(&engine, &wav_in, &pcm, sr, ch);

    // wav → flac : WAV demux → PCM decode → FLAC encode → flac mux.
    transcode(&engine, &wav_in, &flac, CodecId::Flac);
    let info = rff::probe::probe(&engine, &flac).unwrap();
    assert_eq!(info.format_name, "flac");
    assert_eq!(info.streams[0].codec_id, CodecId::Flac);

    // flac → wav : flac demux → FLAC decode → PCM → WAV. Must be bit-exact.
    transcode(&engine, &flac, &wav_out, CodecId::Pcm);
    let out = read_wav_f32(&engine, &wav_out);
    assert_eq!(
        out.len(),
        orig.len(),
        "sample count changed through the round-trip"
    );
    for (o, &e) in out.iter().zip(&orig) {
        let recovered = (o * 32768.0).round() as i32;
        assert_eq!(recovered, e, "FLAC engine round-trip is not lossless");
    }

    for p in [wav_in, flac, wav_out] {
        let _ = fs::remove_file(p);
    }
}
