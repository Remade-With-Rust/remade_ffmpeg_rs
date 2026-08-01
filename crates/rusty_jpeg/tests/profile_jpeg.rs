//! Deterministic in-process benchmark + stage breakdown.
//!
//! The CLI is useless as a speed instrument here: writing a 4K y4m costs ~1.5 s
//! of disk, which swamps a ~200 ms codec and makes a base-subtracted difference
//! meaningless. This measures the codec and nothing else.
//!
//! ```text
//! cargo test --release -p rusty_jpeg --test profile_jpeg -- --nocapture --ignored
//! cargo test --release -p rusty_jpeg --features profile --test profile_jpeg -- --nocapture --ignored
//! ```
//!
//! Throughput comes from the profiler-OFF build; the breakdown from the ON one.

use rusty_jpeg::decode::Decoder;
use rusty_jpeg::encode::{ColorType, Encoder, PlanarYcbcrImage, SamplingFactor};
use std::io::Cursor;
use std::time::Instant;

/// How much high-frequency energy the synthetic source carries.
///
/// This axis exists because the first version of this benchmark had only one
/// content type and it was pathological: 39.1 of 63 AC coefficients non-zero
/// and 2.5x whole-frame compression, where a photograph at q90 leaves most of
/// the AC spectrum zero and compresses ~10x. Every stage share measured on that
/// content overstated the entropy coder. Content density is a first-class
/// variable here, not a detail of the fixture.
#[derive(Clone, Copy, PartialEq, Debug)]
pub enum Density {
    /// Photographic: large smooth regions, soft edges, little fine texture.
    Smooth,
    /// Textured but natural — foliage/fabric scale detail.
    Detail,
    /// Per-pixel noise. Near worst case for the entropy coder; kept because a
    /// codec should be measured at its extremes too, but never alone.
    Noise,
    /// **The one to trust.** Multi-scale 1/f (fractal) detail, which is what
    /// natural images actually have — energy falling off with frequency rather
    /// than concentrated at one scale or spread flat like white noise. Single
    /// sinusoids and white noise both misrepresent the coefficient distribution,
    /// in opposite directions.
    Photo,
}

impl Density {
    fn all() -> [Density; 4] {
        [
            Density::Smooth,
            Density::Detail,
            Density::Photo,
            Density::Noise,
        ]
    }
    fn name(self) -> &'static str {
        match self {
            Density::Smooth => "smooth",
            Density::Detail => "detail",
            Density::Noise => "noise",
            Density::Photo => "photo",
        }
    }
}

/// Deterministic planar Y'CbCr source at a chosen detail density.
fn planar_source_at(w: usize, h: usize, density: Density) -> (Vec<u8>, Vec<u8>, Vec<u8>) {
    let (cw, ch) = (w / 2, h / 2);
    let mut y = vec![0u8; w * h];
    let mut cb = vec![0u8; cw * ch];
    let mut cr = vec![0u8; cw * ch];
    let mut seed = 0x2545_F491_4F6C_DD1Du64;
    let mut rnd = move || {
        seed ^= seed << 13;
        seed ^= seed >> 7;
        seed ^= seed << 17;
        (seed >> 33) as u8
    };
    for j in 0..h {
        for i in 0..w {
            let (fi, fj) = (i as f32 / w as f32, j as f32 / h as f32);
            // Low-frequency base present in every class: broad lighting plus a
            // couple of soft blobs, i.e. the part a DCT codes almost for free.
            let base = 96.0 + 90.0 * (fi * 2.3 + fj * 1.7).sin() * (fj * 1.9).cos();
            let v = match density {
                Density::Smooth => base,
                // Mid-frequency texture, no per-pixel component.
                Density::Detail => base + 34.0 * ((fi * 61.0).sin() * (fj * 47.0).cos()),
                Density::Noise => {
                    base + 34.0 * ((fi * 61.0).sin() * (fj * 47.0).cos())
                        + (rnd() as f32 - 128.0) * 0.5
                }
                // fBm: octaves at halving amplitude and doubling frequency.
                Density::Photo => base + fbm(i, j, PHOTO_AMPLITUDE),
            };
            y[j * w + i] = v.clamp(0.0, 255.0) as u8;
        }
    }
    for j in 0..ch {
        for i in 0..cw {
            let (fi, fj) = (i as f32 / cw as f32, j as f32 / ch as f32);
            let b = 128.0 + 40.0 * (fi * 3.1 + fj * 2.2).sin();
            let r = 128.0 + 40.0 * (fj * 2.7 - fi * 1.4).cos();
            let n = if density == Density::Noise {
                (rnd() as f32 - 128.0) * 0.25
            } else {
                0.0
            };
            cb[j * cw + i] = (b + n).clamp(0.0, 255.0) as u8;
            cr[j * cw + i] = (r + n).clamp(0.0, 255.0) as u8;
        }
    }
    (y, cb, cr)
}

/// Amplitude of the fractal detail in [`Density::Photo`], chosen by sweep (see
/// `calibrate_photo_amplitude`) so a 1080p q90 4:2:0 encode lands at **11.5x**
/// whole-frame compression — inside the ~10-15x band real photographs occupy.
///
/// Compression ratio is the anchor, not non-zero-AC count: the two do not move
/// together, because the smooth base contributes low-frequency bits without
/// adding non-zero AC. At this amplitude the encode carries ~6.6 non-zero AC per
/// block against the noise fixture's 50.
const PHOTO_AMPLITUDE: f32 = 70.0;

