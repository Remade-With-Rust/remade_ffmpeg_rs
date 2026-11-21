//! Deterministic instruction-count instrument for the rasteriser.
//!
//! Every workload renders a fixed scene and prints a work-parity anchor
//! (pixel count, changed pixel count, FNV-1a of the RGBA bytes), so a change
//! that alters a single output byte is visible next to its number. The work
//! being measured lives in `#[inline(never)] hot_*` functions, which is what
//! callgrind counts:
//!
//! ```text
//! valgrind --tool=callgrind --toggle-collect='*hot_*' \
//!     target/release/examples/veinbench <workload>
//! ```
//!
//! `veinbench list` prints the workload names, `veinbench all` prints every
//! anchor, and `veinbench time <workload> <rounds>` prints the best internal
//! duration of the hot section (pin the process to one core).

#[global_allocator]
static GLOBAL_ALLOC: rusty_alloc_api::RustyAlloc = rusty_alloc_api::RustyAlloc;

use rusty_raster::*;
use std::time::{Duration, Instant};

const W: u32 = 512;
const H: u32 = 512;

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
    fn u(&mut self, n: u32) -> u32 {
        (self.next() % u64::from(n)) as u32
    }
    fn color(&mut self, alpha: Option<u8>) -> Color {
        let (r, g, b, a) = (
            self.u(256) as u8,
            self.u(256) as u8,
            self.u(256) as u8,
            self.u(256) as u8,
        );
        Color::from_rgba8(r, g, b, alpha.unwrap_or(a))
    }
}

fn canvas() -> Pixmap {
    let mut pm = Pixmap::new(W, H).unwrap();
    pm.fill(Color::from_rgba8(30, 60, 90, 255));
    pm
}

/// A canvas with varied, partly transparent content, for blend workloads.
fn busy_canvas() -> Pixmap {
    let mut pm = Pixmap::new(W, H).unwrap();
    let mut rng = Rng::new(77);
    for _ in 0..40 {
        let mut paint = Paint::default();
        paint.set_color(rng.color(None));
        let r = Rect::from_xywh(
            rng.f(-40.0, 400.0),
            rng.f(-40.0, 400.0),
            rng.f(30.0, 260.0),
            rng.f(30.0, 260.0),
        )
        .unwrap();
        pm.fill_rect(r, &paint, Transform::identity(), None);
    }
    pm
}

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

fn solid(c: Color, aa: bool) -> Paint<'static> {
    let mut p = Paint::default();
    p.set_color(c);
    p.anti_alias = aa;
    p
}

fn star(cx: f32, cy: f32, r0: f32, r1: f32, n: u32) -> Path {
    let mut pb = PathBuilder::new();
    for i in 0..n {
        // Rational points on the circle: no libm, so the scene is the same on
        // every platform.
        let t = (i as f32 / n as f32) * 4.0 - 2.0;
        let (c, s) = if t.abs() <= 1.0 {
            ((1.0 - t * t) / (1.0 + t * t), 2.0 * t / (1.0 + t * t))
        } else {
            let u = if t > 0.0 { 2.0 - t } else { -2.0 - t };
            (-(1.0 - u * u) / (1.0 + u * u), 2.0 * u / (1.0 + u * u))
        };
        let r = if i % 2 == 0 { r0 } else { r1 };
        let (x, y) = (cx + r * c, cy + r * s);
        if i == 0 {
            pb.move_to(x, y);
        } else {
            pb.line_to(x, y);
        }
    }
    pb.close();
    pb.finish().unwrap()
}

fn blobs(seed: u64, n: u32, size: f32) -> Path {
    let mut rng = Rng::new(seed);
    let mut pb = PathBuilder::new();
    for _ in 0..n {
        let (x, y) = (rng.f(20.0, 490.0), rng.f(20.0, 490.0));
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
    }
    pb.finish().unwrap()
}

