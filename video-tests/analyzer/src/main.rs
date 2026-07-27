//! Function-level speed analyzer — **rff-vp9 vs libvpx/ffmpeg**, on the fixed
//! `video-tests` corpus, encoder AND decoder, everything at DEFAULT settings.
//!
//! ```text
//! analyzer speed    # profiler OFF -> results/speed.tsv   (throughput/size/quality)
//! analyzer stages   # profiler ON  -> results/stages.tsv  (per-function ms / % / calls)
//! analyzer report   # merge both   -> results/REPORT.md
//! ```
//!
//! `run_analysis.sh` drives all three. Speed and stages are separate passes
//! because they cannot come from the same run: the rdtsc scopes inflate wall
//! time on both sides, so a breakdown taken during a timed run would report a
//! throughput nobody experiences.
//!
//! Everything is deterministic: the same clips, the same frame counts, best-of-N
//! timing, median-of-N stage tables, single-threaded on every arm.

mod ffmpeg;
mod libvpx;
mod ours;
mod quality;
mod report;
mod y4m;

use std::path::{Path, PathBuf};

// ---------------------------------------------------------------------------

pub struct Config {
    /// Best-of-N repetitions for every timed measurement.
    pub reps: usize,
    /// Passes for the median-of-N stage profile.
    pub profile_passes: usize,
    /// Frames per clip. The corpus holds 120 (small) / 60 (720p+); capping keeps
    /// a full-corpus run to minutes rather than hours while still crossing every
    /// resolution rung. `FRAMES=0` uses the whole clip.
    pub frames: usize,
}

pub static CFG: Config = Config {
    reps: 3,
    profile_passes: 3,
    frames: 30,
};

fn cfg_frames(clip_len: usize) -> usize {
    let want = std::env::var("FRAMES")
        .ok()
        .and_then(|v| v.parse::<usize>().ok())
        .unwrap_or(CFG.frames);
    if want == 0 {
        clip_len
    } else {
        want.min(clip_len)
    }
}

/// One measured (clip x codec x kind) throughput result.
#[derive(Clone)]
pub struct Row {
    pub clip: String,
    pub class: String,
    pub width: usize,
    pub height: usize,
    pub frames: usize,
    /// `ours` | `libvpx` | `ffmpeg`
    pub codec: String,
    /// `encode` | `decode`
    pub kind: String,
    /// Which bitstream a decode row consumed (`-` for encode rows).
    pub source: String,
    pub wall_ms: f64,
    pub fps: f64,
    pub mpx_s: f64,
    pub bytes: usize,
    pub kbps: f64,
    pub psnr: f64,
    pub ssim: f64,
}

/// One stage/function bucket from either side's profiler.
#[derive(Clone)]
pub struct StageRow {
    pub clip: String,
    pub codec: String,
    pub kind: String,
    pub stage: String,
    /// `self` for an exclusive bucket, `incl` for one that contains children,
    /// `info` for a diagnostic that is not part of any partition.
    pub scope: String,
    pub ms: f64,
    pub calls: u64,
    pub pct: f64,
}

// ---------------------------------------------------------------------------

pub struct ClipEntry {
    pub name: String,
    pub class: String,
    pub path: PathBuf,
}

fn root() -> PathBuf {
    let mut p = std::env::current_dir().expect("cwd");
    if p.ends_with("analyzer") {
        p.pop();
    }
    p
}

/// Where the pixels live. Defaults to the rs_h264 checkout's corpus so the two
/// codecs are measured on byte-identical input and the numbers are directly
/// comparable across repositories; `CLIPS_DIR` overrides.
fn clips_dir() -> PathBuf {
    if let Ok(d) = std::env::var("CLIPS_DIR") {
        return PathBuf::from(d);
    }
    let local = root().join("clips");
    if local.is_dir() {
        return local;
    }
    root().join("..").join("..").join("rs_h264").join("video-tests").join("clips")
}

fn manifest() -> Vec<ClipEntry> {
    let base = root();
    let txt = std::fs::read_to_string(base.join("manifest.tsv"))
        .unwrap_or_else(|e| panic!("read manifest.tsv: {e}"));
    let filter: Vec<String> = std::env::var("CLIPS")
        .map(|s| {
            s.split(',')
                .map(|x| x.trim().to_string())
                .filter(|x| !x.is_empty())
                .collect()
        })
        .unwrap_or_default();
    let dir = clips_dir();
    txt.lines()
        .skip(1)
        .filter_map(|l| {
            let f: Vec<&str> = l.split('\t').collect();
            if f.len() < 6 {
                return None;
            }
            let name = f[0].to_string();
            if !filter.is_empty() && !filter.contains(&name) {
                return None;
            }
            let path = dir.join(format!("{name}.y4m"));
            if !path.exists() {
                eprintln!("  ! missing clip {name} — skipped");
                return None;
            }
            Some(ClipEntry {
                name,
                class: f[5].to_string(),
                path,
            })
        })
        .collect()
}

