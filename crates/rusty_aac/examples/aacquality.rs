//! `aacquality` — the AAC quality-ladder driver (P0 of the Great Gate campaign,
//! `docs/codec-aac-great-gate.md`).
//!
//! Runs the deterministic 8-class corpus through the encoder at four operating
//! points, decodes each stream, and prints the per-clip × per-bitrate NMR table
//! that every rung's verdict is read from. The **null arm runs first**: if the
//! encoder is not byte-deterministic, no delta in the table below means anything,
//! and the run says so and exits non-zero.
//!
//! Usage:
//! ```text
//! cargo run -p rusty_aac --features lab --release --example aacquality
//! cargo run -p rusty_aac --features lab --release --example aacquality -- --signals
//! cargo run -p rusty_aac --features lab --release --example aacquality -- --bitrates 96000,128000
//! ```
//!
//! NMR is the **screen**, not the verdict — see `lab::quality`. PEAQ/ODG against
//! `ffmpeg -c:a aac` remains the banking instrument for any rung.

use rusty_aac::lab::{corpus, ladder, signals::AacSignals};
use rusty_aac::{AacEncoderConfig, WindowShape};
use std::process::ExitCode;

// Primary allocator for this target: our rusty_alloc, the pure-Rust mimalloc
// remake. PROJECT CONVENTION (`CLAUDE.md`) — every encoder-carrying binary,
// bench and example runs under it, because it is what ships. Examples are a
// SEPARATE compilation unit from the library (which correctly declares none), so
// without this line the ladder would silently measure the system heap while the
// shipped binary runs on rusty_alloc. Measured gap on AV2 decode: 1.38x.
#[global_allocator]
static RUSTY_ALLOC: rusty_alloc_api::RustyAlloc = rusty_alloc_api::RustyAlloc;

