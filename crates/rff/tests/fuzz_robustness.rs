//! Aggressive, deterministic, cross-platform robustness fuzzer for the
//! untrusted-input paths — the always-on Windows-friendly complement to the
//! coverage-guided `fuzz/` cargo-fuzz targets (which need libFuzzer/Linux).
//!
//! For every demuxer AND decoder it drives thousands of *mutated* byte streams
//! (real container/codec magic + seeded bit-flips / inserts / deletes / injected
//! length fields / truncation) through the parse, inside `catch_unwind`. Parsing
//! hostile bytes may return `Err`, but must NEVER panic. A panic is a DoS on a
//! media tool, so this is a hard gate.
//!
//! Each execution's input is derived deterministically from `(target, iter)`, so
//! a failure reproduces exactly; a panic hook records the panicking `file:line`
//! and message for a directly-actionable report.

use std::cell::RefCell;
use std::io::Cursor;
use std::panic::{catch_unwind, AssertUnwindSafe};
use std::sync::Once;

use rff::Engine;
use rff_core::Packet;

// ---- deterministic RNG + structure-aware mutator ---------------------------

fn xs(state: &mut u64) -> u64 {
    let mut x = *state;
    x ^= x << 13;
    x ^= x >> 7;
    x ^= x << 17;
    *state = x;
    x
}

/// Build one mutated input from `(seed, starter corpus)`, fully determined by the
/// seed so any finding reproduces. Mutations bias toward the sharp edges: injected
/// big-endian length fields (OOB-read / over-allocation bait), truncation right
/// after magic, and byte flips inside the parsed structure.
fn gen_input(mut s: u64, starters: &[&[u8]]) -> Vec<u8> {
    s |= 1;
    let base = starters[(xs(&mut s) as usize) % starters.len()];
    let mut buf = base.to_vec();
    let nmut = 1 + xs(&mut s) % 24;
    for _ in 0..nmut {
        if buf.is_empty() {
            buf.push(xs(&mut s) as u8);
        }
        match xs(&mut s) % 6 {
            0 => {
                let i = xs(&mut s) as usize % buf.len();
                buf[i] ^= xs(&mut s) as u8;
            }
            1 => {
                let i = xs(&mut s) as usize % (buf.len() + 1);
                buf.insert(i.min(buf.len()), xs(&mut s) as u8);
            }
            2 if buf.len() > 1 => {
                let i = xs(&mut s) as usize % buf.len();
                buf.remove(i);
            }
            3 => {
                // Inject a length/size field. Mask to 24 bits: big enough to expose
                // an unbounded read/alloc as an OOB panic, small enough not to OOM
                // the test harness itself (true gigabyte allocs are a code-review
                // pass, not something to trigger live).
                let i = xs(&mut s) as usize % (buf.len() + 1);
                let v = (xs(&mut s) & 0x00FF_FFFF) as u32;
                for b in v.to_be_bytes() {
                    buf.insert(i.min(buf.len()), b);
                }
            }
            4 => {
                let n = xs(&mut s) as usize % (buf.len() + 1);
                buf.truncate(n);
            }
            _ => {
                let i = xs(&mut s) as usize % buf.len();
                buf[i] = xs(&mut s) as u8;
            }
        }
        if buf.len() > 1 << 16 {
            buf.truncate(1 << 16);
        }
    }
    buf
}

