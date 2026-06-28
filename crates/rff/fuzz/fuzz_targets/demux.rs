//! Coverage-guided fuzzing of every demuxer.
//!
//! The first input byte selects a demuxer (from the sorted list, so a crash
//! reproduces deterministically); the rest is fed in as the byte stream. The
//! contract under test is the same as `tests/demuxer_fuzz.rs`: parsing hostile
//! input may return an error, but must never panic, hang, or read out of
//! bounds. Run with `cargo +nightly fuzz run demux`.

#![no_main]

use std::io::Cursor;

use libfuzzer_sys::fuzz_target;
use rff::Engine;

fuzz_target!(|data: &[u8]| {
    if data.is_empty() {
        return;
    }
    let engine = Engine::new();

    // Stable, sorted name list so a given input always hits the same demuxer.
    let mut demuxers: Vec<&'static str> = engine
        .formats
        .iter()
        .filter(|f| f.can_demux())
        .map(|f| f.name)
        .collect();
    demuxers.sort_unstable();
    if demuxers.is_empty() {
        return;
    }

    let name = demuxers[data[0] as usize % demuxers.len()];
    let input = Box::new(Cursor::new(data[1..].to_vec()));
    if let Ok(mut demuxer) = engine.formats.open_demuxer(name, input) {
        let _ = demuxer.read_header();
        // Bounded so a demuxer that keeps emitting packets can't loop forever.
        for _ in 0..1024 {
            if demuxer.read_packet().is_err() {
                break;
            }
        }
    }
});