fn polyline(seed: u64, n: u32) -> Path {
    let mut rng = Rng::new(seed);
    let mut pb = PathBuilder::new();
    pb.move_to(rng.f(0.0, 512.0), rng.f(0.0, 512.0));
    for i in 0..n {
        if i % 3 == 2 {
            pb.quad_to(
                rng.f(0.0, 512.0),
                rng.f(0.0, 512.0),
                rng.f(0.0, 512.0),
                rng.f(0.0, 512.0),
            );
        } else {
            pb.line_to(rng.f(0.0, 512.0), rng.f(0.0, 512.0));
        }
    }
    pb.finish().unwrap()
}

fn stops(n: usize) -> Vec<GradientStop> {
    let mut rng = Rng::new(n as u64 + 5);
    (0..n)
        .map(|i| {
            let alpha = if n > 2 && i % 2 == 1 { 160 } else { 255 };
            GradientStop::new(i as f32 / (n - 1) as f32, rng.color(Some(alpha)))
        })
        .collect()
}

// ---------------------------------------------------------------------------
// Hot sections. One per workload so callgrind's toggle isolates each.

#[inline(never)]
fn hot_fill_path(pm: &mut Pixmap, path: &Path, paint: &Paint, rule: FillRule, ts: Transform) {
    pm.fill_path(path, paint, rule, ts, None);
}

#[inline(never)]
fn hot_fill_path_mask(pm: &mut Pixmap, path: &Path, paint: &Paint, mask: &Mask) {
    pm.fill_path(
        path,
        paint,
        FillRule::Winding,
        Transform::identity(),
        Some(mask),
    );
}

#[inline(never)]
fn hot_fill_rect(pm: &mut Pixmap, rect: Rect, paint: &Paint, ts: Transform) {
    pm.fill_rect(rect, paint, ts, None);
}

#[inline(never)]
fn hot_stroke(pm: &mut Pixmap, path: &Path, paint: &Paint, stroke: &Stroke, ts: Transform) {
    pm.stroke_path(path, paint, stroke, ts, None);
}

#[inline(never)]
fn hot_draw_pixmap(pm: &mut Pixmap, src: &Pixmap, pp: &PixmapPaint, x: i32, y: i32, ts: Transform) {
    pm.draw_pixmap(x, y, src.as_ref(), pp, ts, None);
}

#[inline(never)]
fn hot_mask_fill(mask: &mut Mask, path: &Path, aa: bool) {
    mask.fill_path(path, FillRule::Winding, aa, Transform::identity());
}

#[inline(never)]
fn hot_apply_mask(pm: &mut Pixmap, mask: &Mask) {
    pm.apply_mask(mask);
}

#[inline(never)]
fn hot_mask_from_pixmap(pm: &Pixmap, ty: MaskType) -> Mask {
    Mask::from_pixmap(pm.as_ref(), ty)
}

#[inline(never)]
fn hot_many_small(pm: &mut Pixmap, n: u32) {
    let mut rng = Rng::new(4242);
    for i in 0..n {
        let (x, y) = (rng.f(0.0, 500.0), rng.f(0.0, 500.0));
        let s = rng.f(3.0, 14.0);
        let paint = solid(rng.color(None), true);
        let mut pb = PathBuilder::new();
        match i % 4 {
            0 => pb.push_circle(x, y, s * 0.5),
            1 => pb.push_rect(Rect::from_xywh(x, y, s, s * 0.7).unwrap()),
            2 => {
                pb.move_to(x, y);
                pb.line_to(x + s, y + s * 0.3);
                pb.line_to(x + s * 0.2, y + s);
                pb.close();
            }
            _ => {
                pb.move_to(x, y);
                pb.quad_to(x + s, y, x + s, y + s);
                pb.cubic_to(x + s * 0.5, y + s * 1.2, x - s * 0.3, y + s, x, y);
                pb.close();
            }
        }
        if let Some(path) = pb.finish() {
            pm.fill_path(
                &path,
                &paint,
                FillRule::Winding,
                Transform::identity(),
                None,
            );
        }
    }
}

