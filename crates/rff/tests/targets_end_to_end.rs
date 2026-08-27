//! The promise gate: every target `rff::targets` advertises can actually be
//! written.
//!
//! `crates/rff/tests/mux_caps.rs` checks the *declarations* against the muxers.
//! This one goes the whole way — it takes a real input, asks what it can be
//! converted into, and then runs every single answer through the transcoder.
//! If the planner ever advertises a conversion the engine refuses, a UI built
//! on it would offer a button that fails; this test fails first instead.

use std::fs;
use std::path::{Path, PathBuf};

use rff::core::{AudioFrame, CodecId, Dictionary, Frame, MediaType, SampleFormat};
use rff::format::Stream;
use rff::targets::{Action, Target};
use rff::transcode::{InputSpec, OutputSpec, StreamCodec, TranscodeSpec};
use rff::Engine;

fn tmp(name: &str, ext: &str) -> PathBuf {
    std::env::temp_dir().join(format!("rff_targets_{}_{name}.{ext}", std::process::id()))
}

/// A short mono PCM wav — small enough that even the slow encoders finish fast.
fn write_wav(engine: &Engine, path: &Path) {
    let pcm: Vec<u8> = (0..4000u32)
        .flat_map(|i| (((i as f32 * 0.05).sin() * 8000.0) as i16).to_le_bytes())
        .collect();
    let af = AudioFrame {
        sample_rate: 48_000,
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
    s.sample_rate = 48_000;
    s.channels = 1;
    s.sample_format = Some(SampleFormat::S16);
    mux.write_header(&[s]).unwrap();
    mux.write_packet(&packet).unwrap();
    mux.write_trailer().unwrap();
}

/// Turn a planned [`Target`] into the transcode it describes. This is the
/// translation a caller would write, so testing it tests the API's usability
/// as well as its truthfulness.
fn spec_for(target: &Target, input: &Path, out: &Path, max_frames: Option<u64>) -> TranscodeSpec {
    let codec_for = |media: MediaType| -> Option<StreamCodec> {
        target.kept().find(|s| s.media_type == media).and_then(|s| {
            match s.action {
                // A copy is the absence of a `-c:*` override.
                Action::Copy => None,
                Action::Transcode { to, .. } => Some(StreamCodec {
                    codec: to,
                    options: Dictionary::default(),
                    sample_format: None,
                }),
                Action::Drop(_) => None,
            }
        })
    };
    TranscodeSpec {
        inputs: vec![InputSpec {
            path: input.to_path_buf(),
            format: None,
        }],
        outputs: vec![OutputSpec {
            path: out.to_path_buf(),
            // Pin the container: `.mkv` and `.webm` share a muxer, and the
            // plan named exactly one of them.
            format: Some(target.format.to_string()),
            video_codec: codec_for(MediaType::Video),
            audio_codec: codec_for(MediaType::Audio),
            subtitle_codec: target
                .kept()
                .find(|s| s.media_type == MediaType::Subtitle)
                .and_then(|s| match s.action {
                    Action::Transcode { to, .. } => Some(to),
                    _ => None,
                }),
            overwrite: true,
            max_video_frames: max_frames,
            ..Default::default()
        }],
    }
}

/// Run every advertised target and report which ones the engine refused.
fn run_all(engine: &Engine, input: &Path, label: &str, max_frames: Option<u64>) -> usize {
    let plan = rff::targets::targets(engine, input).expect("probe the input");
    assert!(
        !plan.targets.is_empty(),
        "{label}: an input we can read must have at least one target"
    );

    let mut failures = Vec::new();
    for target in &plan.targets {
        let out = tmp(&format!("{label}_{}", target.format), target.extension);
        let _ = fs::remove_file(&out);
        match rff::transcode::run(engine, &spec_for(target, input, &out, max_frames)) {
            Ok(report) => {
                assert!(
                    report.packets_written > 0,
                    "{label} -> .{}: wrote no packets",
                    target.extension
                );
                // A path muxer (HLS/DASH) writes a playlist plus segments, so
                // the named path is a manifest either way — it must exist.
                assert!(
                    out.exists(),
                    "{label} -> .{}: reported success but wrote no file",
                    target.extension
                );
            }
            Err(err) => failures.push(format!(
                "  .{:<6} ({}) advertised `{}` but failed: {err}",
                target.extension,
                target.format,
                target.stream_summary()
            )),
        }
        let _ = fs::remove_file(&out);
    }

    assert!(
        failures.is_empty(),
        "{label}: {} of {} advertised targets do not work:\n{}",
        failures.len(),
        plan.targets.len(),
        failures.join("\n")
    );
    plan.targets.len()
}

#[test]
fn every_target_advertised_for_an_audio_input_actually_writes() {
    let engine = Engine::new();
    let wav = tmp("src", "wav");
    write_wav(&engine, &wav);
    let n = run_all(&engine, &wav, "wav", None);
    assert!(n >= 8, "expected a broad audio target list, got {n}");
    let _ = fs::remove_file(&wav);
}

#[test]
fn a_lossless_source_reports_its_lossless_targets_as_lossless() {
    // Truthfulness of the fidelity label, not just of the target list: PCM into
    // FLAC really is lossless, and PCM into MP3 really is not.
    let engine = Engine::new();
    let wav = tmp("fidelity", "wav");
    write_wav(&engine, &wav);
    let plan = rff::targets::targets(&engine, &wav).unwrap();

    assert_eq!(
        plan.target("wav").map(|t| t.fidelity),
        Some(rff::targets::Fidelity::Copy)
    );
    assert_eq!(
        plan.target("flac").map(|t| t.fidelity),
        Some(rff::targets::Fidelity::Lossless)
    );
    assert_eq!(
        plan.target("mp3").map(|t| t.fidelity),
        Some(rff::targets::Fidelity::Lossy)
    );
    // A lossless source is not a "generation loss" case, however lossy the
    // target: that note is reserved for lossy -> lossy.
    assert!(plan
        .targets
        .iter()
        .all(|t| !t.notes.iter().any(|n| n.contains("generation loss"))));
    let _ = fs::remove_file(&wav);
}

#[test]
fn a_video_input_offers_both_video_and_image_targets() {
    let engine = Engine::new();
    // A VP9 conformance vector is a real, small, in-repo video.
    let src = Path::new(env!("CARGO_MANIFEST_DIR"))
        .join("../../vp9-vectors/vp90-2-02-size-130x132.webm");
    if !src.exists() {
        eprintln!("skipping: {} not present", src.display());
        return;
    }
    let plan = rff::targets::targets(&engine, &src).unwrap();
    let kinds: Vec<_> = plan.targets.iter().map(|t| t.kind).collect();
    assert!(kinds.contains(&rff::targets::TargetKind::Video));
    assert!(
        kinds.contains(&rff::targets::TargetKind::Image),
        "a video can always be turned into a still"
    );
    // Video targets are listed before image ones for a video input.
    let first_image = kinds
        .iter()
        .position(|k| *k == rff::targets::TargetKind::Image)
        .unwrap();
    let last_video = kinds
        .iter()
        .rposition(|k| *k == rff::targets::TargetKind::Video)
        .unwrap();
    assert!(last_video < first_image, "video targets should lead");

    // The lossless remuxes are the ones that need no encoder at all.
    let copies: Vec<_> = plan.stream_copies().map(|t| t.format).collect();
    assert!(copies.contains(&"webm"), "vp9 remuxes into webm: {copies:?}");
    assert!(copies.contains(&"ivf"), "vp9 remuxes into ivf: {copies:?}");
    assert!(
        !copies.contains(&"mp4"),
        "mp4 cannot carry vp9 here, so it must not be advertised as a copy"
    );
}

#[test]
fn a_target_list_never_promises_a_format_this_build_cannot_mux() {
    let engine = Engine::new();
    let wav = tmp("promise", "wav");
    write_wav(&engine, &wav);
    for t in &rff::targets::targets(&engine, &wav).unwrap().targets {
        let f = engine.formats.by_name(t.format).expect("registered format");
        assert!(f.can_mux(), "advertised `{}`, which has no muxer", t.format);
    }
    let _ = fs::remove_file(&wav);
}

/// A one-cue SubRip file.
fn write_srt(path: &Path) {
    fs::write(path, "1
00:00:01,000 --> 00:00:03,000
hello

").unwrap();
}

#[test]
fn every_target_advertised_for_a_subtitle_input_actually_writes() {
    let engine = Engine::new();
    let srt = tmp("src", "srt");
    write_srt(&srt);
    let n = run_all(&engine, &srt, "srt", None);
    assert!(n >= 3, "srt should reach at least srt/vtt/mkv, got {n}");

    // A subtitle input must not be offered audio or video containers it cannot
    // fill: those would produce an empty file.
    let plan = rff::targets::targets(&engine, &srt).unwrap();
    assert!(plan.target("wav").is_none());
    assert!(plan.target("mp3").is_none());
    assert!(plan.target("ivf").is_none());
    let _ = fs::remove_file(&srt);
}

#[test]
fn every_target_advertised_for_a_video_input_actually_writes() {
    let engine = Engine::new();
    let src = Path::new(env!("CARGO_MANIFEST_DIR"))
        .join("../../vp9-vectors/vp90-2-02-size-130x132.webm");
    if !src.exists() {
        eprintln!("skipping: {} not present", src.display());
        return;
    }
    // One frame is enough to prove the target works, and keeps the sweep over
    // every video *and* image encoder affordable.
    let n = run_all(&engine, &src, "vp9", Some(1));
    assert!(n >= 12, "expected video + image targets, got {n}");
}
