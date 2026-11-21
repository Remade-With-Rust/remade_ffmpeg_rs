//! rusty_raster vs tiny-skia (Rust) vs Cairo (C), and rusty_svgtree vs resvg
//! (Rust) vs nanosvg (C).
//!
//!   rbench raster ROUNDS      four workloads, timed + compared
//!   rbench svg ROUNDS FILE... render each SVG with all three, timed + compared
#[global_allocator]
static GLOBAL_ALLOC: rusty_alloc_api::RustyAlloc = rusty_alloc_api::RustyAlloc;

use std::ffi::{c_char, c_float, c_int, c_uchar, c_void};
use std::time::Instant;

extern "C" {
    fn cr_new(w: c_int, h: c_int) -> *mut c_void;
    fn cr_free(p: *mut c_void);
    fn cr_clear(p: *mut c_void);
    fn cr_draw(p: *mut c_void, ops: *const c_uchar, nops: c_int, pts: *const c_float, even_odd: c_int,
        width: c_float, cap: c_int, join: c_int, miter: c_float, dash: *const c_float, ndash: c_int,
        kind: c_int, pp: *const c_float, tx: c_float, ty: c_float);
    fn cr_read(p: *mut c_void, out: *mut c_uchar);
    fn nsvg_render(svg: *const c_char, len: usize, scale: c_float, w: c_int, h: c_int, out: *mut c_uchar) -> c_int;
}

const W: u32 = 2048;
const H: u32 = 2048;

#[derive(Clone)]
enum PaintSpec {
    Solid([f32; 4]),
    Linear([f32; 4], [f32; 4], [f32; 4]),
    Radial([f32; 3], [f32; 4], [f32; 4]),
}

#[derive(Clone)]
struct StrokeSpec {
    width: f32,
    cap: i32,  // 0 butt 1 round 2 square
    join: i32, // 0 miter 1 round 2 bevel
    dash: Vec<f32>,
}

#[derive(Clone)]
struct Shape {
    ops: Vec<u8>,
    pts: Vec<f32>,
    even_odd: bool,
    stroke: Option<StrokeSpec>,
    paint: PaintSpec,
}

struct Rng(u64);
impl Rng {
    fn f(&mut self) -> f32 {
        self.0 ^= self.0 << 13;
        self.0 ^= self.0 >> 7;
        self.0 ^= self.0 << 17;
        (self.0 >> 40) as f32 / (1u64 << 24) as f32
    }
    fn color(&mut self) -> [f32; 4] {
        [self.f(), self.f(), self.f(), 0.3 + 0.7 * self.f()]
    }
}