#[inline(never)]
fn hot_path_build(n: u32) -> usize {
    let mut rng = Rng::new(99);
    let mut total = 0;
    for _ in 0..n {
        let mut pb = PathBuilder::new();
        let (x, y) = (rng.f(0.0, 500.0), rng.f(0.0, 500.0));
        pb.move_to(x, y);
        for _ in 0..6 {
            pb.line_to(rng.f(0.0, 500.0), rng.f(0.0, 500.0));
            pb.quad_to(
                rng.f(0.0, 500.0),
                rng.f(0.0, 500.0),
                rng.f(0.0, 500.0),
                rng.f(0.0, 500.0),
            );
            pb.cubic_to(
                rng.f(0.0, 500.0),
                rng.f(0.0, 500.0),
                rng.f(0.0, 500.0),
                rng.f(0.0, 500.0),
                rng.f(0.0, 500.0),
                rng.f(0.0, 500.0),
            );
        }
        pb.close();
        pb.push_circle(x, y, 20.0);
        if let Some(p) = pb.finish() {
            let p = p
                .transform(Transform::from_row(0.9, 0.1, -0.1, 0.9, 3.0, 4.0))
                .unwrap();
            total += p.len() + p.bounds().width() as usize;
        }
    }
    total
}

// ---------------------------------------------------------------------------
// Workloads

type Out = (Pixmap, Duration);
type Workload = (&'static str, fn() -> Out);

fn timed(mut pm: Pixmap, f: impl FnOnce(&mut Pixmap)) -> Out {
    let t = Instant::now();
    f(&mut pm);
    (pm, t.elapsed())
}

fn wl_fill_rect_aa() -> Out {
    let paint = solid(Color::from_rgba8(200, 40, 40, 255), true);
    let rect = Rect::from_xywh(10.3, 12.7, 480.5, 470.2).unwrap();
    timed(canvas(), |pm| {
        for _ in 0..12 {
            hot_fill_rect(pm, rect, &paint, Transform::identity());
        }
    })
}

fn wl_fill_rect_alpha() -> Out {
    let paint = solid(Color::from_rgba8(200, 40, 40, 150), false);
    let rect = Rect::from_xywh(10.0, 12.0, 480.0, 470.0).unwrap();
    timed(canvas(), |pm| {
        for _ in 0..8 {
            hot_fill_rect(pm, rect, &paint, Transform::identity());
        }
    })
}

fn wl_fill_rect_rot() -> Out {
    let paint = solid(Color::from_rgba8(20, 140, 240, 200), true);
    let rect = Rect::from_xywh(120.0, 80.0, 300.0, 280.0).unwrap();
    // A fixed rotation written out, so no libm is involved in the setup.
    let ts = Transform::from_row(0.956, 0.292, -0.292, 0.956, 86.0, -63.5);
    timed(canvas(), |pm| {
        for _ in 0..8 {
            hot_fill_rect(pm, rect, &paint, ts);
        }
    })
}

fn wl_fill_circle_aa() -> Out {
    let paint = solid(Color::from_rgba8(40, 200, 90, 170), true);
    let path = PathBuilder::from_circle(256.0, 256.0, 240.0).unwrap();
    timed(canvas(), |pm| {
        for _ in 0..8 {
            hot_fill_path(pm, &path, &paint, FillRule::Winding, Transform::identity());
        }
    })
}

fn wl_fill_star_aa() -> Out {
    let paint = solid(Color::from_rgba8(240, 200, 30, 255), true);
    let path = star(256.0, 256.0, 250.0, 90.0, 1200);
    timed(canvas(), |pm| {
        for _ in 0..2 {
            hot_fill_path(pm, &path, &paint, FillRule::EvenOdd, Transform::identity());
        }
    })
}

fn wl_fill_blobs_aa() -> Out {
    let paint = solid(Color::from_rgba8(180, 60, 220, 200), true);
    let path = blobs(11, 60, 110.0);
    timed(canvas(), |pm| {
        hot_fill_path(pm, &path, &paint, FillRule::Winding, Transform::identity());
    })
}

fn wl_fill_blobs_noaa() -> Out {
    let paint = solid(Color::from_rgba8(180, 60, 220, 200), false);
    let path = blobs(11, 60, 110.0);
    timed(canvas(), |pm| {
        for _ in 0..3 {
            hot_fill_path(pm, &path, &paint, FillRule::EvenOdd, Transform::identity());
        }
    })
}

fn wl_hairline(aa: bool) -> Out {
    let paint = solid(Color::from_rgba8(250, 250, 250, 220), aa);
    let path = polyline(3, 400);
    let stroke = Stroke {
        width: 0.0,
        ..Stroke::default()
    };
    timed(canvas(), |pm| {
        hot_stroke(pm, &path, &paint, &stroke, Transform::identity());
    })
}

fn wl_hairline_aa() -> Out {
    wl_hairline(true)
}

fn wl_hairline_noaa() -> Out {
    wl_hairline(false)
}

fn wl_stroke_wide() -> Out {
    let paint = solid(Color::from_rgba8(250, 120, 20, 230), true);
    let path = blobs(21, 24, 140.0);
    let stroke = Stroke {
        width: 9.0,
        line_cap: LineCap::Round,
        line_join: LineJoin::Round,
        ..Stroke::default()
    };
    timed(canvas(), |pm| {
        hot_stroke(pm, &path, &paint, &stroke, Transform::identity());
    })
}

fn wl_stroke_miter() -> Out {
    let paint = solid(Color::from_rgba8(20, 220, 220, 255), true);
    let path = polyline(8, 160);
    let stroke = Stroke {
        width: 5.0,
        line_cap: LineCap::Square,
        line_join: LineJoin::Miter,
        ..Stroke::default()
    };
    timed(canvas(), |pm| {
        hot_stroke(pm, &path, &paint, &stroke, Transform::identity());
    })
}

fn wl_stroke_dash() -> Out {
    let paint = solid(Color::from_rgba8(255, 255, 255, 255), true);
    let path = blobs(31, 16, 150.0);
    let stroke = Stroke {
        width: 4.0,
        line_cap: LineCap::Butt,
        line_join: LineJoin::Bevel,
        dash: StrokeDash::new(vec![11.0, 5.0, 3.0, 5.0], 2.0),
        ..Stroke::default()
    };
    timed(canvas(), |pm| {
        hot_stroke(pm, &path, &paint, &stroke, Transform::identity());
    })
}

fn gradient_fill(shader: Shader<'static>, reps: u32, hq: bool) -> Out {
    let paint = Paint {
        shader,
        anti_alias: false,
        force_hq_pipeline: hq,
        ..Paint::default()
    };
    let rect = Rect::from_xywh(6.0, 6.0, 500.0, 500.0).unwrap();
    timed(canvas(), |pm| {
        for _ in 0..reps {
            hot_fill_rect(pm, rect, &paint, Transform::identity());
        }
    })
}

fn linear2() -> Shader<'static> {
    LinearGradient::new(
        Point::from_xy(20.0, 30.0),
        Point::from_xy(480.0, 420.0),
        stops(2),
        SpreadMode::Pad,
        Transform::identity(),
    )
    .unwrap()
}

