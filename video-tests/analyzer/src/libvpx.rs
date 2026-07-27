//! The libvpx reference side: drive the external binaries built in `_ref_libvpx`.
//!
//! FOUR binaries, deliberately:
//!   * `vp9enc.exe` / `vp9dec.exe`           — stock, zero instrumentation.
//!   * `vp9enc-prof.exe` / `vp9dec-prof.exe` — rdtsc stage taps (`-DVPXPROF`).
//!
//! Measuring throughput on the instrumented build would tax libvpx with overhead
//! our own profiler-off runs don't pay, so speed comes from the stock pair and
//! the breakdown from the prof pair — the same split `_ref_x264` uses.
//!
//! That reference is configured `--target=x86_64-win64-gcc` with SIMD through
//! AVX2 and runtime CPU detect, and it was verified to produce a BYTE-IDENTICAL
//! bitstream and matching wall time against ffmpeg's shipped `libvpx-vp9` at the
//! same settings — so its milliseconds stand in for ffmpeg's, and unlike ffmpeg
//! (fully stripped, 0 symbols) it can be attributed per function.

use std::path::{Path, PathBuf};
use std::process::Command;
use std::time::Duration;

/// Root of the external reference checkout. Override with `LIBVPX_DIR`.
pub fn dir() -> PathBuf {
    std::env::var("LIBVPX_DIR")
        .map(PathBuf::from)
        .unwrap_or_else(|_| PathBuf::from("../../../_ref_libvpx"))
}

pub fn bin(tool: &str, prof: bool) -> PathBuf {
    let name = if prof {
        format!("{tool}-prof.exe")
    } else {
        format!("{tool}.exe")
    };
    dir().join(name)
}

/// Every binary the harness needs, or a message naming what to build.
pub fn check() -> Result<(), String> {
    let missing: Vec<String> = [("vp9enc", false), ("vp9enc", true), ("vp9dec", false), ("vp9dec", true)]
        .iter()
        .filter(|(t, p)| !bin(t, *p).exists())
        .map(|(t, p)| bin(t, *p).display().to_string())
        .collect();
    if missing.is_empty() {
        return Ok(());
    }
    Err(format!(
        "libvpx reference not built ({} missing). Run:\n  cd {} && python instrument.py && bash build.sh && bash build.sh prof",
        missing.join(", "),
        dir().display()
    ))
}

pub struct Run {
    /// Whole-process wall clock, INCLUDING startup/init/teardown.
    pub wall: Duration,
    /// The binary's OWN reported codec-loop time. This is the number to compare
    /// against our in-process loop: process startup is ~10-20 ms here, which
    /// would swamp a 25 ms QCIF encode and make libvpx look several times slower
    /// than it is.
    pub inner: Duration,
    pub bytes: usize,
    pub frames: usize,
    /// (stage, ms, calls, pct) harvested from the prof build's TSV dump.
    pub stages: Vec<(String, f64, u64, f64)>,
    /// (counter, value) — the fine-grained call counts.
    pub counts: Vec<(String, u64)>,
}

/// "encoded 120 frames, 3972.407 ms, 15.10 fps" / "decoded 120 frames, ...".
fn parse_self_report(log: &str) -> (usize, Duration) {
    for line in log.lines().rev() {
        let l = line.trim();
        if !(l.starts_with("encoded ") || l.starts_with("decoded ")) {
            continue;
        }
        let mut tok = l.split_whitespace();
        let _verb = tok.next();
        let frames: usize = tok.next().and_then(|s| s.parse().ok()).unwrap_or(0);
        let _frames_word = tok.next();
        let ms: f64 = tok
            .next()
            .and_then(|s| s.trim_end_matches(',').parse().ok())
            .unwrap_or(0.0);
        return (frames, Duration::from_secs_f64(ms / 1e3));
    }
    (0, Duration::ZERO)
}

