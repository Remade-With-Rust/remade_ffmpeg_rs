//! Peak-heap gate: the encoder's high-water mark per stream, measured with a
//! counting global allocator — the host twin of `esp-alloc`'s
//! `internal-heap-stats` on the chip (docs/plans/esp32-encoder-cost.md §3).
//!
//! An allocation-COUNT gate cannot see a peak regression: reusing a buffer
//! across the stream lowers the count while it raises the peak, because the
//! reused buffer now coexists with the largest transient set. So the peak is
//! gated here, in bytes, for the shape the ESP32 firmwares use: 16 kHz mono
//! s16 pushed as bytes, one stream per chunk.
//!
//! Run the chip's configuration with
//! `cargo test --release --no-default-features --features libm --test peak_heap -- --nocapture`;
//! `RUSTY_FLAC_PEAK_DUMP=1` also prints the allocations live at the peak.

use std::alloc::{GlobalAlloc, Layout, System};
use std::sync::atomic::{AtomicBool, AtomicUsize, Ordering};

struct Counting;

static CUR: AtomicUsize = AtomicUsize::new(0);
static PEAK: AtomicUsize = AtomicUsize::new(0);
static LOCK: AtomicBool = AtomicBool::new(false);

/// Live-allocation table (ptr, size) and its snapshot at the peak. Only
/// consulted when tracing is on; sized well above the encoder's live set.
const SLOTS: usize = 4096;
static mut LIVE: [(usize, usize); SLOTS] = [(0, 0); SLOTS];
static mut AT_PEAK: [(usize, usize); SLOTS] = [(0, 0); SLOTS];
static TRACE: AtomicBool = AtomicBool::new(false);

fn lock() {
    while LOCK
        .compare_exchange_weak(false, true, Ordering::Acquire, Ordering::Relaxed)
        .is_err()
    {
        std::hint::spin_loop();
    }
}
fn unlock() {
    LOCK.store(false, Ordering::Release);
}

// SAFETY: forwards to System; the bookkeeping is under a spinlock and never
// allocates.
unsafe impl GlobalAlloc for Counting {
    unsafe fn alloc(&self, l: Layout) -> *mut u8 {
        let p = System.alloc(l);
        if !p.is_null() {
            on_alloc(p as usize, l.size());
        }
        p
    }
    unsafe fn dealloc(&self, p: *mut u8, l: Layout) {
        on_free(p as usize, l.size());
        System.dealloc(p, l)
    }
    unsafe fn realloc(&self, p: *mut u8, l: Layout, new: usize) -> *mut u8 {
        let q = System.realloc(p, l, new);
        if !q.is_null() {
            // Counted as alloc-new then free-old: that is the moment both
            // exist on a heap that cannot grow in place (the conservative
            // reading, and what a first-fit embedded heap does).
            on_alloc(q as usize, new);
            on_free(p as usize, l.size());
        }
        q
    }
}

#[allow(static_mut_refs)]
fn on_alloc(p: usize, size: usize) {
    lock();
    let cur = CUR.load(Ordering::Relaxed) + size;
    CUR.store(cur, Ordering::Relaxed);
    let tracing = TRACE.load(Ordering::Relaxed);
    // SAFETY: under LOCK.
    unsafe {
        if tracing {
            if let Some(s) = LIVE.iter_mut().find(|s| s.1 == 0) {
                *s = (p, size);
            }
        }
        if cur > PEAK.load(Ordering::Relaxed) {
            PEAK.store(cur, Ordering::Relaxed);
            if tracing {
                AT_PEAK = LIVE;
            }
        }
    }
    unlock();
}

#[allow(static_mut_refs)]
fn on_free(p: usize, size: usize) {
    lock();
    CUR.store(CUR.load(Ordering::Relaxed) - size, Ordering::Relaxed);
    if TRACE.load(Ordering::Relaxed) {
        // SAFETY: under LOCK.
        unsafe {
            if let Some(s) = LIVE.iter_mut().find(|s| s.0 == p && s.1 != 0) {
                *s = (0, 0);
            }
        }
    }
    unlock();
}

#[global_allocator]
static A: Counting = Counting;