fn wl_grad_linear2() -> Out {
    gradient_fill(linear2(), 4, false)
}

fn wl_grad_linear5() -> Out {
    let sh = LinearGradient::new(
        Point::from_xy(100.0, 130.0),
        Point::from_xy(300.0, 260.0),
        stops(5),
        SpreadMode::Reflect,
        Transform::identity(),
    )
    .unwrap();
    gradient_fill(sh, 3, false)
}

fn wl_grad_linear2_hq() -> Out {
    gradient_fill(linear2(), 3, true)
}

fn wl_grad_radial2() -> Out {
    let sh = RadialGradient::new(
        Point::from_xy(256.0, 256.0),
        0.0,
        Point::from_xy(256.0, 256.0),
        230.0,
        stops(2),
        SpreadMode::Pad,
        Transform::identity(),
    )
    .unwrap();
    gradient_fill(sh, 3, false)
}

fn wl_grad_radial6() -> Out {
    let sh = RadialGradient::new(
        Point::from_xy(200.0, 220.0),
        0.0,
        Point::from_xy(200.0, 220.0),
        120.0,
        stops(6),
        SpreadMode::Repeat,
        Transform::identity(),
    )
    .unwrap();
    gradient_fill(sh, 3, false)
}

fn wl_grad_conical() -> Out {
    let sh = RadialGradient::new(
        Point::from_xy(180.0, 200.0),
        20.0,
        Point::from_xy(300.0, 290.0),
        210.0,
        stops(4),
        SpreadMode::Pad,
        Transform::identity(),
    )
    .unwrap();
    gradient_fill(sh, 2, false)
}

