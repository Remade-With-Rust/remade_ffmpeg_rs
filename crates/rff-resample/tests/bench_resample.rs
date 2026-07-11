//! Deterministic resampler throughput bench (the A/B gate for optimization
//! bricks). Run with: `cargo test -p rff-resample --release -- --ignored --nocapture`.
//!
//! Times the real-world hot path — 24 s of 44.1 kHz stereo → 48 kHz — best-of-N,
//! and cross-checks output length so a "fast" no-op can't sneak through.
use rff_resample::Resampler;
use std::time::Instant;

fn make_stereo_44k(seconds: usize) -> Vec<f32> {
    let n = 44_100 * seconds;
    let mut v = Vec::with_capacity(n * 2);
    // Deterministic multi-tone so the kernel sees real signal (not silence).
    for i in 0..n {
        let t = i as f64 / 44_100.0;
        let l = (std::f64::consts::TAU * 440.0 * t).sin()
            + 0.5 * (std::f64::consts::TAU * 3000.0 * t).sin();
        let r = (std::f64::consts::TAU * 660.0 * t).sin();
        v.push((l * 0.4) as f32);
        v.push((r * 0.4) as f32);
    }
    v
}

#[test]
#[ignore]
fn bench_44k_to_48k_stereo() {
    let input = make_stereo_44k(24);
    let passes = 7;
    let mut best = f64::INFINITY;
    let mut out_len = 0usize;
    for _ in 0..passes {
        let mut rs = Resampler::new(44_100, 48_000, 2);
        let t = Instant::now();
        let mut out = rs.process(&input);
        out.extend(rs.finish());
        let ms = t.elapsed().as_secs_f64() * 1000.0;
        out_len = out.len();
        if ms < best {
            best = ms;
        }
    }
    let out_frames = out_len / 2;
    let expected = (44_100.0 * 24.0 * 48_000.0 / 44_100.0) as usize;
    eprintln!(
        "44.1k->48k stereo, 24 s: best {best:.2} ms  ({out_frames} out frames/ch, expected ~{expected})"
    );
    assert!(
        (out_frames as isize - expected as isize).abs() < 128,
        "output length drifted: {out_frames} vs {expected}"
    );
}
