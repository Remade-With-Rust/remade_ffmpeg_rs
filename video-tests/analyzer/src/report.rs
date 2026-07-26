//! Merge `speed.tsv` + `stages.tsv` into `REPORT.md`.
//!
//! Reads the TSVs rather than re-measuring, so the report can be regenerated (and
//! its presentation changed) without re-running hours of encodes.

use std::collections::BTreeMap;
use std::path::Path;

struct Speed {
    clip: String,
    class: String,
    width: usize,
    height: usize,
    frames: usize,
    codec: String,
    kind: String,
    source: String,
    wall_ms: f64,
    fps: f64,
    mpx_s: f64,
    bytes: usize,
    kbps: f64,
    psnr: f64,
    ssim: f64,
}

struct Stage {
    clip: String,
    codec: String,
    kind: String,
    stage: String,
    scope: String,
    ms: f64,
    calls: u64,
    pct: f64,
}

fn read_speed(dir: &Path) -> Vec<Speed> {
    let txt = std::fs::read_to_string(dir.join("speed.tsv")).unwrap_or_default();
    txt.lines()
        .skip(1)
        .filter_map(|l| {
            let f: Vec<&str> = l.split('\t').collect();
            if f.len() < 15 {
                return None;
            }
            Some(Speed {
                clip: f[0].into(),
                class: f[1].into(),
                width: f[2].parse().ok()?,
                height: f[3].parse().ok()?,
                frames: f[4].parse().ok()?,
                codec: f[5].into(),
                kind: f[6].into(),
                source: f[7].into(),
                wall_ms: f[8].parse().ok()?,
                fps: f[9].parse().ok()?,
                mpx_s: f[10].parse().ok()?,
                bytes: f[11].parse().ok()?,
                kbps: f[12].parse().ok()?,
                psnr: f[13].parse().unwrap_or(f64::NAN),
                ssim: f[14].parse().unwrap_or(f64::NAN),
            })
        })
        .collect()
}

fn read_stages(dir: &Path) -> Vec<Stage> {
    let txt = std::fs::read_to_string(dir.join("stages.tsv")).unwrap_or_default();
    txt.lines()
        .skip(1)
        .filter_map(|l| {
            let f: Vec<&str> = l.split('\t').collect();
            if f.len() < 9 {
                return None;
            }
            Some(Stage {
                clip: f[0].into(),
                codec: f[1].into(),
                kind: f[2].into(),
                stage: f[3].into(),
                scope: f[4].into(),
                ms: f[5].parse().ok()?,
                calls: f[6].parse().ok()?,
                pct: f[8].parse().ok()?,
            })
        })
        .collect()
}

fn fmt(v: f64, dp: usize) -> String {
    if v.is_nan() {
        "-".into()
    } else {
        format!("{v:.dp$}")
    }
}

/// Aggregate a stage table across clips: total ms and total calls per stage,
/// then percent of the aggregated total. Cross-clip aggregation is by SUM, not
/// by averaging percentages — averaging the per-clip shares would weight a
/// 25 ms QCIF encode the same as a 30 s 1080p one.
fn aggregate(rows: &[&Stage]) -> Vec<(String, String, f64, u64, f64)> {
    let mut by: BTreeMap<String, (String, f64, u64)> = BTreeMap::new();
    for r in rows {
        let e = by
            .entry(r.stage.clone())
            .or_insert((r.scope.clone(), 0.0, 0));
        e.1 += r.ms;
        e.2 += r.calls;
    }
    // TOTAL is the sum of the SELF buckets; inclusive and info rows would
    // double-count, and the profilers already publish an explicit TOTAL row.
    let total = by
        .get("TOTAL")
        .map(|e| e.1)
        .filter(|t| *t > 0.0)
        .unwrap_or_else(|| {
            by.iter()
                .filter(|(k, v)| v.0 == "self" && k.as_str() != "TOTAL")
                .map(|(_, v)| v.1)
                .sum()
        })
        .max(1e-9);
    let mut out: Vec<(String, String, f64, u64, f64)> = by
        .into_iter()
        .map(|(k, (scope, ms, calls))| (k, scope, ms, calls, 100.0 * ms / total))
        .collect();
    // Biggest first — the report is read top-down to pick an optimisation target.
    out.sort_by(|a, b| b.2.partial_cmp(&a.2).unwrap_or(std::cmp::Ordering::Equal));
    out
}

