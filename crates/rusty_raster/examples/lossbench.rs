//! Deterministic instrument for the rows where rusty_raster trailed tiny-skia
//! 0.12 in a downstream run (patterns, two-point conical and sweep gradients,
//! `Mask::fill_path`), with each scene rendered by both crates.
//!
//! The scene code is written once and expanded twice by a macro, against
//! `rusty_raster` and against `tiny_skia`, so the two arms run the same calls
//! on the same inputs. Every workload prints a work-parity anchor (byte count,
//! changed-byte count, FNV-1a of the output); `ours` and `ts.` anchors of one
//! scene match when the two crates agree to the byte. The measured work is in
//! the `#[inline(never)] hot_*` functions:
//!
//! ```text
//! valgrind --tool=callgrind --toggle-collect='*hot_*' \
//!     target/release/examples/lossbench patt_bilinear
//! valgrind --tool=callgrind --toggle-collect='*hot_*' \
//!     target/release/examples/lossbench ts.patt_bilinear
//! ```
//!
//! `lossbench list`, `lossbench all`, `lossbench time <workload> <rounds>`
//! (best internal duration of the hot section; pin the process to one core).

#[global_allocator]
static GLOBAL_ALLOC: rusty_alloc_api::RustyAlloc = rusty_alloc_api::RustyAlloc;

use std::time::Duration;

