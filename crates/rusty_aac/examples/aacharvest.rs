//! `aacharvest` — emit the **gate-calculator CSV** for an AAC arm.
//!
//! This is the piece that stands between a rung looking good and a rung being
//! *bankable*. `_greatgate/gate-calculator` is the sole banking authority; it
//! refuses a verdict from a harvest that cannot show its instruments, and until
//! now no AAC harvest existed at all.
//!
//! ```text
//! cargo run -p rusty_aac --features lab --release --example aacharvest -- \
//!     --arm a2 --bitrates 64000,96000,128000 > harvest-a2.csv
//! ```
//!
//! Then, from `_greatgate/gate-calculator/`:
//!
//! ```text
//! cargo run --release -- --input harvest-a2.csv --depth 3
//! ```
//!
//! # The measurement contract (`codec-measurement`)
//!
//! * **§15 — the counter is primary.** `work` is a deterministic count of the
//!   rate loop's exact-coder evaluations, read from `rusty_aac::encode::work`.
//!   One run, immune to scheduler drift, and it *sizes* the effect. The clock is
//!   confirmatory only.
//! * **§13 — never share a loop between deterministic and timed quantities.** The
//!   quality/work pass and the timing pass are separate; fusing them would drag
//!   the timing to one un-interleaved sample per point.
//! * **§3 — ABBA + a null arm.** Timing alternates base/arm per repetition, and
//!   the null arm (base vs base) is measured and printed as the resolution floor.
//! * **Honest about what is missing.** `cpu_ms` here is best-of-N **wall clock on
//!   an unpinned process**, because an example cannot set its own affinity
//!   portably. The method line says so, and this harvest is therefore NOT
//!   `--attest-full-stack` material on the speed axis until a pinned run replaces
//!   that column. That downgrade is the audit working, not a gap to paper over.

use rusty_aac::lab::{corpus, ladder, signals::AacSignals};
use rusty_aac::{encode::work, AacEncoderConfig};
use std::time::Instant;

// Primary allocator for this target — project convention (`CLAUDE.md`). The
// `cpu_ms` column below is only comparable to the shipped encoder because both
// run under rusty_alloc; measured gap on AV2 decode was 1.38x.
#[global_allocator]
static RUSTY_ALLOC: rusty_alloc_api::RustyAlloc = rusty_alloc_api::RustyAlloc;

/// Timing repetitions per (clip, bitrate) cell, ABBA-alternated.
const REPS: usize = 5;

fn arm_config(arm: &str, bitrate: u32) -> AacEncoderConfig {
    let base = AacEncoderConfig {
        bitrate_bps: bitrate,
        ..Default::default()
    };
    match arm {
        "a1" => AacEncoderConfig {
            short_block_psy: true,
            ..base
        },
        "a2" => AacEncoderConfig {
            tonality_smr: true,
            ..base
        },
        "a3" => AacEncoderConfig { tns: true, ..base },
        "a9" => AacEncoderConfig {
            relative_transients: true,
            ..base
        },
        "a9a1" => AacEncoderConfig {
            relative_transients: true,
            short_block_psy: true,
            ..base
        },
        other => panic!("unknown arm {other:?} (a1|a2|a3|a9|a9a1)"),
    }
}

/// Stable, name-keyed train/holdout split (great-gate §4 rule 4: the branch fit
/// and every leaf fit must share ONE split, and it must not move between rungs).
fn split_of(name: &str) -> &'static str {
    let h = name.bytes().fold(0u32, |a, b| a.wrapping_mul(31).wrapping_add(b as u32));
    if h % 2 == 0 {
        "train"
    } else {
        "holdout"
    }
}

/// Encode one clip and return (total work evals, per-frame audible fraction).
fn measure_quality(sig: &corpus::Signal, cfg: AacEncoderConfig) -> (u64, Vec<f32>) {
    let _ = work::take(); // clear
    let adts = ladder::encode_adts_with(sig, cfg);
    let (evals, _bands) = work::take();
    let decoded = ladder::decode_mono(&adts);
    let per_frame = rusty_aac::lab::quality::per_frame_audible(&sig.mono(), &decoded, sig.sample_rate);
    (evals, per_frame)
}

/// Best-of-N wall time for one configuration, in ms. Separate pass from quality.
fn time_ms(sig: &corpus::Signal, cfg: AacEncoderConfig) -> f64 {
    let t = Instant::now();
    let _ = ladder::encode_adts_with(sig, cfg);
    t.elapsed().as_secs_f64() * 1000.0
}

