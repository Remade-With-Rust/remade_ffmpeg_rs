//! `allocaudit` — a deterministic allocation counter for the MP3 encode/decode
//! hot paths.
//!
//! ```text
//!   cargo run -p rusty_mp3 --release --example allocaudit
//!   cargo run -p rusty_mp3 --release --example allocaudit -- 500 320
//! ```
//!
//! Why a counter and not a timer: an allocation is a few hundred nanoseconds
//! under `rusty_alloc`, so a per-granule alloc is far below what a wall clock can
//! resolve on a busy box — but the COUNT is exact, reproducible, and immune to
//! scheduler drift. It both proves the structural claim and sizes it, which is
//! what decides whether a hoist is worth doing (codec-measurement: the counter is
//! the primary instrument, the clock is confirmation).
//!
//! Allocator convention: this does **not** replace the project allocator. It is a
//! counting shim that delegates every call to
//! [`rusty_alloc_api::RustyAlloc`], so the numbers below are measured under the
//! allocator that actually ships. `alloc_zeroed` is counted separately on
//! purpose: it isolates the `vec![0f32; n]` pattern, where the zero-fill is paid
//! and then immediately overwritten.

use std::alloc::{GlobalAlloc, Layout};
use std::sync::atomic::{AtomicUsize, Ordering::Relaxed};

use rusty_mp3::{Mp3Decoder, Mp3Encoder, Mp3EncoderConfig};

static N_ALLOC: AtomicUsize = AtomicUsize::new(0);
static N_ZEROED: AtomicUsize = AtomicUsize::new(0);
static N_REALLOC: AtomicUsize = AtomicUsize::new(0);
static N_FREE: AtomicUsize = AtomicUsize::new(0);
static BYTES: AtomicUsize = AtomicUsize::new(0);

/// Counting shim over the project allocator. Delegates everything; only the
/// tallies are ours.
struct Counting;

unsafe impl GlobalAlloc for Counting {
    unsafe fn alloc(&self, l: Layout) -> *mut u8 {
        N_ALLOC.fetch_add(1, Relaxed);
        BYTES.fetch_add(l.size(), Relaxed);
        unsafe { rusty_alloc_api::RustyAlloc.alloc(l) }
    }
    unsafe fn alloc_zeroed(&self, l: Layout) -> *mut u8 {
        N_ZEROED.fetch_add(1, Relaxed);
        BYTES.fetch_add(l.size(), Relaxed);
        unsafe { rusty_alloc_api::RustyAlloc.alloc_zeroed(l) }
    }
    unsafe fn realloc(&self, p: *mut u8, l: Layout, new: usize) -> *mut u8 {
        N_REALLOC.fetch_add(1, Relaxed);
        BYTES.fetch_add(new.saturating_sub(l.size()), Relaxed);
        unsafe { rusty_alloc_api::RustyAlloc.realloc(p, l, new) }
    }
    unsafe fn dealloc(&self, p: *mut u8, l: Layout) {
        N_FREE.fetch_add(1, Relaxed);
        unsafe { rusty_alloc_api::RustyAlloc.dealloc(p, l) }
    }
}

#[global_allocator]
static GLOBAL_ALLOC: Counting = Counting;

#[derive(Clone, Copy)]
struct Snap {
    alloc: usize,
    zeroed: usize,
    realloc: usize,
    free: usize,
    bytes: usize,
}

fn snap() -> Snap {
    Snap {
        alloc: N_ALLOC.load(Relaxed),
        zeroed: N_ZEROED.load(Relaxed),
        realloc: N_REALLOC.load(Relaxed),
        free: N_FREE.load(Relaxed),
        bytes: BYTES.load(Relaxed),
    }
}

impl Snap {
    fn since(self, base: Snap) -> Snap {
        Snap {
            alloc: self.alloc - base.alloc,
            zeroed: self.zeroed - base.zeroed,
            realloc: self.realloc - base.realloc,
            free: self.free - base.free,
            bytes: self.bytes - base.bytes,
        }
    }
    fn total(self) -> usize {
        self.alloc + self.zeroed + self.realloc
    }
}

const SR: u32 = 44_100;
const CH: u16 = 2;
const SPF: usize = 1152; // MPEG-1 Layer III samples per frame per channel

