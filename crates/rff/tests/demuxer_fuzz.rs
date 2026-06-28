//! Robustness sweep: every demuxer must *fail*, not *panic*, on hostile input.
//!
//! A media tool's demuxers are the first thing untrusted bytes reach, so a
//! panic there is a denial-of-service (and `panic=abort` builds would crash).
//! This feeds each registered demuxer a spread of malformed inputs — empty,
//! all-zero/all-one, pseudo-random, and real container magic followed by
//! garbage — and asserts it returns `Err`/EOF rather than unwinding. It is the
//! always-on, cross-platform complement to the `fuzz/` cargo-fuzz targets.

use std::io::Cursor;
use std::panic::{catch_unwind, AssertUnwindSafe};

use rff::Engine;

/// A spread of malformed byte streams, labelled for failure reporting.
fn malformed_seeds() -> Vec<(&'static str, Vec<u8>)> {
    let mut seeds: Vec<(&'static str, Vec<u8>)> = vec![
        ("empty", Vec::new()),
        ("zeros", vec![0u8; 8192]),
        ("ones", vec![0xFFu8; 8192]),
        (
            "lcg-random",
            (0..8192u32)
                .map(|i| (i.wrapping_mul(2_654_435_761) >> 16) as u8)
                .collect(),
        ),
    ];

    // Real container magic, then garbage — gets past format sniffing/branching
    // into the actual parsing paths where the sharp edges live.
    let magics: &[(&str, &[u8])] = &[
        ("avi", b"RIFF\x00\x10\x00\x00AVI LIST"),
        ("wav", b"RIFF\x00\x10\x00\x00WAVEfmt "),
        ("mkv", b"\x1aE\xdf\xa3"),
        ("mp4", b"\x00\x00\x00\x18ftypmp42"),
        ("ogg", b"OggS\x00\x02\x00\x00\x00\x00\x00\x00"),
        ("ts", b"\x47\x40\x00\x10\x00\x00\xb0\x0d"),
        ("flv", b"FLV\x01\x05\x00\x00\x00\x09"),
        ("png", b"\x89PNG\r\n\x1a\n\x00\x00\x00\rIHDR"),
        ("jpeg", b"\xff\xd8\xff\xe0\x00\x10JFIF"),
        ("gif", b"GIF89a\x10\x00\x10\x00"),
        ("flac", b"fLaC\x00\x00\x00\x22"),
        ("webvtt", b"WEBVTT\n\n00:00:00.000 --> "),
        ("srt", b"1\n00:00:01,000 --> 00:00:"),
    ];
    for (tag, magic) in magics {
        let mut withgarbage = magic.to_vec();
        withgarbage.extend((0..4096u32).map(|i| (i.wrapping_mul(31)) as u8));
        seeds.push((tag, withgarbage));
        seeds.push((tag, magic.to_vec())); // truncated right after the magic
    }
    seeds
}

#[test]
fn demuxers_never_panic_on_malformed_input() {
    let engine = Engine::new();

    // Quiet the panic hook so a *caught* panic doesn't spam the log; restore it
    // afterward so a genuine test failure still prints.
    let prev_hook = std::panic::take_hook();
    std::panic::set_hook(Box::new(|_| {}));

    let mut panics: Vec<String> = Vec::new();
    for fmt in engine.formats.iter() {
        if !fmt.can_demux() {
            continue;
        }
        for (tag, seed) in malformed_seeds() {
            let name = fmt.name;
            let outcome = catch_unwind(AssertUnwindSafe(|| {
                let input = Box::new(Cursor::new(seed));
                let Ok(mut demuxer) = engine.formats.open_demuxer(name, input) else {
                    return;
                };
                let _ = demuxer.read_header();
                // Pull a bounded number of packets; stop at the first error/EOF.
                for _ in 0..64 {
                    if demuxer.read_packet().is_err() {
                        break;
                    }
                }
            }));
            if outcome.is_err() {
                panics.push(format!("{name} on `{tag}`"));
            }
        }
    }

    std::panic::set_hook(prev_hook);
    assert!(
        panics.is_empty(),
        "demuxers panicked on malformed input ({} cases): {panics:#?}",
        panics.len()
    );
}