/// Deterministic value-noise fBm — smooth interpolated noise summed over
/// octaves at halving amplitude, giving the 1/f spectrum natural images have.
fn fbm(x: usize, y: usize, amplitude: f32) -> f32 {
    #[inline]
    fn hash(xi: i32, yi: i32) -> f32 {
        let mut h = (xi as u32).wrapping_mul(0x9E37_79B9) ^ (yi as u32).wrapping_mul(0x85EB_CA6B);
        h ^= h >> 15;
        h = h.wrapping_mul(0x2545_F491);
        h ^= h >> 13;
        (h >> 8) as f32 / 8_388_608.0 - 1.0
    }
    #[inline]
    fn value_noise(fx: f32, fy: f32) -> f32 {
        let (x0, y0) = (fx.floor() as i32, fy.floor() as i32);
        let (tx, ty) = (fx - x0 as f32, fy - y0 as f32);
        // Smoothstep so octaves are continuous, not blocky.
        let (sx, sy) = (tx * tx * (3.0 - 2.0 * tx), ty * ty * (3.0 - 2.0 * ty));
        let a = hash(x0, y0) + (hash(x0 + 1, y0) - hash(x0, y0)) * sx;
        let b = hash(x0, y0 + 1) + (hash(x0 + 1, y0 + 1) - hash(x0, y0 + 1)) * sx;
        a + (b - a) * sy
    }
    let mut sum = 0.0;
    let mut amp = amplitude;
    let mut freq = 1.0 / 64.0;
    for _ in 0..6 {
        sum += amp * value_noise(x as f32 * freq, y as f32 * freq);
        amp *= 0.5;
        freq *= 2.0;
    }
    sum
}

/// The original fixture, kept so existing callers keep their meaning.
fn planar_source(w: usize, h: usize) -> (Vec<u8>, Vec<u8>, Vec<u8>) {
    planar_source_at(w, h, Density::Noise)
}

fn encode_planar(
    y: &[u8],
    cb: &[u8],
    cr: &[u8],
    w: usize,
    h: usize,
    quality: u8,
    optimize: bool,
) -> Vec<u8> {
    let mut out = Vec::with_capacity(w * h);
    let img =
        PlanarYcbcrImage::new(y, cb, cr, [w, w / 2, w / 2], w as u16, h as u16, (2, 2)).unwrap();
    let mut enc = Encoder::new(&mut out, quality);
    enc.set_sampling_factor(SamplingFactor::R_4_2_0);
    enc.set_optimized_huffman_tables(optimize);
    enc.encode_image(img).unwrap();
    out
}

/// Best-of-N wall time, in ms.
fn best_of<F: FnMut()>(n: usize, mut f: F) -> f64 {
    let mut best = f64::MAX;
    for _ in 0..n {
        let t = Instant::now();
        f();
        best = best.min(t.elapsed().as_secs_f64() * 1000.0);
    }
    best
}

#[test]
#[ignore = "benchmark; run explicitly with --ignored --nocapture"]
fn profile_encode() {
    const N: usize = 7;
    // One density at a time, with the profiler reset between them. The buckets
    // are global, so profiling two content classes in one run averages them —
    // and the noise fixture (50 non-zero AC/block) would swamp the photographic
    // one (6.6), which is the exact contamination this descent set out to remove.
    //
    // Timings here are only meaningful in a profiler-OFF build; with `profile`
    // on, read the SHARES and ignore the milliseconds.
    for (w, h) in [(3840usize, 2160usize)] {
        for density in [Density::Photo, Density::Noise] {
            let (y, cb, cr) = planar_source_at(w, h, density);
            let mpx = (w * h) as f64 / 1e6;
            println!(
                "
=== encode {}x{} {} (best-of-{N}) ===",
                w,
                h,
                density.name()
            );
            println!(
                "{:<10} {:>10} {:>10} {:>12}",
                "huff", "ms", "Mpx/s", "bytes"
            );
            for optimize in [false, true] {
                rusty_jpeg::prof::reset();
                let mut bytes = 0usize;
                let ms = best_of(N, || {
                    bytes = encode_planar(&y, &cb, &cr, w, h, 90, optimize).len();
                });
                println!(
                    "{:<10} {:>10.1} {:>10.1} {:>12}",
                    if optimize { "optimal" } else { "default" },
                    ms,
                    mpx / (ms / 1000.0),
                    bytes
                );
                if optimize {
                    println!("{}", rusty_jpeg::prof::dump());
                }
            }
        }
    }
}

#[test]
#[ignore = "benchmark; run explicitly with --ignored --nocapture"]
fn profile_decode() {
    const N: usize = 7;
    println!("\n=== rusty_jpeg decode (best-of-{N}, in-process) ===");
    println!("{:<12} {:>12} {:>10} {:>10}", "size", "mode", "ms", "Mpx/s");

    for (w, h) in [(1920usize, 1080usize), (3840, 2160)] {
        let (y, cb, cr) = planar_source(w, h);
        let jpeg = encode_planar(&y, &cb, &cr, w, h, 90, true);
        let mpx = (w * h) as f64 / 1e6;

        let ms = best_of(N, || {
            let mut d = Decoder::new(Cursor::new(&jpeg));
            std::hint::black_box(d.decode_planar().unwrap());
        });
        println!(
            "{:<12} {:>12} {:>10.1} {:>10.1}",
            format!("{w}x{h}"),
            "planar",
            ms,
            mpx / (ms / 1000.0)
        );

        let ms = best_of(N, || {
            let mut d = Decoder::new(Cursor::new(&jpeg));
            std::hint::black_box(d.decode().unwrap());
        });
        println!(
            "{:<12} {:>12} {:>10.1} {:>10.1}",
            format!("{w}x{h}"),
            "rgb",
            ms,
            mpx / (ms / 1000.0)
        );
    }
}

