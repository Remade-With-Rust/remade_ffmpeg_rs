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
/// Bytes a `realloc` had to carry over (the OLD size) -- an upper bound on what
/// a growing `Vec` copies, since an in-place grow moves nothing.
static MOVED: AtomicUsize = AtomicUsize::new(0);
/// Live heap bytes, and their high-water mark (reset per phase).
static LIVE: AtomicUsize = AtomicUsize::new(0);
static PEAK: AtomicUsize = AtomicUsize::new(0);

fn grow_live(by: usize) {
    let now = LIVE.fetch_add(by, Relaxed) + by;
    PEAK.fetch_max(now, Relaxed);
}

/// `ALLOCAUDIT_SIZES=1`: a histogram of request sizes, so each per-frame
/// allocation can be attributed to its source by its size. Fixed slots of
/// atomics with linear probing -- an allocator cannot allocate.
const SLOTS: usize = 512;
static SIZE_KEY: [AtomicUsize; SLOTS] = [const { AtomicUsize::new(0) }; SLOTS];
static SIZE_N: [AtomicUsize; SLOTS] = [const { AtomicUsize::new(0) }; SLOTS];
static SIZES_ON: std::sync::atomic::AtomicBool = std::sync::atomic::AtomicBool::new(false);

/// `ALLOCAUDIT_TRACE=<bytes>`: print ONE backtrace for the first streamed-encode
/// allocation of exactly that size -- the histogram says how many, this says
/// who. Capturing allocates, so a latch makes the capture's own allocations
/// (and every later match) skip. Build in the dev profile for symbols.
static TRACE_SIZE: AtomicUsize = AtomicUsize::new(usize::MAX);
static TRACE_LATCH: std::sync::atomic::AtomicBool = std::sync::atomic::AtomicBool::new(false);

fn tally_size(size: usize) {
    if !SIZES_ON.load(Relaxed) {
        return;
    }
    if size == TRACE_SIZE.load(Relaxed) && !TRACE_LATCH.swap(true, Relaxed) {
        eprintln!(
            "allocation of {size} B:\n{}",
            std::backtrace::Backtrace::force_capture()
        );
    }
    let key = size + 1; // 0 marks an empty slot
    let mut i = (size.wrapping_mul(0x9E37_79B9)) % SLOTS;
    for _ in 0..SLOTS {
        match SIZE_KEY[i].compare_exchange(0, key, Relaxed, Relaxed) {
            Ok(_) => {
                SIZE_N[i].fetch_add(1, Relaxed);
                return;
            }
            Err(k) if k == key => {
                SIZE_N[i].fetch_add(1, Relaxed);
                return;
            }
            Err(_) => i = (i + 1) % SLOTS,
        }
    }
}

/// Counting shim over the project allocator. Delegates everything; only the
/// tallies are ours.
struct Counting;