fn workloads() -> Vec<(&'static str, Vec<Shape>)> {
    let mut r = Rng(0x1234_5678_9abc);
    let (w, h) = (W as f32, H as f32);
    // 1. many small anti-aliased paths: triangles and circles (as cubics).
    let mut small = Vec::new();
    for i in 0..20_000 {
        let (cx, cy, s) = (r.f() * w, r.f() * h, 4.0 + r.f() * 20.0);
        if i % 2 == 0 {
            small.push(Shape {
                ops: vec![0, 1, 1, 4],
                pts: vec![cx, cy - s, cx + s, cy + s, cx - s, cy + s * 0.7],
                even_odd: false,
                stroke: None,
                paint: PaintSpec::Solid(r.color()),
            });
        } else {
            let k = 0.552_284_8 * s;
            small.push(Shape {
                ops: vec![0, 3, 3, 3, 3, 4],
                pts: vec![
                    cx + s, cy, cx + s, cy + k, cx + k, cy + s, cx, cy + s, cx - k, cy + s, cx - s, cy + k,
                    cx - s, cy, cx - s, cy - k, cx - k, cy - s, cx, cy - s, cx + k, cy - s, cx + s, cy - k,
                    cx + s, cy,
                ],
                even_odd: false,
                stroke: None,
                paint: PaintSpec::Solid(r.color()),
            });
        }
    }
    // 2. one large complex path: a 6000-vertex self-intersecting star, even-odd.
    let n = 6000;
    let mut ops = vec![0u8];
    let mut pts = Vec::new();
    for i in 0..n {
        let a = i as f32 * std::f32::consts::TAU * 2.003 / n as f32 * 97.0;
        let rad = if i % 2 == 0 { 0.48 } else { 0.18 + 0.2 * r.f() } * w;
        pts.extend_from_slice(&[w / 2.0 + rad * a.cos(), h / 2.0 + rad * a.sin()]);
        if i > 0 {
            ops.push(1);
        }
    }
    ops.push(4);
    let big = vec![Shape { ops, pts, even_odd: true, stroke: None, paint: PaintSpec::Solid([0.1, 0.4, 0.8, 1.0]) }];
    // 3. thick dashed strokes: 600 cubics, width 8, round joins/caps, dashes.
    let mut strokes = Vec::new();
    for _ in 0..600 {
        let p: Vec<f32> = (0..8).map(|i| if i % 2 == 0 { r.f() * w } else { r.f() * h }).collect();
        strokes.push(Shape {
            ops: vec![0, 3],
            pts: p,
            even_odd: false,
            stroke: Some(StrokeSpec { width: 8.0, cap: 1, join: 1, dash: vec![24.0, 12.0] }),
            paint: PaintSpec::Solid(r.color()),
        });
    }
    // 4. gradient fills: 300 linear and 300 radial rectangles.
    let mut grads = Vec::new();
    for i in 0..600 {
        let (x, y, s) = (r.f() * (w - 300.0), r.f() * (h - 300.0), 60.0 + r.f() * 240.0);
        let paint = if i % 2 == 0 {
            PaintSpec::Linear([x, y, x + s, y + s], r.color(), r.color())
        } else {
            PaintSpec::Radial([x + s / 2.0, y + s / 2.0, s / 2.0], r.color(), r.color())
        };
        grads.push(Shape {
            ops: vec![0, 1, 1, 1, 4],
            pts: vec![x, y, x + s, y, x + s, y + s, x, y + s],
            even_odd: false,
            stroke: None,
            paint,
        });
    }
    vec![
        ("20k small AA paths", small),
        ("6000-vertex even-odd star", big),
        ("600 dashed thick cubics", strokes),
        ("600 gradient rects", grads),
    ]
}

macro_rules! skia_like {
    ($name:ident, $k:ident) => {
        fn $name(pix: &mut $k::Pixmap, shapes: &[Shape]) {
            use $k::*;
            pix.fill(Color::TRANSPARENT);
            for s in shapes {
                let mut pb = PathBuilder::new();
                let mut q = 0;
                for &op in &s.ops {
                    let p = &s.pts;
                    match op {
                        0 => { pb.move_to(p[q], p[q + 1]); q += 2; }
                        1 => { pb.line_to(p[q], p[q + 1]); q += 2; }
                        2 => { pb.quad_to(p[q], p[q + 1], p[q + 2], p[q + 3]); q += 4; }
                        3 => { pb.cubic_to(p[q], p[q + 1], p[q + 2], p[q + 3], p[q + 4], p[q + 5]); q += 6; }
                        _ => pb.close(),
                    }
                }
                let Some(path) = pb.finish() else { continue };
                let c = |v: [f32; 4]| Color::from_rgba(v[0], v[1], v[2], v[3]).unwrap();
                let mut paint = Paint::default();
                paint.anti_alias = true;
                match &s.paint {
                    PaintSpec::Solid(v) => paint.set_color(c(*v)),
                    PaintSpec::Linear(g, a, b) => {
                        paint.shader = LinearGradient::new(
                            Point::from_xy(g[0], g[1]), Point::from_xy(g[2], g[3]),
                            vec![GradientStop::new(0.0, c(*a)), GradientStop::new(1.0, c(*b))],
                            SpreadMode::Pad, Transform::identity()).unwrap();
                    }
                    PaintSpec::Radial(g, a, b) => {
                        paint.shader = RadialGradient::new(
                            Point::from_xy(g[0], g[1]), 0.0, Point::from_xy(g[0], g[1]), g[2],
                            vec![GradientStop::new(0.0, c(*a)), GradientStop::new(1.0, c(*b))],
                            SpreadMode::Pad, Transform::identity()).unwrap();
                    }
                }
                match &s.stroke {
                    None => pix.fill_path(&path, &paint,
                        if s.even_odd { FillRule::EvenOdd } else { FillRule::Winding }, Transform::identity(), None),
                    Some(st) => {
                        let stroke = Stroke {
                            width: st.width,
                            miter_limit: 4.0,
                            line_cap: [LineCap::Butt, LineCap::Round, LineCap::Square][st.cap as usize],
                            line_join: [LineJoin::Miter, LineJoin::Round, LineJoin::Bevel][st.join as usize],
                            dash: if st.dash.is_empty() { None } else { StrokeDash::new(st.dash.clone(), 0.0) },
                        };
                        pix.stroke_path(&path, &paint, &stroke, Transform::identity(), None);
                    }
                }
            }
        }
    };
}
skia_like!(draw_rr, rusty_raster);
skia_like!(draw_ts, tiny_skia);

