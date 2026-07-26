//! Function-level speed analyzer — **our AV1 (`rusty_av1e` / `rusty_av1d`) vs
//! libaom, dav1d and ffmpeg**, on the same fixed `video-tests` corpus the VP9
//! analyzer uses, encoder AND decoder, everything at DEFAULT settings.
//!
//! ```text
//! analyzer-av1 speed    # profiler OFF -> results/av1/speed.tsv
//! analyzer-av1 stages   # profiler ON  -> results/av1/stages.tsv
//! analyzer-av1 report   # merge both   -> results/av1/REPORT.md
//! ```
//!
//! `run_analysis_av1.sh` drives all three. Unlike the VP9 analyzer (whose
//! profilers are runtime-gated, so one binary serves both passes), the AV1 forks
//! gate their profilers with a cargo feature, so this binary is built twice —
//! with and without `--features profile`.

mod ours;
mod refs;
mod report;

use rff_video_harness as h;
use std::path::Path;

/// Best-of-N repetitions for every timed measurement.
const REPS: usize = 3;
/// Passes for the median-of-N stage profile.
const PASSES: usize = 3;
/// Frames per clip unless `FRAMES` says otherwise.
const FRAMES: usize = 30;

const CODEC: &str = "av1";

// ---------------------------------------------------------------------------