/// Guards the benchmark itself: the source must actually be hard to code. Flat
/// content would make every speed number optimistic and unrepresentative.
#[test]
fn benchmark_source_is_not_trivially_compressible() {
    let (w, h) = (256usize, 256usize);
    let (y, cb, cr) = planar_source(w, h);
    let jpeg = encode_planar(&y, &cb, &cr, w, h, 90, true);
    let raw = w * h * 3 / 2;
    let ratio = raw as f64 / jpeg.len() as f64;
    assert!(
        ratio < 12.0,
        "benchmark content compresses {ratio:.1}x — too easy to be representative"
    );
    // And it must still be a valid JPEG.
    let mut d = Decoder::new(Cursor::new(&jpeg));
    assert!(d.decode().is_ok());
}

// ---------------------------------------------------------------------------
// Interleaved A/B with a null arm.
//
// Arm-by-arm timing lies on a drifting machine: a run that moved the *untouched*
// `default` arm by 17% made a real comparison impossible. So both arms run in
// one process, alternating, ABBA-ordered to cancel position bias — and a `null`
// arm (both sides identical) establishes what the harness itself can resolve.
// Any difference smaller than the null arm's spread is not a result.
// ---------------------------------------------------------------------------

fn encode_with(y: &[u8], cb: &[u8], cr: &[u8], w: usize, h: usize, streaming: bool) -> Vec<u8> {
    let mut out = Vec::with_capacity(w * h);
    let img =
        PlanarYcbcrImage::new(y, cb, cr, [w, w / 2, w / 2], w as u16, h as u16, (2, 2)).unwrap();
    let mut enc = Encoder::new(&mut out, 90);
    enc.set_sampling_factor(SamplingFactor::R_4_2_0);
    enc.set_optimized_huffman_tables(true);
    enc.set_streaming_optimize(streaming);
    enc.encode_image(img).unwrap();
    out
}

#[test]
#[ignore = "benchmark; run explicitly with --ignored --nocapture"]
fn ab_streaming_vs_buffered_optimize() {
    // RE-TEST of a previously "neutral" verdict.
    //
    // Streaming optimize makes two passes over the image; buffered makes one and
    // pays ~218 MB of block traffic instead. That traded even when FillBuffers
    // was 17.3% of encode — because streaming paid it twice. FillBuffers is now
    // 1.0%, so the trade has moved and the old measurement no longer applies.
    // A refutation expires when its baseline does.
    const REPS: usize = 41;
    let (w, h) = (3840usize, 2160usize);
    println!(
        "
=== streaming vs buffered huffman-optimize (paired, ABBA, n={REPS}) ==="
    );
    println!(
        "{:<26} {:>10} {:>8} {:>10} {:>20}",
        "arm B", "wins(A)", "z", "med B/A", "verdict"
    );
    for density in [Density::Photo] {
        let (y, cb, cr) = planar_source_at(w, h, density);
        let (wn, zn, mn) = paired_ab(
            REPS,
            || time_encode_opt(&y, &cb, &cr, w, h, true),
            || time_encode_opt(&y, &cb, &cr, w, h, true),
        );
        println!(
            "{:<26} {:>10} {:>8.2} {:>10.4} {:>20}",
            format!("null [{}]", density.name()),
            format!("{wn}/{REPS}"),
            zn,
            mn,
            if zn.abs() > 2.0 {
                "HARNESS BIASED"
            } else {
                "harness clean"
            }
        );
        let (wr, zr, mr) = paired_ab(
            REPS,
            || time_encode_opt(&y, &cb, &cr, w, h, true),
            || time_encode_opt(&y, &cb, &cr, w, h, false),
        );
        println!(
            "{:<26} {:>10} {:>8.2} {:>10.4} {:>20}",
            format!("buffered [{}]", density.name()),
            format!("{wr}/{REPS}"),
            zr,
            mr,
            if zr > 2.0 {
                "streaming WINS"
            } else if zr < -2.0 {
                "buffered WINS"
            } else {
                "inside noise"
            }
        );
    }
}

fn time_encode_opt(y: &[u8], cb: &[u8], cr: &[u8], w: usize, h: usize, streaming: bool) -> f64 {
    let t = Instant::now();
    let mut out = Vec::with_capacity(w * h);
    let img =
        PlanarYcbcrImage::new(y, cb, cr, [w, w / 2, w / 2], w as u16, h as u16, (2, 2)).unwrap();
    let mut enc = Encoder::new(&mut out, 90);
    enc.set_sampling_factor(SamplingFactor::R_4_2_0);
    enc.set_optimized_huffman_tables(true);
    enc.set_streaming_optimize(streaming);
    enc.encode_image(img).unwrap();
    std::hint::black_box(&out);
    t.elapsed().as_secs_f64() * 1000.0
}

