//! `mp3lab` — the MP3 encoder lab CLI.
//!
//! ```text
//!   cargo run -p rusty_mp3 --features lab --example mp3lab -- bricks
//!   cargo run -p rusty_mp3 --features lab --example mp3lab -- next
//!   cargo run -p rusty_mp3 --features lab --example mp3lab -- corpus
//!   cargo run -p rusty_mp3 --features lab --example mp3lab -- variants N4
//!   cargo run -p rusty_mp3 --features lab --example mp3lab -- run N4 iso
//!   cargo run ... -- run N4 iso --bias 0.0 --step 0.0008   # override on the fly
//! ```
//!
//! `run` writes a JSON report to `lab-results/<brick>-<variant>.json` next to the
//! crate, so experiment results accumulate and diff over time.

use std::fs;
use std::path::PathBuf;

use rusty_mp3::lab::{self, bricks, experiment::Overrides, quantizer, signals, variant};

fn main() {
    let args: Vec<String> = std::env::args().skip(1).collect();
    let cmd = args.first().map(String::as_str).unwrap_or("help");

    match cmd {
        "bricks" => print!("{}", bricks::table()),
        "next" => match bricks::next_unbuilt() {
            Some(b) => println!("next to build:\n{b}"),
            None => println!("all bricks built 🎉"),
        },
        "corpus" => {
            for s in signals::corpus() {
                println!(
                    "  {:<18} {} samples @ {} Hz",
                    s.name,
                    s.pcm.len(),
                    s.sample_rate
                );
            }
        }
        "variants" => {
            let Some(id) = args.get(1) else {
                eprintln!("usage: mp3lab variants <brick>");
                std::process::exit(2);
            };
            match id.to_ascii_uppercase().as_str() {
                "N4" => print!("{}", variant::list(quantizer::VARIANTS)),
                other => {
                    eprintln!("brick {other} has no variant table yet");
                    std::process::exit(1);
                }
            }
        }
        "run" => run(&args[1..]),
        _ => {
            eprintln!(
                "mp3lab — MP3 encoder lab\n\n\
                 commands:\n  \
                 bricks              status table of every brick\n  \
                 next                the next brick to build\n  \
                 corpus              list the test signals\n  \
                 variants <brick>    list a brick's variants\n  \
                 run <brick> <variant> [--bias x] [--step x]\n"
            );
        }
    }
}

fn run(args: &[String]) {
    if args.len() < 2 {
        eprintln!("usage: mp3lab run <brick> <variant> [--bias x] [--step x]");
        std::process::exit(2);
    }
    let brick = &args[0];
    let variant = &args[1];

    // Parse the on-the-fly overrides.
    let mut ov = Overrides::default();
    let mut i = 2;
    while i < args.len() {
        match args[i].as_str() {
            "--bias" => {
                ov.bias = args.get(i + 1).and_then(|v| v.parse().ok());
                i += 2;
            }
            "--step" => {
                ov.step = args.get(i + 1).and_then(|v| v.parse().ok());
                i += 2;
            }
            other => {
                eprintln!("unknown flag {other}");
                std::process::exit(2);
            }
        }
    }

    match lab::run(brick, variant, ov) {
        Ok(report) => {
            print!("{}", report.to_text());
            let dir = PathBuf::from(env!("CARGO_MANIFEST_DIR")).join("lab-results");
            if let Err(e) = fs::create_dir_all(&dir) {
                eprintln!("warn: could not create {}: {e}", dir.display());
                return;
            }
            let path = dir.join(format!("{}-{}.json", report.brick, report.variant));
            match fs::write(&path, report.to_json()) {
                Ok(()) => println!("\nwrote {}", path.display()),
                Err(e) => eprintln!("warn: could not write {}: {e}", path.display()),
            }
        }
        Err(msg) => {
            eprintln!("{msg}");
            std::process::exit(1);
        }
    }
}

// Primary allocator for this target: our rusty_alloc, the pure-Rust mimalloc
// remake. Codec hot paths allocate heavily and the system heap dominates the
// profile there (measured 1.38x end-to-end on AV2 decode). Per project
// convention this belongs in binary/bench/example roots, never in a library --
// a library that declares one hijacks every dependent's allocator choice.
#[global_allocator]
static RUSTY_ALLOC: rusty_alloc_api::RustyAlloc = rusty_alloc_api::RustyAlloc;
