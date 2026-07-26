//! The C reference arms for AV1: libaom (encode) and dav1d (decode).
//!
//! Same shape and same justification as the VP9 analyzer's `libvpx` module:
//! ffmpeg's shipped binary is stripped, so it yields a total and nothing else.
//! These are our own builds of the same two libraries ffmpeg links — with
//! symbols and rdtsc taps — so the reference can be attributed per function.
//!
//! FOUR binaries per library, for the reason the x264/libvpx references use:
//! a stock pair (untaxed, the throughput arm) and a tapped pair (the breakdown
//! arm). Measuring throughput on an instrumented build would charge the C side
//! overhead our own profiler-off build does not pay.
//!
//! WHY THESE TWO. ffmpeg's default AV1 *encoder* is `libaom-av1` and its default
//! AV1 *decoder* is `libdav1d`. dav1d is additionally the upstream our decoder is
//! a port of, so `ours` vs `dav1d` is a fork-versus-original comparison and the
//! stage names line up almost one-for-one.

use std::path::{Path, PathBuf};
use std::process::Command;
use std::time::Duration;

/// Root of an external reference checkout, overridable per library.
fn dir(env: &str, default: &str) -> PathBuf {
    std::env::var(env)
        .map(PathBuf::from)
        .unwrap_or_else(|_| PathBuf::from(default))
}

pub fn aom_dir() -> PathBuf {
    dir("AOM_DIR", "../../../_ref_aom")
}
pub fn dav1d_dir() -> PathBuf {
    dir("DAV1D_DIR", "../../../_ref_dav1d")
}

fn bin(root: &Path, tool: &str, prof: bool) -> PathBuf {
    root.join(if prof {
        format!("{tool}-prof.exe")
    } else {
        format!("{tool}.exe")
    })
}

pub fn aom_bin(prof: bool) -> PathBuf {
    bin(&aom_dir(), "av1enc", prof)
}
pub fn dav1d_bin(prof: bool) -> PathBuf {
    bin(&dav1d_dir(), "av1dec", prof)
}

/// Report which reference binaries are missing and how to build them.
pub fn check() -> Result<(), String> {
    let mut missing = Vec::new();
    for p in [aom_bin(false), aom_bin(true)] {
        if !p.exists() {
            missing.push(format!(
                "{} (build: cd {} && python instrument.py && bash build.sh && bash build.sh prof)",
                p.display(),
                aom_dir().display()
            ));
        }
    }
    for p in [dav1d_bin(false), dav1d_bin(true)] {
        if !p.exists() {
            missing.push(format!(
                "{} (build: cd {} && python instrument.py && bash build.sh && bash build.sh prof)",
                p.display(),
                dav1d_dir().display()
            ));
        }
    }
    if missing.is_empty() {
        Ok(())
    } else {
        Err(format!("AV1 reference not built:\n  {}", missing.join("\n  ")))
    }
}

pub struct Run {
    /// Whole-process wall clock, INCLUDING startup/init/teardown.
    pub wall: Duration,
    /// The binary's OWN reported codec-loop time — the number to compare against
    /// our in-process loop, since process startup here is 10-20 ms and would
    /// swamp a small clip.
    pub inner: Duration,
    pub bytes: usize,
    pub frames: usize,
    /// (stage, ms, calls, pct) from the tapped build's TSV dump.
    pub stages: Vec<(String, f64, u64, f64)>,
}

/// "encoded 30 frames, 1234.567 ms, 24.30 fps" / "decoded ...".
fn parse_self_report(log: &str) -> (usize, Duration) {
    for line in log.lines().rev() {
        let l = line.trim();
        if !(l.starts_with("encoded ") || l.starts_with("decoded ")) {
            continue;
        }
        let mut tok = l.split_whitespace();
        tok.next();
        let frames: usize = tok.next().and_then(|s| s.parse().ok()).unwrap_or(0);
        tok.next();
        let ms: f64 = tok
            .next()
            .and_then(|s| s.trim_end_matches(',').parse().ok())
            .unwrap_or(0.0);
        return (frames, Duration::from_secs_f64(ms / 1e3));
    }
    (0, Duration::ZERO)
}

fn parse_stages(path: &Path) -> Vec<(String, f64, u64, f64)> {
    let txt = std::fs::read_to_string(path).unwrap_or_default();
    let _ = std::fs::remove_file(path);
    txt.lines()
        .filter_map(|l| {
            let f: Vec<&str> = l.split('\t').collect();
            // Both C references emit the same `vpxprof`-shaped row so one parser
            // serves every arm in the family.
            if f.first() != Some(&"cprof") || f.len() < 5 {
                return None;
            }
            Some((
                f[1].to_string(),
                f[2].parse().unwrap_or(0.0),
                f[3].parse().unwrap_or(0),
                f[4].parse().unwrap_or(0.0),
            ))
        })
        .collect()
}

fn run(exe: &Path, args: &[&str], prof_out: Option<&Path>) -> Result<Run, String> {
    let mut cmd = Command::new(exe);
    cmd.args(args);
    match prof_out {
        Some(p) => {
            cmd.env("CPROF_OUT", p);
        }
        None => {
            cmd.env_remove("CPROF_OUT");
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
    Ok(Run {
        wall,
        inner,
        bytes: 0,
        frames,
        stages: prof_out.map(parse_stages).unwrap_or_default(),
    })
}

/// Encode a y4m to an IVF with libaom at ITS defaults, single-threaded.
pub fn aom_encode(src: &Path, out: &Path, prof: bool) -> Result<Run, String> {
    let exe = aom_bin(prof);
    let prof_out = out.with_extension("cprof.tsv");
    let src_s = src.to_string_lossy().into_owned();
    let out_s = out.to_string_lossy().into_owned();
    let mut r = run(
        &exe,
        &[&src_s, &out_s, "--threads", "1"],
        if prof { Some(&prof_out) } else { None },
    )?;
    r.bytes = std::fs::metadata(out).map(|m| m.len() as usize).unwrap_or(0);
    Ok(r)
}

/// Decode an IVF with dav1d, best of `reps` passes, single-threaded.
pub fn dav1d_decode(src: &Path, reps: usize, prof: bool) -> Result<Run, String> {
    let exe = dav1d_bin(prof);
    let prof_out = src.with_extension("dec.cprof.tsv");
    let src_s = src.to_string_lossy().into_owned();
    let reps_s = reps.max(1).to_string();
    run(
        &exe,
        &[&src_s, "--threads", "1", "--reps", &reps_s],
        if prof { Some(&prof_out) } else { None },
    )
}