/// Both strategies must produce a decodable JPEG of comparable size — they build
/// different (both valid) tables, so bytes are not expected to match exactly.
#[test]
fn streaming_and_buffered_optimize_agree() {
    let (w, h) = (128usize, 96usize);
    let (y, cb, cr) = planar_source(w, h);
    let stream = encode_with(&y, &cb, &cr, w, h, true);
    let buffer = encode_with(&y, &cb, &cr, w, h, false);

    for (name, jpeg) in [("streaming", &stream), ("buffered", &buffer)] {
        let mut d = Decoder::new(Cursor::new(jpeg));
        let px = d
            .decode()
            .unwrap_or_else(|e| panic!("{name} did not decode: {e}"));
        assert_eq!(px.len(), w * h * 3, "{name}");
    }
    // Streaming counts in MCU order and buffered in raster order, so the DC
    // histograms differ slightly; the resulting sizes should still be within a
    // few percent of each other.
    let ratio = stream.len() as f64 / buffer.len() as f64;
    assert!(
        (0.95..1.05).contains(&ratio),
        "streaming {} B vs buffered {} B (ratio {ratio:.3}) — tables diverged more than expected",
        stream.len(),
        buffer.len()
    );
}

/// D6a: how representative is each content class? Deterministic counts only —
/// no timing, so this is immune to machine state.
///
/// Run with `--features counters` for the per-block symbol figures.
#[test]
#[ignore = "instrument; run explicitly with --ignored --nocapture"]
fn content_density_report() {
    let (w, h) = (1920usize, 1080usize);
    let raw = w * h * 3 / 2;
    println!("\n=== content representativeness (1920x1080, q90, 4:2:0) ===");
    println!(
        "{:<10} {:>12} {:>10} {:>16} {:>14}",
        "density", "bytes", "ratio", "nonzero-AC/blk", "symbols/blk"
    );
    for d in Density::all() {
        let (y, cb, cr) = planar_source_at(w, h, d);
        rusty_jpeg::prof::reset_counts();
        let jpeg = encode_planar(&y, &cb, &cr, w, h, 90, true);
        let c = rusty_jpeg::prof::read();
        use rusty_jpeg::prof::Count;
        // Blocks in a 4:2:0 frame: luma 4 per MCU + 2 chroma.
        let mcus = w.div_ceil(16) * h.div_ceil(16);
        let blocks = (mcus * 6) as f64;
        println!(
            "{:<10} {:>12} {:>9.1}x {:>16.1} {:>14.1}",
            d.name(),
            jpeg.len(),
            raw as f64 / jpeg.len() as f64,
            c[Count::NonZeroAc as usize] as f64 / blocks,
            c[Count::Symbols as usize] as f64 / blocks,
        );
    }
    println!(
        "\n  A photograph at q90 compresses ~10-15x and leaves most of the 63 AC\n  \
         coefficients zero. Any class far from that overstates entropy work.\n  \
         (Counts read 0 unless built with --features counters.)"
    );
}

/// Calibration for `PHOTO_AMPLITUDE`. Sweeps the fractal amplitude and reports
/// where the encode lands, so the constant is chosen from data rather than
/// guessed. Target band: ~10-15x compression, roughly 8-20 non-zero AC per
/// block, which is where photographic content at q90 sits.
#[test]
#[ignore = "instrument; run explicitly with --ignored --nocapture"]
fn calibrate_photo_amplitude() {
    let (w, h) = (1920usize, 1080usize);
    let raw = w * h * 3 / 2;
    println!("\n=== PHOTO_AMPLITUDE calibration (1920x1080, q90, 4:2:0) ===");
    println!(
        "{:>6} {:>12} {:>9} {:>16}",
        "amp", "bytes", "ratio", "nonzero-AC/blk"
    );
    let mcus = w.div_ceil(16) * h.div_ceil(16);
    let blocks = (mcus * 6) as f64;
    for amp in [26.0f32, 40.0, 55.0, 70.0, 90.0, 115.0] {
        let (cw, ch) = (w / 2, h / 2);
        let mut y = vec![0u8; w * h];
        for j in 0..h {
            for i in 0..w {
                let (fi, fj) = (i as f32 / w as f32, j as f32 / h as f32);
                let base = 96.0 + 90.0 * (fi * 2.3 + fj * 1.7).sin() * (fj * 1.9).cos();
                y[j * w + i] = (base + fbm(i, j, amp)).clamp(0.0, 255.0) as u8;
            }
        }
        let (mut cb, mut cr) = (vec![0u8; cw * ch], vec![0u8; cw * ch]);
        for j in 0..ch {
            for i in 0..cw {
                let (fi, fj) = (i as f32 / cw as f32, j as f32 / ch as f32);
                cb[j * cw + i] = (128.0 + 40.0 * (fi * 3.1 + fj * 2.2).sin()) as u8;
                cr[j * cw + i] = (128.0 + 40.0 * (fj * 2.7 - fi * 1.4).cos()) as u8;
            }
        }
        rusty_jpeg::prof::reset_counts();
        let jpeg = encode_planar(&y, &cb, &cr, w, h, 90, true);
        let c = rusty_jpeg::prof::read();
        println!(
            "{:>6.0} {:>12} {:>8.1}x {:>16.1}",
            amp,
            jpeg.len(),
            raw as f64 / jpeg.len() as f64,
            c[rusty_jpeg::prof::Count::NonZeroAc as usize] as f64 / blocks
        );
    }
}

