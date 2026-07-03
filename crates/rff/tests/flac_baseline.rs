//! Compression baseline: our in-house FLAC encoder vs ffmpeg's (libavcodec) FLAC
//! on the same PCM, so we know where our ratio sits relative to the reference.
//! `#[ignore]`d (needs `ffmpeg` on PATH). Run:
//!   cargo test -p rff --test flac_baseline -- --ignored --nocapture

use std::fs;
use std::path::{Path, PathBuf};
use std::process::Command;

use rff::core::{AudioFrame, CodecId, Dictionary, Frame, SampleFormat};
use rff::format::Stream;
use rff::transcode::{InputSpec, OutputSpec, StreamCodec, TranscodeSpec};
use rff::Engine;

fn tmp(name: &str) -> PathBuf {
    std::env::temp_dir().join(format!("rff_flacbase_{}_{name}", std::process::id()))
}

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

fn transcode_flac(engine: &Engine, input: &Path, output: &Path) {
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
                codec: CodecId::Flac,
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

#[test]
#[ignore = "baseline vs ffmpeg; needs ffmpeg on PATH; run with --ignored --nocapture"]
fn flac_ratio_vs_ffmpeg() {
    let engine = Engine::new();
    let sr = 44_100u32;
    let n = sr as usize * 8; // 8 s stereo — metadata overhead is negligible here

    // Music-like: 5 harmonics of 220 Hz, a 0.3 Hz tremolo envelope, light noise,
    // and a correlated right channel — exercises LPC, stereo decorrelation, and
    // partitioned Rice all at once.
    let mut pcm = Vec::with_capacity(n * 4);
    let mut st = 0x1234_5678u32;
    let mut prevl = 0i32;
    for i in 0..n {
        let t = i as f64 / sr as f64;
        let env = 0.5 + 0.5 * (2.0 * std::f64::consts::PI * 0.3 * t).sin();
        let mut s = 0.0;
        for h in 1..=5 {
            s += (2.0 * std::f64::consts::PI * 220.0 * h as f64 * t).sin() / h as f64;
        }
        st ^= st << 13;
        st ^= st >> 17;
        st ^= st << 5;
        let noise = ((st >> 24) as f64 - 128.0) / 128.0 * 0.02;
        let l = ((s * env * 0.3 + noise) * 20000.0).clamp(-32768.0, 32767.0) as i32;
        let r = (l - (l - prevl) / 6).clamp(-32768, 32767);
        prevl = l;
        pcm.extend_from_slice(&(l as i16).to_le_bytes());
        pcm.extend_from_slice(&(r as i16).to_le_bytes());
    }

    let wav = tmp("in.wav");
    let ours = tmp("ours.flac");
    let ff = tmp("ff.flac");
    write_wav(&engine, &wav, &pcm, sr, 2);
    transcode_flac(&engine, &wav, &ours);

    // ffmpeg at its strongest (-compression_level 8), metadata stripped for fairness.
    let ffmpeg = std::env::var("FFMPEG_BIN").unwrap_or_else(|_| "ffmpeg".to_string());
    let ff_ok = Command::new(&ffmpeg)
        .args(["-v", "error", "-y", "-i"])
        .arg(&wav)
        .args([
            "-c:a",
            "flac",
            "-compression_level",
            "8",
            "-map_metadata",
            "-1",
            "-metadata_header_padding",
            "0",
            "-fflags",
            "+bitexact",
        ])
        .arg(&ff)
        .status()
        .map(|s| s.success())
        .unwrap_or(false);

    let raw = pcm.len() as f64;
    let our_size = fs::metadata(&ours).map(|m| m.len()).unwrap_or(0) as f64;
    eprintln!("\n=== FLAC compression baseline (8 s stereo, 44.1 kHz) ===");
    eprintln!("  raw PCM : {:>9} B", raw as u64);
    eprintln!(
        "  ours    : {:>9} B  ({:.1}% of raw)",
        our_size as u64,
        100.0 * our_size / raw
    );
    if ff_ok {
        let ff_size = fs::metadata(&ff).map(|m| m.len()).unwrap_or(0) as f64;
        eprintln!(
            "  ffmpeg-8: {:>9} B  ({:.1}% of raw)",
            ff_size as u64,
            100.0 * ff_size / raw
        );
        eprintln!(
            "  ours / ffmpeg : {:.1}%   (100% = parity; >100% = we are larger)",
            100.0 * our_size / ff_size
        );
    } else {
        eprintln!("  ffmpeg  : (not found on PATH — comparison skipped)");
    }
    eprintln!();

    for p in [wav, ours, ff] {
        let _ = fs::remove_file(p);
    }
}