fn draw_cairo(ctx: *mut c_void, shapes: &[Shape]) {
    unsafe { cr_clear(ctx) };
    for s in shapes {
        let (kind, pp): (c_int, Vec<f32>) = match &s.paint {
            PaintSpec::Solid(v) => (0, v.to_vec()),
            PaintSpec::Linear(g, a, b) => (1, [g.as_slice(), a, b].concat()),
            PaintSpec::Radial(g, a, b) => (2, [g.as_slice(), a, b].concat()),
        };
        let (width, cap, join, dash) = match &s.stroke {
            Some(st) => (st.width, st.cap, st.join, st.dash.clone()),
            None => (0.0, 0, 0, Vec::new()),
        };
        unsafe {
            cr_draw(ctx, s.ops.as_ptr(), s.ops.len() as c_int, s.pts.as_ptr(), c_int::from(s.even_odd),
                width, cap, join, 4.0, dash.as_ptr(), dash.len() as c_int, kind, pp.as_ptr(), 0.0, 0.0);
        }
    }
}

/// PSNR between two RGBA8 buffers (premultiplied or straight, same for both),
/// max channel diff and % of differing bytes.
fn compare(a: &[u8], b: &[u8]) -> (f64, u8, f64) {
    let mut se = 0f64;
    let mut maxd = 0u8;
    let mut n = 0usize;
    for (x, y) in a.iter().zip(b) {
        let d = x.abs_diff(*y);
        maxd = maxd.max(d);
        if d != 0 {
            n += 1;
        }
        se += f64::from(d) * f64::from(d);
    }
    let mse = se / a.len() as f64;
    let psnr = if mse == 0.0 { f64::INFINITY } else { 10.0 * (255.0f64 * 255.0 / mse).log10() };
    (psnr, maxd, n as f64 * 100.0 / a.len() as f64)
}

type Arm<'a> = (&'static str, Box<dyn FnMut() + 'a>);

fn race(label: &str, rounds: usize, mut arms: Vec<Arm>) {
    let k = arms.len();
    let mut t = vec![Vec::new(); k];
    for a in arms.iter_mut() {
        (a.1)();
    }
    for r in 0..rounds {
        for j in 0..k {
            let i = if r % 2 == 0 { j } else { k - 1 - j };
            let s = Instant::now();
            (arms[i].1)();
            t[i].push(s.elapsed().as_secs_f64() * 1e3);
        }
    }
    let base = t[0].iter().cloned().fold(f64::MAX, f64::min);
    for (i, (name, _)) in arms.iter().enumerate() {
        let mut v = t[i].clone();
        v.sort_by(f64::total_cmp);
        let wins = (0..rounds).filter(|&r| t[0][r] < t[i][r]).count();
        let z = (wins as f64 - rounds as f64 / 2.0) / (0.5 * (rounds as f64).sqrt());
        let extra = if i == 0 { String::new() } else {
            format!("\tours wins {wins}/{rounds} z={z:+.2}\tthem/ours time {:.2}x", v[0] / base)
        };
        println!("{label}\t{name}\tmin {:.2} ms\tmed {:.2} ms{extra}", v[0], v[v.len() / 2]);
    }
}