// ---------------------------------------------------------------------------
// Paired A/B with a win-rate z-score.
//
// This box drifts: one profiler run read 2.76x more cycles than another for
// identical work, because rdtsc counts reference cycles and the CPU had
// throttled. Sequential arm-by-arm timing therefore samples two different
// machines. Both arms run alternating inside one process, ABBA-ordered, and the
// statistic is the PAIRED WIN RATE: under the null hypothesis "no difference"
// that is a fair coin, so z = (wins - N/2) / (0.5*sqrt(N)) and |z| > 2 is a
// verdict regardless of how far the medians drifted.
//
// Win rate answers *whether*. Only the median ratio answers *how much*, and it
// needs the same N.
// ---------------------------------------------------------------------------

/// Run `a` and `b` head to head `reps` times, alternating which goes first.
/// Returns (wins_for_a, z, median ratio b/a).
fn paired_ab(
    reps: usize,
    mut a: impl FnMut() -> f64,
    mut b: impl FnMut() -> f64,
) -> (usize, f64, f64) {
    let mut wins = 0usize;
    let mut ratios = Vec::with_capacity(reps);
    for rep in 0..reps {
        let (ta, tb) = if rep % 2 == 0 {
            let ta = a();
            let tb = b();
            (ta, tb)
        } else {
            let tb = b();
            let ta = a();
            (ta, tb)
        };
        if !ta.is_finite() || !tb.is_finite() {
            continue; // a sample the instrument failed to take is not a tie
        }
        if ta < tb {
            wins += 1;
        }
        ratios.push(tb / ta);
    }
    ratios.sort_by(|x, y| x.partial_cmp(y).unwrap());
    let n = ratios.len();
    let median = if n == 0 { f64::NAN } else { ratios[n / 2] };
    let z = (wins as f64 - n as f64 / 2.0) / (0.5 * (n as f64).sqrt());
    (wins, z, median)
}

fn time_encode(y: &[u8], cb: &[u8], cr: &[u8], w: usize, h: usize, branchy: bool) -> f64 {
    let t = Instant::now();
    let mut out = Vec::with_capacity(w * h);
    let img =
        PlanarYcbcrImage::new(y, cb, cr, [w, w / 2, w / 2], w as u16, h as u16, (2, 2)).unwrap();
    let mut enc = Encoder::new(&mut out, 90);
    enc.set_sampling_factor(SamplingFactor::R_4_2_0);
    enc.set_optimized_huffman_tables(true);
    enc.set_branchy_quantize(branchy);
    enc.encode_image(img).unwrap();
    std::hint::black_box(&out);
    t.elapsed().as_secs_f64() * 1000.0
}

#[test]
#[ignore = "benchmark; run explicitly with --ignored --nocapture"]
fn ab_branchless_quantize() {
    const REPS: usize = 25;
    let (w, h) = (1920usize, 1080usize);

    println!("\n=== branchless vs branchy quantize (paired, ABBA, n={REPS}) ===");
    println!(
        "{:<24} {:>10} {:>8} {:>10} {:>26}",
        "arm B", "wins(A)", "z", "med B/A", "verdict"
    );

    for density in [Density::Photo, Density::Noise] {
        let (y, cb, cr) = planar_source_at(w, h, density);

        // Null arm: both sides branchless. Establishes what this harness can
        // resolve; anything smaller than its spread is not a result.
        let (wn, zn, mn) = paired_ab(
            REPS,
            || time_encode(&y, &cb, &cr, w, h, false),
            || time_encode(&y, &cb, &cr, w, h, false),
        );
        println!(
            "{:<24} {:>10} {:>8.2} {:>10.4} {:>26}",
            format!("null [{}]", density.name()),
            format!("{wn}/{REPS}"),
            zn,
            mn,
            if zn.abs() > 2.0 {
                "HARNESS BIASED"
            } else {
                "harness clean"
            }
        );

        // Real arm: A = branchless (shipped), B = branchy (old).
        let (wr, zr, mr) = paired_ab(
            REPS,
            || time_encode(&y, &cb, &cr, w, h, false),
            || time_encode(&y, &cb, &cr, w, h, true),
        );
        println!(
            "{:<24} {:>10} {:>8.2} {:>10.4} {:>26}",
            format!("branchy [{}]", density.name()),
            format!("{wr}/{REPS}"),
            zr,
            mr,
            if zr > 2.0 {
                "branchless WINS"
            } else if zr < -2.0 {
                "branchless LOSES"
            } else {
                "inside noise"
            }
        );
    }
}

fn time_encode_fill(y: &[u8], cb: &[u8], cr: &[u8], w: usize, h: usize, push: bool) -> f64 {
    let t = Instant::now();
    let mut out = Vec::with_capacity(w * h);
    let mut img =
        PlanarYcbcrImage::new(y, cb, cr, [w, w / 2, w / 2], w as u16, h as u16, (2, 2)).unwrap();
    img.set_push_path(push);
    let mut enc = Encoder::new(&mut out, 90);
    enc.set_sampling_factor(SamplingFactor::R_4_2_0);
    enc.set_optimized_huffman_tables(true);
    enc.encode_image(img).unwrap();
    std::hint::black_box(&out);
    t.elapsed().as_secs_f64() * 1000.0
}