fn wl_grad_sweep() -> Out {
    let sh = SweepGradient::new(
        Point::from_xy(256.0, 256.0),
        0.0,
        360.0,
        stops(5),
        SpreadMode::Pad,
        Transform::identity(),
    )
    .unwrap();
    gradient_fill(sh, 2, false)
}

fn blend_fill(modes: &[BlendMode], aa: bool) -> Out {
    let path = PathBuilder::from_circle(256.0, 256.0, 250.0).unwrap();
    let mut rng = Rng::new(5);
    let paints: Vec<Paint> = modes
        .iter()
        .map(|&m| {
            let mut p = solid(rng.color(Some(170)), aa);
            p.blend_mode = m;
            p
        })
        .collect();
    timed(busy_canvas(), |pm| {
        for p in &paints {
            hot_fill_path(pm, &path, p, FillRule::Winding, Transform::identity());
        }
    })
}

fn wl_blend_lowp() -> Out {
    use BlendMode::*;
    blend_fill(
        &[
            Multiply,
            Screen,
            Overlay,
            Darken,
            Lighten,
            HardLight,
            Difference,
            Exclusion,
            Xor,
            Plus,
            Modulate,
            DestinationOut,
            SourceAtop,
        ],
        false,
    )
}

fn wl_blend_highp() -> Out {
    use BlendMode::*;
    blend_fill(
        &[
            ColorDodge, ColorBurn, SoftLight, Hue, Saturation, Color, Luminosity,
        ],
        false,
    )
}

fn wl_srcover_hq() -> Out {
    let mut paint = solid(Color::from_rgba8(200, 40, 40, 150), true);
    paint.force_hq_pipeline = true;
    let path = PathBuilder::from_circle(256.0, 256.0, 250.0).unwrap();
    timed(busy_canvas(), |pm| {
        for _ in 0..4 {
            hot_fill_path(pm, &path, &paint, FillRule::Winding, Transform::identity());
        }
    })
}

fn wl_gamma_srgb() -> Out {
    let mut paint = solid(Color::from_rgba8(200, 140, 40, 150), false);
    paint.colorspace = ColorSpace::FullSRGBGamma;
    let rect = Rect::from_xywh(40.0, 40.0, 400.0, 400.0).unwrap();
    timed(busy_canvas(), |pm| {
        hot_fill_rect(pm, rect, &paint, Transform::identity());
    })
}

fn wl_mask_fill() -> Out {
    let mut mask = Mask::new(W, H).unwrap();
    mask.fill_path(
        &star(256.0, 256.0, 250.0, 120.0, 24),
        FillRule::Winding,
        true,
        Transform::identity(),
    );
    let paint = solid(Color::from_rgba8(240, 240, 60, 200), true);
    let path = PathBuilder::from_circle(256.0, 256.0, 230.0).unwrap();
    timed(canvas(), |pm| {
        for _ in 0..6 {
            hot_fill_path_mask(pm, &path, &paint, &mask);
        }
    })
}

fn wl_mask_build() -> Out {
    let path = blobs(41, 40, 120.0);
    let mut mask = Mask::new(W, H).unwrap();
    let t = Instant::now();
    hot_mask_fill(&mut mask, &path, true);
    let d = t.elapsed();
    // Report the mask through an RGBA pixmap so the anchor code is shared.
    let mut pm = Pixmap::new(W, H).unwrap();
    for (px, m) in pm.data_mut().chunks_exact_mut(4).zip(mask.data()) {
        px.copy_from_slice(&[*m, *m, *m, *m]);
    }
    (pm, d)
}