// SAFETY: every method forwards the caller's arguments unchanged to the project
// allocator, which upholds the `GlobalAlloc` contract; the counters are relaxed
// atomics that never allocate, so the shim adds no obligation of its own.
unsafe impl GlobalAlloc for Counting {
    unsafe fn alloc(&self, l: Layout) -> *mut u8 {
        N_ALLOC.fetch_add(1, Relaxed);
        BYTES.fetch_add(l.size(), Relaxed);
        grow_live(l.size());
        tally_size(l.size());
        // SAFETY: forwards this method's own contract unchanged (see the impl).
        unsafe { rusty_alloc_api::RustyAlloc.alloc(l) }
    }
    unsafe fn alloc_zeroed(&self, l: Layout) -> *mut u8 {
        N_ZEROED.fetch_add(1, Relaxed);
        BYTES.fetch_add(l.size(), Relaxed);
        grow_live(l.size());
        tally_size(l.size());
        // SAFETY: forwards this method's own contract unchanged (see the impl).
        unsafe { rusty_alloc_api::RustyAlloc.alloc_zeroed(l) }
    }
    unsafe fn realloc(&self, p: *mut u8, l: Layout, new: usize) -> *mut u8 {
        N_REALLOC.fetch_add(1, Relaxed);
        BYTES.fetch_add(new.saturating_sub(l.size()), Relaxed);
        MOVED.fetch_add(l.size(), Relaxed);
        if new >= l.size() {
            grow_live(new - l.size());
        } else {
            LIVE.fetch_sub(l.size() - new, Relaxed);
        }
        // SAFETY: forwards this method's own contract unchanged (see the impl).
        unsafe { rusty_alloc_api::RustyAlloc.realloc(p, l, new) }
    }
    unsafe fn dealloc(&self, p: *mut u8, l: Layout) {
        N_FREE.fetch_add(1, Relaxed);
        LIVE.fetch_sub(l.size(), Relaxed);
        // SAFETY: forwards this method's own contract unchanged (see the impl).
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
    fn since(self, base: Self) -> Self {
        Self {
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
        h ^= u64::from(b);
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

    let sizes = std::env::var_os("ALLOCAUDIT_SIZES").is_some();
    if let Some(n) = std::env::var("ALLOCAUDIT_TRACE")
        .ok()
        .and_then(|v| v.parse().ok())
    {
        TRACE_SIZE.store(n, Relaxed);
    }
    SIZES_ON.store(sizes || TRACE_SIZE.load(Relaxed) != usize::MAX, Relaxed);
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
    SIZES_ON.store(false, Relaxed);

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

    // ---- ENCODE the same input pushed WHOLE, as s16 -- the CLI's shape: a WAV
    // demuxes to ONE frame, so the adapter hands the encoder the entire file in a
    // single push. Streaming one frame at a time (above) never shows what that
    // costs; this does. Same samples as the streamed pass, quantised to s16.
    let s16: Vec<i16> = pcm.iter().map(|&x| (x * 32767.0) as i16).collect();
    let mut enc2 = Mp3Encoder::new(Mp3EncoderConfig {
        bitrate_kbps: kbps,
        vbr_quality: None,
    });
    let mut mp3_whole: Vec<u8> = Vec::with_capacity(frames * 1024);
    let (moved0, live0) = (MOVED.load(Relaxed), LIVE.load(Relaxed));
    PEAK.store(live0, Relaxed);
    let t2 = snap();
    enc2.push_pcm_s16(&s16, CH, SR).unwrap();
    enc2.finish();
    while let Ok(p) = enc2.next_packet() {
        mp3_whole.extend_from_slice(&p);
    }
    let whole_stats = snap().since(t2);
    let whole_moved = MOVED.load(Relaxed) - moved0;
    let whole_peak = PEAK.load(Relaxed) - live0;
    let whole_hash = fnv1a(&mp3_whole);

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
    println!(
        "\n  encode, whole input in ONE s16 push (the CLI/WAV shape):\n    \
         {} allocs ({:.2}/frame), {} KiB requested, {} KiB carried by realloc, \
         peak live +{} KiB\n    bitstream fnv1a: {whole_hash:#018x}",
        whole_stats.total(),
        whole_stats.total() as f64 / ef as f64,
        whole_stats.bytes / 1024,
        whole_moved / 1024,
        whole_peak / 1024,
    );
    if sizes {
        let mut rows: Vec<(usize, usize)> = (0..SLOTS)
            .filter_map(|i| {
                let k = SIZE_KEY[i].load(Relaxed);
                (k != 0).then(|| (k - 1, SIZE_N[i].load(Relaxed)))
            })
            .collect();
        rows.sort_by(|a, b| b.1.cmp(&a.1).then(a.0.cmp(&b.0)));
        println!(
            "
  streamed-encode allocation sizes (bytes x count, per frame):"
        );
        for (size, n) in rows.iter().take(16) {
            println!(
                "    {size:>8} B  x {n:>7}   {:.2}/frame",
                *n as f64 / ef as f64
            );
        }
    }
}
