//! Subtitle conversion through the engine: srt → vtt, ass → srt, and
//! srt → Matroska → back. All ride the shared text-cue packet contract, so
//! each conversion is a (relabelled) stream copy.

use std::fs;
use std::path::PathBuf;

use rff::core::CodecId;
use rff::transcode::{InputSpec, OutputSpec, TranscodeSpec};
use rff::Engine;

fn tmp(name: &str, ext: &str) -> PathBuf {
    std::env::temp_dir().join(format!("rff_subs_{}_{name}.{ext}", std::process::id()))
}

const SRT: &str = "1\n00:00:01,000 --> 00:00:02,500\nHello there\n\n2\n00:00:03,000 --> 00:00:04,000\nSecond cue\n";

fn convert(engine: &Engine, from: &PathBuf, to: &PathBuf, codec: Option<CodecId>) {
    let spec = TranscodeSpec {
        inputs: vec![InputSpec {
            path: from.clone(),
            format: None,
        }],
        outputs: vec![OutputSpec {
            path: to.clone(),
            overwrite: true,
            subtitle_codec: codec,
            ..Default::default()
        }],
    };
    rff::transcode::run(engine, &spec).expect("subtitle convert");
}

#[test]
fn srt_converts_to_vtt() {
    let engine = Engine::new();
    let (src, dst) = (tmp("s2v_in", "srt"), tmp("s2v_out", "vtt"));
    fs::write(&src, SRT).unwrap();
    convert(&engine, &src, &dst, None);

    let vtt = fs::read_to_string(&dst).unwrap();
    assert!(vtt.starts_with("WEBVTT"), "{vtt}");
    assert!(vtt.contains("00:00:01.000 --> 00:00:02.500"), "{vtt}");
    assert!(vtt.contains("Hello there"));
    for p in [src, dst] {
        let _ = fs::remove_file(p);
    }
}

#[test]
fn ass_converts_to_srt_with_tags_stripped() {
    let engine = Engine::new();
    let (src, dst) = (tmp("a2s_in", "ass"), tmp("a2s_out", "srt"));
    fs::write(
        &src,
        "[Script Info]\nTitle: x\n\n[Events]\nFormat: Layer, Start, End, Style, Name, MarginL, MarginR, MarginV, Effect, Text\nDialogue: 0,0:00:01.00,0:00:02.50,Default,,0,0,0,,{\\i1}Styled{\\i0} text\\Nline two\n",
    )
    .unwrap();
    convert(&engine, &src, &dst, None);

    let srt = fs::read_to_string(&dst).unwrap();
    assert!(srt.contains("00:00:01,000 --> 00:00:02,500"), "{srt}");
    assert!(srt.contains("Styled text\nline two") || srt.contains("Styled text\r\nline two"));
    assert!(!srt.contains('{'), "override tags leaked: {srt}");
    for p in [src, dst] {
        let _ = fs::remove_file(p);
    }
}

#[test]
fn srt_rides_into_mkv_and_back_out() {
    let engine = Engine::new();
    let (src, mkv, back) = (
        tmp("mkv_in", "srt"),
        tmp("mkv_mid", "mkv"),
        tmp("mkv_out", "srt"),
    );
    fs::write(&src, SRT).unwrap();
    convert(&engine, &src, &mkv, None);

    // The mkv carries a subtitle track.
    let info = rff::probe::probe(&engine, &mkv).unwrap();
    assert_eq!(info.format_name, "matroska");
    assert_eq!(info.streams[0].codec_id, CodecId::Subrip);

    convert(&engine, &mkv, &back, None);
    let srt = fs::read_to_string(&back).unwrap();
    assert!(srt.contains("Hello there"), "{srt}");
    assert!(srt.contains("00:00:03,000 --> 00:00:04,000"), "{srt}");
    for p in [src, mkv, back] {
        let _ = fs::remove_file(p);
    }
}
