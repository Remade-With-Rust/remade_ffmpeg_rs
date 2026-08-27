//! MP4 must never write a file that opens and then decodes nothing.
//!
//! The muxer used to accept any codec and fall back to a **zero fourcc** for
//! the ones it could not describe. Nothing failed: `write_trailer` returned
//! `Ok`, the file existed and had a plausible size, and the breakage only
//! surfaced in someone else's player (FFmpeg reads the `\0\0\0\0` sample entry,
//! falls back to `rawvideo`, and rejects every frame).
//!
//! `crates/rff-format-mp4/src/lib.rs` has the unit-level refusals. This file
//! drives the *whole engine* — the path a user actually takes — and then reads
//! the bytes back to confirm what landed on disk.

use std::fs;
use std::path::{Path, PathBuf};

use rff::core::{AudioFrame, CodecId, Dictionary, Frame, SampleFormat};
use rff::format::Stream;
use rff::transcode::{InputSpec, OutputSpec, StreamCodec, TranscodeSpec};
use rff::Engine;

fn tmp(name: &str, ext: &str) -> PathBuf {
    std::env::temp_dir().join(format!("rff_mp4corrupt_{}_{name}.{ext}", std::process::id()))
}

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

fn convert(engine: &Engine, input: &Path, out: &Path, audio: Option<CodecId>) -> rff::core::Result<()> {
    let spec = TranscodeSpec {
        inputs: vec![InputSpec {
            path: input.to_path_buf(),
            format: None,
        }],
        outputs: vec![OutputSpec {
            path: out.to_path_buf(),
            format: Some("mp4".into()),
            audio_codec: audio.map(|codec| StreamCodec {
                codec,
                options: Dictionary::default(),
                sample_format: None,
            }),
            overwrite: true,
            ..Default::default()
        }],
    };
    rff::transcode::run(engine, &spec).map(|_| ())
}

/// Every `stsd` sample-entry fourcc in an MP4, in file order.
///
/// Layout after the `stsd` tag: version+flags(4), entry_count(4),
/// entry_size(4), then the four-character format.
fn sample_entry_fourccs(file: &[u8]) -> Vec<[u8; 4]> {
    let mut out = Vec::new();
    for (i, w) in file.windows(4).enumerate() {
        if w == b"stsd" {
            if let Some(fourcc) = file.get(i + 16..i + 20) {
                out.push(fourcc.try_into().unwrap());
            }
        }
    }
    out
}

#[test]
fn an_unmappable_codec_is_refused_instead_of_written_as_a_zero_fourcc() {
    let engine = Engine::new();
    let wav = tmp("src", "wav");
    let out = tmp("copy", "mp4");
    write_wav(&engine, &wav);
    let _ = fs::remove_file(&out);

    // No `-c:a`, so the engine tries to stream-copy PCM into MP4. MP4 has no
    // sample entry for PCM here; before the fix this wrote a zero fourcc and
    // returned Ok.
    let err = convert(&engine, &wav, &out, None).expect_err("pcm cannot be muxed into mp4");
    let msg = err.to_string();
    assert!(msg.contains("pcm"), "the error must name the codec: {msg}");
    assert!(
        msg.contains("-c:a aac"),
        "the error must say how to proceed: {msg}"
    );

    let _ = fs::remove_file(&wav);
    let _ = fs::remove_file(&out);
}

#[test]
fn a_supported_codec_writes_a_real_sample_entry() {
    let engine = Engine::new();
    let wav = tmp("src2", "wav");
    let out = tmp("aac", "mp4");
    write_wav(&engine, &wav);
    let _ = fs::remove_file(&out);

    convert(&engine, &wav, &out, Some(CodecId::Aac)).expect("wav -> aac in mp4");

    let file = fs::read(&out).unwrap();
    let fourccs = sample_entry_fourccs(&file);
    assert_eq!(fourccs.len(), 1, "one audio track expected");
    assert_eq!(&fourccs[0], b"mp4a");

    // The engine reads its own file back as AAC — not as an unknown track.
    let info = rff::probe::probe(&engine, &out).expect("probe the mp4 we wrote");
    assert_eq!(info.streams.len(), 1);
    assert_eq!(info.streams[0].codec_id, CodecId::Aac);

    let _ = fs::remove_file(&wav);
    let _ = fs::remove_file(&out);
}

#[test]
fn no_mp4_this_engine_writes_carries_a_zero_fourcc() {
    // The invariant, stated directly against the bytes: whatever we manage to
    // write, every sample entry names a real codec.
    let engine = Engine::new();
    let wav = tmp("src3", "wav");
    write_wav(&engine, &wav);

    for codec in [CodecId::Aac, CodecId::Opus] {
        let out = tmp(&format!("scan_{}", codec.name()), "mp4");
        let _ = fs::remove_file(&out);
        convert(&engine, &wav, &out, Some(codec)).unwrap_or_else(|e| {
            panic!("{} is declared muxable into mp4 but failed: {e}", codec.name())
        });
        let file = fs::read(&out).unwrap();
        let fourccs = sample_entry_fourccs(&file);
        assert!(!fourccs.is_empty(), "{}: no sample entry at all", codec.name());
        for fourcc in fourccs {
            assert_ne!(
                &fourcc,
                b"\0\0\0\0",
                "{}: wrote a zero fourcc",
                codec.name()
            );
            assert!(
                fourcc.iter().all(|b| b.is_ascii_graphic()),
                "{}: wrote a non-printable fourcc {fourcc:?}",
                codec.name()
            );
        }
        let _ = fs::remove_file(&out);
    }
    let _ = fs::remove_file(&wav);
}