/// Container magics (+ a couple of structured leads) to seed demuxer fuzzing.
fn container_starters() -> Vec<&'static [u8]> {
    vec![
        b"RIFF\x00\x10\x00\x00AVI LIST",
        b"RIFF\x00\x10\x00\x00WAVEfmt ",
        b"\x1aE\xdf\xa3\x01\x00\x00\x00", // matroska/webm EBML
        b"\x00\x00\x00\x18ftypmp42",
        b"\x00\x00\x00\x1cftypisom",
        b"OggS\x00\x02\x00\x00\x00\x00\x00\x00",
        b"\x47\x40\x00\x10\x00\x00\xb0\x0d", // ts packet
        b"FLV\x01\x05\x00\x00\x00\x09",
        b"\x89PNG\r\n\x1a\n\x00\x00\x00\rIHDR",
        b"\xff\xd8\xff\xe0\x00\x10JFIF\x00\x01",
        b"GIF89a\x10\x00\x10\x00\x80\x00\x00",
        b"fLaC\x00\x00\x00\x22",
        b"RIFF\x00\x10\x00\x00WEBPVP8 ",
        b"WEBVTT\n\n00:00:00.000 --> 00:00:01.000\n",
        b"1\n00:00:01,000 --> 00:00:02,000\n",
        b"\x00\x00\x00\x00", // degenerate
    ]
}

/// Codec sync words / packet leads to seed decoder fuzzing.
fn codec_starters() -> Vec<&'static [u8]> {
    vec![
        b"\x00\x00\x00\x01\x67", // H.264/265 NAL (SPS-ish)
        b"\x00\x00\x01\x09",
        b"\xff\xfb\x90\x00", // MP3 sync (MPEG1 L3)
        b"\xff\xf3\x80\x00", // MP3 sync (MPEG2)
        b"\xff\xf1\x50\x80", // ADTS AAC
        b"\x82\x49\x83\x42", // VP9 keyframe-ish
        b"\x83\x00\x00\x00",
        b"OpusHead\x01\x02",
        b"\x1f\x8b\x08\x00",             // gzip-ish
        b"\x00\x00\x00\x00\x00\x00\x00", // zeros
        b"\xff\xff\xff\xff",
        b"\x30\x01\x02\x03",
    ]
}

fn install_panic_capture() {
    static ONCE: Once = Once::new();
    ONCE.call_once(|| {
        std::panic::set_hook(Box::new(|info| {
            let loc = info
                .location()
                .map(|l| format!("{}:{}", l.file(), l.line()))
                .unwrap_or_else(|| "<unknown>".into());
            let msg = if let Some(s) = info.payload().downcast_ref::<&str>() {
                (*s).to_string()
            } else if let Some(s) = info.payload().downcast_ref::<String>() {
                s.clone()
            } else {
                "<non-string panic>".into()
            };
            LAST_PANIC.with(|c| *c.borrow_mut() = Some(format!("{loc} :: {msg}")));
        }));
    });
}

thread_local! {
    static LAST_PANIC: RefCell<Option<String>> = const { RefCell::new(None) };
}

/// One collected finding: where it panicked and how to reproduce it.
struct Finding {
    site: String,   // panicking file:line :: message
    target: String, // demuxer/decoder name
    seed: u64,      // gen_input seed
    hex: String,    // the input bytes (first 64) for a quick eyeball
}

/// Iterations per target. Kept modest so the always-on CI gate stays fast; crank it
/// for a thorough local/nightly sweep: `RFF_FUZZ_ITERS=20000 cargo test -p rff …`.
fn iters() -> u64 {
    std::env::var("RFF_FUZZ_ITERS")
        .ok()
        .and_then(|v| v.parse().ok())
        .unwrap_or(400)
}

fn run_and_capture<F: FnOnce()>(f: F) -> Option<String> {
    LAST_PANIC.with(|c| *c.borrow_mut() = None);
    let r = catch_unwind(AssertUnwindSafe(f));
    if r.is_err() {
        Some(
            LAST_PANIC
                .with(|c| c.borrow_mut().take())
                .unwrap_or_else(|| "<no location>".into()),
        )
    } else {
        None
    }
}

fn report(findings: Vec<Finding>) {
    if findings.is_empty() {
        return;
    }
    // Dedup by panic site; keep the first reproduction of each.
    let mut seen = std::collections::BTreeMap::<String, &Finding>::new();
    for f in &findings {
        seen.entry(f.site.clone()).or_insert(f);
    }
    let mut msg = format!(
        "\n{} distinct panic site(s) across the untrusted-input fuzz sweep:\n",
        seen.len()
    );
    for (site, f) in &seen {
        msg += &format!(
            "  • {site}\n      target={} seed={} input={}…\n",
            f.target, f.seed, f.hex
        );
    }
    panic!("{msg}");
}

