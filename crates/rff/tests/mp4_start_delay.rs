//! #115: a video track that starts after the audio keeps its start delay
//! through a remux into MP4 -- written as an empty edit, the way FFmpeg's
//! movenc writes it -- whether the source is MP4 or Matroska.

use std::fs;
use std::path::{Path, PathBuf};

use rff::core::{CodecId, Dictionary};
use rff::transcode::{InputSpec, OutputSpec, StreamCodec, TranscodeSpec};
use rff::Engine;

fn tmp(name: &str, ext: &str) -> PathBuf {
    std::env::temp_dir().join(format!("rff_delay_{}_{name}.{ext}", std::process::id()))
}

fn write_y4m(path: &Path) {
    let (w, h, frames) = (64usize, 64usize, 12usize);
    let mut out = b"YUV4MPEG2 W64 H64 F25:1 Ip A1:1 C420jpeg\n".to_vec();
    for n in 0..frames {
        out.extend_from_slice(b"FRAME\n");
        out.extend((0..w * h).map(|i| ((i + n * 9) % 251) as u8));
        out.extend(std::iter::repeat_n(128u8, w * h / 2));
    }
    fs::write(path, out).unwrap();
}

fn write_wav(path: &Path) {
    // 1 s at 8 kHz mono, 16-bit.
    let pcm: Vec<u8> = (0..8000)
        .flat_map(|i| (((i as f32 * 0.2).sin() * 12000.0) as i16).to_le_bytes())
        .collect();
    let mut out = b"RIFF".to_vec();
    out.extend_from_slice(&(36 + pcm.len() as u32).to_le_bytes());
    out.extend_from_slice(b"WAVEfmt ");
    for v in [16u32, 0x0001_0001, 8000, 16000, 0x0010_0002] {
        out.extend_from_slice(&v.to_le_bytes());
    }
    out.extend_from_slice(b"data");
    out.extend_from_slice(&(pcm.len() as u32).to_le_bytes());
    out.extend_from_slice(&pcm);
    fs::write(path, out).unwrap();
}

fn codec(codec: CodecId) -> Option<StreamCodec> {
    Some(StreamCodec {
        codec,
        options: Dictionary::new(),
        sample_format: None,
    })
}

fn run(
    engine: &Engine,
    inputs: &[&Path],
    out: &Path,
    video: Option<StreamCodec>,
    audio: Option<StreamCodec>,
) {
    let spec = TranscodeSpec {
        inputs: inputs
            .iter()
            .map(|p| InputSpec {
                path: p.to_path_buf(),
                format: None,
            })
            .collect(),
        outputs: vec![OutputSpec {
            path: out.to_path_buf(),
            video_codec: video,
            audio_codec: audio,
            overwrite: true,
            ..Default::default()
        }],
    };
    rff::transcode::run(engine, &spec).expect("transcode");
}

/// Each stream's first presentation time, in seconds.
fn first_shown(engine: &Engine, format: &str, path: &Path) -> Vec<f64> {
    let mut dem = engine
        .formats
        .open_demuxer(format, Box::new(fs::File::open(path).unwrap()))
        .unwrap();
    let streams = dem.read_header().unwrap();
    let mut first = vec![f64::INFINITY; streams.len()];
    while let Ok(p) = dem.read_packet() {
        let tb = streams[p.stream_index].time_base;
        let at = p.pts.unwrap() as f64 * f64::from(tb.num) / f64::from(tb.den);
        first[p.stream_index] = first[p.stream_index].min(at);
    }
    first
}

/// Rewrite `src` (MP4) with stream 0's packets about `delay` seconds later,
/// returning the delay applied (a whole number of time-base ticks).
fn delay_stream0(engine: &Engine, src: &Path, out: &Path, delay: f64) -> f64 {
    let mut dem = engine
        .formats
        .open_demuxer("mp4", Box::new(fs::File::open(src).unwrap()))
        .unwrap();
    let streams = dem.read_header().unwrap();
    let tb = streams[0].time_base;
    let shift = (delay * f64::from(tb.den) / f64::from(tb.num)).round() as i64;
    let mut mux = engine
        .formats
        .open_muxer_seekable("mp4", Box::new(fs::File::create(out).unwrap()))
        .unwrap();
    mux.write_header(&streams).unwrap();
    while let Ok(mut p) = dem.read_packet() {
        if p.stream_index == 0 {
            p.pts = p.pts.map(|t| t + shift);
            p.dts = p.dts.map(|t| t + shift);
        }
        mux.write_packet(&p).unwrap();
    }
    mux.write_trailer().unwrap();
    shift as f64 * f64::from(tb.num) / f64::from(tb.den)
}

#[test]
fn a_video_start_delay_survives_a_remux_into_mp4() {
    let engine = Engine::new();
    let (y4m, wav) = (tmp("v", "y4m"), tmp("a", "wav"));
    let (base, delayed, mkv) = (
        tmp("base", "mp4"),
        tmp("delayed", "mp4"),
        tmp("delayed", "mkv"),
    );
    let (from_mp4, from_mkv) = (tmp("from_mp4", "mp4"), tmp("from_mkv", "mp4"));
    write_y4m(&y4m);
    write_wav(&wav);
    run(
        &engine,
        &[&y4m, &wav],
        &base,
        codec(CodecId::H264),
        codec(CodecId::Opus),
    );
    let delay = delay_stream0(&engine, &base, &delayed, 0.5);
    run(&engine, &[&delayed], &from_mp4, None, None);
    run(&engine, &[&delayed], &mkv, None, None);
    run(&engine, &[&mkv], &from_mkv, None, None);

    let lead = |t: &[f64]| t[0] - t[1];
    let want = lead(&first_shown(&engine, "mp4", &base)) + delay;
    assert!(delay > 0.4, "the source must actually be delayed");
    for (format, path) in [
        ("mp4", &delayed),
        ("mp4", &from_mp4),
        ("matroska", &mkv),
        ("mp4", &from_mkv),
    ] {
        let got = lead(&first_shown(&engine, format, path));
        assert!(
            (got - want).abs() < 0.002,
            "{}: video starts {got:.3} s after the audio, want {want:.3} s",
            path.display()
        );
    }

    for p in [y4m, wav, base, delayed, mkv, from_mp4, from_mkv] {
        let _ = fs::remove_file(p);
    }
}
