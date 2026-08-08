//! `aacexport` — write the deterministic synthetic corpus out as WAV files so
//! external tools (ffmpeg, PEAQ) can consume it.
//!
//! ```text
//! cargo run -p rusty_aac --features lab --release --example aacexport -- <outdir>
//! ```
//!
//! Written as **s16**: the PEAQ driver scales float input by 32768 when it sees
//! `|x| <= 1.5`, so a float WAV is interpreted by a heuristic. s16 removes the
//! guess, and the corpus peaks at 0.7 so the quantization headroom is ample.

use rusty_aac::lab::{corpus, wav};

#[global_allocator]
static RUSTY_ALLOC: rusty_alloc_api::RustyAlloc = rusty_alloc_api::RustyAlloc;

fn main() {
    let out = std::env::args().nth(1).unwrap_or_else(|| ".".to_string());
    std::fs::create_dir_all(&out).expect("create outdir");
    for sig in corpus::corpus() {
        let path = format!("{out}/syn_{}.wav", sig.name());
        wav::write_s16(&path, &sig.pcm, sig.channels, sig.sample_rate).expect("write wav");
        println!(
            "{path}\t{} ch\t{} Hz\t{} frames",
            sig.channels,
            sig.sample_rate,
            sig.frames()
        );
    }
}
