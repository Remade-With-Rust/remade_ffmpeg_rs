//! Standing gate: every `MuxCaps` declaration matches the muxer it describes.
//!
//! `rff-targets` tells people what an input can be converted into, entirely on
//! the strength of these declarations. A declaration that has drifted away from
//! its muxer turns that promise into a lie, and the failure is silent — the UI
//! offers a target, and the conversion fails (or worse, writes a file with a
//! zero codec tag). So we drive the real muxers here:
//!
//! * **Positive** — every codec a format declares is accepted by its muxer.
//! * **Negative** — a codec a format does *not* declare is refused... by the
//!   muxers that validate at all. Several containers accept any stream at
//!   header time and only garble it later; those are listed in [`PERMISSIVE`]
//!   with what they actually do, so this test documents the gap instead of
//!   hiding it.

use std::io::Write;
use std::sync::{Arc, Mutex};

use rff_core::{CodecId, MediaType, PixelFormat, Rational, SampleFormat};
use rff_format::{Format, Stream};

/// Containers whose muxer accepts an unmappable codec at `write_header` instead
/// of refusing it. Each writes something wrong rather than erroring:
///
/// * `avi` — writes a zero fourcc, so readers misinterpret the payload (FFmpeg
///   falls back to `rawvideo` and then rejects every frame).
/// * `mpegts`, `hls` — fall back to `stream_type` 0x06 ("private data").
/// * `flv` — tags every audio track as AAC and every video track as AVC.
/// * `srt`, `webvtt` — write the packet payload out as cue text.
///
/// `mp4` was on this list and is not any more: [`codec_fourcc`] now returns
/// `Option`, and `write_header` refuses what it cannot describe. `dash` never
/// belonged here — it validated from the start; it is a path muxer, so the
/// byte-sink probes below skip it and never caught the mistake.
///
/// Removing a name from this list means that muxer learned to validate; that is
/// an improvement, and [`permissive_muxers_are_still_permissive`] will say so.
const PERMISSIVE: &[&str] = &["avi", "mpegts", "hls", "flv", "srt", "webvtt"];

/// An in-memory sink so the test never touches the filesystem.
#[derive(Clone, Default)]
struct Sink(Arc<Mutex<Vec<u8>>>);

impl Write for Sink {
    fn write(&mut self, buf: &[u8]) -> std::io::Result<usize> {
        self.0.lock().unwrap().extend_from_slice(buf);
        Ok(buf.len())
    }
    fn flush(&mut self) -> std::io::Result<()> {
        Ok(())
    }
}

/// A stream that is plausible for `codec` in every respect a muxer inspects:
/// dimensions for video, rate/channels/layout for audio.
fn stream(codec: CodecId) -> Stream {
    let mut s = Stream::new(0, codec);
    s.time_base = Rational::new(1, 1000);
    match codec.media_type() {
        MediaType::Video => {
            s.width = 320;
            s.height = 240;
            if codec == CodecId::RawVideo {
                s.pixel_format = Some(PixelFormat::Yuv420p);
            }
        }
        MediaType::Audio => {
            s.sample_rate = 48_000;
            s.channels = 2;
            if codec == CodecId::Pcm {
                s.sample_format = Some(SampleFormat::S16);
            }
        }
        _ => {}
    }
    s
}

/// Open a byte-sink muxer for `f`. Path muxers (HLS/DASH) are driven separately.
fn open(f: &Format) -> Option<Box<dyn rff_format::Muxer>> {
    f.muxer.map(|factory| factory(Box::new(Sink::default())))
}

fn engine() -> rff::Engine {
    rff::Engine::new()
}

#[test]
fn every_declared_codec_is_accepted_by_its_muxer() {
    let engine = engine();
    let mut checked = 0;
    for f in engine.formats.iter() {
        let Some(_) = f.muxer else { continue };
        for &codec in f.mux_caps.codecs {
            let mut muxer = open(f).expect("muxer present");
            let result = muxer.write_header(std::slice::from_ref(&stream(codec)));
            assert!(
                result.is_ok(),
                "`{}` declares it can mux `{codec}`, but its muxer refused: {}",
                f.name,
                result.unwrap_err()
            );
            checked += 1;
        }
    }
    assert!(checked >= 40, "only {checked} (format, codec) pairs checked");
}

