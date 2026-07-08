//! Robustness fuzz harness for the demux (parse-untrusted-input) surface.
//!
//! Feeds malformed / random / magic-prefixed byte buffers to EVERY registered
//! demuxer's `read_header` + `read_packet` under `catch_unwind`, and reports any
//! that panic. Demuxers parse attacker-controlled files, so a panic is a
//! denial-of-service (safe Rust means it can't be worse than a crash — this pass
//! hunts the crashes). Run in debug (default) so integer-overflow also panics:
//!
//!   cargo test -p rff --test fuzz_demux -- --nocapture
//!
//! Deterministic (seeded), so a failure is reproducible from the printed seed.

use std::io::Cursor;
use std::panic;

use rff::Engine;
use rff_format::Input;

/// xorshift64 — deterministic, no external RNG.
struct Rng(u64);
impl Rng {
    fn next(&mut self) -> u64 {
        self.0 ^= self.0 << 13;
        self.0 ^= self.0 >> 7;
        self.0 ^= self.0 << 17;
        self.0
    }
    fn byte(&mut self) -> u8 {
        (self.next() >> 33) as u8
    }
    fn range(&mut self, n: usize) -> usize {
        (self.next() % n as u64) as usize
    }
}

/// Container magics — prepending one gets the fuzzer past `probe` and deep into a
/// specific format's parser, where the interesting bugs live.
const MAGICS: &[&[u8]] = &[
    b"RIFF____WAVE",           // wav
    b"RIFF____AVI ",           // avi
    &[0x1A, 0x45, 0xDF, 0xA3], // matroska/webm (EBML)
    b"OggS",                   // ogg
    b"____ftypisom",           // mp4/mov
    b"____ftypmp42",
    b"____ftypavif", // avif
    b"fLaC",         // flac
    &[0xFF, 0xD8, 0xFF],       // jpeg
    &[0x89, b'P', b'N', b'G', 0x0D, 0x0A, 0x1A, 0x0A], // png
    b"GIF89a",                 // gif
    b"FLV\x01",                // flv
    &[0x47],                   // mpeg-ts sync
    b"RIFF____WEBP",           // webp
    &[0xFF, 0x0A],             // jpeg-xl
    b"WEBVTT",                 // webvtt
    b"1\n00:00:00,000",        // srt
];

/// Build one malformed input: random bytes, optionally prefixed with a magic and
/// optionally derived by mutating a prior buffer.
fn gen(rng: &mut Rng) -> Vec<u8> {
    let len = rng.range(4096) + 1;
    let mut v = Vec::with_capacity(len);
    for _ in 0..len {
        v.push(rng.byte());
    }
    // 60%: prepend a container magic so we reach a real parser.
    if rng.range(100) < 60 {
        let m = MAGICS[rng.range(MAGICS.len())];
        let mut out = m.to_vec();
        out.extend_from_slice(&v);
        // Sprinkle plausible big-endian "sizes/counts" so length fields point far.
        for _ in 0..rng.range(8) {
            let pos = rng.range(out.len());
            let word = [0x7F, 0xFF, 0xFF, 0xFF];
            for (i, &b) in word.iter().enumerate() {
                if pos + i < out.len() {
                    out[pos + i] = b;
                }
            }
        }
        out
    } else {
        v
    }
}

/// Drive one demuxer over `data`; bounded so a demuxer that never returns Eof
/// can't loop forever within the harness.
fn drive(engine: &Engine, name: &str, data: &[u8]) {
    let input: Input = Box::new(Cursor::new(data.to_vec()));
    let Ok(mut dmx) = engine.formats.open_demuxer(name, input) else {
        return;
    };
    if dmx.read_header().is_err() {
        return;
    }
    for _ in 0..10_000 {
        match dmx.read_packet() {
            Ok(_) => {}
            Err(_) => break,
        }
    }
}

#[test]
fn fuzz_demuxers_no_panic() {
    use std::io::Write;
    use std::sync::atomic::{AtomicU64, Ordering};
    let engine = Engine::new();
    let names: Vec<&'static str> = engine
        .formats
        .iter()
        .filter(|f| f.demuxer.is_some())
        .map(|f| f.name)
        .collect();
    assert!(!names.is_empty(), "no demuxers registered");
    println!("fuzzing {} demuxers: {:?}", names.len(), names);
    let _ = std::io::stdout().flush();

    // Watchdog: a demuxer that infinite-loops inside a single read_header/
    // read_packet call (not bounded by the iteration cap) would hang the whole
    // run silently. Heartbeat before each drive; if it stalls, the watchdog
    // prints the exact (format, case_seed) and aborts so the hang is diagnosable.
    static HEARTBEAT: AtomicU64 = AtomicU64::new(0);
    let cur: std::sync::Arc<std::sync::Mutex<(&'static str, u64)>> =
        std::sync::Arc::new(std::sync::Mutex::new(("", 0)));
    let cur_wd = cur.clone();
    std::thread::spawn(move || {
        let mut last = 0u64;
        let mut stalls = 0;
        loop {
            std::thread::sleep(std::time::Duration::from_millis(500));
            let hb = HEARTBEAT.load(Ordering::Relaxed);
            if hb == last {
                stalls += 1;
                if stalls >= 8 {
                    let (fmt, cs) = *cur_wd.lock().unwrap();
                    eprintln!("\n!!! HANG: demuxer '{fmt}' looped >4s on case_seed=0x{cs:016x}");
                    std::process::abort();
                }
            } else {
                stalls = 0;
                last = hb;
            }
        }
    });

    // Silence the default panic hook so catch_unwind doesn't flood stderr; we
    // record the message ourselves.
    let msg = std::sync::Arc::new(std::sync::Mutex::new(String::new()));
    let msg_hook = msg.clone();
    panic::set_hook(Box::new(move |info| {
        *msg_hook.lock().unwrap() = info.to_string();
    }));

    let seed = 0x0D15_EA5E_C0FF_EE00u64;
    let iters = 40_000usize;
    let mut rng = Rng(seed);
    let mut failures: Vec<(String, u64, String)> = Vec::new();

    for _ in 0..iters {
        let case_seed = rng.0;
        let data = gen(&mut rng);
        for &name in &names {
            *cur.lock().unwrap() = (name, case_seed);
            HEARTBEAT.fetch_add(1, Ordering::Relaxed);
            let r = panic::catch_unwind(panic::AssertUnwindSafe(|| drive(&engine, name, &data)));
            if r.is_err() {
                let where_ = msg.lock().unwrap().clone();
                failures.push((name.to_string(), case_seed, where_));
                if failures.len() > 40 {
                    break;
                }
            }
        }
        if failures.len() > 40 {
            break;
        }
    }

    let _ = panic::take_hook();

    if !failures.is_empty() {
        eprintln!("\n=== {} demuxer panic(s) (seed 0x{seed:016x}, {iters} iters) ===", failures.len());
        // De-dup by (format, panic site) so repeats collapse.
        let mut seen = std::collections::BTreeSet::new();
        for (fmt, cs, where_) in &failures {
            let key = format!("{fmt} :: {}", where_.lines().next().unwrap_or(""));
            if seen.insert(key.clone()) {
                eprintln!("  [{fmt}] case_seed=0x{cs:016x}  {}", where_.lines().next().unwrap_or(""));
            }
        }
        panic!("{} demuxer(s) panicked on malformed input (see list above)", seen.len());
    }
    println!("OK — no demuxer panicked over {iters} malformed inputs");
}