#[test]
#[ignore = "benchmark; run explicitly with --ignored --nocapture"]
fn ab_chunked_chroma_fill() {
    // n=41: the paired-median estimator uses single samples from each arm, so it
    // is individually noisy and needs N well above 20 before its magnitude can
    // be quoted. The win-rate is decisive far sooner; the median is not.
    const REPS: usize = 41;
    println!("\n=== chunked vs per-byte-push chroma replication (paired, ABBA, n={REPS}) ===");
    println!(
        "{:<26} {:>10} {:>8} {:>10} {:>20}",
        "arm B", "wins(A)", "z", "med B/A", "verdict"
    );
    for (w, h) in [(3840usize, 2160usize)] {
        for density in [Density::Photo] {
            let (y, cb, cr) = planar_source_at(w, h, density);
            let (wn, zn, mn) = paired_ab(
                REPS,
                || time_encode_fill(&y, &cb, &cr, w, h, false),
                || time_encode_fill(&y, &cb, &cr, w, h, false),
            );
            println!(
                "{:<26} {:>10} {:>8.2} {:>10.4} {:>20}",
                format!("null [{w}x{h}]"),
                format!("{wn}/{REPS}"),
                zn,
                mn,
                if zn.abs() > 2.0 {
                    "HARNESS BIASED"
                } else {
                    "harness clean"
                }
            );
            let (wr, zr, mr) = paired_ab(
                REPS,
                || time_encode_fill(&y, &cb, &cr, w, h, false),
                || time_encode_fill(&y, &cb, &cr, w, h, true),
            );
            println!(
                "{:<26} {:>10} {:>8.2} {:>10.4} {:>20}",
                format!("push [{w}x{h}]"),
                format!("{wr}/{REPS}"),
                zr,
                mr,
                if zr > 2.0 {
                    "chunked WINS"
                } else if zr < -2.0 {
                    "chunked LOSES"
                } else {
                    "inside noise"
                }
            );
        }
    }
}

fn time_encode_blocks(y: &[u8], cb: &[u8], cr: &[u8], w: usize, h: usize, push: bool) -> f64 {
    let t = Instant::now();
    let mut out = Vec::with_capacity(w * h);
    let img =
        PlanarYcbcrImage::new(y, cb, cr, [w, w / 2, w / 2], w as u16, h as u16, (2, 2)).unwrap();
    let mut enc = Encoder::new(&mut out, 90);
    enc.set_sampling_factor(SamplingFactor::R_4_2_0);
    enc.set_optimized_huffman_tables(true);
    enc.set_streaming_optimize(false); // buffered path is what this touches
    enc.set_push_blocks(push);
    enc.encode_image(img).unwrap();
    std::hint::black_box(&out);
    t.elapsed().as_secs_f64() * 1000.0
}

#[test]
#[ignore = "benchmark; run explicitly with --ignored --nocapture"]
fn ab_inplace_block_write() {
    const REPS: usize = 41;
    let (w, h) = (3840usize, 2160usize);
    println!("\n=== in-place block write vs push-a-temporary (paired, ABBA, n={REPS}) ===");
    println!(
        "{:<26} {:>10} {:>8} {:>10} {:>20}",
        "arm B", "wins(A)", "z", "med B/A", "verdict"
    );
    let (y, cb, cr) = planar_source_at(w, h, Density::Photo);
    let (wn, zn, mn) = paired_ab(
        REPS,
        || time_encode_blocks(&y, &cb, &cr, w, h, false),
        || time_encode_blocks(&y, &cb, &cr, w, h, false),
    );
    println!(
        "{:<26} {:>10} {:>8.2} {:>10.4} {:>20}",
        "null",
        format!("{wn}/{REPS}"),
        zn,
        mn,
        if zn.abs() > 2.0 {
            "HARNESS BIASED"
        } else {
            "harness clean"
        }
    );
    let (wr, zr, mr) = paired_ab(
        REPS,
        || time_encode_blocks(&y, &cb, &cr, w, h, false),
        || time_encode_blocks(&y, &cb, &cr, w, h, true),
    );
    println!(
        "{:<26} {:>10} {:>8.2} {:>10.4} {:>20}",
        "push-temporary",
        format!("{wr}/{REPS}"),
        zr,
        mr,
        if zr > 2.0 {
            "in-place WINS"
        } else if zr < -2.0 {
            "in-place LOSES"
        } else {
            "inside noise"
        }
    );
}

/// End-to-end gate for the AVX2 quantizer: the kernel is bit-identical to the
/// scalar oracle, so the whole encoded file must be byte-identical too. This
/// catches anything the unit test cannot — a mis-wired dispatch, a table the
/// unit test did not cover, an interaction with the block loop.
#[test]
fn avx2_quantize_produces_byte_identical_files() {
    for (w, h) in [(64usize, 64usize), (127, 65), (1920, 1080)] {
        for density in [Density::Photo, Density::Noise] {
            // Chroma must be ceil(w/2) x ceil(h/2); the shared fixture floors,
            // which is fine for even sizes but under-sizes odd ones.
            let (cw, ch) = (w.div_ceil(2), h.div_ceil(2));
            let (y_full, _, _) =
                planar_source_at(w.next_multiple_of(2), h.next_multiple_of(2), density);
            let y: Vec<u8> = (0..h)
                .flat_map(|j| {
                    let row = j * w.next_multiple_of(2);
                    y_full[row..row + w].to_vec()
                })
                .collect();
            let cb: Vec<u8> = (0..cw * ch).map(|i| (i % 256) as u8).collect();
            let cr: Vec<u8> = (0..cw * ch).map(|i| ((i * 7) % 256) as u8).collect();
            let mk = |branchy: bool| {
                let mut out = Vec::new();
                let img =
                    PlanarYcbcrImage::new(&y, &cb, &cr, [w, cw, cw], w as u16, h as u16, (2, 2))
                        .unwrap();
                let mut enc = Encoder::new(&mut out, 90);
                enc.set_sampling_factor(SamplingFactor::R_4_2_0);
                enc.set_optimized_huffman_tables(true);
                enc.set_branchy_quantize(branchy);
                enc.encode_image(img).unwrap();
                out
            };
            let avx2 = mk(false);
            let scalar = mk(true);
            assert_eq!(
                avx2.len(),
                scalar.len(),
                "{w}x{h} {:?}: length differs",
                density.name()
            );
            assert!(
                avx2 == scalar,
                "{w}x{h} {}: AVX2 and scalar quantizers produced different bytes",
                density.name()
            );
            // And it must still decode.
            let mut d = Decoder::new(Cursor::new(&avx2));
            assert!(d.decode().is_ok(), "{w}x{h} {}", density.name());
        }
    }
}