fn raster(rounds: usize) {
    for (label, shapes) in workloads() {
        let mut rr = rusty_raster::Pixmap::new(W, H).unwrap();
        let mut ts = tiny_skia::Pixmap::new(W, H).unwrap();
        let ctx = unsafe { cr_new(W as c_int, H as c_int) };
        draw_rr(&mut rr, &shapes);
        draw_ts(&mut ts, &shapes);
        draw_cairo(ctx, &shapes);
        let mut cairo_px = vec![0u8; (W * H * 4) as usize];
        unsafe { cr_read(ctx, cairo_px.as_mut_ptr()) };
        let (p1, m1, d1) = compare(rr.data(), ts.data());
        let (p2, m2, d2) = compare(rr.data(), &cairo_px);
        let (p3, m3, d3) = compare(ts.data(), &cairo_px);
        println!("{label}\tvs tiny-skia: PSNR {p1:.2} dB max {m1} diff {d1:.3}%\tvs Cairo: PSNR {p2:.2} dB max {m2} diff {d2:.2}%\t(tiny-skia vs Cairo: {p3:.2} dB max {m3} diff {d3:.2}%)");
        let (rrp, tsp) = (std::cell::RefCell::new(rr), std::cell::RefCell::new(ts));
        let sh = &shapes;
        race(label, rounds, vec![
            ("rusty_raster", Box::new(|| draw_rr(&mut rrp.borrow_mut(), sh))),
            ("rusty_raster (null arm)", Box::new(|| draw_rr(&mut rrp.borrow_mut(), sh))),
            ("tiny-skia 0.12 (Rust)", Box::new(|| draw_ts(&mut tsp.borrow_mut(), sh))),
            ("Cairo (C)", Box::new(move || draw_cairo(ctx, sh))),
        ]);
        unsafe { cr_free(ctx) };
        println!();
    }
}

/// Render scale to fit the SVG into a 1024-px box.
fn fit(w: f32, h: f32) -> (f32, u32, u32) {
    let s = (1024.0 / w.max(h)).min(16.0);
    (s, (w * s).ceil().max(1.0) as u32, (h * s).ceil().max(1.0) as u32)
}

fn unpremultiply(p: &mut [u8]) {
    for px in p.chunks_exact_mut(4) {
        let a = px[3];
        if a != 0 && a != 255 {
            for c in &mut px[..3] {
                *c = ((u16::from(*c) * 255 + u16::from(a) / 2) / u16::from(a)).min(255) as u8;
            }
        }
    }
}

