//! The ffmpeg arm — the bar the user actually measures against, at DEFAULTS.
//!
//! Encode is `ffmpeg -i clip.y4m -c:v libvpx-vp9 out.ivf` with nothing else set
//! but `-threads 1`; decode is ffmpeg's own native VP9 decoder (ffvp9, the
//! `vp9` decoder — not `libvpx-vp9`), which is what a plain `ffmpeg -i in.webm`
//! actually selects.
//!
//! TOTALS ONLY on this side. The shipped ffmpeg is fully stripped, so no
//! per-function attribution is possible from it; the function-level reference
//! numbers come from `_ref_libvpx`, which was verified byte-identical and
//! speed-matched to ffmpeg's own libvpx at matched settings.
//!
//! Every duration here is NET OF PROCESS STARTUP, measured once against a
//! do-nothing invocation. That is not a rounding detail: startup is tens of
//! milliseconds, which on a QCIF clip is several times the actual decode, and
//! left in it would make every stream report the same throughput — the startup,
//! not the codec.

use std::path::Path;
use std::process::Command;
use std::sync::OnceLock;
use std::time::{Duration, Instant};

pub fn bin() -> String {
    // NOTE: our own CLI also installs as `ffmpeg.exe`. `FFMPEG` must point at
    // the real upstream binary; the default resolves through PATH, which on this
    // machine is the winget-installed build.
    std::env::var("FFMPEG").unwrap_or_else(|_| "ffmpeg".into())
}

/// Fixed cost of spawning ffmpeg and doing essentially no work, measured once.
pub fn startup() -> Duration {
    static T: OnceLock<Duration> = OnceLock::new();
    *T.get_or_init(|| {
        let mut best = Duration::MAX;
        for _ in 0..5 {
            let t = Instant::now();
            let ok = Command::new(bin())
                .args([
                    "-hide_banner", "-loglevel", "error", "-nostdin",
                    "-f", "lavfi", "-i", "nullsrc=s=16x16", "-frames:v", "1",
                    "-f", "null", "-",
                ])
                .output()
                .map(|o| o.status.success())
                .unwrap_or(false);
            if ok {
                best = best.min(t.elapsed());
            }
        }
        if best == Duration::MAX {
            Duration::ZERO
        } else {
            best
        }
    })
}

fn timed(args: &[&str], reps: usize) -> Option<Duration> {
    let mut best = Duration::MAX;
    for _ in 0..reps.max(1) {
        let t = Instant::now();
        let o = Command::new(bin()).args(args).output().ok()?;
        if !o.status.success() {
            eprintln!(
                "    ! ffmpeg failed: {}",
                String::from_utf8_lossy(&o.stderr).lines().last().unwrap_or("?")
            );
            return None;
        }
        best = best.min(t.elapsed());
    }
    Some(best.saturating_sub(startup()).max(Duration::from_micros(1)))
}

/// Encode a y4m with ffmpeg at its defaults for the given codec, single-threaded.
/// `codec_args` is e.g. `["-c:v", "libvpx-vp9"]` or `["-c:v", "libaom-av1"]` —
/// nothing else is set, so ffmpeg resolves its own defaults for everything the
/// wrapper decides (bitrate, GOP length, speed preset).
pub fn encode_with(
    src: &Path,
    out: &Path,
    codec_args: &[&str],
    reps: usize,
) -> Option<(Duration, usize)> {
    let src = src.to_string_lossy().into_owned();
    let out_s = out.to_string_lossy().into_owned();
    let mut args: Vec<&str> = vec![
        "-hide_banner", "-loglevel", "error", "-nostdin", "-y", "-i", &src,
    ];
    args.extend_from_slice(codec_args);
    args.extend_from_slice(&["-threads", "1", &out_s]);
    let d = timed(&args, reps)?;
    let bytes = std::fs::metadata(out).map(|m| m.len() as usize).unwrap_or(0);
    Some((d, bytes))
}

/// Encode with `libvpx-vp9` (the VP9 analyzer's bar).
pub fn encode(src: &Path, out: &Path, reps: usize) -> Option<(Duration, usize)> {
    encode_with(src, out, &["-c:v", "libvpx-vp9"], reps)
}

/// Decode with a named ffmpeg decoder, single-threaded, net of process startup.
/// `codec_args` is e.g. `["-c:v", "vp9"]` (ffvp9) or `["-c:v", "libdav1d"]`.
pub fn decode_with(src: &Path, codec_args: &[&str], reps: usize) -> Option<Duration> {
    let src = src.to_string_lossy().into_owned();
    let mut args: Vec<&str> = vec!["-hide_banner", "-loglevel", "error", "-nostdin", "-threads", "1"];
    args.extend_from_slice(codec_args);
    args.extend_from_slice(&["-i", &src, "-f", "null", "-"]);
    timed(&args, reps)
}

/// Decode with ffmpeg's native VP9 decoder (ffvp9).
pub fn decode(src: &Path, reps: usize) -> Option<Duration> {
    decode_with(src, &["-c:v", "vp9"], reps)
}

/// Decode `src` to raw I420 in memory — the neutral reconstruction used for the
/// quality metric, so neither encoder is scored by its own decoder.
pub fn decode_to_yuv(src: &Path) -> Option<Vec<u8>> {
    let o = Command::new(bin())
        .args(["-hide_banner", "-loglevel", "error", "-nostdin", "-threads", "1", "-i"])
        .arg(src)
        .args(["-f", "rawvideo", "-pix_fmt", "yuv420p", "-"])
        .output()
        .ok()?;
    if !o.status.success() || o.stdout.is_empty() {
        return None;
    }
    Some(o.stdout)
}