/// Deterministic stereo music-ish PCM: a few partials plus a decorrelating LCG
/// wobble, so the psymodel and the stereo decision both see realistic input and
/// the run is byte-reproducible.
fn make_pcm(frames: usize) -> Vec<f32> {
    let n = frames * SPF;
    let mut out = Vec::with_capacity(n * CH as usize);
    let mut lcg: u32 = 0x1234_5678;
    for i in 0..n {
        lcg = lcg.wrapping_mul(1_664_525).wrapping_add(1_013_904_223);
        let noise = (lcg >> 9) as f32 / (1 << 23) as f32 - 0.5;
        let t = i as f32 / SR as f32;
        let base = 0.34 * (2.0 * std::f32::consts::PI * 220.0 * t).sin()
            + 0.22 * (2.0 * std::f32::consts::PI * 440.0 * t).sin()
            + 0.11 * (2.0 * std::f32::consts::PI * 1760.0 * t).sin();
        out.push((base + 0.04 * noise).clamp(-1.0, 1.0));
        out.push((base * 0.92 + 0.05 * noise).clamp(-1.0, 1.0));
    }
    out
}

/// FNV-1a over the emitted bitstream — the byte-identity gate. Any change that
/// claims to be output-preserving must leave this hash untouched.
fn fnv1a(bytes: &[u8]) -> u64 {
    let mut h: u64 = 0xcbf2_9ce4_8422_2325;
    for &b in bytes {
        h ^= b as u64;
        h = h.wrapping_mul(0x0000_0100_0000_01b3);
    }
    h
}

fn main() {
    let args: Vec<String> = std::env::args().skip(1).collect();
    let frames: usize = args.first().and_then(|s| s.parse().ok()).unwrap_or(200);
    let kbps: u32 = args.get(1).and_then(|s| s.parse().ok()).unwrap_or(192);

    let pcm = make_pcm(frames);

    // ---- ENCODE, streaming one frame at a time (how the CLI drives it) ----
    let mut enc = Mp3Encoder::new(Mp3EncoderConfig {
        bitrate_kbps: kbps,
        vbr_quality: None,
    });
    let mut mp3: Vec<u8> = Vec::with_capacity(frames * 1024);

    let t0 = snap();
    for f in 0..frames {
        let s = f * SPF * CH as usize;
        let e = s + SPF * CH as usize;
        enc.push_pcm_f32(&pcm[s..e], CH, SR).unwrap();
        while let Ok(p) = enc.next_packet() {
            mp3.extend_from_slice(&p);
        }
    }
    enc.finish();
    while let Ok(p) = enc.next_packet() {
        mp3.extend_from_slice(&p);
    }
    let enc_stats = snap().since(t0);

    // ---- DECODE the stream we just produced ----
    let mut dec = Mp3Decoder::new();
    let mut pcm_out = 0usize;
    let t1 = snap();
    dec.push(&mp3);
    dec.flush();
    while let Ok(frame) = dec.next_frame() {
        // `samples` is interleaved, so divide back out to per-channel.
        pcm_out += frame.samples.len() / frame.channels.max(1) as usize;
    }
    let dec_stats = snap().since(t1);

    // Snapshot everything BEFORE printing: println! allocates.
    let (ef, df) = (frames.max(1), frames.max(1));
    let mp3_len = mp3.len();
    let mp3_hash = fnv1a(&mp3);

    println!("rusty_mp3 allocation audit — under rusty_alloc (counting shim delegates to it)");
    println!(
        "  workload: {frames} frames, {CH} ch @ {SR} Hz, CBR {kbps}k  ->  {mp3_len} bytes, \
         {pcm_out} decoded samples/ch"
    );
    println!("  bitstream fnv1a: {mp3_hash:#018x}   <- byte-identity gate\n");
    println!(
        "  {:<8} {:>9} {:>9} {:>9} {:>9} {:>11} {:>12}",
        "phase", "alloc", "zeroed", "realloc", "free", "total", "per-frame"
    );
    for (name, s, per) in [("encode", enc_stats, ef), ("decode", dec_stats, df)] {
        println!(
            "  {:<8} {:>9} {:>9} {:>9} {:>9} {:>11} {:>12.2}",
            name,
            s.alloc,
            s.zeroed,
            s.realloc,
            s.free,
            s.total(),
            s.total() as f64 / per as f64
        );
    }
    println!(
        "\n  bytes requested: encode {} KiB, decode {} KiB",
        enc_stats.bytes / 1024,
        dec_stats.bytes / 1024
    );
    println!(
        "  zeroed share of encode allocations: {:.1}%  \
         (the vec![0f32; n] pattern — zero-fill paid, then overwritten)",
        100.0 * enc_stats.zeroed as f64 / enc_stats.total().max(1) as f64
    );
}