/// Output bytes and the duration of the hot section.
type Out = (Vec<u8>, Duration);
type Workload = (&'static str, fn() -> Out);

const W: u32 = 1024;
const H: u32 = 768;

struct Rng(u64);

impl Rng {
    fn new(seed: u64) -> Self {
        Rng(seed.wrapping_mul(0x9E37_79B9_7F4A_7C15) | 1)
    }
    fn next(&mut self) -> u64 {
        let mut x = self.0;
        x ^= x << 13;
        x ^= x >> 7;
        x ^= x << 17;
        self.0 = x;
        x
    }
    fn f(&mut self, lo: f32, hi: f32) -> f32 {
        let u = (self.next() >> 40) as f32 / (1u64 << 24) as f32;
        lo + (hi - lo) * u
    }
    fn u8(&mut self) -> u8 {
        (self.next() >> 32) as u8
    }
}

macro_rules! scenes {
    ($arm:ident, $krate:ident) => {
        mod $arm {
            use super::{Out, Rng, H, W};
            use std::time::Instant;
            use $krate::*;

            fn canvas() -> Pixmap {
                let mut pm = Pixmap::new(W, H).unwrap();
                pm.fill(Color::from_rgba8(30, 60, 90, 255));
                pm
            }

            fn timed(mut pm: Pixmap, f: impl FnOnce(&mut Pixmap)) -> Out {
                let t = Instant::now();
                f(&mut pm);
                let d = t.elapsed();
                (pm.data().to_vec(), d)
            }

            /// A premultiplied checker with gradients and partial alpha.
            fn source_image(w: u32, h: u32) -> Pixmap {
                let mut v = Vec::with_capacity((w * h * 4) as usize);
                for y in 0..h {
                    for x in 0..w {
                        let checker = ((x / 8) + (y / 8)) % 2 == 0;
                        let a = if checker { 255u32 } else { 60 + (x * 5) % 190 };
                        let r = (x * 255 / (w - 1)) * a / 255;
                        let g = (y * 255 / (h - 1)) * a / 255;
                        let b = if checker { a } else { a / 3 };
                        v.extend_from_slice(&[r as u8, g as u8, b as u8, a as u8]);
                    }
                }
                Pixmap::from_vec(v, IntSize::from_wh(w, h).unwrap()).unwrap()
            }

            fn stops(n: usize, seed: u64) -> Vec<GradientStop> {
                let mut rng = Rng::new(seed);
                (0..n)
                    .map(|i| {
                        let alpha = if n > 2 && i % 2 == 1 { 160 } else { 255 };
                        let c = Color::from_rgba8(rng.u8(), rng.u8(), rng.u8(), alpha);
                        GradientStop::new(i as f32 / (n - 1) as f32, c)
                    })
                    .collect()
            }

            #[inline(never)]
            fn hot_fill_rect(pm: &mut Pixmap, rect: Rect, paint: &Paint) {
                pm.fill_rect(rect, paint, Transform::identity(), None);
            }

            #[inline(never)]
            fn hot_fill_path(pm: &mut Pixmap, path: &Path, paint: &Paint) {
                pm.fill_path(path, paint, FillRule::Winding, Transform::identity(), None);
            }

            #[inline(never)]
            fn hot_mask_fills(mask: &mut Mask, paths: &[(Path, FillRule, bool)]) {
                for (path, rule, aa) in paths {
                    mask.fill_path(path, *rule, *aa, Transform::identity());
                }
            }

            /// The whole canvas, non-anti-aliased: interior spans only.
            fn shade_rect(shader: Shader) -> Out {
                let paint = Paint {
                    shader,
                    anti_alias: false,
                    ..Paint::default()
                };
                let rect = Rect::from_xywh(0.0, 0.0, W as f32, H as f32).unwrap();
                timed(canvas(), |pm| hot_fill_rect(pm, rect, &paint))
            }

            /// An anti-aliased disc: interior spans plus coverage runs.
            fn shade_disc(shader: Shader) -> Out {
                let paint = Paint {
                    shader,
                    anti_alias: true,
                    ..Paint::default()
                };
                let path = PathBuilder::from_circle(500.0, 390.0, 360.0).unwrap();
                timed(canvas(), |pm| hot_fill_path(pm, &path, &paint))
            }

            fn pattern(
                src: &Pixmap,
                spread: SpreadMode,
                q: FilterQuality,
                opacity: f32,
                ts: Transform,
            ) -> Shader<'_> {
                Pattern::new(src.as_ref(), spread, q, opacity, ts)
            }

            const TS_A: (f32, f32, f32, f32, f32, f32) = (1.3, 0.2, -0.2, 1.3, 5.0, 9.0);
            const TS_B: (f32, f32, f32, f32, f32, f32) = (2.1, 0.3, -0.4, 2.3, 60.0, 10.0);

            fn ts(t: (f32, f32, f32, f32, f32, f32)) -> Transform {
                Transform::from_row(t.0, t.1, t.2, t.3, t.4, t.5)
            }

            /// Pattern A: small tile, Repeat, rotated, opaque paint, whole canvas.
            fn pattern_a(q: FilterQuality) -> Out {
                let src = source_image(64, 48);
                shade_rect(pattern(&src, SpreadMode::Repeat, q, 1.0, ts(TS_A)))
            }

            /// Pattern B: larger image, `spread`, sheared, opacity 0.8, AA disc.
            fn pattern_b(q: FilterQuality, spread: SpreadMode) -> Out {
                let src = source_image(160, 120);
                shade_disc(pattern(&src, spread, q, 0.8, ts(TS_B)))
            }

            pub fn patt_nearest() -> Out {
                pattern_a(FilterQuality::Nearest)
            }
            pub fn patt_nearest_b() -> Out {
                pattern_b(FilterQuality::Nearest, SpreadMode::Reflect)
            }
            pub fn patt_bilinear() -> Out {
                pattern_a(FilterQuality::Bilinear)
            }
            pub fn patt_bilinear_b() -> Out {
                pattern_b(FilterQuality::Bilinear, SpreadMode::Reflect)
            }
            pub fn patt_bicubic() -> Out {
                pattern_a(FilterQuality::Bicubic)
            }
            pub fn patt_bicubic_b() -> Out {
                pattern_b(FilterQuality::Bicubic, SpreadMode::Pad)
            }

            /// Two-point conical, focal point inside the end circle
            /// (well-behaved), Pad, whole canvas.
            pub fn conical() -> Out {
                let sh = RadialGradient::new(
                    Point::from_xy(300.0, 330.0),
                    30.0,
                    Point::from_xy(560.0, 420.0),
                    420.0,
                    stops(4, 11),
                    SpreadMode::Pad,
                    Transform::identity(),
                )
                .unwrap();
                shade_rect(sh)
            }

            /// Two-point conical, focal point outside the end circle (the
            /// masked "greater" kind), Reflect, AA disc.
            pub fn conical_b() -> Out {
                let sh = RadialGradient::new(
                    Point::from_xy(200.0, 380.0),
                    10.0,
                    Point::from_xy(600.0, 400.0),
                    150.0,
                    stops(3, 12),
                    SpreadMode::Reflect,
                    Transform::identity(),
                )
                .unwrap();
                shade_disc(sh)
            }

            /// Full turn, 5 stops, Pad, whole canvas.
            pub fn sweep() -> Out {
                let sh = SweepGradient::new(
                    Point::from_xy(512.0, 384.0),
                    0.0,
                    360.0,
                    stops(5, 13),
                    SpreadMode::Pad,
                    Transform::identity(),
                )
                .unwrap();
                shade_rect(sh)
            }

            /// Partial turn, 3 stops, Repeat, rotated, AA disc.
            pub fn sweep_b() -> Out {
                let sh = SweepGradient::new(
                    Point::from_xy(400.0, 300.0),
                    30.0,
                    290.0,
                    stops(3, 14),
                    SpreadMode::Repeat,
                    Transform::from_row(0.956, 0.292, -0.292, 0.956, 120.0, -90.0),
                )
                .unwrap();
                shade_disc(sh)
            }

            fn blob(rng: &mut Rng, size: f32) -> Path {
                let (x, y) = (rng.f(30.0, W as f32 - 30.0), rng.f(30.0, H as f32 - 30.0));
                let mut pb = PathBuilder::new();
                pb.move_to(x, y);
                for _ in 0..4 {
                    pb.cubic_to(
                        x + rng.f(-size, size),
                        y + rng.f(-size, size),
                        x + rng.f(-size, size),
                        y + rng.f(-size, size),
                        x + rng.f(-size, size),
                        y + rng.f(-size, size),
                    );
                }
                pb.close();
                pb.finish().unwrap()
            }

            fn mask_out(mask: &Mask, d: std::time::Duration) -> Out {
                (mask.data().to_vec(), d)
            }

            /// Shape-layer coverage: 524 separate cubic blobs, alternating
            /// winding / even-odd, one in five aliased.
            pub fn mask_blobs() -> Out {
                let mut rng = Rng::new(524);
                let paths: Vec<(Path, FillRule, bool)> = (0..524)
                    .map(|i| {
                        let size = rng.f(12.0, 70.0);
                        let rule = if i % 2 == 0 {
                            FillRule::Winding
                        } else {
                            FillRule::EvenOdd
                        };
                        (blob(&mut rng, size), rule, i % 5 != 4)
                    })
                    .collect();
                let mut mask = Mask::new(W, H).unwrap();
                let t = Instant::now();
                hot_mask_fills(&mut mask, &paths);
                mask_out(&mask, t.elapsed())
            }

            /// A few large self-overlapping shapes in one even-odd path,
            /// anti-aliased: long rows, many crossings per row.
            pub fn mask_big() -> Out {
                let mut rng = Rng::new(60);
                let mut pb = PathBuilder::new();
                for _ in 0..60 {
                    let (x, y) = (rng.f(60.0, W as f32 - 60.0), rng.f(60.0, H as f32 - 60.0));
                    pb.move_to(x, y);
                    for _ in 0..4 {
                        pb.cubic_to(
                            x + rng.f(-220.0, 220.0),
                            y + rng.f(-220.0, 220.0),
                            x + rng.f(-220.0, 220.0),
                            y + rng.f(-220.0, 220.0),
                            x + rng.f(-220.0, 220.0),
                            y + rng.f(-220.0, 220.0),
                        );
                    }
                    pb.close();
                }
                let paths = vec![(pb.finish().unwrap(), FillRule::EvenOdd, true)];
                let mut mask = Mask::new(W, H).unwrap();
                let t = Instant::now();
                hot_mask_fills(&mut mask, &paths);
                mask_out(&mask, t.elapsed())
            }
        }
    };
}

