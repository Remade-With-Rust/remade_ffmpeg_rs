//! Robustness fuzz harness for the decode surface — every registered decoder,
//! fed malformed/random packet bytes, must not panic (a decoder parses an
//! attacker's bitstream; a panic is a DoS). This also drives the decode-path
//! `unsafe` SIMD kernels with hostile lengths. Run in debug so overflow panics:
//!
//!   cargo test -p rff --test fuzz_decode -- --nocapture

use std::panic;

use rff::Engine;
use rff_core::{CodecId, Packet};
use rff_codec::CodecParams;

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

fn params(id: CodecId) -> CodecParams {
    // Generic-but-plausible config so `configure` doesn't reject before parsing.
    CodecParams {
        codec_id: id,
        width: 320,
        height: 240,
        pixel_format: None,
        sample_rate: 48_000,
        channels: 2,
        sample_format: None,
        extradata: Vec::new(),
    }
}

fn drive(engine: &Engine, id: CodecId, data: &[u8]) {
    let Some(codec) = engine.codecs.by_id(id) else {
        return;
    };
    let Some(factory) = codec.decoder else {
        return;
    };
    let mut dec = factory();
    if dec.configure(&params(id)).is_err() {
        return;
    }
    let pkt = Packet::from_data(0, data.to_vec());
    if dec.send_packet(&pkt).is_err() {
        // still try to drain — some decoders buffer regardless.
    }
    for _ in 0..2_000 {
        match dec.receive_frame() {
            Ok(_) => {}
            Err(_) => break,
        }
    }
    dec.flush();
    for _ in 0..2_000 {
        if dec.receive_frame().is_err() {
            break;
        }
    }
}

#[test]
fn fuzz_decoders_no_panic() {
    let engine = Engine::new();
    let ids: Vec<CodecId> = engine
        .codecs
        .iter()
        .filter(|c| c.decoder.is_some())
        .map(|c| c.id)
        .collect();
    assert!(!ids.is_empty(), "no decoders registered");
    println!("fuzzing {} decoders", ids.len());
    let _ = std::io::Write::flush(&mut std::io::stdout());

    // Watchdog: name + abort on a decoder that infinite-loops within one call.
    use std::sync::atomic::{AtomicU64, Ordering};
    static HEARTBEAT: AtomicU64 = AtomicU64::new(0);
    let cur = std::sync::Arc::new(std::sync::Mutex::new((CodecId::Opus, 0u64)));
    let cur_wd = cur.clone();
    std::thread::spawn(move || {
        let (mut last, mut stalls) = (0u64, 0);
        loop {
            std::thread::sleep(std::time::Duration::from_millis(500));
            let hb = HEARTBEAT.load(Ordering::Relaxed);
            if hb == last {
                stalls += 1;
                if stalls >= 8 {
                    let (id, cs) = *cur_wd.lock().unwrap();
                    eprintln!("\n!!! HANG: decoder {id:?} looped >4s on case_seed=0x{cs:016x}");
                    std::process::abort();
                }
            } else {
                stalls = 0;
                last = hb;
            }
        }
    });

    let msg = std::sync::Arc::new(std::sync::Mutex::new(String::new()));
    let msg_hook = msg.clone();
    panic::set_hook(Box::new(move |info| {
        *msg_hook.lock().unwrap() = info.to_string();
    }));

    let seed = 0xFACE_B00C_1234_5678u64;
    let iters = 30_000usize;
    let mut rng = Rng(seed);
    let mut failures: Vec<(CodecId, u64, String)> = Vec::new();

    for _ in 0..iters {
        let case_seed = rng.0;
        // Random bytes of varied length (occasionally longer, to hit size fields).
        let len = if rng.range(100) < 10 { rng.range(65536) } else { rng.range(4096) } + 1;
        let mut data = Vec::with_capacity(len);
        for _ in 0..len {
            data.push(rng.byte());
        }
        for &id in &ids {
            *cur.lock().unwrap() = (id, case_seed);
            HEARTBEAT.fetch_add(1, Ordering::Relaxed);
            let r = panic::catch_unwind(panic::AssertUnwindSafe(|| drive(&engine, id, &data)));
            if r.is_err() {
                failures.push((id, case_seed, msg.lock().unwrap().clone()));
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
        eprintln!("\n=== {} decoder panic(s) (seed 0x{seed:016x}, {iters} iters) ===", failures.len());
        let mut seen = std::collections::BTreeSet::new();
        for (id, cs, where_) in &failures {
            let key = format!("{id:?} :: {}", where_.lines().next().unwrap_or(""));
            if seen.insert(key.clone()) {
                eprintln!("  [{id:?}] case_seed=0x{cs:016x}  {}", where_.lines().next().unwrap_or(""));
            }
        }
        panic!("{} decoder(s) panicked on malformed input", seen.len());
    }
    println!("OK — no decoder panicked over {iters} malformed inputs");
}