fn svg(rounds: usize, files: &[String]) {
    println!("file\tsize\trusty_svgtree vs resvg PSNR\tvs nanosvg PSNR\tresvg vs nanosvg PSNR");
    let mut timed = Vec::new();
    let (mut ok, mut ours_fail, mut resvg_fail) = (0, 0, 0);
    let mut psnr_rv = Vec::new();
    let mut psnr_ns = Vec::new();
    let mut psnr_rn = Vec::new();
    for f in files {
        let data = std::fs::read(f).unwrap();
        let ours = rusty_svgtree::Tree::from_data(&data, &rusty_svgtree::Options::default());
        let theirs = resvg::usvg::Tree::from_data(&data, &resvg::usvg::Options::default());
        let (ours, theirs) = match (ours, theirs) {
            (Ok(a), Ok(b)) => (a, b),
            (Err(_), Ok(_)) => { ours_fail += 1; println!("{f}\twe refuse, resvg renders"); continue; }
            (Ok(_), Err(_)) => { resvg_fail += 1; continue; }
            _ => continue,
        };
        let (s, w, h) = fit(theirs.size().width(), theirs.size().height());
        let mut a = rusty_svgtree::rusty_raster::Pixmap::new(w, h).unwrap();
        rusty_svgtree::render(&ours, rusty_svgtree::rusty_raster::Transform::from_scale(s, s), &mut a.as_mut());
        let mut b = resvg::tiny_skia::Pixmap::new(w, h).unwrap();
        resvg::render(&theirs, resvg::tiny_skia::Transform::from_scale(s, s), &mut b.as_mut());
        let mut c = vec![0u8; (w * h * 4) as usize];
        let nsok = unsafe { nsvg_render(data.as_ptr().cast(), data.len(), s, w as c_int, h as c_int, c.as_mut_ptr()) } == 0;
        let mut au = a.data().to_vec();
        let mut bu = b.data().to_vec();
        unpremultiply(&mut au);
        unpremultiply(&mut bu);
        let (p1, ..) = compare(a.data(), b.data());
        let (p2, ..) = if nsok { compare(&au, &c) } else { (f64::NAN, 0, 0.0) };
        let (p3, ..) = if nsok { compare(&bu, &c) } else { (f64::NAN, 0, 0.0) };
        println!("{f}\t{w}x{h}\t{p1:.2}\t{p2:.2}\t{p3:.2}");
        ok += 1;
        psnr_rv.push(p1.min(99.0));
        if nsok {
            psnr_ns.push(p2.min(99.0));
            psnr_rn.push(p3.min(99.0));
        }
        timed.push((f.clone(), data, s, w, h));
    }
    let med = |v: &mut Vec<f64>| { v.sort_by(f64::total_cmp); if v.is_empty() { f64::NAN } else { v[v.len() / 2] } };
    println!("\nrendered by both: {ok}; we refuse while resvg renders: {ours_fail}; resvg refuses: {resvg_fail}");
    println!("median PSNR: ours vs resvg {:.2} dB, ours vs nanosvg {:.2} dB, resvg vs nanosvg {:.2} dB (99 = identical)",
        med(&mut psnr_rv), med(&mut psnr_ns), med(&mut psnr_rn));
    // Speed over the whole corpus (parse + render), one sample = every file once.
    let corpus = &timed;
    race("SVG corpus parse+render", rounds, vec![
        ("rusty_svgtree", Box::new(|| for (_, d, s, w, h) in corpus {
            let t = rusty_svgtree::Tree::from_data(d, &rusty_svgtree::Options::default()).unwrap();
            let mut p = rusty_svgtree::rusty_raster::Pixmap::new(*w, *h).unwrap();
            rusty_svgtree::render(&t, rusty_svgtree::rusty_raster::Transform::from_scale(*s, *s), &mut p.as_mut());
        })),
        ("rusty_svgtree (null arm)", Box::new(|| for (_, d, s, w, h) in corpus {
            let t = rusty_svgtree::Tree::from_data(d, &rusty_svgtree::Options::default()).unwrap();
            let mut p = rusty_svgtree::rusty_raster::Pixmap::new(*w, *h).unwrap();
            rusty_svgtree::render(&t, rusty_svgtree::rusty_raster::Transform::from_scale(*s, *s), &mut p.as_mut());
        })),
        ("resvg 0.48 (Rust)", Box::new(|| for (_, d, s, w, h) in corpus {
            let t = resvg::usvg::Tree::from_data(d, &resvg::usvg::Options::default()).unwrap();
            let mut p = resvg::tiny_skia::Pixmap::new(*w, *h).unwrap();
            resvg::render(&t, resvg::tiny_skia::Transform::from_scale(*s, *s), &mut p.as_mut());
        })),
        ("nanosvg (C)", Box::new(|| for (_, d, s, w, h) in corpus {
            let mut out = vec![0u8; (*w * *h * 4) as usize];
            unsafe { nsvg_render(d.as_ptr().cast(), d.len(), *s, *w as c_int, *h as c_int, out.as_mut_ptr()) };
        })),
    ]);
}

fn main() {
    let a: Vec<String> = std::env::args().collect();
    match a[1].as_str() {
        "raster" => raster(a[2].parse().unwrap()),
        "svg" => svg(a[2].parse().unwrap(), &a[3..]),
        _ => panic!("mode"),
    }
}