scenes!(ours, rusty_raster);
scenes!(theirs, tiny_skia);

macro_rules! workloads {
    ($($name:ident),* $(,)?) => {
        const WORKLOADS: &[Workload] = &[
            $((stringify!($name), ours::$name),)*
        ];
        const TS_WORKLOADS: &[Workload] = &[
            $((concat!("ts.", stringify!($name)), theirs::$name),)*
        ];
    };
}

workloads!(
    patt_nearest,
    patt_nearest_b,
    patt_bilinear,
    patt_bilinear_b,
    patt_bicubic,
    patt_bicubic_b,
    conical,
    conical_b,
    sweep,
    sweep_b,
    mask_blobs,
    mask_big,
);

fn anchor(name: &str, bytes: &[u8]) {
    let mut h = 0xcbf2_9ce4_8422_2325u64;
    let mut nonzero = 0u32;
    for &b in bytes {
        nonzero += u32::from(b != 0);
        h = (h ^ u64::from(b)).wrapping_mul(0x0000_0100_0000_01b3);
    }
    println!(
        "{name} bytes {} nonzero {nonzero} fnv {h:016x}",
        bytes.len()
    );
}

fn find(name: &str) -> fn() -> Out {
    match WORKLOADS
        .iter()
        .chain(TS_WORKLOADS)
        .find(|(n, _)| *n == name)
    {
        Some((_, f)) => *f,
        None => panic!("unknown workload {name}; try `lossbench list`"),
    }
}

fn main() {
    let args: Vec<String> = std::env::args().collect();
    let cmd = args.get(1).map(String::as_str).unwrap_or("all");
    match cmd {
        "list" => {
            for (n, _) in WORKLOADS {
                println!("{n}");
            }
        }
        "all" => {
            for (n, f) in WORKLOADS {
                anchor(n, &f().0);
            }
        }
        "all-ts" => {
            for (n, f) in TS_WORKLOADS {
                anchor(n, &f().0);
            }
        }
        "time" => {
            let name = args.get(2).map(String::as_str).unwrap_or("patt_bilinear");
            let rounds: u32 = args.get(3).and_then(|v| v.parse().ok()).unwrap_or(15);
            let f = find(name);
            let mut best = Duration::MAX;
            for _ in 0..rounds {
                best = best.min(f().1);
            }
            println!("{name} best_ns {}", best.as_nanos());
        }
        name => anchor(name, &find(name)().0),
    }
}