fn parse_stages(path: &Path) -> (Vec<(String, f64, u64, f64)>, Vec<(String, u64)>) {
    let txt = std::fs::read_to_string(path).unwrap_or_default();
    let _ = std::fs::remove_file(path);
    let mut stages = Vec::new();
    let mut counts = Vec::new();
    for l in txt.lines() {
        let f: Vec<&str> = l.split('\t').collect();
        if f.first() == Some(&"vpxprof") && f.len() >= 5 {
            stages.push((
                f[1].to_string(),
                f[2].parse().unwrap_or(0.0),
                f[3].parse().unwrap_or(0),
                f[4].parse().unwrap_or(0.0),
            ));
        } else if f.first() == Some(&"vpxcount") && f.len() >= 3 {
            counts.push((f[1].to_string(), f[2].parse().unwrap_or(0)));
        }
    }
    (stages, counts)
}

fn run(exe: &Path, args: &[&str], prof_out: Option<&Path>) -> Result<Run, String> {
    let mut cmd = Command::new(exe);
    cmd.args(args);
    match prof_out {
        Some(p) => {
            cmd.env("VPXPROF_OUT", p);
        }
        None => {
            cmd.env_remove("VPXPROF_OUT");
        }
    }
    let t = std::time::Instant::now();
    let o = cmd
        .output()
        .map_err(|e| format!("spawn {}: {e}", exe.display()))?;
    let wall = t.elapsed();
    let log = String::from_utf8_lossy(&o.stderr).into_owned();
    if !o.status.success() {
        return Err(format!(
            "{} failed: {}",
            exe.file_name().unwrap_or_default().to_string_lossy(),
            log.lines().last().unwrap_or("?")
        ));
    }
    let (frames, inner) = parse_self_report(&log);
    let (stages, counts) = match prof_out {
        Some(p) => parse_stages(p),
        None => (Vec::new(), Vec::new()),
    };
    Ok(Run {
        wall,
        inner,
        bytes: 0,
        frames,
        stages,
        counts,
    })
}

/// Encode `src` (a y4m) to `out` (an IVF) at libvpx's DEFAULT settings, pinned to
/// one thread. `prof` selects the instrumented binary and harvests its taps.
pub fn encode(src: &Path, out: &Path, prof: bool) -> Result<Run, String> {
    encode_cfg(src, out, prof, &[])
}

/// Encode with EXTRA arguments appended after the pinned `--threads 1` — the
/// matched-operating-point path (`--crf N --cpu-used N --lag N`). `encode` is
/// this with an empty slice, so the default arm and the matched arm differ only
/// in the flags, never in the driver.
///
/// Best-of-N is NOT done here: the reference reports its own codec-loop time and
/// re-running it would also re-encode the file. The caller repeats if it wants a
/// distribution.
pub fn encode_cfg(src: &Path, out: &Path, prof: bool, extra: &[String]) -> Result<Run, String> {
    let exe = bin("vp9enc", prof);
    let prof_out = out.with_extension("vpxprof.tsv");
    let src_s = src.to_string_lossy().into_owned();
    let out_s = out.to_string_lossy().into_owned();
    let mut args: Vec<&str> = vec![&src_s, &out_s, "--threads", "1"];
    args.extend(extra.iter().map(String::as_str));
    let mut r = run(&exe, &args, if prof { Some(&prof_out) } else { None })?;
    r.bytes = std::fs::metadata(out).map(|m| m.len() as usize).unwrap_or(0);
    Ok(r)
}

/// Best-of-N wrapper: the reference is an external process, so the only honest
/// repeat is a full re-invocation. Keeps the fastest reported codec-loop time.
pub fn encode_best(src: &Path, out: &Path, extra: &[String], reps: usize) -> Result<Run, String> {
    let mut best: Option<Run> = None;
    for _ in 0..reps.max(1) {
        let r = encode_cfg(src, out, false, extra)?;
        if best.as_ref().is_none_or(|b| r.inner < b.inner) {
            best = Some(r);
        }
    }
    best.ok_or_else(|| "no reps".to_string())
}

/// Decode `src` (an IVF) with the libvpx reference, best of `reps` passes.
pub fn decode(src: &Path, reps: usize, prof: bool) -> Result<Run, String> {
    let exe = bin("vp9dec", prof);
    let prof_out = src.with_extension("dec.vpxprof.tsv");
    let src_s = src.to_string_lossy().into_owned();
    let reps_s = reps.max(1).to_string();
    run(
        &exe,
        &[&src_s, "--threads", "1", "--reps", &reps_s],
        if prof { Some(&prof_out) } else { None },
    )
}