fn results_dir() -> PathBuf {
    let d = root().join("results");
    std::fs::create_dir_all(&d).expect("create results dir");
    d
}

fn scratch() -> PathBuf {
    let d = results_dir().join("_tmp");
    std::fs::create_dir_all(&d).expect("create scratch dir");
    d
}

// ---------------------------------------------------------------------------

fn write_rows(path: &Path, rows: &[Row]) {
    let mut s = String::from(
        "clip\tclass\twidth\theight\tframes\tcodec\tkind\tsource\twall_ms\tfps\tmpx_s\tbytes\tkbps\tpsnr\tssim\n",
    );
    for r in rows {
        s.push_str(&format!(
            "{}\t{}\t{}\t{}\t{}\t{}\t{}\t{}\t{:.2}\t{:.2}\t{:.3}\t{}\t{:.1}\t{:.3}\t{:.5}\n",
            r.clip, r.class, r.width, r.height, r.frames, r.codec, r.kind, r.source,
            r.wall_ms, r.fps, r.mpx_s, r.bytes, r.kbps, r.psnr, r.ssim
        ));
    }
    std::fs::write(path, s).expect("write rows");
}

fn write_stages(path: &Path, rows: &[StageRow]) {
    let mut s = String::from("clip\tcodec\tkind\tstage\tscope\tms\tcalls\tns_per_call\tpct\n");
    for r in rows {
        let nspc = if r.calls > 0 {
            r.ms * 1e6 / r.calls as f64
        } else {
            0.0
        };
        s.push_str(&format!(
            "{}\t{}\t{}\t{}\t{}\t{:.4}\t{}\t{:.1}\t{:.2}\n",
            r.clip, r.codec, r.kind, r.stage, r.scope, r.ms, r.calls, nspc, r.pct
        ));
    }
    std::fs::write(path, s).expect("write stages");
}

fn row_of(c: &ClipEntry, clip: &y4m::Clip, frames: usize, codec: &str, kind: &str) -> Row {
    Row {
        clip: c.name.clone(),
        class: c.class.clone(),
        width: clip.width,
        height: clip.height,
        frames,
        codec: codec.into(),
        kind: kind.into(),
        source: "-".into(),
        wall_ms: 0.0,
        fps: 0.0,
        mpx_s: 0.0,
        bytes: 0,
        kbps: 0.0,
        psnr: f64::NAN,
        ssim: f64::NAN,
    }
}

fn fill(r: &mut Row, clip: &y4m::Clip, frames: usize, d: std::time::Duration, bytes: usize) {
    let secs = d.as_secs_f64().max(1e-9);
    r.wall_ms = secs * 1e3;
    r.fps = frames as f64 / secs;
    r.mpx_s = (clip.width * clip.height * frames) as f64 / secs / 1e6;
    r.bytes = bytes;
    let dur = frames as f64 / clip.fps_f64().max(1e-9);
    r.kbps = if bytes > 0 && dur > 0.0 {
        bytes as f64 * 8.0 / dur / 1e3
    } else {
        0.0
    };
}

// ---------------------------------------------------------------------------