/// Deterministic 16 kHz mono s16 content, three classes:
/// - `quiet`: the plan's quiet-room microphone (coloured noise near −40 dBFS
///   plus a 50 Hz hum);
/// - `loud`: three tones and coloured noise near −6 dBFS (music-like);
/// - `noise`: full-scale white noise, the worst case for residual size and
///   so for the Rice-sum rows.
///
/// The chip bench encodes the same formulas, so host and chip rows compare.
fn content(class: &str, n: usize, seed: u64) -> Vec<u8> {
    let mut s = seed;
    let mut lp = 0.0f64;
    let mut out = Vec::with_capacity(n * 2);
    let tone = |i: usize, hz: f64| (i as f64 * 2.0 * std::f64::consts::PI * hz / 16000.0).sin();
    for i in 0..n {
        s = s
            .wrapping_mul(6364136223846793005)
            .wrapping_add(1442695040888963407);
        let white = ((s >> 33) as f64 / (1u64 << 31) as f64) - 0.5;
        lp = 0.9 * lp + 0.1 * white;
        let v = match class {
            "quiet" => lp * 2600.0 + tone(i, 50.0) * 120.0,
            "loud" => {
                tone(i, 220.0) * 6000.0
                    + tone(i, 331.0) * 4000.0
                    + tone(i, 1250.0) * 1500.0
                    + lp * 9000.0
            }
            _ => white * 65535.0,
        };
        out.extend_from_slice(&(v.clamp(-32768.0, 32767.0) as i16).to_le_bytes());
    }
    out
}

/// One stream through the firmware's call shape; returns (peak above the
/// pre-encode level, stream length).
fn peak_of(pcm: &[u8], level: u32) -> (usize, usize) {
    let base = CUR.load(Ordering::Relaxed);
    PEAK.store(base, Ordering::Relaxed);
    let mut enc = rusty_flac::Encoder::new(16000, 1, 16).unwrap();
    enc.set_compression_level(level);
    enc.push_s16le_bytes(pcm).unwrap();
    let out = enc.finish();
    let peak = PEAK.load(Ordering::Relaxed) - base;
    (peak, out.len())
}

#[allow(static_mut_refs)]
fn dump(pcm: &[u8], level: u32) {
    // SAFETY: single-threaded test; the table is cleared before use.
    unsafe {
        lock();
        LIVE = [(0, 0); SLOTS];
        AT_PEAK = [(0, 0); SLOTS];
        unlock();
    }
    TRACE.store(true, Ordering::Relaxed);
    let (peak, _) = peak_of(pcm, level);
    TRACE.store(false, Ordering::Relaxed);
    let mut sizes: Vec<usize> = unsafe { AT_PEAK.iter().map(|s| s.1).filter(|&s| s > 0).collect() };
    sizes.sort_unstable_by(|a, b| b.cmp(a));
    println!(
        "  live at peak ({} samples, level {level}, peak {peak}): {:?}",
        pcm.len() / 2,
        sizes
    );
}

/// Ceilings, bytes, for the chip's shape: the measured peak rounded up to
/// the next KiB, so any regression of a block-sized buffer fails. For
/// reference, 0.1.3 peaked at 84,848 / 188,796 / 209,276 B (quiet 512 /
/// 4096 / 8192, level 0) and the perf(encode) series before these ceilings
/// at 92,100 / 231,876 / 252,356 B -- too much for an 8,192-sample stream on
/// a 256 KB ESP32-S3 heap.
/// `8192` is the first 4096 samples twice (the plan's A+A).
const CEILINGS: &[(&str, usize, u32, usize)] = &[
    // (content, samples, level, max peak bytes)
    ("quiet", 512, 0, 37_888),
    ("quiet", 4096, 0, 103_424),
    ("quiet", 8192, 0, 123_904),
    ("quiet", 8192, 5, 123_904),
    ("quiet", 8192, 8, 123_904),
    ("quiet", 8000, 5, 125_952),
    ("loud", 8192, 0, 123_904),
    ("loud", 8192, 8, 123_904),
    ("noise", 8192, 0, 140_288),
    ("noise", 8192, 8, 140_288),
];

#[test]
fn encoder_peak_heap_per_stream() {
    let mut failed = Vec::new();
    println!("content samples level peak_bytes flac_bytes");
    for &(class, n, level, ceiling) in CEILINGS {
        let seed = match class {
            "quiet" => 7,
            "loud" => 9,
            _ => 11,
        };
        let pcm = if n == 8192 {
            let a = content(class, 4096, seed);
            [a.clone(), a].concat()
        } else {
            content(class, n, seed)
        };
        let (peak, len) = peak_of(&pcm, level);
        println!("{class:>7} {n:>7} {level:>5} {peak:>10} {len:>10}");
        if peak > ceiling {
            failed.push((class, n, level, peak, ceiling));
        }
        if std::env::var_os("RUSTY_FLAC_PEAK_DUMP").is_some() {
            dump(&pcm, level);
        }
    }
    assert!(failed.is_empty(), "peak-heap ceilings exceeded: {failed:?}");
}