fn main() {
    let args: Vec<String> = std::env::args().skip(1).collect();
    let arm = args
        .windows(2)
        .find(|w| w[0] == "--arm")
        .map(|w| w[1].clone())
        .unwrap_or_else(|| "a2".to_string());
    let bitrates: Vec<u32> = args
        .windows(2)
        .find(|w| w[0] == "--bitrates")
        .map(|w| w[1].split(',').filter_map(|s| s.trim().parse().ok()).collect())
        .unwrap_or_else(|| vec![64_000, 96_000, 128_000]);

    // ---- method line, to stderr so it never contaminates the CSV -------------
    eprintln!("# aacharvest method line (codec-measurement)");
    eprintln!("#   arm            : {arm}");
    eprintln!("#   allocator      : rusty_alloc (project default)");
    eprintln!("#   unit           : one AAC frame of one (clip, bitrate) cell");
    eprintln!("#   quality        : per-frame audible-band fraction, our NMR mask");
    eprintln!("#   work           : DETERMINISTIC rate-loop exact-coder evals (PRIMARY)");
    eprintln!("#   cpu_ms         : best-of-{REPS} WALL clock, ABBA-alternated, UNPINNED");
    eprintln!("#   pinned         : NO  <-- speed axis is not bankable from this run");
    eprintln!("#   interleaved    : YES (ABBA per repetition)");
    eprintln!("#   full-stack     : quality measured through the complete routed arm");

    // ---- null arm first: the resolution floor -------------------------------
    let probe = corpus::signal(corpus::Class::MusicTonal);
    let cfg0 = AacEncoderConfig {
        bitrate_bps: 128_000,
        ..Default::default()
    };
    let mut null_pairs = Vec::new();
    for _ in 0..REPS {
        let a = time_ms(&probe, cfg0);
        let b = time_ms(&probe, cfg0);
        null_pairs.push((a - b).abs() / a.max(1e-9) * 100.0);
    }
    null_pairs.sort_by(|a, b| a.partial_cmp(b).unwrap());
    eprintln!(
        "#   NULL ARM       : median {:.2}%, worst {:.2}%  <-- resolution floor",
        null_pairs[null_pairs.len() / 2],
        null_pairs.last().copied().unwrap_or(0.0)
    );
    // Deterministic-work null: identical configs must count identically, or the
    // counter is not deterministic and the primary evidence is void.
    let (w1, _) = measure_quality(&probe, cfg0);
    let (w2, _) = measure_quality(&probe, cfg0);
    eprintln!(
        "#   WORK NULL      : {w1} vs {w2} -> {}",
        if w1 == w2 { "DETERMINISTIC" } else { "NON-DETERMINISTIC (counter void)" }
    );
    eprintln!("#");

    // ---- CSV ----------------------------------------------------------------
    println!(
        "gain,clip,clip_total,split,work,cpu_ms,shipped,bitrate_kbps,tonality,attack,lpc_gain,loudness_dbfs,pe,rolloff_hz"
    );

    for sig in corpus::corpus() {
        let planes = sig.planes();
        let refs: Vec<&[f32]> = planes.iter().map(|p| p.as_slice()).collect();
        let signals = AacSignals::analyze(&refs, sig.sample_rate);
        let split = split_of(sig.name());

        for &b in &bitrates {
            let base_cfg = AacEncoderConfig {
                bitrate_bps: b,
                ..Default::default()
            };
            let arm_cfg = arm_config(&arm, b);

            // Pass 1 — quality + deterministic work (one run each; no timing here).
            let (w_base, q_base) = measure_quality(&sig, base_cfg);
            let (w_arm, q_arm) = measure_quality(&sig, arm_cfg);

            // Pass 2 — timing, ABBA-alternated, best-of-N. Separate loop.
            let (mut t_base, mut t_arm) = (f64::INFINITY, f64::INFINITY);
            for r in 0..REPS {
                if r % 2 == 0 {
                    t_base = t_base.min(time_ms(&sig, base_cfg));
                    t_arm = t_arm.min(time_ms(&sig, arm_cfg));
                } else {
                    t_arm = t_arm.min(time_ms(&sig, arm_cfg));
                    t_base = t_base.min(time_ms(&sig, base_cfg));
                }
            }

            let nframes = q_base.len().min(q_arm.len()).max(1);
            // The clip's metric MASS, so the calculator can form macro_gain =
            // gain / clip_total. Every corpus harness here aggregates per clip,
            // so macro is the number that ships and micro is the one that
            // flatters a rule concentrated on a few big clips.
            let clip_total: f32 = q_base[..nframes].iter().sum::<f32>() * 100.0;
            // Per-unit attribution of the clip-level counters.
            let dwork = (w_base as f64 - w_arm as f64) / nframes as f64;
            let dms = (t_base - t_arm) / nframes as f64;

            for f in 0..nframes {
                // gain: percentage points of audible bands SAVED by the arm.
                // Positive = the arm is better, which is the sign convention the
                // calculator scores on.
                let gain = (q_base[f] - q_arm[f]) * 100.0;
                let fs = &signals.frames[f.min(signals.frames.len() - 1)];
                println!(
                    "{:.6},{},{:.4},{},{:.4},{:.6},{},{},{:.4},{:.4},{:.4},{:.2},{:.1},{:.0}",
                    gain,
                    sig.name(),
                    clip_total.max(1e-6),
                    split,
                    dwork,
                    dms,
                    if fs.shipped_transient { 1 } else { 0 },
                    b / 1000,
                    fs.tonality_mean,
                    fs.attack_max,
                    fs.lpc_gain,
                    fs.loudness_dbfs,
                    fs.pe,
                    fs.rolloff_hz,
                );
            }
        }
    }

    eprintln!("#");
    eprintln!("# Feed to: _greatgate/gate-calculator -- --input <this>.csv --depth 3");
    eprintln!("# Do NOT pass --attest-full-stack: cpu_ms above is unpinned wall clock.");
}