fn cmd_speed() {
    let refs_ok = refs::check();
    if let Err(ref e) = refs_ok {
        // The C references are a bonus arm here, not a precondition: ours-vs-ffmpeg
        // is still a complete measurement without them, and refusing to run would
        // make the harness useless until a second toolchain is in place.
        eprintln!("!! {e}\n   continuing with the ffmpeg arm only.\n");
    }
    let clips = h::manifest();
    if clips.is_empty() {
        eprintln!("!! no clips found under {}", h::clips_dir().display());
        std::process::exit(2);
    }
    let tmp = h::scratch(CODEC);
    let mut rows: Vec<h::Row> = Vec::new();

    for c in &clips {
        let clip = match h::y4m::read(&c.path, 0) {
            Ok(v) => v,
            Err(e) => {
                eprintln!("  ! {e}");
                continue;
            }
        };
        let n = h::frames_per_clip(clip.frames.len(), FRAMES);
        eprintln!(
            "\n=== {} ({}) {}x{} x{} frames @ {:.2} fps ===",
            c.name,
            c.class,
            clip.width,
            clip.height,
            n,
            clip.fps_f64()
        );

        // Every external arm gets the SAME truncated y4m our in-process arm
        // encodes, so no side is silently given more or fewer frames.
        let src = tmp.join(format!("{}.src.y4m", c.name));
        if let Err(e) = h::y4m::write_prefix(&clip, &src, n) {
            eprintln!("  ! {e}");
            continue;
        }

        // ---- ours -----------------------------------------------------------
        let ours_ivf = tmp.join(format!("{}.ours.ivf", c.name));
        match ours::encode_speed(&clip, n, REPS) {
            Ok((d, packets)) => {
                let bytes: usize = packets.iter().map(|p| p.len()).sum();
                let mut r = h::Row::new(c, &clip, n, "ours", "encode");
                r.fill(&clip, n, d, bytes);
                std::fs::write(
                    &ours_ivf,
                    h::to_ivf(b"AV01", clip.width, clip.height, clip.fps, &packets),
                )
                .ok();
                if let Some((p, s)) = h::quality::measure(&ours_ivf, &clip, n) {
                    r.psnr = p;
                    r.ssim = s;
                }
                eprintln!(
                    "  ours   enc {:>9.1} ms {:>8.2} fps {:>7.3} Mpx/s {:>7.0} kb/s {:>6.2} dB",
                    r.wall_ms, r.fps, r.mpx_s, r.kbps, r.psnr
                );
                rows.push(r);
                decode_arms(&mut rows, &packets, &ours_ivf, c, &clip, n, "ours", refs_ok.is_ok());
            }
            Err(e) => eprintln!("  ours   enc FAILED: {e}"),
        }

        // ---- libaom (what ffmpeg's default AV1 encoder runs) ----------------
        let aom_ivf = tmp.join(format!("{}.aom.ivf", c.name));
        if refs_ok.is_ok() {
            match refs::aom_encode(&src, &aom_ivf, false) {
                Ok(run) => {
                    let nf = run.frames.max(n);
                    let mut r = h::Row::new(c, &clip, nf, "libaom", "encode");
                    r.fill(&clip, nf, run.inner, run.bytes);
                    if let Some((p, s)) = h::quality::measure(&aom_ivf, &clip, nf) {
                        r.psnr = p;
                        r.ssim = s;
                    }
                    eprintln!(
                        "  libaom enc {:>9.1} ms {:>8.2} fps {:>7.3} Mpx/s {:>7.0} kb/s {:>6.2} dB",
                        r.wall_ms, r.fps, r.mpx_s, r.kbps, r.psnr
                    );
                    rows.push(r);
                    let pkts = h::read_ivf(&aom_ivf);
                    decode_arms(&mut rows, &pkts, &aom_ivf, c, &clip, nf, "libaom", true);
                }
                Err(e) => eprintln!("  libaom enc FAILED: {e}"),
            }
        }

        // ---- ffmpeg at ITS defaults (the headline bar) ----------------------
        let ff_ivf = tmp.join(format!("{}.ffmpeg.ivf", c.name));
        if let Some((d, bytes)) = ffmpeg_encode(&src, &ff_ivf) {
            let mut r = h::Row::new(c, &clip, n, "ffmpeg", "encode");
            r.fill(&clip, n, d, bytes);
            if let Some((p, s)) = h::quality::measure(&ff_ivf, &clip, n) {
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
        // Checkpoint after EVERY clip — a full run is tens of minutes and one
        // transient failure should cost one clip, not the whole run.
        h::write_rows(&h::results_dir(CODEC).join("speed.tsv"), &rows);
    }
    eprintln!("\n{} row(s) over {} clip(s)", rows.len(), clips.len());
}

/// ffmpeg's own default AV1 encode: `-c:v libaom-av1`, nothing else set but the
/// single-thread pin.
fn ffmpeg_encode(src: &Path, out: &Path) -> Option<(std::time::Duration, usize)> {
    h::ffmpeg::encode_with(src, out, &["-c:v", "libaom-av1"], REPS)
}

/// Time every decoder arm over one bitstream.
#[allow(clippy::too_many_arguments)]
fn decode_arms(
    rows: &mut Vec<h::Row>,
    packets: &[Vec<u8>],
    ivf: &Path,
    c: &h::ClipEntry,
    clip: &h::y4m::Clip,
    frames: usize,
    source: &str,
    refs_ok: bool,
) {
    // ours (in-process, bytes already in RAM)
    match ours::decode_speed(packets, REPS) {
        Ok((d, n)) => {
            let mut r = h::Row::new(c, clip, n, "ours", "decode");
            r.source = source.into();
            r.fill(clip, n, d, packets.iter().map(|p| p.len()).sum());
            eprintln!(
                "  ours   dec {:>9.1} ms {:>8.2} fps {:>7.3} Mpx/s  ({source} stream)",
                r.wall_ms, r.fps, r.mpx_s
            );
            rows.push(r);
        }
        Err(e) => eprintln!("    ! ours decode: {e}"),
    }
    // dav1d (the C original our decoder is a port of)
    if refs_ok {
        match refs::dav1d_decode(ivf, REPS, false) {
            Ok(run) => {
                let n = run.frames.max(frames);
                let mut r = h::Row::new(c, clip, n, "dav1d", "decode");
                r.source = source.into();
                r.fill(clip, n, run.inner, 0);
                eprintln!(
                    "  dav1d  dec {:>9.1} ms {:>8.2} fps {:>7.3} Mpx/s  ({source} stream)",
                    r.wall_ms, r.fps, r.mpx_s
                );
                rows.push(r);
            }
            Err(e) => eprintln!("    ! dav1d decode: {e}"),
        }
    }
    // ffmpeg's default AV1 decoder
    if let Some(d) = h::ffmpeg::decode_with(ivf, &["-c:v", "libdav1d"], REPS) {
        let mut r = h::Row::new(c, clip, frames, "ffmpeg", "decode");
        r.source = source.into();
        r.fill(clip, frames, d, 0);
        eprintln!(
            "  ffmpeg dec {:>9.1} ms {:>8.2} fps {:>7.3} Mpx/s  ({source} stream)",
            r.wall_ms, r.fps, r.mpx_s
        );
        rows.push(r);
    }
}

// ---------------------------------------------------------------------------

fn cmd_stages() {
    if !cfg!(feature = "profile") {
        eprintln!("!! built WITHOUT the `profile` feature — every stage bucket would read 0.");
        eprintln!("   Rebuild with --features profile. Aborting.");
        std::process::exit(2);
    }
    let refs_ok = refs::check().is_ok();
    let clips = h::manifest();
    let tmp = h::scratch(CODEC);
    let mut rows: Vec<h::StageRow> = Vec::new();

    for c in &clips {
        let clip = match h::y4m::read(&c.path, 0) {
            Ok(v) => v,
            Err(e) => {
                eprintln!("  ! {e}");
                continue;
            }
        };
        let n = h::frames_per_clip(clip.frames.len(), FRAMES);
        eprintln!("\n=== stages: {} ({n} frames) ===", c.name);
        // Each pass is a full encode; at 1080p that is tens of seconds and the
        // medians are stable well before then.
        let passes = if clip.width * clip.height > 1_000_000 { 1 } else { PASSES };

        rows.extend(enc_rows(&c.name, &ours::encode_stages(&clip, n, passes)));
        if let Ok((_, packets)) = ours::encode_speed(&clip, n, 1) {
            rows.extend(dec_rows(&c.name, &ours::decode_stages(&packets, passes)));
        }
        eprintln!("  ours   encode + decode stages captured");

        if refs_ok {
            let src = tmp.join(format!("{}.src.y4m", c.name));
            if h::y4m::write_prefix(&clip, &src, n).is_ok() {
                let out = tmp.join(format!("{}.aomp.ivf", c.name));
                match refs::aom_encode(&src, &out, true) {
                    Ok(run) => {
                        rows.extend(ref_rows(&c.name, "libaom", "encode", &run));
                        eprintln!("  libaom encode: {} stage buckets", run.stages.len());
                    }
                    Err(e) => eprintln!("  libaom encode: {e}"),
                }
                match refs::dav1d_decode(&out, passes, true) {
                    Ok(run) => {
                        rows.extend(ref_rows(&c.name, "dav1d", "decode", &run));
                        eprintln!("  dav1d  decode: {} stage buckets", run.stages.len());
                    }
                    Err(e) => eprintln!("  dav1d decode: {e}"),
                }
                let _ = std::fs::remove_file(&src);
                let _ = std::fs::remove_file(&out);
            }
        }
        h::write_stages(&h::results_dir(CODEC).join("stages.tsv"), &rows);
    }
    eprintln!("\n{} stage row(s) over {} clip(s)", rows.len(), clips.len());
}

/// Turn a rav1e snapshot into rows.
///
/// rav1e's profiler is nested-INCLUSIVE and publishes the nesting itself:
/// `Total` wraps everything; the top-level stages are those that are neither
/// `Total`, an RDO child, nor info; the RDO children live inside `PartitionRdo`.
/// So the partition is: top-level stages (with `PartitionRdo` replaced by its
/// children plus its own glue) + the residue.
fn enc_rows(clip: &str, snap: &[(ours::enc_prof::Stage, f64, u64)]) -> Vec<h::StageRow> {
    use ours::enc_prof::Stage;
    let get = |s: Stage| snap.iter().find(|(x, ..)| *x == s).map(|&(_, ms, c)| (ms, c));
    let total = get(Stage::Total).map(|(ms, _)| ms).unwrap_or(0.0);
    let pdo = get(Stage::PartitionRdo).map(|(ms, _)| ms).unwrap_or(0.0);
    let sum_top: f64 = snap
        .iter()
        .filter(|(s, ..)| *s != Stage::Total && !s.is_rdo_child() && !s.is_info())
        .map(|&(_, ms, _)| ms)
        .sum();
    let sum_children: f64 = snap
        .iter()
        .filter(|(s, ..)| s.is_rdo_child())
        .map(|&(_, ms, _)| ms)
        .sum();
    let residue = (total - sum_top).max(0.0);
    let denom = total.max(1e-9);

    let mut rows = Vec::new();
    let mut push = |stage: &str, scope: &str, ms: f64, calls: u64| {
        rows.push(h::StageRow {
            clip: clip.into(),
            codec: "ours".into(),
            kind: "encode".into(),
            stage: stage.into(),
            scope: scope.into(),
            ms,
            calls,
            pct: 100.0 * ms / denom,
        });
    };
    for &(s, ms, calls) in snap {
        if s == Stage::Total || calls == 0 {
            continue;
        }
        let scope = if s.is_info() {
            "info"
        } else if s == Stage::PartitionRdo {
            "incl"
        } else {
            "self"
        };
        push(s.name(), scope, ms, calls);
    }
    // PartitionRdo's own time: the pure-Rust search/context/entropy overhead no
    // kernel scope captures. This is the row that pairs with a C encoder's glue.
    push(
        "partition+mode RDO(self)",
        "self",
        (pdo - sum_children).max(0.0),
        get(Stage::PartitionRdo).map(|(_, c)| c).unwrap_or(0),
    );
    push("orchestration/glue", "self", residue, 0);
    push("TOTAL", "self", total, 0);
    rows
}

/// Turn a rav1d snapshot into rows. `TileSbrow` contains `ReconIntra`/
/// `ReconInter`; everything else at top level is disjoint.
fn dec_rows(clip: &str, snap: &[(ours::dec_prof::Stage, f64, u64)]) -> Vec<h::StageRow> {
    use ours::dec_prof::Stage;
    let get = |s: Stage| snap.iter().find(|(x, ..)| *x == s).map(|&(_, ms, c)| (ms, c));
    let total = get(Stage::Total).map(|(ms, _)| ms).unwrap_or(0.0);
    let tile = get(Stage::TileSbrow).map(|(ms, _)| ms).unwrap_or(0.0);
    let recon: f64 = [Stage::ReconIntra, Stage::ReconInter]
        .iter()
        .filter_map(|&s| get(s).map(|(ms, _)| ms))
        .sum();
    let sum_top: f64 = snap
        .iter()
        .filter(|(s, ..)| {
            *s != Stage::Total
                && !s.is_info()
                && !matches!(*s, Stage::ReconIntra | Stage::ReconInter)
        })
        .map(|&(_, ms, _)| ms)
        .sum();
    let denom = total.max(1e-9);

    let mut rows = Vec::new();
    let mut push = |stage: &str, scope: &str, ms: f64, calls: u64| {
        rows.push(h::StageRow {
            clip: clip.into(),
            codec: "ours".into(),
            kind: "decode".into(),
            stage: stage.into(),
            scope: scope.into(),
            ms,
            calls,
            pct: 100.0 * ms / denom,
        });
    };
    for &(s, ms, calls) in snap {
        if s == Stage::Total || calls == 0 {
            continue;
        }
        let scope = if s.is_info() {
            "info"
        } else if s == Stage::TileSbrow {
            "incl"
        } else {
            "self"
        };
        push(s.name(), scope, ms, calls);
    }
    push(
        "tile decode(self)",
        "self",
        (tile - recon).max(0.0),
        get(Stage::TileSbrow).map(|(_, c)| c).unwrap_or(0),
    );
    push("orchestration/glue", "self", (total - sum_top).max(0.0), 0);
    push("TOTAL", "self", total, 0);
    rows
}

/// A C reference dump — already exclusive self-time and already percented.
fn ref_rows(clip: &str, codec: &str, kind: &str, r: &refs::Run) -> Vec<h::StageRow> {
    r.stages
        .iter()
        .map(|(stage, ms, calls, pct)| h::StageRow {
            clip: clip.into(),
            codec: codec.into(),
            kind: kind.into(),
            stage: stage.clone(),
            scope: if stage.starts_with('_') { "info" } else { "self" }.into(),
            ms: *ms,
            calls: *calls,
            pct: *pct,
        })
        .collect()
}

// ---------------------------------------------------------------------------

fn main() {
    let mode = std::env::args().nth(1).unwrap_or_else(|| "help".into());
    match mode.as_str() {
        "speed" => cmd_speed(),
        "stages" => cmd_stages(),
        "report" => report::generate(&h::results_dir(CODEC)),
        _ => {
            eprintln!("usage: analyzer-av1 <speed|stages|report>");
            eprintln!("  speed   profiler OFF — throughput / size / PSNR / SSIM at defaults");
            eprintln!("  stages  profiler ON  — per-function ms, %, calls, ns/call");
            eprintln!("            (requires --features profile)");
            eprintln!("  report  merge results/av1/*.tsv into results/av1/REPORT.md");
            eprintln!();
            eprintln!("env: CLIPS=a,b   restrict the corpus");
            eprintln!("     FRAMES=N    frames per clip (0 = whole clip; default 30)");
            eprintln!("     CLIPS_DIR   where the .y4m files live");
            eprintln!("     AOM_DIR / DAV1D_DIR   the C reference checkouts");
            eprintln!("     FFMPEG      the upstream ffmpeg binary");
            std::process::exit(1);
        }
    }
}