/// Decode a REAL jpeg file, in-process, best-of-N.
///
/// `JPEG=path cargo test --release -p rusty_jpeg --test profile_jpeg decode_file
/// -- --ignored --nocapture`
///
/// Synthetic fixtures mis-rank stages (this campaign's whole D6a finding), and
/// the CLI is I/O-bound at these sizes, so the honest decoder number comes from
/// feeding a real file straight to the codec with nothing else in the path.
#[test]
#[ignore = "benchmark; run explicitly with --ignored --nocapture"]
fn decode_file() {
    let Ok(path) = std::env::var("JPEG") else {
        println!("set JPEG=<path> to run");
        return;
    };
    let bytes = std::fs::read(&path).expect("read jpeg");
    const N: usize = 15;

    // Geometry first, so throughput can be reported per megapixel.
    let mut d = Decoder::new(Cursor::new(&bytes));
    let planar = d.decode_planar().expect("decode_planar");
    let (w, h) = (planar.width as usize, planar.height as usize);
    let mpx = (w * h) as f64 / 1e6;
    println!(
        "\n=== decode {path} ({w}x{h}, {:?}, {} B) best-of-{N} ===",
        planar.chroma_subsampling(),
        bytes.len()
    );

    let ms = best_of(N, || {
        let mut d = Decoder::new(Cursor::new(&bytes));
        std::hint::black_box(d.decode_planar().unwrap());
    });
    println!(
        "  planar : {ms:>7.2} ms  {:>7.1} Mpx/s",
        mpx / (ms / 1000.0)
    );

    let ms_rgb = best_of(N, || {
        let mut d = Decoder::new(Cursor::new(&bytes));
        std::hint::black_box(d.decode().unwrap());
    });
    println!(
        "  rgb    : {ms_rgb:>7.2} ms  {:>7.1} Mpx/s",
        mpx / (ms_rgb / 1000.0)
    );
}

/// Chroma downsampling must AVERAGE the box, not point-sample it.
///
/// Point-sampling is decimation: it aliases chroma above the subsampled Nyquist
/// straight into the baseband, and no bitrate recovers it. The tell that found
/// it was a PSNR that sat flat at ~14.1 dB from quality 50 to 95 while the file
/// doubled — error that does not respond to bitrate is not quantization error.
///
/// This gates the fix with content built to expose it: fine vertical chroma bars
/// at luma-neutral transitions, where essentially all the signal is chroma.
#[test]
fn chroma_downsampling_averages_rather_than_decimates() {
    use rusty_jpeg::decode::Decoder;
    use rusty_jpeg::encode::{ColorType, Encoder, SamplingFactor};
    use std::io::Cursor;

    const W: usize = 128;
    const H: usize = 128;
    let mut rgb = vec![0u8; W * H * 3];
    for y in 0..H {
        for x in 0..W {
            let o = (y * W + x) * 3;
            // 3-pixel bars: right at the 4:2:0 chroma Nyquist.
            let (r, g, b) = if (x / 3) % 2 == 0 {
                (200u8, 60u8, 60u8)
            } else {
                (60, 200, 200)
            };
            rgb[o] = r;
            rgb[o + 1] = g;
            rgb[o + 2] = b;
        }
    }

    let mut jpg = Vec::new();
    let mut enc = Encoder::new(&mut jpg, 90);
    enc.set_sampling_factor(SamplingFactor::R_4_2_0);
    enc.encode(&rgb, W as u16, H as u16, ColorType::Rgb)
        .expect("encode");

    let out = Decoder::new(Cursor::new(&jpg)).decode().expect("decode");
    let mse: f64 = rgb
        .iter()
        .zip(&out)
        .map(|(&a, &b)| {
            let d = a as f64 - b as f64;
            d * d
        })
        .sum::<f64>()
        / rgb.len() as f64;
    let psnr = 10.0 * (255.0f64 * 255.0 / mse).log10();

    // Point-sampling scores ~14.2 dB on this content; box-averaging ~16.5 dB.
    // The threshold sits between them with room for codec drift on either side.
    assert!(
        psnr > 15.5,
        "chroma PSNR {psnr:.2} dB suggests the downsampler is decimating rather \
         than averaging (point-sampling scores ~14.2 dB here, averaging ~16.5)"
    );
}