#[test]
fn an_undeclared_codec_is_refused_by_every_validating_muxer() {
    let engine = engine();
    // One codec of each media type, so we always have something a container
    // does not declare but might still be asked to carry.
    let probes = [
        CodecId::Vp9,
        CodecId::H264,
        CodecId::Mp3,
        CodecId::Flac,
        CodecId::Png,
        CodecId::Subrip,
    ];
    for f in engine.formats.iter() {
        if f.muxer.is_none() || PERMISSIVE.contains(&f.name) {
            continue;
        }
        for codec in probes {
            if f.mux_caps.accepts(codec) {
                continue;
            }
            let mut muxer = open(f).expect("muxer present");
            let result = muxer.write_header(std::slice::from_ref(&stream(codec)));
            assert!(
                result.is_err(),
                "`{}` does not declare `{codec}`, but its muxer accepted one — either \
                 the declaration is missing a codec, or `{}` belongs in PERMISSIVE",
                f.name,
                f.name
            );
        }
    }
}

#[test]
fn permissive_muxers_are_still_permissive() {
    // If this fails, a muxer started validating: delete its PERMISSIVE entry
    // (and the note in `docs/compatibility.md`), because the negative gate above
    // now covers it for real.
    let engine = engine();
    let mut fixed = Vec::new();
    let mut probed = 0;
    for name in PERMISSIVE {
        let Some(f) = engine.formats.by_name(name) else {
            continue; // a path muxer (hls) has no byte-sink factory
        };
        let Some(_) = f.muxer else { continue };
        // Pick a codec of a media type the container does carry, so we are
        // testing codec validation rather than media-type rejection.
        let victim = [CodecId::Vp9, CodecId::Flac, CodecId::Subrip]
            .into_iter()
            .find(|c| !f.mux_caps.accepts(*c) && f.mux_caps.accepts_media(c.media_type()));
        let Some(victim) = victim else { continue };
        probed += 1;
        let mut muxer = open(f).expect("muxer present");
        if muxer
            .write_header(std::slice::from_ref(&stream(victim)))
            .is_err()
        {
            fixed.push(*name);
        }
    }
    assert!(
        fixed.is_empty(),
        "{fixed:?} now refuse an unmappable codec — good. Drop them from \
         PERMISSIVE (and update the note in `docs/compatibility.md`) so the \
         negative gate covers them for real.",
    );
    assert!(
        probed >= 3,
        "only {probed} PERMISSIVE formats were actually probed — this test is \
         no longer checking anything meaningful"
    );
}

#[test]
fn every_writable_format_declares_at_least_one_codec() {
    let engine = engine();
    for f in engine.formats.iter() {
        if f.can_mux() {
            assert!(
                !f.mux_caps.codecs.is_empty(),
                "`{}` has a muxer but declares no codecs, so nothing can target it",
                f.name
            );
        } else {
            assert!(
                f.mux_caps.codecs.is_empty(),
                "`{}` declares codecs but has no muxer",
                f.name
            );
        }
    }
}

#[test]
fn declared_codecs_are_registered_or_documented() {
    // A container declaring a codec the build has no encoder *or* decoder for
    // can still stream-copy it, which is legitimate — but it should be a codec
    // the engine otherwise knows about, not a typo.
    let engine = engine();
    for f in engine.formats.iter() {
        for &codec in f.mux_caps.codecs {
            let known = engine.codecs.by_id(codec).is_some()
                // Text-cue "codecs" ride the subtitle path, not the registry.
                || matches!(codec, CodecId::Subrip | CodecId::WebVtt);
            assert!(known, "`{}` declares unknown codec `{codec}`", f.name);
        }
    }
}