#[test]
fn demuxers_never_panic_fuzz() {
    install_panic_capture();
    let engine = Engine::new();
    let starters = container_starters();
    let mut names: Vec<&'static str> = engine
        .formats
        .iter()
        .filter(|f| f.can_demux())
        .map(|f| f.name)
        .collect();
    names.sort_unstable();

    let mut findings = Vec::new();
    let mut execs = 0u64;
    let ntargets = names.len();
    for name in names {
        let mut base = 0x9E37_79B9_7F4A_7C15u64 ^ hash(name);
        for it in 0..iters() {
            execs += 1;
            let seed = base.wrapping_add(it).wrapping_mul(0x2545_F491_4F6C_DD1D);
            let input = gen_input(seed, &starters);
            let inp = input.clone();
            if let Some(site) = run_and_capture(|| {
                if let Ok(mut d) = engine
                    .formats
                    .open_demuxer(name, Box::new(Cursor::new(inp)))
                {
                    let _ = d.read_header();
                    for _ in 0..64 {
                        if d.read_packet().is_err() {
                            break;
                        }
                    }
                }
            }) {
                findings.push(Finding {
                    site,
                    target: name.into(),
                    seed,
                    hex: hexlead(&input),
                });
            }
        }
        base = base.wrapping_add(1);
        let _ = base;
    }
    eprintln!(
        "demux fuzz: {ntargets} demuxers × {} iters = {execs} execs",
        iters()
    );
    report(findings);
}

#[test]
fn decoders_never_panic_fuzz() {
    install_panic_capture();
    let engine = Engine::new();
    let starters = codec_starters();
    let mut names: Vec<&'static str> = engine
        .codecs
        .iter()
        .filter(|c| c.can_decode())
        .map(|c| c.name)
        .collect();
    names.sort_unstable();

    // `avif` decodes via the external `rav1d` crate, whose `validate_input!` calls
    // `debug_abort()` on a failed input check — which `abort()`s ONLY under
    // `cfg!(debug_assertions)` (i.e. debug builds). `abort()` isn't a panic, so
    // `catch_unwind` can't contain it and it would kill the sweep. In RELEASE the same
    // path returns `Err` gracefully, so we include `avif` there to prove it. (This is
    // why a shipped release binary does NOT crash on hostile AVIF — see SECURITY.)
    let external_aborts: &[&str] = if cfg!(debug_assertions) {
        &["avif"]
    } else {
        &[]
    };

    let mut findings = Vec::new();
    for name in names {
        if external_aborts.contains(&name) {
            continue;
        }
        let Some(codec) = engine.codecs.by_name(name) else {
            continue;
        };
        for it in 0..iters() {
            let seed = (0xD1B5_4A32_D192_ED03u64 ^ hash(name))
                .wrapping_add(it)
                .wrapping_mul(0x2545_F491_4F6C_DD1D);
            let input = gen_input(seed, &starters);
            let inp = input.clone();
            let id = codec.id;
            if let Some(site) = run_and_capture(|| {
                if let Ok(mut dec) = engine.codecs.find_decoder(id) {
                    let _ = dec.send_packet(&Packet::from_data(0, inp));
                    for _ in 0..64 {
                        if dec.receive_frame().is_err() {
                            break;
                        }
                    }
                }
            }) {
                findings.push(Finding {
                    site,
                    target: name.into(),
                    seed,
                    hex: hexlead(&input),
                });
            }
        }
    }
    report(findings);
}

fn hash(s: &str) -> u64 {
    let mut h = 0xcbf2_9ce4_8422_2325u64;
    for b in s.bytes() {
        h ^= b as u64;
        h = h.wrapping_mul(0x0000_0100_0000_01B3);
    }
    h
}

fn hexlead(b: &[u8]) -> String {
    b.iter().take(64).map(|x| format!("{x:02x}")).collect()
}