/// With optimized Huffman tables the encoder must still emit ONE INTERLEAVED
/// scan, and that scan must round-trip.
///
/// Two defects hid behind this gap. The route was chosen on an internal memory
/// budget, so `-optimize_huffman` produced one scan PER COMPONENT at every
/// practical resolution — legal, but a layout mainstream encoders never emit.
/// And when that was fixed, the histogram was gathered AFTER `write_frame_header`
/// had already emitted the DHT segments, so the file declared one set of tables
/// and coded the scan with another. Our own decoder tolerated it; ffmpeg did not.
///
/// The suite missed both because the raw `Encoder` defaults `optimize_huffman`
/// OFF, so nothing exercised the path the CLI actually uses.
#[test]
fn optimized_huffman_emits_one_interleaved_scan_that_round_trips() {
    use rusty_jpeg::decode::Decoder;
    use rusty_jpeg::encode::{ColorType, Encoder, SamplingFactor};
    use std::io::Cursor;

    /// Components in the first SOS. >1 means interleaved.
    fn first_sos_components(d: &[u8]) -> usize {
        let mut i = 2;
        while i + 4 < d.len() {
            if d[i] != 0xFF {
                i += 1;
                continue;
            }
            let m = d[i + 1];
            if m == 0xD8 || m == 0xD9 || (0xD0..=0xD7).contains(&m) {
                i += 2;
                continue;
            }
            let ln = ((d[i + 2] as usize) << 8) | d[i + 3] as usize;
            if m == 0xDA {
                return d[i + 4] as usize;
            }
            i += 2 + ln;
        }
        0
    }

    // Ragged dimensions on purpose: the last MCU then needs blocks the raster
    // block grid never materialized, which is where an interleaved walk off the
    // end of the grid would panic.
    for (w, h) in [(64usize, 64usize), (127, 65), (200, 97)] {
        let mut rgb = vec![0u8; w * h * 3];
        for y in 0..h {
            for x in 0..w {
                let o = (y * w + x) * 3;
                rgb[o] = ((x * 7 + y * 3) % 256) as u8;
                rgb[o + 1] = ((x * 3 + y * 11) % 256) as u8;
                rgb[o + 2] = ((x * 13 + y * 5) % 256) as u8;
            }
        }

        let mut jpg = Vec::new();
        let mut enc = Encoder::new(&mut jpg, 90);
        enc.set_sampling_factor(SamplingFactor::R_4_2_0);
        enc.set_optimized_huffman_tables(true);
        enc.encode(&rgb, w as u16, h as u16, ColorType::Rgb)
            .expect("encode");

        assert!(
            first_sos_components(&jpg) > 1,
            "{w}x{h}: optimized Huffman emitted a NON-INTERLEAVED scan"
        );

        let out = Decoder::new(Cursor::new(&jpg))
            .decode()
            .expect("decode own optimized-Huffman output");
        assert_eq!(out.len(), w * h * 3, "{w}x{h}: wrong output size");

        // A table/scan mismatch survives a size check but destroys the image, so
        // gate on fidelity too.
        let mse: f64 = rgb
            .iter()
            .zip(&out)
            .map(|(&a, &b)| {
                let d = a as f64 - b as f64;
                d * d
            })
            .sum::<f64>()
            / rgb.len() as f64;
        let psnr = 10.0 * (255.0f64 * 255.0 / mse).log10();
        assert!(psnr > 20.0, "{w}x{h}: round-trip PSNR only {psnr:.1} dB");
    }
}

/// Trellis quantization must produce a SMALLER file that still decodes, without
/// collapsing quality.
///
/// It works by choosing where each block's EOB falls: dropping a trailing
/// coefficient can delete several symbols at once, because keeping it forces the
/// run before it to be coded too. The gate is rate AND distortion together —
/// "smaller" alone is trivially satisfiable by throwing the image away.
#[test]
fn trellis_reduces_size_without_collapsing_quality() {
    use rusty_jpeg::decode::Decoder;
    use rusty_jpeg::encode::{ColorType, Encoder, SamplingFactor};
    use std::io::Cursor;

    const W: usize = 128;
    const H: usize = 128;
    let mut rgb = vec![0u8; W * H * 3];
    for y in 0..H {
        for x in 0..W {
            let o = (y * W + x) * 3;
            let v = ((x * x + y * y) / 7 % 200) as u8;
            rgb[o] = v;
            rgb[o + 1] = v.wrapping_add(40);
            rgb[o + 2] = v.wrapping_add(90);
        }
    }

    let encode = |trellis: bool| -> (usize, f64) {
        let mut jpg = Vec::new();
        {
            let mut enc = Encoder::new(&mut jpg, 85);
            enc.set_sampling_factor(SamplingFactor::R_4_2_0);
            enc.set_trellis(trellis);
            enc.encode(&rgb, W as u16, H as u16, ColorType::Rgb)
                .expect("encode");
        }
        let out = Decoder::new(Cursor::new(&jpg)).decode().expect("decode");
        let mse: f64 = rgb
            .iter()
            .zip(&out)
            .map(|(&a, &b)| {
                let d = a as f64 - b as f64;
                d * d
            })
            .sum::<f64>()
            / rgb.len() as f64;
        (jpg.len(), 10.0 * (255.0f64 * 255.0 / mse).log10())
    };

    let (size_off, psnr_off) = encode(false);
    let (size_on, psnr_on) = encode(true);

    assert!(
        size_on < size_off,
        "trellis did not shrink the file: {size_on} vs {size_off}"
    );
    // It trades distortion for rate by design; what it must not do is fall off a
    // cliff. The BD-rate sweep that chose lambda showed well under 1 dB here.
    assert!(
        psnr_on > psnr_off - 1.5,
        "trellis cost too much quality: {psnr_on:.2} dB vs {psnr_off:.2} dB \
         (sizes {size_on} vs {size_off})"
    );
}