fn cmd_speed() {
    if let Err(e) = libvpx::check() {
        eprintln!("!! {e}");
        std::process::exit(2);
    }
    let clips = manifest();
    if clips.is_empty() {
        eprintln!("!! no clips found under {}", clips_dir().display());
        std::process::exit(2);
    }
    let tmp = scratch();
    let mut rows: Vec<Row> = Vec::new();

    for c in &clips {
        let clip = match y4m::read(&c.path, 0) {
            Ok(v) => v,
            Err(e) => {
                eprintln!("  ! {e}");
                continue;
            }
        };
        let n = cfg_frames(clip.frames.len());
        eprintln!(
            "\n=== {} ({}) {}x{} x{} frames @ {:.2} fps ===",
            c.name, c.class, clip.width, clip.height, n, clip.fps_f64()
        );

        // Every external arm is fed the SAME truncated y4m our in-process arm
        // encodes, so no side is silently given more or fewer frames.
        let src = tmp.join(format!("{}.src.y4m", c.name));
        if let Err(e) = y4m::write_prefix(&clip, &src, n) {
            eprintln!("  ! {e}");
            continue;
        }

        // ---- ours: encode at defaults, then decode our own stream ----------
        let ours_ivf = tmp.join(format!("{}.ours.ivf", c.name));
        match ours::encode_speed(&clip, n, CFG.reps) {
            Ok((d, packets)) => {
                let bytes: usize = packets.iter().map(|p| p.len()).sum();
                let mut r = row_of(c, &clip, n, "ours", "encode");
                fill(&mut r, &clip, n, d, bytes);
                std::fs::write(&ours_ivf, ours::to_ivf(&clip, &packets)).ok();
                if let Some((p, s)) = quality::measure(&ours_ivf, &clip, n) {
                    r.psnr = p;
                    r.ssim = s;
                }
                eprintln!(
                    "  ours   enc {:>9.1} ms {:>8.2} fps {:>7.3} Mpx/s {:>7.0} kb/s {:>6.2} dB",
                    r.wall_ms, r.fps, r.mpx_s, r.kbps, r.psnr
                );
                rows.push(r);

                for (name, label) in [("ours", "ours"), ("libvpx", "libvpx"), ("ffmpeg", "ffmpeg")] {
                    if let Some(mut d) = decode_arm(name, &packets, &ours_ivf, c, &clip, n) {
                        d.source = "ours".into();
                        eprintln!(
                            "  {label:<6} dec {:>9.1} ms {:>8.2} fps {:>7.3} Mpx/s  (our stream)",
                            d.wall_ms, d.fps, d.mpx_s
                        );
                        rows.push(d);
                    }
                }
            }
            Err(e) => eprintln!("  ours   enc FAILED: {e}"),
        }

        // ---- libvpx reference: encode at ITS defaults ----------------------
        let lv_ivf = tmp.join(format!("{}.libvpx.ivf", c.name));
        match libvpx::encode(&src, &lv_ivf, false) {
            Ok(run) => {
                let mut r = row_of(c, &clip, run.frames.max(n), "libvpx", "encode");
                fill(&mut r, &clip, run.frames.max(n), run.inner, run.bytes);
                if let Some((p, s)) = quality::measure(&lv_ivf, &clip, run.frames.max(n)) {
                    r.psnr = p;
                    r.ssim = s;
                }
                eprintln!(
                    "  libvpx enc {:>9.1} ms {:>8.2} fps {:>7.3} Mpx/s {:>7.0} kb/s {:>6.2} dB",
                    r.wall_ms, r.fps, r.mpx_s, r.kbps, r.psnr
                );
                rows.push(r);

                // Every decoder against the REFERENCE bitstream — the decode
                // comparison that matters, since it is the stream in the wild.
                let pkts = read_ivf(&lv_ivf);
                for (name, label) in [("ours", "ours"), ("libvpx", "libvpx"), ("ffmpeg", "ffmpeg")] {
                    if let Some(mut d) = decode_arm(name, &pkts, &lv_ivf, c, &clip, run.frames.max(n)) {
                        d.source = "libvpx".into();
                        eprintln!(
                            "  {label:<6} dec {:>9.1} ms {:>8.2} fps {:>7.3} Mpx/s  (libvpx stream)",
                            d.wall_ms, d.fps, d.mpx_s
                        );
                        rows.push(d);
                    }
                }
            }
            Err(e) => eprintln!("  libvpx enc FAILED: {e}"),
        }

        // ---- ffmpeg: encode at ITS defaults (the headline bar) -------------
        let ff_ivf = tmp.join(format!("{}.ffmpeg.ivf", c.name));
        if let Some((d, bytes)) = ffmpeg::encode(&src, &ff_ivf, CFG.reps) {
            let mut r = row_of(c, &clip, n, "ffmpeg", "encode");
            fill(&mut r, &clip, n, d, bytes);
            if let Some((p, s)) = quality::measure(&ff_ivf, &clip, n) {
                r.psnr = p;
                r.ssim = s;
            }
            eprintln!(
                "  ffmpeg enc {:>9.1} ms {:>8.2} fps {:>7.3} Mpx/s {:>7.0} kb/s {:>6.2} dB",
                r.wall_ms, r.fps, r.mpx_s, r.kbps, r.psnr
            );
            rows.push(r);
        }

        let _ = std::fs::remove_file(&src);
        // Checkpoint after EVERY clip. A full-corpus run is tens of minutes;
        // writing only at the end means one transient failure throws away every
        // clip that already succeeded (it did, once). Rewriting the whole file
        // costs microseconds against a multi-second clip, so there is no reason
        // to be clever about appending.
        write_rows(&results_dir().join("speed.tsv"), &rows);
    }
    eprintln!("
{} row(s) over {} clip(s)", rows.len(), clips.len());
}

// ---------------------------------------------------------------------------
// pareto — the MATCHED-OPERATING-POINT pass.
//
// `speed` runs every arm at its own defaults, which answers "what do I get out
// of the box" but CANNOT answer "why are we slower": our default is constant
// quality at qindex 64 with no lookahead, while libvpx's is 256 kbps VBR with
// lag 25. On park_joy that is a 72x bitrate difference, so the two arms are not
// doing remotely the same work and neither the speed ratio nor the quality
// delta means anything.
//
// Here both arms are pinned to the SAME rate-control mode (constant quality),
// the SAME cq-level, the SAME speed preset and the SAME lookahead, and the
// ladder is swept so encode time can be read at matched quality rather than at
// two unrelated points on two different curves.
// ---------------------------------------------------------------------------

/// The matched operating point, as (our options, libvpx flags). Empty when
/// `MATCH_CRF` is unset, so `stages` keeps its historical default-settings
/// behaviour unless a matched run is explicitly requested.
fn matched_cfg(frames: usize) -> (rff_core::Dictionary, Vec<String>) {
    let mut opts = rff_core::Dictionary::new();
    let mut extra: Vec<String> = Vec::new();
    let crf = match std::env::var("MATCH_CRF").ok().and_then(|v| v.parse::<u32>().ok()) {
        Some(c) => c,
        None => return (opts, extra),
    };
    let sp = std::env::var("MATCH_SPEED")
        .ok()
        .and_then(|v| v.parse::<u32>().ok())
        .unwrap_or(0);
    let lag = std::env::var("MATCH_LAG")
        .ok()
        .and_then(|v| v.parse::<u32>().ok())
        .unwrap_or(0);
    opts.set("crf", crf.to_string());
    opts.set("speed", sp.to_string());
    if lag > 0 {
        opts.set("lag", lag.to_string());
    }
    extra.extend([
        "--crf".into(),
        crf.to_string(),
        "--cpu-used".into(),
        sp.to_string(),
        "--lag".into(),
        lag.to_string(),
        "--frames".into(),
        frames.to_string(),
    ]);
    (opts, extra)
}

fn env_list(key: &str, default: &str) -> Vec<u32> {
    std::env::var(key)
        .unwrap_or_else(|_| default.to_string())
        .split(',')
        .filter_map(|s| s.trim().parse().ok())
        .collect()
}

fn cmd_pareto() {
    if let Err(e) = libvpx::check() {
        eprintln!("!! {e}");
        std::process::exit(2);
    }
    let clips = manifest();
    let tmp = scratch();
    let crfs = env_list("PARETO_CRF", "20,32,43,55");
    let speeds = env_list("PARETO_SPEED", "0");
    let lag: u32 = env_list("PARETO_LAG", "0").first().copied().unwrap_or(0);
    let mut rows: Vec<Row> = Vec::new();

    eprintln!(
        "matched operating point: crf {crfs:?} x speed {speeds:?}, lag {lag}, constant-quality both arms"
    );

    for c in &clips {
        let clip = match y4m::read(&c.path, 0) {
            Ok(v) => v,
            Err(e) => {
                eprintln!("  ! {e}");
                continue;
            }
        };
        let n = cfg_frames(clip.frames.len());
        eprintln!("\n=== {} ({}) {}x{} x{n} ===", c.name, c.class, clip.width, clip.height);

        let src = tmp.join(format!("{}.p.src.y4m", c.name));
        if y4m::write_prefix(&clip, &src, n).is_err() {
            continue;
        }

        for &sp in &speeds {
            for &crf in &crfs {
                let tag = format!("crf{crf}/s{sp}/lag{lag}");

                // ---- ours, explicitly configured -------------------------
                let mut opts = rff_core::Dictionary::new();
                opts.set("crf", crf.to_string());
                opts.set("speed", sp.to_string());
                if lag > 0 {
                    opts.set("lag", lag.to_string());
                }
                let ivf = tmp.join(format!("{}.p.ours.ivf", c.name));
                match ours::encode_speed_cfg(&clip, n, CFG.reps, &opts) {
                    Ok((d, packets)) => {
                        let bytes: usize = packets.iter().map(|p| p.len()).sum();
                        let mut r = row_of(c, &clip, n, "ours", "encode");
                        r.source = tag.clone();
                        fill(&mut r, &clip, n, d, bytes);
                        std::fs::write(&ivf, ours::to_ivf(&clip, &packets)).ok();
                        if let Some((p, s)) = quality::measure(&ivf, &clip, n) {
                            r.psnr = p;
                            r.ssim = s;
                        }
                        eprintln!(
                            "  ours   {tag:<16} {:>9.1} ms {:>8.0} kb/s {:>6.2} dB",
                            r.wall_ms, r.kbps, r.psnr
                        );
                        rows.push(r);
                    }
                    Err(e) => eprintln!("  ours   {tag} FAILED: {e}"),
                }

                // ---- libvpx, the same operating point --------------------
                // Skipped for A/B loops over our own knobs, where the reference
                // arm is constant and only doubles the wall time.
                if std::env::var("PARETO_NOREF").is_ok() {
                    write_rows(&results_dir().join("pareto.tsv"), &rows);
                    continue;
                }
                let mut extra = vec![
                    "--crf".to_string(),
                    crf.to_string(),
                    "--cpu-used".to_string(),
                    sp.to_string(),
                    "--lag".to_string(),
                    lag.to_string(),
                    "--frames".to_string(),
                    n.to_string(),
                ];
                if sp >= 5 {
                    extra.push("--realtime".to_string());
                }
                let lv = tmp.join(format!("{}.p.libvpx.ivf", c.name));
                match libvpx::encode_best(&src, &lv, &extra, CFG.reps) {
                    Ok(run) => {
                        let mut r = row_of(c, &clip, n, "libvpx", "encode");
                        r.source = tag.clone();
                        fill(&mut r, &clip, n, run.inner, run.bytes);
                        if let Some((p, s)) = quality::measure(&lv, &clip, n) {
                            r.psnr = p;
                            r.ssim = s;
                        }
                        eprintln!(
                            "  libvpx {tag:<16} {:>9.1} ms {:>8.0} kb/s {:>6.2} dB",
                            r.wall_ms, r.kbps, r.psnr
                        );
                        rows.push(r);
                    }
                    Err(e) => eprintln!("  libvpx {tag} FAILED: {e}"),
                }
                write_rows(&results_dir().join("pareto.tsv"), &rows);
            }
        }
        let _ = std::fs::remove_file(&src);
    }
    eprintln!("\n{} row(s) -> results/pareto.tsv", rows.len());
}

// ---------------------------------------------------------------------------
// noise — the NULL ARM.
//
// Two arms that are the SAME encoder at the SAME settings, run ABBA-interleaved.
// Any reported difference is the harness's own floor, and every speed claim
// smaller than it is a coin flip. This machine has previously manufactured
// +0.2%..+10.8% between two identical VP9 encoders, so it is measured, not
// assumed.
// ---------------------------------------------------------------------------

fn cmd_noise() {
    let clips = manifest();
    let reps: usize = std::env::var("NOISE_REPS")
        .ok()
        .and_then(|v| v.parse().ok())
        .unwrap_or(6);
    let crf: u32 = env_list("PARETO_CRF", "32").first().copied().unwrap_or(32);
    let sp: u32 = env_list("PARETO_SPEED", "0").first().copied().unwrap_or(0);

    for c in &clips {
        let clip = match y4m::read(&c.path, 0) {
            Ok(v) => v,
            Err(_) => continue,
        };
        let n = cfg_frames(clip.frames.len());
        let mut opts = rff_core::Dictionary::new();
        opts.set("crf", crf.to_string());
        opts.set("speed", sp.to_string());

        let (mut a, mut b) = (Vec::new(), Vec::new());
        for i in 0..reps {
            // ABBA: alternate which arm runs first so a warm-second-run effect
            // cancels instead of accumulating into whichever arm always leads.
            let first_is_a = i % 2 == 0;
            let mut one = || {
                ours::encode_speed_cfg(&clip, n, 1, &opts)
                    .map(|(d, _)| d.as_secs_f64() * 1e3)
                    .unwrap_or(f64::NAN)
            };
            let (x, y) = (one(), one());
            if first_is_a {
                a.push(x);
                b.push(y);
            } else {
                b.push(x);
                a.push(y);
            }
        }
        let best = |v: &[f64]| v.iter().cloned().fold(f64::INFINITY, f64::min);
        let (ba, bb) = (best(&a), best(&b));
        let spread = (bb - ba) / ba * 100.0;
        let allmin = ba.min(bb);
        let allmax = a.iter().chain(b.iter()).cloned().fold(0.0f64, f64::max);
        eprintln!(
            "{:<24} null arm: best-of-{reps} A {ba:8.1} ms  B {bb:8.1} ms  \
             delta {spread:+6.2}%   worst/best {:.2}x",
            c.name,
            allmax / allmin
        );
    }
    eprintln!(
        "\nAny A/B speed claim smaller than |delta| above is inside the harness floor \
         and is NOT a result."
    );
}

/// Time one decoder arm over a stream that exists both as in-memory packets
/// (for our in-process decoder) and as an IVF file (for the external ones).
fn decode_arm(
    which: &str,
    packets: &[Vec<u8>],
    ivf: &Path,
    c: &ClipEntry,
    clip: &y4m::Clip,
    frames: usize,
) -> Option<Row> {
    let mut r = row_of(c, clip, frames, which, "decode");
    match which {
        "ours" => match ours::decode_speed(packets, CFG.reps) {
            Ok((d, n)) => {
                r.frames = n;
                fill(&mut r, clip, n, d, packets.iter().map(|p| p.len()).sum());
                Some(r)
            }
            Err(e) => {
                eprintln!("    ! ours decode: {e}");
                None
            }
        },
        "libvpx" => match libvpx::decode(ivf, CFG.reps, false) {
            Ok(run) => {
                let n = run.frames.max(frames);
                r.frames = n;
                fill(&mut r, clip, n, run.inner, 0);
                Some(r)
            }
            Err(e) => {
                eprintln!("    ! libvpx decode: {e}");
                None
            }
        },
        "ffmpeg" => {
            let d = ffmpeg::decode(ivf, CFG.reps)?;
            fill(&mut r, clip, frames, d, 0);
            Some(r)
        }
        _ => None,
    }
}

/// Split an IVF back into its frame payloads (the form our decoder consumes).
fn read_ivf(path: &Path) -> Vec<Vec<u8>> {
    let data = match std::fs::read(path) {
        Ok(d) => d,
        Err(_) => return Vec::new(),
    };
    if data.len() < 32 || &data[0..4] != b"DKIF" {
        return Vec::new();
    }
    let hdr = u16::from_le_bytes([data[6], data[7]]) as usize;
    let mut out = Vec::new();
    let mut pos = hdr;
    while pos + 12 <= data.len() {
        let sz = u32::from_le_bytes([data[pos], data[pos + 1], data[pos + 2], data[pos + 3]]) as usize;
        pos += 12;
        if pos + sz > data.len() {
            break;
        }
        out.push(data[pos..pos + sz].to_vec());
        pos += sz;
    }
    out
}

// ---------------------------------------------------------------------------

fn cmd_stages() {
    if let Err(e) = libvpx::check() {
        eprintln!("!! {e}");
        std::process::exit(2);
    }
    let clips = manifest();
    let tmp = scratch();
    let mut rows: Vec<StageRow> = Vec::new();

    for c in &clips {
        let clip = match y4m::read(&c.path, 0) {
            Ok(v) => v,
            Err(e) => {
                eprintln!("  ! {e}");
                continue;
            }
        };
        let n = cfg_frames(clip.frames.len());
        eprintln!("\n=== stages: {} ({n} frames) ===", c.name);
        // Each pass is a full encode; at 1080p that is tens of seconds, and the
        // stage medians are stable well before then. Spend passes where they buy
        // stability (small, noisy clips), not where they only buy wall time.
        let passes = if clip.width * clip.height > 1_000_000 {
            1
        } else {
            CFG.profile_passes
        };

        // The matched operating point, if one was asked for. Without it each
        // arm profiles its own defaults, which are a 5-72x bitrate apart on this
        // corpus — and an arm that codes more coefficients inflates its own
        // coefficient stages for reasons unrelated to efficiency.
        let (opts, extra) = matched_cfg(n);
        if !extra.is_empty() {
            eprintln!("  (matched operating point: {})", extra.join(" "));
        }

        // ---- ours: encoder -------------------------------------------------
        let s = ours::encode_stages_cfg(&clip, n, passes, &opts);
        rows.extend(our_encode_rows(&c.name, &s));

        eprintln!("  ours   encode stages captured");

        // ---- libvpx: encoder, from the instrumented twin -------------------
        let src = tmp.join(format!("{}.src.y4m", c.name));
        if y4m::write_prefix(&clip, &src, n).is_ok() {
            let out = tmp.join(format!("{}.libvpxp.ivf", c.name));
            match libvpx::encode_cfg(&src, &out, true, &extra) {
                Ok(run) => {
                    rows.extend(ref_rows(&c.name, "encode", &run));
                    eprintln!("  libvpx encode: {} stage buckets", run.stages.len());
                }
                Err(e) => eprintln!("  libvpx encode: {e}"),
            }

            // ---- BOTH decoders, on the SAME bitstream ----------------------
            // The reference stream, specifically: it is what a decoder in the
            // wild actually sees, and profiling each decoder on its own
            // encoder's output would compare two different workloads (our
            // encoder emits far more coefficients at default settings, which
            // alone would inflate our detokenize share).
            let pkts = read_ivf(&out);
            if !pkts.is_empty() {
                let s = ours::decode_stages(&pkts, passes);
                rows.extend(our_decode_rows(&c.name, &s));
            }
            // reps=1, NOT `passes`: the C profiler accumulates every rep into one
            // dump (it resets once, at first frame), whereas our Rust profiler
            // resets before each pass and reports one decode. Passing `passes`
            // here would report N decodes against our 1 and silently scale the
            // reference's milliseconds by N.
            match libvpx::decode(&out, 1, true) {
                Ok(run) => {
                    rows.extend(ref_rows(&c.name, "decode", &run));
                    eprintln!("  libvpx decode: {} stage buckets", run.stages.len());
                }
                Err(e) => eprintln!("  libvpx decode: {e}"),
            }
            let _ = std::fs::remove_file(&src);
            let _ = std::fs::remove_file(&out);
        }
        write_stages(&results_dir().join("stages.tsv"), &rows);
    }
    eprintln!("
{} stage row(s) over {} clip(s)", rows.len(), clips.len());
}

/// Turn our ENCODER snapshot into rows.
///
/// Our encoder profiler is nested-INCLUSIVE (a parent's time contains its
/// children's), unlike the libvpx twin's stack-based exclusive profiler. So each
/// bucket is emitted twice over: `incl` as measured, and — for parents — a
/// `self` figure with the scoped children subtracted, which is what pairs up
/// with the reference table. The denominator is the disjoint top-level set the
/// profiler itself defines; the residue is unscoped orchestration.
fn our_encode_rows(clip: &str, s: &[(f64, u64); rff_codec_vp9::encode_prof::N]) -> Vec<StageRow> {
    use rff_codec_vp9::encode_prof as p;
    let named: f64 = (0..p::N)
        .filter(|&i| p::is_toplevel(i))
        .map(|i| s[i].0)
        .sum();
    // Bucket 0 is TOTAL(decision): the raw decision-pass wall, which contains the
    // scoped stages, the unscoped orchestration between them, AND the profiler's
    // own tax. Charging the tax to orchestration would invent work that does not
    // exist, so it comes out first and is reported as its own info row.
    let wall = s[0].0;
    let scope_calls: u64 = (1..p::N).map(|i| s[i].1).sum();
    let tax_ms = scope_calls as f64 * p::overhead_ns().1 / 1e6;
    let glue = (wall - named - tax_ms).max(0.0);
    let total = (named + glue).max(1e-9);
    let mut child_ms = vec![0.0f64; p::N];
    for i in 0..p::N {
        if let Some(par) = p::parent(i) {
            child_ms[par] += s[i].0;
        }
    }
    let mut rows = Vec::new();
    for i in 1..p::N {
        if s[i].1 == 0 {
            continue;
        }
        let scope = if p::is_info(i) {
            "info"
        } else if child_ms[i] > 0.0 {
            "incl"
        } else {
            "self"
        };
        rows.push(StageRow {
            clip: clip.into(),
            codec: "ours".into(),
            kind: "encode".into(),
            stage: p::name(i).trim().into(),
            scope: scope.into(),
            ms: s[i].0,
            calls: s[i].1,
            pct: 100.0 * s[i].0 / total,
        });
        if child_ms[i] > 0.0 {
            let self_ms = (s[i].0 - child_ms[i]).max(0.0);
            rows.push(StageRow {
                clip: clip.into(),
                codec: "ours".into(),
                kind: "encode".into(),
                stage: format!("{}(self)", p::name(i).trim()),
                scope: "self".into(),
                ms: self_ms,
                calls: s[i].1,
                pct: 100.0 * self_ms / total,
            });
        }
    }
    rows.push(StageRow {
        clip: clip.into(),
        codec: "ours".into(),
        kind: "encode".into(),
        stage: "orchestration/glue".into(),
        scope: "self".into(),
        ms: glue,
        calls: 0,
        pct: 100.0 * glue / total,
    });
    rows.push(StageRow {
        clip: clip.into(),
        codec: "ours".into(),
        kind: "encode".into(),
        stage: "_INSTRUMENT_TAX".into(),
        scope: "info".into(),
        ms: tax_ms,
        calls: scope_calls,
        pct: 100.0 * tax_ms / total,
    });
    rows.push(StageRow {
        clip: clip.into(),
        codec: "ours".into(),
        kind: "encode".into(),
        stage: "_DECISION_WALL".into(),
        scope: "info".into(),
        ms: wall,
        calls: s[0].1,
        pct: 100.0 * wall / total,
    });
    rows.push(StageRow {
        clip: clip.into(),
        codec: "ours".into(),
        kind: "encode".into(),
        stage: "TOTAL".into(),
        scope: "self".into(),
        ms: total,
        calls: s[0].1,
        pct: 100.0,
    });
    rows
}

/// Our DECODER snapshot is already exclusive self-time, so it maps one-to-one.
fn our_decode_rows(clip: &str, s: &[(f64, u64); rff_codec_vp9::prof::N]) -> Vec<StageRow> {
    use rff_codec_vp9::prof as p;
    let total: f64 = s.iter().map(|&(ms, _)| ms).sum::<f64>().max(1e-9);
    let mut rows: Vec<StageRow> = (0..p::N)
        .filter(|&i| i == 0 || s[i].1 > 0)
        .map(|i| StageRow {
            clip: clip.into(),
            codec: "ours".into(),
            kind: "decode".into(),
            stage: p::name(i).into(),
            scope: "self".into(),
            ms: s[i].0,
            calls: s[i].1,
            pct: 100.0 * s[i].0 / total,
        })
        .collect();
    rows.push(StageRow {
        clip: clip.into(),
        codec: "ours".into(),
        kind: "decode".into(),
        stage: "TOTAL".into(),
        scope: "self".into(),
        ms: total,
        calls: s.iter().map(|&(_, c)| c).sum(),
        pct: 100.0,
    });
    rows
}

/// The libvpx twin's dump — already exclusive self-time and already percented.
fn ref_rows(clip: &str, kind: &str, r: &libvpx::Run) -> Vec<StageRow> {
    let mut rows: Vec<StageRow> = r
        .stages
        .iter()
        .map(|(stage, ms, calls, pct)| StageRow {
            clip: clip.into(),
            codec: "libvpx".into(),
            kind: kind.into(),
            stage: stage.clone(),
            scope: if stage.starts_with('_') { "info" } else { "self" }.into(),
            ms: *ms,
            calls: *calls,
            pct: *pct,
        })
        .collect();
    for (name, v) in &r.counts {
        rows.push(StageRow {
            clip: clip.into(),
            codec: "libvpx".into(),
            kind: kind.into(),
            stage: format!("count:{name}"),
            scope: "info".into(),
            ms: 0.0,
            calls: *v,
            pct: 0.0,
        });
    }
    rows
}

// ---------------------------------------------------------------------------

fn main() {
    let mode = std::env::args().nth(1).unwrap_or_else(|| "help".into());
    match mode.as_str() {
        "speed" => cmd_speed(),
        "stages" => cmd_stages(),
        "pareto" => cmd_pareto(),
        "noise" => cmd_noise(),
        "report" => report::generate(&results_dir()),
        _ => {
            eprintln!("usage: analyzer <speed|stages|pareto|noise|report>");
            eprintln!("  speed   profiler OFF — throughput / size / PSNR / SSIM at defaults");
            eprintln!("  stages  profiler ON  — per-function ms, %, calls, ns/call");
            eprintln!("  pareto  MATCHED operating point — both arms at the same crf/speed/lag");
            eprintln!("  noise   NULL arm — the harness's own A/B floor, ABBA-interleaved");
            eprintln!("  report  merge results/*.tsv into results/REPORT.md");
            eprintln!();
            eprintln!("env: CLIPS=a,b   restrict the corpus");
            eprintln!("     FRAMES=N    frames per clip (0 = whole clip; default 30)");
            eprintln!("     PARETO_CRF=20,32,43,55   the matched cq ladder");
            eprintln!("     PARETO_SPEED=0           matched speed preset (ours -speed == libvpx --cpu-used)");
            eprintln!("     PARETO_LAG=0             matched lookahead");
            eprintln!("     CLIPS_DIR   where the .y4m files live");
            eprintln!("     LIBVPX_DIR  the _ref_libvpx checkout");
            eprintln!("     FFMPEG      the upstream ffmpeg binary");
            std::process::exit(1);
        }
    }
}