fn pixmap_draw(quality: FilterQuality, ts: Transform, reps: u32, opacity: f32) -> Out {
    let src = source_image(160, 120);
    let pp = PixmapPaint {
        opacity,
        blend_mode: BlendMode::SourceOver,
        quality,
    };
    timed(canvas(), |pm| {
        for i in 0..reps {
            hot_draw_pixmap(pm, &src, &pp, 20 + i as i32 * 7, 30 + i as i32 * 5, ts);
        }
    })
}

fn wl_pixmap_nearest() -> Out {
    pixmap_draw(FilterQuality::Nearest, Transform::identity(), 40, 1.0)
}

fn wl_pixmap_nearest_ts() -> Out {
    pixmap_draw(
        FilterQuality::Nearest,
        Transform::from_row(2.1, 0.3, -0.4, 2.3, 60.0, 10.0),
        3,
        0.8,
    )
}

fn wl_pixmap_bilinear() -> Out {
    pixmap_draw(
        FilterQuality::Bilinear,
        Transform::from_row(2.1, 0.3, -0.4, 2.3, 60.0, 10.0),
        2,
        1.0,
    )
}

fn wl_pixmap_bicubic() -> Out {
    pixmap_draw(
        FilterQuality::Bicubic,
        Transform::from_row(2.1, 0.3, -0.4, 2.3, 60.0, 10.0),
        1,
        1.0,
    )
}

fn wl_pattern_repeat() -> Out {
    let src = source_image(64, 48);
    let paint = Paint {
        shader: Pattern::new(
            src.as_ref(),
            SpreadMode::Repeat,
            FilterQuality::Bilinear,
            0.9,
            Transform::from_row(1.3, 0.2, -0.2, 1.3, 5.0, 9.0),
        ),
        anti_alias: true,
        ..Paint::default()
    };
    let path = PathBuilder::from_circle(256.0, 256.0, 240.0).unwrap();
    timed(canvas(), |pm| {
        hot_fill_path(pm, &path, &paint, FillRule::Winding, Transform::identity());
    })
}

/// A group layer as an SVG renderer composites it: transparent, with a few
/// opaque and translucent shapes and their anti-aliased edges.
fn layer() -> Pixmap {
    let mut pm = Pixmap::new(W, H).unwrap();
    let mut rng = Rng::new(123);
    for i in 0..14 {
        let alpha = if i % 5 == 4 { 140 } else { 255 };
        let paint = solid(rng.color(Some(alpha)), true);
        let (x, y, s) = (rng.f(20.0, 420.0), rng.f(20.0, 420.0), rng.f(20.0, 90.0));
        let path = if i % 2 == 0 {
            PathBuilder::from_circle(x, y, s * 0.5).unwrap()
        } else {
            PathBuilder::from_rect(Rect::from_xywh(x, y, s, s * 0.6).unwrap())
        };
        pm.fill_path(
            &path,
            &paint,
            FillRule::Winding,
            Transform::identity(),
            None,
        );
    }
    pm
}

fn layer_draw(opacity: f32, x: i32, y: i32, reps: u32) -> Out {
    let src = layer();
    let pp = PixmapPaint {
        opacity,
        blend_mode: BlendMode::SourceOver,
        quality: FilterQuality::Nearest,
    };
    timed(busy_canvas(), |pm| {
        for _ in 0..reps {
            hot_draw_pixmap(pm, &src, &pp, x, y, Transform::identity());
        }
    })
}

/// The layer composite at full opacity, layer and canvas the same size.
fn wl_layer_opaque() -> Out {
    layer_draw(1.0, 0, 0, 6)
}

/// The layer composite with group opacity, partly off the canvas.
fn wl_layer_alpha() -> Out {
    layer_draw(0.7, 37, -21, 4)
}

fn wl_apply_mask() -> Out {
    let mut mask = Mask::new(W, H).unwrap();
    mask.fill_path(
        &star(256.0, 256.0, 250.0, 120.0, 24),
        FillRule::Winding,
        true,
        Transform::identity(),
    );
    timed(busy_canvas(), |pm| {
        for _ in 0..8 {
            hot_apply_mask(pm, &mask);
        }
    })
}

