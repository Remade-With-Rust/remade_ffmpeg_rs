//! Decode an Ogg Vorbis file with the in-house decoder: best-of-N timing of
//! the packet decode (Ogg paging done once, outside the timed loop), and the
//! per-stage profile when built with `--features profile`.
//!
//! ```text
//! cargo run --release -p rusty_vorbis --example vorbisdec -- file.ogg [passes]
//! cargo run --release -p rusty_vorbis --features profile --example vorbisdec -- file.ogg
//! ```

// Project convention: example/bench roots run under the shipping allocator.
#[global_allocator]
static GLOBAL_ALLOC: rusty_alloc_api::RustyAlloc = rusty_alloc_api::RustyAlloc;

#[path = "../tests/common/ogg.rs"]
mod ogg;

use rusty_vorbis::decode::Decoder;
use std::time::Instant;

fn main() {
    let args: Vec<String> = std::env::args().collect();
    let path = args.get(1).expect("usage: vorbisdec <file.ogg> [passes]");
    let passes: usize = args.get(2).map_or(10, |s| s.parse().unwrap());
    let pk = ogg::read_packets(&std::fs::read(path).unwrap());
    let mut dec = Decoder::new(&pk[0].data, &pk[1].data, &pk[2].data).unwrap();
    let ch = dec.channels();
    let mut buf = vec![0.0f32; dec.max_frames_per_packet() * ch];
    let (mut best, mut best_cyc, mut frames) = (f64::INFINITY, u64::MAX, 0usize);
    for _ in 0..passes {
        #[cfg(feature = "profile")]
        rusty_vorbis::decode::prof::reset();
        dec.reset();
        let c0 = thread_cycles();
        let t = Instant::now();
        let mut f = 0usize;
        for p in &pk[3..] {
            f += dec.decode_interleaved(&p.data, &mut buf).unwrap();
        }
        best = best.min(t.elapsed().as_secs_f64() * 1e3);
        best_cyc = best_cyc.min(thread_cycles().wrapping_sub(c0));
        frames = f;
    }
    let secs = frames as f64 / f64::from(dec.sample_rate());
    println!(
        "{path}: {ch} ch, {} Hz, {frames} frames ({secs:.2} s); best of {passes}: {best:.3} ms ({:.0}x realtime)",
        dec.sample_rate(),
        secs * 1e3 / best
    );
    println!(
        "BEST_MS={best:.4} FRAMES={frames} BEST_MCYC={:.4}",
        best_cyc as f64 / 1e6
    );
    #[cfg(feature = "profile")]
    {
        let snap = rusty_vorbis::decode::prof::snapshot();
        let total: u64 = snap.iter().map(|s| s.1).sum();
        println!("stage profile (last pass, rdtsc cycles):");
        for (name, cyc, calls) in snap {
            println!(
                "  {name:<30} {:>6.1}%  {:>12} cyc  {calls:>8} calls",
                100.0 * cyc as f64 / total.max(1) as f64,
                cyc
            );
        }
    }
}

/// CPU cycles charged to this thread (Windows `QueryThreadCycleTime`): unlike
/// wall time it does not accrue while the thread is descheduled.
#[cfg(windows)]
fn thread_cycles() -> u64 {
    extern "system" {
        fn GetCurrentThread() -> isize;
        fn QueryThreadCycleTime(h: isize, c: *mut u64) -> i32;
    }
    let mut c = 0u64;
    // SAFETY: plain Win32 call with a valid out-pointer.
    unsafe { QueryThreadCycleTime(GetCurrentThread(), &mut c) };
    c
}

#[cfg(not(windows))]
fn thread_cycles() -> u64 {
    0
}