fn main() -> ExitCode {
    let args: Vec<String> = std::env::args().skip(1).collect();
    let show_signals = args.iter().any(|a| a == "--signals");
    let bitrates: Vec<u32> = args
        .windows(2)
        .find(|w| w[0] == "--bitrates")
        .map(|w| {
            w[1].split(',')
                .filter_map(|s| s.trim().parse().ok())
                .collect()
        })
        .unwrap_or_else(|| ladder::DEFAULT_BITRATES.to_vec());

    println!("rusty_aac quality ladder — allocator: rusty_alloc (project default)");
    println!("corpus: {} classes, {} operating points\n", corpus::Class::all().len(), bitrates.len());

    // ---- the null arm, before anything is believed -------------------------
    print!("null arm (encode twice, require byte-identical) ... ");
    let null = ladder::null_arm(&bitrates[..1.min(bitrates.len())]);
    if null.is_clean() {
        println!("CLEAN ({} cells)", null.identical);
    } else {
        println!("FAILED");
        for (c, b) in &null.divergent {
            println!("  nondeterministic: {} @ {} bps", c.name(), b);
        }
        eprintln!("\nThe encoder is not byte-deterministic. Every delta this harness");
        eprintln!("could report is noise of unknown size. Fix before measuring.");
        return ExitCode::FAILURE;
    }

    // ---- the ladder ---------------------------------------------------------
    println!("\n{:<20} {:>7} {:>10} {:>9} {:>9} {:>8}", "clip", "target", "measured", "audible%", "meanNMR", "frames");
    println!("{}", "-".repeat(68));

    let mut worst_rate_err = 0f64;
    for sig in corpus::corpus() {
        for &b in &bitrates {
            let p = ladder::point(&sig, b);
            worst_rate_err = worst_rate_err.max(p.rate_error().abs());
            println!(
                "{:<20} {:>6}k {:>9.0} {:>9.2} {:>9.1} {:>8}",
                sig.name(),
                b / 1000,
                p.measured_bps,
                p.nmr.pct_audible,
                p.nmr.mean_nmr_db,
                p.nmr.frames,
            );
        }
        println!();
    }
    println!("worst rate error vs target: {:.1}%", worst_rate_err * 100.0);

    // ---- the P1 signal vector ----------------------------------------------
    if show_signals {
        println!("\nP1 content signals (per-clip aggregates; gates read percentiles, not these)");
        println!(
            "{:<20} {:>9} {:>9} {:>9} {:>10} {:>9} {:>9}",
            "clip", "atk p90", "tonality", "lpc p75", "loud dBFS", "PE p90", "rolloff"
        );
        println!("{}", "-".repeat(80));
        for sig in corpus::corpus() {
            let planes = sig.planes();
            let refs: Vec<&[f32]> = planes.iter().map(|p| p.as_slice()).collect();
            let s = AacSignals::analyze(&refs, sig.sample_rate);
            println!(
                "{:<20} {:>9.2} {:>9.3} {:>9.2} {:>10.1} {:>9.0} {:>8.0}k",
                sig.name(),
                s.percentile_of(|f| f.attack_max, 0.90),
                s.mean_of(|f| f.tonality_mean),
                s.percentile_of(|f| f.lpc_gain, 0.75),
                s.mean_of(|f| f.loudness_dbfs),
                s.percentile_of(|f| f.pe, 0.90),
                s.mean_of(|f| f.rolloff_hz) / 1000.0,
            );
        }
    }

    // ---- Rung 0 (arm A8): the force-on comparison ---------------------------
    if args.iter().any(|a| a == "--arms") {
        println!("\nRung 0 / arm A8 — window shape. FORCE-ON comparison.");
        println!(
            "great-gate §4: force-on-everywhere must nearly TIE the anchor on the full\n\
             ladder before a dispatch is built on it — a big force-on gap predicts a\n\
             dominated dispatch. Delta is (arm − sine); NEGATIVE audible% = better."
        );
        println!(
            "\n{:<20} {:>7} {:>11} {:>11} {:>11}",
            "clip", "target", "sine aud%", "kbd Δaud%", "auto Δaud%"
        );
        println!("{}", "-".repeat(64));

        let (mut kbd_wins, mut kbd_losses) = (0u32, 0u32);
        let (mut sum_kbd, mut sum_auto, mut cells) = (0f64, 0f64, 0u32);
        for sig in corpus::corpus() {
            for &b in &bitrates {
                let cfg = |shape| AacEncoderConfig {
                    bitrate_bps: b,
                    window_shape: shape,
                    ..Default::default()
                };
                let s = ladder::point_with(&sig, cfg(WindowShape::Sine));
                let k = ladder::point_with(&sig, cfg(WindowShape::Kbd));
                let a = ladder::point_with(&sig, cfg(WindowShape::Auto));
                let dk = k.nmr.pct_audible - s.nmr.pct_audible;
                let da = a.nmr.pct_audible - s.nmr.pct_audible;
                if dk < -0.01 {
                    kbd_wins += 1;
                } else if dk > 0.01 {
                    kbd_losses += 1;
                }
                sum_kbd += dk as f64;
                sum_auto += da as f64;
                cells += 1;
                println!(
                    "{:<20} {:>6}k {:>11.2} {:>+11.2} {:>+11.2}",
                    sig.name(),
                    b / 1000,
                    s.nmr.pct_audible,
                    dk,
                    da
                );
            }
        }
        let n = cells.max(1) as f64;
        println!("\nmean Δaudible%:  kbd {:+.3}   auto {:+.3}", sum_kbd / n, sum_auto / n);
        println!("kbd per-cell sign: {kbd_wins} better, {kbd_losses} worse, {} tied",
                 cells - kbd_wins - kbd_losses);
        println!(
            "\nREAD THIS AS A SIGN TABLE, NOT A MEAN. A tool that wins on some content and\n\
             loses on other is a DISPATCH signal, not a mean-loss to discard. Banking any\n\
             of this needs the gate-calculator with work + pinned cpu_ms columns and\n\
             --attest-full-stack; until then it is HYPOTHESES ONLY."
        );
    }

    // ---- Rungs 1-3: per-class sign table ------------------------------------
    if args.iter().any(|a| a == "--rungs") {
        println!("\nRungs 1-3 — per-class Δaudible% vs the shipped encoder (NEGATIVE = better).");
        println!(
            "Exit criterion is WORST CLASS <= 0, verified per class, never on average.\n"
        );
        let arms: [(&str, fn(u32) -> AacEncoderConfig); 5] = [
            ("A6 PNS", |b| AacEncoderConfig {
                bitrate_bps: b,
                pns: true,
                ..Default::default()
            }),
            ("A7 intensity", |b| AacEncoderConfig {
                bitrate_bps: b,
                intensity: true,
                ..Default::default()
            }),
            ("A2 ton-SMR", |b| AacEncoderConfig {
                bitrate_bps: b,
                tonality_smr: true,
                ..Default::default()
            }),
            ("A3 TNS", |b| AacEncoderConfig {
                bitrate_bps: b,
                tns: true,
                ..Default::default()
            }),
            ("A9 detector", |b| AacEncoderConfig {
                bitrate_bps: b,
                relative_transients: true,
                ..Default::default()
            }),
        ];
        print!("{:<20} {:>7} {:>10}", "clip", "target", "base aud%");
        for (n, _) in arms.iter() {
            print!(" {:>12}", n);
        }
        println!();
        println!("{}", "-".repeat(102));

        let mut worst = [f32::NEG_INFINITY; 5];
        let mut sums = [0f64; 5];
        let mut cells = 0u32;
        for sig in corpus::corpus() {
            for &b in &bitrates {
                let base = ladder::point_with(
                    &sig,
                    AacEncoderConfig {
                        bitrate_bps: b,
                        ..Default::default()
                    },
                );
                let mut d = [0f32; 5];
                for (i, (_, mk)) in arms.iter().enumerate() {
                    let p = ladder::point_with(&sig, mk(b));
                    d[i] = p.nmr.pct_audible - base.nmr.pct_audible;
                    worst[i] = worst[i].max(d[i]);
                    sums[i] += d[i] as f64;
                }
                cells += 1;
                print!(
                    "{:<20} {:>6}k {:>10.2}",
                    sig.name(),
                    b / 1000,
                    base.nmr.pct_audible
                );
                for v in d.iter() {
                    print!(" {:>+12.2}", v);
                }
                println!();
            }
        }
        let n = cells.max(1) as f64;
        println!("\n{:<16} {:>12} {:>12}", "arm", "mean Δ", "WORST Δ");
        for (i, (name, _)) in arms.iter().enumerate() {
            println!(
                "{:<16} {:>+12.3} {:>+12.3}  {}",
                name,
                sums[i] / n,
                worst[i],
                if worst[i] <= 0.0 { "PASSES worst-class" } else { "FAILS worst-class" }
            );
        }
        println!(
            "\nAll three remain HYPOTHESES ONLY until a gate-calculator harvest carries\n\
             work + pinned cpu_ms and --attest-full-stack. Off by default regardless."
        );
    }

    println!("\nNMR is the screen; PEAQ/ODG vs `ffmpeg -c:a aac` is the verdict.");
    ExitCode::SUCCESS
}