fn wl_mask_from_pixmap() -> Out {
    let src = busy_canvas();
    let t = Instant::now();
    let mut lum = hot_mask_from_pixmap(&src, MaskType::Luminance);
    let mut alpha = hot_mask_from_pixmap(&src, MaskType::Alpha);
    for _ in 0..3 {
        lum = hot_mask_from_pixmap(&src, MaskType::Luminance);
        alpha = hot_mask_from_pixmap(&src, MaskType::Alpha);
    }
    let d = t.elapsed();
    let mut pm = Pixmap::new(W, H).unwrap();
    for ((px, l), a) in pm
        .data_mut()
        .chunks_exact_mut(4)
        .zip(lum.data())
        .zip(alpha.data())
    {
        px.copy_from_slice(&[*l, *l, *l, *a]);
    }
    (pm, d)
}

fn wl_many_small() -> Out {
    timed(canvas(), |pm| hot_many_small(pm, 6000))
}

fn wl_path_build() -> Out {
    let t = Instant::now();
    let n = hot_path_build(3000);
    let d = t.elapsed();
    let mut pm = Pixmap::new(W, H).unwrap();
    pm.data_mut()[..8].copy_from_slice(&(n as u64).to_le_bytes());
    (pm, d)
}

const WORKLOADS: &[Workload] = &[
    ("fill_rect_aa", wl_fill_rect_aa),
    ("fill_rect_alpha", wl_fill_rect_alpha),
    ("fill_rect_rot", wl_fill_rect_rot),
    ("fill_circle_aa", wl_fill_circle_aa),
    ("fill_star_aa", wl_fill_star_aa),
    ("fill_blobs_aa", wl_fill_blobs_aa),
    ("fill_blobs_noaa", wl_fill_blobs_noaa),
    ("hairline_aa", wl_hairline_aa),
    ("hairline_noaa", wl_hairline_noaa),
    ("stroke_wide", wl_stroke_wide),
    ("stroke_miter", wl_stroke_miter),
    ("stroke_dash", wl_stroke_dash),
    ("grad_linear2", wl_grad_linear2),
    ("grad_linear5", wl_grad_linear5),
    ("grad_linear2_hq", wl_grad_linear2_hq),
    ("grad_radial2", wl_grad_radial2),
    ("grad_radial6", wl_grad_radial6),
    ("grad_conical", wl_grad_conical),
    ("grad_sweep", wl_grad_sweep),
    ("blend_lowp", wl_blend_lowp),
    ("blend_highp", wl_blend_highp),
    ("srcover_hq", wl_srcover_hq),
    ("gamma_srgb", wl_gamma_srgb),
    ("mask_fill", wl_mask_fill),
    ("mask_build", wl_mask_build),
    ("pixmap_nearest", wl_pixmap_nearest),
    ("pixmap_nearest_ts", wl_pixmap_nearest_ts),
    ("pixmap_bilinear", wl_pixmap_bilinear),
    ("pixmap_bicubic", wl_pixmap_bicubic),
    ("pattern_repeat", wl_pattern_repeat),
    ("many_small", wl_many_small),
    ("path_build", wl_path_build),
    ("layer_opaque", wl_layer_opaque),
    ("layer_alpha", wl_layer_alpha),
    ("apply_mask", wl_apply_mask),
    ("mask_from_pixmap", wl_mask_from_pixmap),
];

fn anchor(name: &str, pm: &Pixmap) {
    let mut h = 0xcbf2_9ce4_8422_2325u64;
    let mut changed = 0u32;
    for px in pm.data().chunks_exact(4) {
        if px != [30, 60, 90, 255] {
            changed += 1;
        }
        for &b in px {
            h = (h ^ u64::from(b)).wrapping_mul(0x0000_0100_0000_01b3);
        }
    }
    println!(
        "{name} px {} changed {changed} fnv {h:016x}",
        pm.width() * pm.height()
    );
}

fn find(name: &str) -> fn() -> Out {
    match WORKLOADS.iter().find(|(n, _)| *n == name) {
        Some((_, f)) => *f,
        None => panic!("unknown workload {name}; try `veinbench list`"),
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
        "time" => {
            let name = args.get(2).map(String::as_str).unwrap_or("fill_circle_aa");
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