fn stage_table(out: &mut String, rows: &[&Stage], title: &str, note: &str) {
    if rows.is_empty() {
        return;
    }
    out.push_str(&format!("\n#### {title}\n\n{note}\n\n"));
    out.push_str("| stage | scope | ms | % | calls | ns/call |\n");
    out.push_str("|---|---|---:|---:|---:|---:|\n");
    for (stage, scope, ms, calls, pct) in aggregate(rows) {
        let nspc = if calls > 0 {
            format!("{:.1}", ms * 1e6 / calls as f64)
        } else {
            "-".into()
        };
        let (ms_s, pct_s) = if stage.starts_with("count:") {
            ("-".to_string(), "-".to_string())
        } else {
            (fmt(ms, 2), fmt(pct, 2))
        };
        out.push_str(&format!(
            "| `{stage}` | {scope} | {ms_s} | {pct_s} | {calls} | {nspc} |\n"
        ));
    }
}

pub fn generate(dir: &Path) {
    let speed = read_speed(dir);
    let stages = read_stages(dir);
    let mut o = String::new();

    o.push_str("# VP9 — function-level speed report\n\n");
    o.push_str(
        "`rff-vp9` against the libvpx reference and ffmpeg, on the pinned `video-tests` \
         corpus. **Everything at default settings on every arm**, single-threaded.\n\n\
         Generated by `video-tests/analyzer` — regenerate with \
         `bash video-tests/run_analysis.sh`.\n\n",
    );
    o.push_str(
        "**How to read the arms.** `ffmpeg` is the bar: `-c:v libvpx-vp9` at its own defaults \
         for encode, the native `vp9` decoder for decode, timed net of process startup, \
         totals only (the shipped binary is stripped, so it cannot be attributed per \
         function). `libvpx` is our own SIMD build of the same library — verified \
         BYTE-IDENTICAL output and matching wall time against ffmpeg's at matched \
         settings — which *can* be attributed per function. `ours` is `rff-vp9` in \
         process, through the same public codec registry the CLI uses.\n\n",
    );

    // ---- throughput ------------------------------------------------------
    if !speed.is_empty() {
        o.push_str("## Throughput\n\n### Encode\n\n");
        o.push_str("| clip | class | size | frames | codec | ms | fps | Mpx/s | kb/s | PSNR dB | SSIM |\n");
        o.push_str("|---|---|---|---:|---|---:|---:|---:|---:|---:|---:|\n");
        for r in speed.iter().filter(|r| r.kind == "encode") {
            o.push_str(&format!(
                "| {} | {} | {}x{} | {} | {} | {} | {} | {} | {} | {} | {} |\n",
                r.clip, r.class, r.width, r.height, r.frames, r.codec,
                fmt(r.wall_ms, 1), fmt(r.fps, 2), fmt(r.mpx_s, 3), fmt(r.kbps, 0),
                fmt(r.psnr, 2), fmt(r.ssim, 4)
            ));
        }

        o.push_str("\n### Decode\n\n");
        o.push_str(
            "Each decoder is run against BOTH bitstreams — ours and the reference's — \
             because decode cost depends on what the encoder chose, not only on the \
             decoder.\n\n",
        );
        o.push_str("| clip | size | frames | stream | decoder | ms | fps | Mpx/s |\n");
        o.push_str("|---|---|---:|---|---|---:|---:|---:|\n");
        for r in speed.iter().filter(|r| r.kind == "decode") {
            o.push_str(&format!(
                "| {} | {}x{} | {} | {} | {} | {} | {} | {} |\n",
                r.clip, r.width, r.height, r.frames, r.source, r.codec,
                fmt(r.wall_ms, 1), fmt(r.fps, 2), fmt(r.mpx_s, 3)
            ));
        }

        // Head-to-head ratios, the line most likely to be quoted.
        o.push_str("\n### Head to head (Mpx/s, higher is better)\n\n");
        o.push_str("| clip | kind | ours | libvpx | ffmpeg | ours vs libvpx |\n");
        o.push_str("|---|---|---:|---:|---:|---:|\n");
        let clips: Vec<String> = {
            let mut v: Vec<String> = speed.iter().map(|r| r.clip.clone()).collect();
            v.dedup();
            v
        };
        for c in &clips {
            for kind in ["encode", "decode"] {
                let pick = |codec: &str| -> Option<f64> {
                    speed
                        .iter()
                        .find(|r| {
                            &r.clip == c
                                && r.kind == kind
                                && r.codec == codec
                                // For decode, compare on the reference bitstream:
                                // the stream every decoder in the wild sees.
                                && (kind == "encode" || r.source == "libvpx")
                        })
                        .map(|r| r.mpx_s)
                };
                let (a, b, f) = (pick("ours"), pick("libvpx"), pick("ffmpeg"));
                if a.is_none() && b.is_none() {
                    continue;
                }
                let ratio = match (a, b) {
                    (Some(a), Some(b)) if b > 0.0 => format!("{:.2}x", a / b),
                    _ => "-".into(),
                };
                o.push_str(&format!(
                    "| {} | {} | {} | {} | {} | {} |\n",
                    c, kind,
                    a.map(|v| fmt(v, 3)).unwrap_or("-".into()),
                    b.map(|v| fmt(v, 3)).unwrap_or("-".into()),
                    f.map(|v| fmt(v, 3)).unwrap_or("-".into()),
                    ratio
                ));
            }
        }
    }

    // ---- per-function ----------------------------------------------------
    o.push_str("\n## Per-function breakdown\n\n");
    o.push_str(
        "Aggregated over the whole corpus by SUM of milliseconds — not by averaging \
         per-clip percentages, which would weight a 25 ms QCIF encode the same as a \
         30 s 1080p one.\n\n\
         `scope` distinguishes **self** (exclusive: time in this function only, \
         children excluded — these sum to 100%), **incl** (inclusive: contains the \
         nested children listed under it), and **info** (a diagnostic that is \
         deliberately NOT part of the partition — summing it double-counts).\n\n\
         Our decoder and both libvpx tables are exclusive by construction. Our \
         *encoder* profiler is nested-inclusive, so parents appear twice: once as \
         measured (`incl`) and once as `name(self)` with the scoped children \
         subtracted — the `self` row is the one that pairs with the reference.\n",
    );

    for kind in ["encode", "decode"] {
        for codec in ["ours", "libvpx"] {
            let rows: Vec<&Stage> = stages
                .iter()
                .filter(|s| s.kind == kind && s.codec == codec)
                .collect();
            let note = match (codec, kind) {
                ("ours", "encode") =>
                    "`rff-vp9` encoder. Denominator is the profiler's disjoint top-level \
                     set; `orchestration/glue` is the decision-pass time not inside any \
                     scoped stage.",
                ("ours", "decode") =>
                    "`rff-vp9` decoder, exclusive self-time. `other/glue` is unattributed \
                     work INSIDE a frame decode only — time between frames is discarded, \
                     not folded in.",
                ("libvpx", "encode") =>
                    "The libvpx reference encoder (what ffmpeg's `libvpx-vp9` runs), \
                     exclusive self-time from the instrumented twin.",
                _ =>
                    "The libvpx reference decoder, exclusive self-time from the \
                     instrumented twin.",
            };
            stage_table(
                &mut o,
                &rows,
                &format!("{codec} — {kind}"),
                note,
            );
        }
    }

    o.push_str(
        "\n### Caveats that bound these numbers\n\n\
         * `_INSTRUMENT_TAX` (libvpx rows) is the profiler measuring itself: scope \
           entries x the measured per-scope cost on this machine. When the glue \
           bucket is the same order as the tax, there is no hidden work left — only \
           the instrument.\n\
         * Both profilers subtract the per-scope `rdtsc` latency from each bucket, so \
           the millions-of-calls kernels are not inflated by their own taps.\n\
         * `adapt_probs` reads zero on libvpx streams because libvpx defaults to \
           `frame_parallel_decoding_mode = 1`, which disables backward adaptation. \
           That is the encoder's default, not a missing tap.\n\
         * AVX-512 is compiled but never dispatched: this CPU has it fused off, so the \
           runtime detector selects AVX2 — exactly as it does inside ffmpeg's libvpx.\n",
    );

    let path = dir.join("REPORT.md");
    std::fs::write(&path, o).expect("write REPORT.md");
    eprintln!("wrote {}", path.display());
}
