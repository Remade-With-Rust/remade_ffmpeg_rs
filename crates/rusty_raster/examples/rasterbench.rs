//! Paired speed comparison against tiny-skia 0.12 on the oracle corpus.
//!
//! Both arms render the same 645 scenes (`tests/oracle.rs`) in one process;
//! rounds alternate the leading arm (ABBA), each arm reports its own
//! monotonic-clock duration, and a null arm (rusty_raster against itself)
//! gives the noise floor. Pin the process to one core at High priority.
//!
//! `cargo run -p rusty_raster --release --example rasterbench -- [rounds] [reps]`

#[global_allocator]
static GLOBAL_ALLOC: rusty_alloc_api::RustyAlloc = rusty_alloc_api::RustyAlloc;

#[allow(dead_code)]
#[path = "../tests/oracle.rs"]
mod oracle;

use std::time::Instant;

fn run(
    arm: fn(&oracle::Scene) -> Option<Vec<u8>>,
    scenes: &[oracle::Scene],
    reps: u32,
) -> (f64, usize, u64) {
    let t0 = Instant::now();
    let mut n = 0usize;
    let mut sum = 0u64;
    for _ in 0..reps {
        for sc in scenes {
            if let Some(px) = arm(sc) {
                n += 1;
                sum = sum.wrapping_add(px.iter().map(|&b| u64::from(b)).sum::<u64>());
            }
        }
    }
    (t0.elapsed().as_secs_f64() * 1e3, n, sum)
}

fn paired(
    name: &str,
    a: fn(&oracle::Scene) -> Option<Vec<u8>>,
    b: fn(&oracle::Scene) -> Option<Vec<u8>>,
    scenes: &[oracle::Scene],
    rounds: u32,
    reps: u32,
) {
    let mut ratios = Vec::new();
    let (mut wins, mut min_a, mut min_b) = (0u32, f64::MAX, f64::MAX);
    let mut gaps = Vec::new();
    for r in 0..rounds {
        let order_ab = r % 4 == 0 || r % 4 == 3; // ABBA
        let (ta, na, tb, nb) = if order_ab {
            let x = run(a, scenes, reps);
            let y = run(b, scenes, reps);
            (x.0, x.1, y.0, y.1)
        } else {
            let y = run(b, scenes, reps);
            let x = run(a, scenes, reps);
            (x.0, x.1, y.0, y.1)
        };
        assert_eq!(na, nb, "work parity: scene counts differ");
        ratios.push(tb / ta);
        if ta < tb {
            wins += 1;
        }
        min_a = min_a.min(ta);
        min_b = min_b.min(tb);
        gaps.push((ta - tb).abs());
    }
    ratios.sort_by(f64::total_cmp);
    let med = ratios[ratios.len() / 2];
    let z = (f64::from(wins) - f64::from(rounds) / 2.0) / (0.5 * f64::from(rounds).sqrt());
    println!(
        "{name}: rounds {rounds} x {reps} reps x {} scenes | median ratio B/A {med:.3} (range {:.3}..{:.3}) | A faster in {wins}/{rounds} (z = {z:+.2}) | min A {min_a:.1} ms, min B {min_b:.1} ms",
        scenes.len(),
        ratios[0],
        ratios[ratios.len() - 1],
    );
}

fn main() {
    let args: Vec<String> = std::env::args().collect();
    let rounds: u32 = args.get(1).and_then(|v| v.parse().ok()).unwrap_or(20);
    let reps: u32 = args.get(2).and_then(|v| v.parse().ok()).unwrap_or(10);
    let scenes = oracle::corpus();
    // Warm up both arms and print the work-parity anchor.
    let (_, n_rr, s_rr) = run(oracle::render_rr, &scenes, 1);
    let (_, n_ts, s_ts) = run(oracle::render_ts, &scenes, 1);
    println!("method: one process, Instant (QPC) per arm, ABBA rounds, null arm first; scenes rendered rr {n_rr} ts {n_ts}; byte sums rr {s_rr} ts {s_ts}");
    paired(
        "null (A = rusty_raster, B = rusty_raster)",
        oracle::render_rr,
        oracle::render_rr,
        &scenes,
        rounds,
        reps,
    );
    paired(
        "A = rusty_raster, B = tiny-skia 0.12",
        oracle::render_rr,
        oracle::render_ts,
        &scenes,
        rounds,
        reps,
    );
}
