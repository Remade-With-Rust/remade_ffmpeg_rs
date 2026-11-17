//! `Demuxer::set_discard` is a hint that must never change the kept packets:
//! for every stream, the packets a demuxer returns with the OTHER streams
//! discarded equal that stream's packets read with nothing discarded. Run on
//! MP4 (buffered and seekable readers) and Matroska, over H.264 (whose AVCC to
//! Annex-B conversion is what a discarded sample skips) interleaved with Opus.

use std::fs;
use std::path::{Path, PathBuf};

use rff::core::{CodecId, Dictionary};
use rff::transcode::{InputSpec, OutputSpec, StreamCodec, TranscodeSpec};
use rff::Engine;

fn tmp(name: &str, ext: &str) -> PathBuf {
    std::env::temp_dir().join(format!("rff_discard_{}_{name}.{ext}", std::process::id()))
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
    // 0.5 s at 8 kHz mono, 16-bit.
    let pcm: Vec<u8> = (0..4000)
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

type Kept = (usize, Option<i64>, Option<i64>, i64, bool, Vec<u8>);

fn read_all(
    engine: &Engine,
    format: &str,
    path: &Path,
    seekable: bool,
    discard: Option<&[bool]>,
) -> Vec<Kept> {
    let file = fs::File::open(path).unwrap();
    let mut dem = if seekable {
        engine
            .formats
            .open_demuxer_seekable(format, Box::new(file))
            .unwrap()
    } else {
        engine.formats.open_demuxer(format, Box::new(file)).unwrap()
    };
    dem.read_header().unwrap();
    if let Some(d) = discard {
        dem.set_discard(d);
    }
    let mut out = Vec::new();
    while let Ok(p) = dem.read_packet() {
        out.push((
            p.stream_index,
            p.pts,
            p.dts,
            p.duration,
            p.flags.keyframe,
            p.data,
        ));
    }
    out
}

#[test]
fn discarding_streams_leaves_the_kept_packets_unchanged() {
    let engine = Engine::new();
    let (y4m, wav, mp4, mkv) = (
        tmp("v", "y4m"),
        tmp("a", "wav"),
        tmp("av", "mp4"),
        tmp("av", "mkv"),
    );
    write_y4m(&y4m);
    write_wav(&wav);
    run(
        &engine,
        &[&y4m, &wav],
        &mp4,
        codec(CodecId::H264),
        codec(CodecId::Opus),
    );
    run(&engine, &[&mp4], &mkv, None, None);

    for (format, path, seekable) in [
        ("mp4", &mp4, false),
        ("mp4", &mp4, true),
        ("matroska", &mkv, false),
    ] {
        let all = read_all(&engine, format, path, seekable, None);
        let streams = all.iter().map(|p| p.0).max().unwrap() + 1;
        assert_eq!(streams, 2, "{format}: expected video + audio");
        for keep in 0..streams {
            let discard: Vec<bool> = (0..streams).map(|s| s != keep).collect();
            let kept = read_all(&engine, format, path, seekable, Some(&discard));
            let expected: Vec<Kept> = all.iter().filter(|p| p.0 == keep).cloned().collect();
            assert!(!expected.is_empty(), "{format}: stream {keep} has packets");
            assert_eq!(
                kept, expected,
                "{format} seekable={seekable}: stream {keep}"
            );
        }
    }

    for p in [y4m, wav, mp4, mkv] {
        let _ = fs::remove_file(p);
    }
}
