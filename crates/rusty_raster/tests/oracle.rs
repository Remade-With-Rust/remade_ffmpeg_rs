//! Differential oracle: every scene in a deterministic corpus is rendered by
//! `rusty_raster` and by `tiny-skia` 0.12 (dev-dependency) through the same
//! backend-neutral scene description, and the two RGBA8 buffers are compared.
//!
//! Per scene: max per-channel difference, mismatch percentage (pixels with any
//! channel differing) and PSNR over all channel bytes. Run with
//! `cargo test -p rusty_raster --release --test oracle -- --nocapture` for the
//! table. `RR_ORACLE_DUMP=<dir>` writes both renders of every differing scene
//! as PNG.

// ---------------------------------------------------------------------------
// Scene description (backend-neutral)

#[derive(Clone, Debug)]
enum Cmd {
    M(f32, f32),
    L(f32, f32),
    Q(f32, f32, f32, f32),
    C(f32, f32, f32, f32, f32, f32),
    Z,
    Rect(f32, f32, f32, f32),
    Circle(f32, f32, f32),
    Oval(f32, f32, f32, f32),
}

type Stops = Vec<(f32, [u8; 4])>;

#[derive(Clone, Debug)]
enum ShaderSpec {
    Solid([u8; 4]),
    Linear {
        p0: (f32, f32),
        p1: (f32, f32),
        stops: Stops,
        spread: u8,
        ts: [f32; 6],
    },
    Radial {
        c0: (f32, f32),
        r0: f32,
        c1: (f32, f32),
        r1: f32,
        stops: Stops,
        spread: u8,
        ts: [f32; 6],
    },
    Sweep {
        c: (f32, f32),
        a0: f32,
        a1: f32,
        stops: Stops,
        spread: u8,
        ts: [f32; 6],
    },
    Pattern {
        spread: u8,
        quality: u8,
        opacity: f32,
        ts: [f32; 6],
    },
}

#[derive(Clone, Debug)]
struct PaintSpec {
    shader: ShaderSpec,
    blend: u8,
    aa: bool,
    colorspace: u8,
    hq: bool,
}

#[derive(Clone, Debug)]
struct StrokeSpec {
    width: f32,
    miter: f32,
    cap: u8,
    join: u8,
    dash: Option<(Vec<f32>, f32)>,
}

#[derive(Clone, Debug)]
enum MaskSpec {
    Path {
        cmds: Vec<Cmd>,
        even_odd: bool,
        aa: bool,
        ts: [f32; 6],
        invert: bool,
    },
    /// `Mask::from_pixmap` of the shared source image, drawn at the origin.
    FromSource { luminance: bool },
}

#[derive(Clone, Debug)]
enum Op {
    Fill {
        cmds: Vec<Cmd>,
        paint: PaintSpec,
        even_odd: bool,
        ts: [f32; 6],
        mask: Option<MaskSpec>,
    },
    Stroke {
        cmds: Vec<Cmd>,
        paint: PaintSpec,
        stroke: StrokeSpec,
        ts: [f32; 6],
        mask: Option<MaskSpec>,
    },
    FillRect {
        r: (f32, f32, f32, f32),
        paint: PaintSpec,
        ts: [f32; 6],
        mask: Option<MaskSpec>,
    },
    Pixmap {
        x: i32,
        y: i32,
        opacity: f32,
        blend: u8,
        quality: u8,
        ts: [f32; 6],
        mask: Option<MaskSpec>,
    },
}

#[derive(Clone, Debug)]
pub struct Scene {
    cat: &'static str,
    name: String,
    w: u32,
    h: u32,
    bg: Option<[u8; 4]>,
    ops: Vec<Op>,
}

const ID: [f32; 6] = [1.0, 0.0, 0.0, 1.0, 0.0, 0.0];
const SRC_W: u32 = 37;
const SRC_H: u32 = 23;

/// The shared source image for `draw_pixmap`, patterns and pixmap masks:
/// premultiplied RGBA with varying alpha, hard edges and a smooth ramp.
fn src_bytes() -> Vec<u8> {
    let mut v = Vec::with_capacity((SRC_W * SRC_H * 4) as usize);
    for y in 0..SRC_H {
        for x in 0..SRC_W {
            let checker = ((x / 4) + (y / 4)) % 2 == 0;
            let a = if checker { 255u32 } else { 60 + (x * 5) % 190 };
            let r = (x * 255 / (SRC_W - 1)) * a / 255;
            let g = (y * 255 / (SRC_H - 1)) * a / 255;
            let b = if checker { a } else { (a / 3).min(a) };
            v.extend_from_slice(&[r as u8, g as u8, b as u8, a as u8]);
        }
    }
    v
}

// ---------------------------------------------------------------------------
// The two interpreters, generated from one body.

macro_rules! backend {
    ($fname:ident, $ns:ident) => {
        pub fn $fname(sc: &Scene) -> Option<Vec<u8>> {
            use $ns as s;

            fn ts(t: &[f32; 6]) -> s::Transform {
                s::Transform::from_row(t[0], t[1], t[2], t[3], t[4], t[5])
            }

            fn path(c: &[Cmd]) -> Option<s::Path> {
                let mut pb = s::PathBuilder::new();
                for cmd in c {
                    match *cmd {
                        Cmd::M(x, y) => pb.move_to(x, y),
                        Cmd::L(x, y) => pb.line_to(x, y),
                        Cmd::Q(a, b, x, y) => pb.quad_to(a, b, x, y),
                        Cmd::C(a, b, c2, d, x, y) => pb.cubic_to(a, b, c2, d, x, y),
                        Cmd::Z => pb.close(),
                        Cmd::Rect(x, y, w, h) => {
                            if let Some(r) = s::Rect::from_xywh(x, y, w, h) {
                                pb.push_rect(r)
                            }
                        }
                        Cmd::Circle(x, y, r) => pb.push_circle(x, y, r),
                        Cmd::Oval(x, y, w, h) => {
                            if let Some(r) = s::Rect::from_xywh(x, y, w, h) {
                                pb.push_oval(r)
                            }
                        }
                    }
                }
                pb.finish()
            }

            fn color(c: [u8; 4]) -> s::Color {
                s::Color::from_rgba8(c[0], c[1], c[2], c[3])
            }

            fn stops(v: &Stops) -> Vec<s::GradientStop> {
                v.iter()
                    .map(|(p, c)| s::GradientStop::new(*p, color(*c)))
                    .collect()
            }

            fn spread(v: u8) -> s::SpreadMode {
                match v % 3 {
                    0 => s::SpreadMode::Pad,
                    1 => s::SpreadMode::Repeat,
                    _ => s::SpreadMode::Reflect,
                }
            }

            fn quality(v: u8) -> s::FilterQuality {
                match v % 3 {
                    0 => s::FilterQuality::Nearest,
                    1 => s::FilterQuality::Bilinear,
                    _ => s::FilterQuality::Bicubic,
                }
            }

            fn blend(v: u8) -> s::BlendMode {
                use s::BlendMode::*;
                const ALL: [s::BlendMode; 29] = [
                    SourceOver,
                    Clear,
                    Source,
                    Destination,
                    DestinationOver,
                    SourceIn,
                    DestinationIn,
                    SourceOut,
                    DestinationOut,
                    SourceAtop,
                    DestinationAtop,
                    Xor,
                    Plus,
                    Modulate,
                    Screen,
                    Overlay,
                    Darken,
                    Lighten,
                    ColorDodge,
                    ColorBurn,
                    HardLight,
                    SoftLight,
                    Difference,
                    Exclusion,
                    Multiply,
                    Hue,
                    Saturation,
                    Color,
                    Luminosity,
                ];
                ALL[v as usize % 29]
            }

            fn colorspace(v: u8) -> s::ColorSpace {
                match v % 4 {
                    0 => s::ColorSpace::Linear,
                    1 => s::ColorSpace::Gamma2,
                    2 => s::ColorSpace::SimpleSRGB,
                    _ => s::ColorSpace::FullSRGBGamma,
                }
            }

            fn paint<'a>(p: &PaintSpec, src: &'a s::Pixmap) -> Option<s::Paint<'a>> {
                let shader = match &p.shader {
                    ShaderSpec::Solid(c) => s::Shader::SolidColor(color(*c)),
                    ShaderSpec::Linear {
                        p0,
                        p1,
                        stops: st,
                        spread: sp,
                        ts: t,
                    } => s::LinearGradient::new(
                        s::Point::from_xy(p0.0, p0.1),
                        s::Point::from_xy(p1.0, p1.1),
                        stops(st),
                        spread(*sp),
                        ts(t),
                    )?,
                    ShaderSpec::Radial {
                        c0,
                        r0,
                        c1,
                        r1,
                        stops: st,
                        spread: sp,
                        ts: t,
                    } => s::RadialGradient::new(
                        s::Point::from_xy(c0.0, c0.1),
                        *r0,
                        s::Point::from_xy(c1.0, c1.1),
                        *r1,
                        stops(st),
                        spread(*sp),
                        ts(t),
                    )?,
                    ShaderSpec::Sweep {
                        c,
                        a0,
                        a1,
                        stops: st,
                        spread: sp,
                        ts: t,
                    } => s::SweepGradient::new(
                        s::Point::from_xy(c.0, c.1),
                        *a0,
                        *a1,
                        stops(st),
                        spread(*sp),
                        ts(t),
                    )?,
                    ShaderSpec::Pattern {
                        spread: sp,
                        quality: q,
                        opacity,
                        ts: t,
                    } => s::Pattern::new(src.as_ref(), spread(*sp), quality(*q), *opacity, ts(t)),
                };
                Some(s::Paint {
                    shader,
                    blend_mode: blend(p.blend),
                    anti_alias: p.aa,
                    colorspace: colorspace(p.colorspace),
                    force_hq_pipeline: p.hq,
                })
            }

            fn mask(m: &MaskSpec, w: u32, h: u32, src: &s::Pixmap) -> Option<s::Mask> {
                match m {
                    MaskSpec::Path {
                        cmds,
                        even_odd,
                        aa,
                        ts: t,
                        invert,
                    } => {
                        let mut mk = s::Mask::new(w, h)?;
                        if let Some(p) = path(cmds) {
                            let rule = if *even_odd {
                                s::FillRule::EvenOdd
                            } else {
                                s::FillRule::Winding
                            };
                            mk.fill_path(&p, rule, *aa, ts(t));
                        }
                        if *invert {
                            mk.invert();
                        }
                        Some(mk)
                    }
                    MaskSpec::FromSource { luminance } => {
                        let mut canvas = s::Pixmap::new(w, h)?;
                        canvas.draw_pixmap(
                            0,
                            0,
                            src.as_ref(),
                            &s::PixmapPaint::default(),
                            s::Transform::identity(),
                            None,
                        );
                        let ty = if *luminance {
                            s::MaskType::Luminance
                        } else {
                            s::MaskType::Alpha
                        };
                        Some(s::Mask::from_pixmap(canvas.as_ref(), ty))
                    }
                }
            }

            let mut pm = s::Pixmap::new(sc.w, sc.h)?;
            if let Some(c) = sc.bg {
                pm.fill(color(c));
            }
            let src = s::Pixmap::from_vec(src_bytes(), s::IntSize::from_wh(SRC_W, SRC_H)?)?;
            let (w, h) = (sc.w, sc.h);
            for op in &sc.ops {
                match op {
                    Op::Fill {
                        cmds,
                        paint: p,
                        even_odd,
                        ts: t,
                        mask: m,
                    } => {
                        let (Some(pa), Some(pt)) = (paint(p, &src), path(cmds)) else {
                            continue;
                        };
                        let mk = m.as_ref().and_then(|m| mask(m, w, h, &src));
                        let rule = if *even_odd {
                            s::FillRule::EvenOdd
                        } else {
                            s::FillRule::Winding
                        };
                        pm.fill_path(&pt, &pa, rule, ts(t), mk.as_ref());
                    }
                    Op::Stroke {
                        cmds,
                        paint: p,
                        stroke: st,
                        ts: t,
                        mask: m,
                    } => {
                        let (Some(pa), Some(pt)) = (paint(p, &src), path(cmds)) else {
                            continue;
                        };
                        let mk = m.as_ref().and_then(|m| mask(m, w, h, &src));
                        let stroke = s::Stroke {
                            width: st.width,
                            miter_limit: st.miter,
                            line_cap: match st.cap % 3 {
                                0 => s::LineCap::Butt,
                                1 => s::LineCap::Round,
                                _ => s::LineCap::Square,
                            },
                            line_join: match st.join % 4 {
                                0 => s::LineJoin::Miter,
                                1 => s::LineJoin::MiterClip,
                                2 => s::LineJoin::Round,
                                _ => s::LineJoin::Bevel,
                            },
                            dash: st
                                .dash
                                .as_ref()
                                .and_then(|(a, o)| s::StrokeDash::new(a.clone(), *o)),
                        };
                        pm.stroke_path(&pt, &pa, &stroke, ts(t), mk.as_ref());
                    }
                    Op::FillRect {
                        r,
                        paint: p,
                        ts: t,
                        mask: m,
                    } => {
                        let (Some(pa), Some(rect)) =
                            (paint(p, &src), s::Rect::from_xywh(r.0, r.1, r.2, r.3))
                        else {
                            continue;
                        };
                        let mk = m.as_ref().and_then(|m| mask(m, w, h, &src));
                        pm.fill_rect(rect, &pa, ts(t), mk.as_ref());
                    }
                    Op::Pixmap {
                        x,
                        y,
                        opacity,
                        blend: b,
                        quality: q,
                        ts: t,
                        mask: m,
                    } => {
                        let mk = m.as_ref().and_then(|m| mask(m, w, h, &src));
                        let pp = s::PixmapPaint {
                            opacity: *opacity,
                            blend_mode: blend(*b),
                            quality: quality(*q),
                        };
                        pm.draw_pixmap(*x, *y, src.as_ref(), &pp, ts(t), mk.as_ref());
                    }
                }
            }
            Some(pm.data().to_vec())
        }
    };
}

backend!(render_rr, rusty_raster);
backend!(render_ts, tiny_skia);

// ---------------------------------------------------------------------------
// Deterministic corpus

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
    fn b(&mut self) -> bool {
        self.next() & 1 == 1
    }
    fn rgba(&mut self) -> [u8; 4] {
        [
            self.u(256) as u8,
            self.u(256) as u8,
            self.u(256) as u8,
            self.u(256) as u8,
        ]
    }
    fn opaque(&mut self) -> [u8; 4] {
        [self.u(256) as u8, self.u(256) as u8, self.u(256) as u8, 255]
    }
}

fn solid(c: [u8; 4], aa: bool) -> PaintSpec {
    PaintSpec {
        shader: ShaderSpec::Solid(c),
        blend: 0,
        aa,
        colorspace: 0,
        hq: false,
    }
}

fn random_stops(r: &mut Rng) -> Stops {
    let n = 2 + r.u(4);
    let mut v: Stops = (0..n).map(|_| (r.f(0.0, 1.0), r.rgba())).collect();
    v.sort_by(|a, b| a.0.total_cmp(&b.0));
    if r.b() {
        v[0].0 = 0.0;
        let l = v.len() - 1;
        v[l].0 = 1.0;
    }
    v
}

fn random_transform(r: &mut Rng, w: f32, h: f32) -> [f32; 6] {
    match r.u(5) {
        0 => ID,
        1 => [1.0, 0.0, 0.0, 1.0, r.f(-10.0, 10.0), r.f(-10.0, 10.0)],
        2 => {
            let s = r.f(0.3, 2.5);
            let t = r.f(0.3, 2.5);
            [
                s,
                0.0,
                0.0,
                t,
                r.f(-w * 0.3, w * 0.3),
                r.f(-h * 0.3, h * 0.3),
            ]
        }
        3 => {
            let a = r.f(-3.2, 3.2);
            let (sn, cs) = a.sin_cos();
            let (cx, cy) = (w / 2.0, h / 2.0);
            [
                cs,
                sn,
                -sn,
                cs,
                cx - cs * cx + sn * cy,
                cy - sn * cx - cs * cy,
            ]
        }
        _ => [
            r.f(0.5, 1.5),
            r.f(-0.5, 0.5),
            r.f(-0.5, 0.5),
            r.f(0.5, 1.5),
            r.f(-10.0, 10.0),
            r.f(-10.0, 10.0),
        ],
    }
}

fn random_path(r: &mut Rng, w: f32, h: f32, base: u32, extra: u32) -> Vec<Cmd> {
    let segs = base + if extra > 0 { r.u(extra) } else { 0 };
    let mut v = vec![Cmd::M(r.f(-0.1 * w, 1.1 * w), r.f(-0.1 * h, 1.1 * h))];
    let mut p = || (r.f(-0.2 * w, 1.2 * w), r.f(-0.2 * h, 1.2 * h));
    for _ in 0..segs {
        let a = p();
        let b = p();
        let c = p();
        let k = ((a.0 * 7.0 + b.1 * 13.0).abs() as u32) % 3;
        v.push(match k {
            0 => Cmd::L(a.0, a.1),
            1 => Cmd::Q(a.0, a.1, b.0, b.1),
            _ => Cmd::C(a.0, a.1, b.0, b.1, c.0, c.1),
        });
    }
    v.push(Cmd::Z);
    v
}

fn star(cx: f32, cy: f32, r_out: f32, r_in: f32, n: u32, rot: f32) -> Vec<Cmd> {
    let mut v = Vec::new();
    for i in 0..n * 2 {
        let a = rot + i as f32 * std::f32::consts::PI / n as f32;
        let r = if i % 2 == 0 { r_out } else { r_in };
        let (x, y) = (cx + r * a.cos(), cy + r * a.sin());
        v.push(if i == 0 { Cmd::M(x, y) } else { Cmd::L(x, y) });
    }
    v.push(Cmd::Z);
    v
}

fn pentagram(cx: f32, cy: f32, r: f32) -> Vec<Cmd> {
    let mut v = Vec::new();
    for i in 0..5 {
        let a =
            -std::f32::consts::FRAC_PI_2 + (i * 2 % 5) as f32 * 2.0 * std::f32::consts::PI / 5.0;
        let (x, y) = (cx + r * a.cos(), cy + r * a.sin());
        v.push(if i == 0 { Cmd::M(x, y) } else { Cmd::L(x, y) });
    }
    v.push(Cmd::Z);
    v
}

fn random_paint(r: &mut Rng, w: f32, h: f32) -> PaintSpec {
    let shader = match r.u(5) {
        0 => ShaderSpec::Solid(r.rgba()),
        1 => ShaderSpec::Linear {
            p0: (r.f(0.0, w), r.f(0.0, h)),
            p1: (r.f(0.0, w), r.f(0.0, h)),
            stops: random_stops(r),
            spread: r.u(3) as u8,
            ts: random_transform(r, w, h),
        },
        2 => {
            let c0 = (r.f(0.0, w), r.f(0.0, h));
            let focal = r.b();
            ShaderSpec::Radial {
                c0,
                r0: if r.b() { 0.0 } else { r.f(0.0, w / 4.0) },
                c1: if focal {
                    (r.f(0.0, w), r.f(0.0, h))
                } else {
                    c0
                },
                r1: r.f(1.0, w),
                stops: random_stops(r),
                spread: r.u(3) as u8,
                ts: random_transform(r, w, h),
            }
        }
        3 => {
            let a0 = r.f(0.0, 360.0);
            ShaderSpec::Sweep {
                c: (r.f(0.0, w), r.f(0.0, h)),
                a0,
                a1: a0 + r.f(1.0, 360.0),
                stops: random_stops(r),
                spread: r.u(3) as u8,
                ts: random_transform(r, w, h),
            }
        }
        _ => ShaderSpec::Pattern {
            spread: r.u(3) as u8,
            quality: r.u(3) as u8,
            opacity: r.f(0.2, 1.0),
            ts: random_transform(r, w, h),
        },
    };
    PaintSpec {
        shader,
        blend: if r.u(3) == 0 { r.u(29) as u8 } else { 0 },
        aa: r.u(5) != 0,
        colorspace: if r.u(4) == 0 { r.u(4) as u8 } else { 0 },
        hq: r.u(6) == 0,
    }
}

fn random_stroke(r: &mut Rng) -> StrokeSpec {
    StrokeSpec {
        width: match r.u(4) {
            0 => r.f(0.0, 1.0),
            1 => r.f(1.0, 4.0),
            _ => r.f(4.0, 25.0),
        },
        miter: r.f(1.0, 10.0),
        cap: r.u(3) as u8,
        join: r.u(4) as u8,
        dash: if r.u(3) == 0 {
            let n = 2 * (1 + r.u(3));
            Some(((0..n).map(|_| r.f(0.5, 15.0)).collect(), r.f(-20.0, 20.0)))
        } else {
            None
        },
    }
}

fn random_mask(r: &mut Rng, w: f32, h: f32) -> Option<MaskSpec> {
    match r.u(6) {
        0 => Some(MaskSpec::Path {
            cmds: random_path(r, w, h, 3, 4),
            even_odd: r.b(),
            aa: r.b(),
            ts: ID,
            invert: r.b(),
        }),
        1 => Some(MaskSpec::FromSource { luminance: r.b() }),
        _ => None,
    }
}

pub fn corpus() -> Vec<Scene> {
    let mut out = Vec::new();
    let mut push = |cat: &'static str, name: String, w: u32, h: u32, bg, ops| {
        out.push(Scene {
            cat,
            name,
            w,
            h,
            bg,
            ops,
        });
    };
    let white = Some([255, 255, 255, 255]);

    // 1. Basic shapes, AA on/off, both fill rules.
    let shapes: Vec<(&str, Vec<Cmd>)> = vec![
        ("rect", vec![Cmd::Rect(10.3, 12.7, 60.2, 35.9)]),
        ("circle", vec![Cmd::Circle(50.0, 50.0, 33.3)]),
        ("oval", vec![Cmd::Oval(8.5, 20.25, 80.0, 45.5)]),
        ("tiny-circle", vec![Cmd::Circle(50.2, 50.7, 0.8)]),
        ("thin-rect", vec![Cmd::Rect(10.0, 10.0, 0.3, 70.0)]),
        (
            "triangle",
            vec![
                Cmd::M(10.0, 90.0),
                Cmd::L(50.0, 5.5),
                Cmd::L(93.3, 80.1),
                Cmd::Z,
            ],
        ),
        ("rounded", {
            let (x, y, w, h, k) = (15.0, 15.0, 70.0, 60.0, 12.0);
            vec![
                Cmd::M(x + k, y),
                Cmd::L(x + w - k, y),
                Cmd::Q(x + w, y, x + w, y + k),
                Cmd::L(x + w, y + h - k),
                Cmd::C(x + w, y + h, x + w, y + h, x + w - k, y + h),
                Cmd::L(x + k, y + h),
                Cmd::Q(x, y + h, x, y + h - k),
                Cmd::L(x, y + k),
                Cmd::Q(x, y, x + k, y),
                Cmd::Z,
            ]
        }),
        (
            "donut",
            vec![Cmd::Circle(50.0, 50.0, 40.0), Cmd::Circle(50.0, 50.0, 20.0)],
        ),
        ("off-canvas", vec![Cmd::Circle(-10.0, 105.0, 45.0)]),
    ];
    for (name, cmds) in &shapes {
        for aa in [true, false] {
            for eo in [false, true] {
                push(
                    "shapes",
                    format!("{name}/aa={aa}/eo={eo}"),
                    100,
                    100,
                    white,
                    vec![Op::Fill {
                        cmds: cmds.clone(),
                        paint: solid([30, 90, 200, 220], aa),
                        even_odd: eo,
                        ts: ID,
                        mask: None,
                    }],
                );
            }
        }
    }

    // 2. Self-intersections: stars and pentagrams, both rules.
    for (i, cmds) in [
        pentagram(50.0, 52.0, 45.0),
        star(50.0, 50.0, 46.0, 18.0, 7, 0.3),
        star(50.0, 50.0, 46.0, 40.0, 13, 0.0),
    ]
    .iter()
    .enumerate()
    {
        for eo in [false, true] {
            push(
                "self-intersect",
                format!("star{i}/eo={eo}"),
                100,
                100,
                white,
                vec![Op::Fill {
                    cmds: cmds.clone(),
                    paint: solid([200, 40, 40, 255], true),
                    even_odd: eo,
                    ts: ID,
                    mask: None,
                }],
            );
        }
    }

    // 3. Random curves (fills).
    for seed in 0..40u64 {
        let mut r = Rng::new(1000 + seed);
        let cmds = random_path(&mut r, 120.0, 90.0, 2, 8);
        let eo = r.b();
        let aa = r.u(4) != 0;
        let t = random_transform(&mut r, 120.0, 90.0);
        let c = r.rgba();
        push(
            "curves",
            format!("seed{seed}"),
            120,
            90,
            white,
            vec![Op::Fill {
                cmds,
                paint: solid(c, aa),
                even_odd: eo,
                ts: t,
                mask: None,
            }],
        );
    }

    // 4. Every join x cap, three widths, open and closed.
    let open = vec![
        Cmd::M(15.0, 80.0),
        Cmd::L(35.0, 20.0),
        Cmd::L(55.0, 75.0),
        Cmd::Q(85.0, 10.0, 90.0, 60.0),
    ];
    let sharp = vec![Cmd::M(10.0, 70.0), Cmd::L(90.0, 60.0), Cmd::L(12.0, 50.0)];
    for (pn, base) in [("open", &open), ("sharp", &sharp)] {
        for join in 0..4u8 {
            for cap in 0..3u8 {
                for width in [1.5f32, 6.0, 14.0] {
                    for closed in [false, true] {
                        let mut cmds = base.clone();
                        if closed {
                            cmds.push(Cmd::Z);
                        }
                        push(
                            "joins-caps",
                            format!("{pn}/j{join}/c{cap}/w{width}/closed={closed}"),
                            100,
                            100,
                            white,
                            vec![Op::Stroke {
                                cmds,
                                paint: solid([20, 20, 20, 255], true),
                                stroke: StrokeSpec {
                                    width,
                                    miter: 4.0,
                                    cap,
                                    join,
                                    dash: None,
                                },
                                ts: ID,
                                mask: None,
                            }],
                        );
                    }
                }
            }
        }
    }
    // Miter limits on a sharp corner.
    for miter in [1.0f32, 1.5, 2.0, 4.0, 10.0, 100.0] {
        for join in [0u8, 1] {
            push(
                "joins-caps",
                format!("miter{miter}/j{join}"),
                100,
                100,
                white,
                vec![Op::Stroke {
                    cmds: sharp.clone(),
                    paint: solid([0, 0, 120, 255], true),
                    stroke: StrokeSpec {
                        width: 9.0,
                        miter,
                        cap: 0,
                        join,
                        dash: None,
                    },
                    ts: ID,
                    mask: None,
                }],
            );
        }
    }

    // 5. Thin strokes and hairlines (incl. under transforms).
    for (i, width) in [0.0f32, 0.1, 0.25, 0.5, 0.75, 1.0, 1.25].iter().enumerate() {
        for aa in [true, false] {
            for (ti, t) in [
                ID,
                [2.0, 0.0, 0.0, 0.5, 0.0, 10.0],
                [0.7, 0.7, -0.7, 0.7, 50.0, -20.0],
            ]
            .iter()
            .enumerate()
            {
                push(
                    "thin-strokes",
                    format!("w{i}/aa={aa}/t{ti}"),
                    100,
                    100,
                    white,
                    vec![Op::Stroke {
                        cmds: vec![
                            Cmd::M(5.0, 5.0),
                            Cmd::L(95.0, 40.3),
                            Cmd::Q(10.0, 95.0, 50.0, 50.0),
                            Cmd::C(80.0, 0.0, 99.0, 99.0, 3.0, 97.0),
                        ],
                        paint: solid([0, 0, 0, 255], aa),
                        stroke: StrokeSpec {
                            width: *width,
                            miter: 4.0,
                            cap: (i % 3) as u8,
                            join: 0,
                            dash: None,
                        },
                        ts: *t,
                        mask: None,
                    }],
                );
            }
        }
    }

    // 6. Dashes.
    for seed in 0..30u64 {
        let mut r = Rng::new(2000 + seed);
        let cmds = random_path(&mut r, 100.0, 100.0, 2, 5);
        let n = 2 * (1 + r.u(3));
        let dash = Some(((0..n).map(|_| r.f(0.5, 12.0)).collect(), r.f(-15.0, 15.0)));
        let stroke = StrokeSpec {
            width: r.f(0.5, 8.0),
            miter: 4.0,
            cap: r.u(3) as u8,
            join: r.u(4) as u8,
            dash,
        };
        push(
            "dashes",
            format!("seed{seed}"),
            100,
            100,
            white,
            vec![Op::Stroke {
                cmds,
                paint: solid([10, 120, 10, 255], true),
                stroke,
                ts: ID,
                mask: None,
            }],
        );
    }
    push(
        "dashes",
        "circle-dotted".into(),
        100,
        100,
        white,
        vec![Op::Stroke {
            cmds: vec![Cmd::Circle(50.0, 50.0, 40.0)],
            paint: solid([0, 0, 0, 255], true),
            stroke: StrokeSpec {
                width: 4.0,
                miter: 4.0,
                cap: 1,
                join: 2,
                dash: Some((vec![0.0, 8.0], 0.0)),
            },
            ts: ID,
            mask: None,
        }],
    );

    // 7. Gradients: every kind x spread, plus random ones.
    for spread in 0..3u8 {
        let st: Stops = vec![
            (0.0, [255, 0, 0, 255]),
            (0.4, [0, 255, 0, 128]),
            (1.0, [0, 0, 255, 255]),
        ];
        let kinds = [
            ShaderSpec::Linear {
                p0: (30.0, 40.0),
                p1: (70.0, 60.0),
                stops: st.clone(),
                spread,
                ts: ID,
            },
            ShaderSpec::Radial {
                c0: (50.0, 50.0),
                r0: 0.0,
                c1: (50.0, 50.0),
                r1: 25.0,
                stops: st.clone(),
                spread,
                ts: ID,
            },
            ShaderSpec::Radial {
                c0: (40.0, 45.0),
                r0: 0.0,
                c1: (55.0, 50.0),
                r1: 30.0,
                stops: st.clone(),
                spread,
                ts: ID,
            },
            ShaderSpec::Radial {
                c0: (30.0, 50.0),
                r0: 5.0,
                c1: (60.0, 50.0),
                r1: 20.0,
                stops: st.clone(),
                spread,
                ts: ID,
            },
            ShaderSpec::Radial {
                c0: (30.0, 50.0),
                r0: 20.0,
                c1: (60.0, 50.0),
                r1: 20.0,
                stops: st.clone(),
                spread,
                ts: ID,
            },
            ShaderSpec::Sweep {
                c: (50.0, 50.0),
                a0: 0.0,
                a1: 270.0,
                stops: st.clone(),
                spread,
                ts: ID,
            },
        ];
        for (k, shader) in kinds.into_iter().enumerate() {
            push(
                "gradients",
                format!("kind{k}/spread{spread}"),
                100,
                100,
                white,
                vec![Op::FillRect {
                    r: (0.0, 0.0, 100.0, 100.0),
                    paint: PaintSpec {
                        shader,
                        blend: 0,
                        aa: true,
                        colorspace: 0,
                        hq: false,
                    },
                    ts: ID,
                    mask: None,
                }],
            );
        }
    }
    for seed in 0..40u64 {
        let mut r = Rng::new(3000 + seed);
        let mut p = random_paint(&mut r, 100.0, 100.0);
        if let ShaderSpec::Solid(_) | ShaderSpec::Pattern { .. } = p.shader {
            p.shader = ShaderSpec::Linear {
                p0: (r.f(0.0, 100.0), r.f(0.0, 100.0)),
                p1: (r.f(0.0, 100.0), r.f(0.0, 100.0)),
                stops: random_stops(&mut r),
                spread: r.u(3) as u8,
                ts: ID,
            };
        }
        p.blend = 0;
        let cmds = random_path(&mut r, 100.0, 100.0, 3, 0);
        push(
            "gradients",
            format!("random{seed}"),
            100,
            100,
            white,
            vec![Op::Fill {
                cmds,
                paint: p,
                even_odd: false,
                ts: ID,
                mask: None,
            }],
        );
    }
    // Colour spaces on a linear gradient.
    for cs in 0..4u8 {
        for hq in [false, true] {
            push(
                "gradients",
                format!("colorspace{cs}/hq={hq}"),
                100,
                40,
                None,
                vec![Op::FillRect {
                    r: (0.0, 0.0, 100.0, 40.0),
                    paint: PaintSpec {
                        shader: ShaderSpec::Linear {
                            p0: (0.0, 0.0),
                            p1: (100.0, 0.0),
                            stops: vec![(0.0, [0, 0, 0, 255]), (1.0, [255, 255, 255, 128])],
                            spread: 0,
                            ts: ID,
                        },
                        blend: 0,
                        aa: true,
                        colorspace: cs,
                        hq,
                    },
                    ts: ID,
                    mask: None,
                }],
            );
        }
    }

    // 8. Transforms on a fixed shape.
    for seed in 0..20u64 {
        let mut r = Rng::new(4000 + seed);
        let t = random_transform(&mut r, 100.0, 100.0);
        push(
            "transforms",
            format!("seed{seed}"),
            100,
            100,
            white,
            vec![
                Op::Fill {
                    cmds: star(50.0, 50.0, 40.0, 15.0, 5, 0.0),
                    paint: solid([120, 30, 160, 230], true),
                    even_odd: false,
                    ts: t,
                    mask: None,
                },
                Op::Stroke {
                    cmds: vec![Cmd::Rect(20.0, 20.0, 60.0, 60.0)],
                    paint: solid([0, 0, 0, 255], true),
                    stroke: StrokeSpec {
                        width: 3.0,
                        miter: 4.0,
                        cap: 0,
                        join: 0,
                        dash: None,
                    },
                    ts: t,
                    mask: None,
                },
            ],
        );
    }

    // 9. Every blend mode, solid and gradient source, over a gradient backdrop.
    for b in 0..29u8 {
        for grad_src in [false, true] {
            let backdrop = Op::FillRect {
                r: (0.0, 0.0, 64.0, 64.0),
                paint: PaintSpec {
                    shader: ShaderSpec::Linear {
                        p0: (0.0, 0.0),
                        p1: (64.0, 64.0),
                        stops: vec![
                            (0.0, [255, 200, 0, 255]),
                            (0.5, [0, 120, 255, 100]),
                            (1.0, [40, 200, 90, 255]),
                        ],
                        spread: 0,
                        ts: ID,
                    },
                    blend: 0,
                    aa: true,
                    colorspace: 0,
                    hq: false,
                },
                ts: ID,
                mask: None,
            };
            let shader = if grad_src {
                ShaderSpec::Radial {
                    c0: (32.0, 32.0),
                    r0: 0.0,
                    c1: (32.0, 32.0),
                    r1: 30.0,
                    stops: vec![(0.0, [255, 0, 128, 255]), (1.0, [20, 20, 220, 90])],
                    spread: 0,
                    ts: ID,
                }
            } else {
                ShaderSpec::Solid([200, 30, 120, 170])
            };
            push(
                "blend-modes",
                format!("mode{b}/grad={grad_src}"),
                64,
                64,
                None,
                vec![
                    backdrop,
                    Op::Fill {
                        cmds: vec![Cmd::Circle(32.0, 32.0, 26.0)],
                        paint: PaintSpec {
                            shader,
                            blend: b,
                            aa: true,
                            colorspace: 0,
                            hq: false,
                        },
                        even_odd: false,
                        ts: ID,
                        mask: None,
                    },
                ],
            );
        }
    }

    // 10. Masks.
    for seed in 0..20u64 {
        let mut r = Rng::new(5000 + seed);
        let mask = random_mask(&mut r, 80.0, 80.0).or(Some(MaskSpec::Path {
            cmds: vec![Cmd::Circle(40.0, 40.0, 30.0)],
            even_odd: false,
            aa: true,
            ts: ID,
            invert: r.b(),
        }));
        let p = random_paint(&mut r, 80.0, 80.0);
        let cmds = random_path(&mut r, 80.0, 80.0, 4, 0);
        push(
            "masks",
            format!("seed{seed}"),
            80,
            80,
            white,
            vec![
                Op::Fill {
                    cmds,
                    paint: p,
                    even_odd: false,
                    ts: ID,
                    mask: mask.clone(),
                },
                Op::Stroke {
                    cmds: vec![Cmd::M(0.0, 0.0), Cmd::L(80.0, 80.0)],
                    paint: solid([0, 0, 0, 255], true),
                    stroke: StrokeSpec {
                        width: 5.0,
                        miter: 4.0,
                        cap: 1,
                        join: 0,
                        dash: None,
                    },
                    ts: ID,
                    mask,
                },
            ],
        );
    }

    // 11. draw_pixmap: every quality, transforms, opacity, blend.
    for q in 0..3u8 {
        for (ti, t) in [
            ID,
            [2.5, 0.0, 0.0, 2.5, 3.0, 4.0],
            [0.6, 0.0, 0.0, 0.6, 10.0, 10.0],
            [1.8, 0.9, -0.9, 1.8, 40.0, 0.0],
            [1.3, 0.0, 0.4, 1.7, -5.0, 2.0],
        ]
        .iter()
        .enumerate()
        {
            for opacity in [1.0f32, 0.55] {
                push(
                    "draw-pixmap",
                    format!("q{q}/t{ti}/op{opacity}"),
                    96,
                    96,
                    white,
                    vec![Op::Pixmap {
                        x: 7,
                        y: 9,
                        opacity,
                        blend: 0,
                        quality: q,
                        ts: *t,
                        mask: None,
                    }],
                );
            }
        }
    }
    for seed in 0..12u64 {
        let mut r = Rng::new(6000 + seed);
        push(
            "draw-pixmap",
            format!("random{seed}"),
            96,
            96,
            white,
            vec![Op::Pixmap {
                x: r.u(40) as i32 - 10,
                y: r.u(40) as i32 - 10,
                opacity: r.f(0.1, 1.0),
                blend: r.u(29) as u8,
                quality: r.u(3) as u8,
                ts: random_transform(&mut r, 96.0, 96.0),
                mask: random_mask(&mut r, 96.0, 96.0),
            }],
        );
    }

    // 12. Mixed random scenes.
    for seed in 0..120u64 {
        let mut r = Rng::new(7000 + seed);
        let w = 40 + r.u(140);
        let h = 40 + r.u(140);
        let (wf, hf) = (w as f32, h as f32);
        let bg = if r.b() { Some(r.opaque()) } else { None };
        let mut ops = Vec::new();
        for _ in 0..1 + r.u(5) {
            let t = random_transform(&mut r, wf, hf);
            let mask = random_mask(&mut r, wf, hf);
            let paint = random_paint(&mut r, wf, hf);
            ops.push(match r.u(4) {
                0 | 1 => Op::Fill {
                    cmds: random_path(&mut r, wf, hf, 1, 8),
                    paint,
                    even_odd: r.b(),
                    ts: t,
                    mask,
                },
                2 => Op::Stroke {
                    cmds: random_path(&mut r, wf, hf, 1, 6),
                    paint,
                    stroke: random_stroke(&mut r),
                    ts: t,
                    mask,
                },
                _ => Op::FillRect {
                    r: (r.f(-10.0, wf), r.f(-10.0, hf), r.f(1.0, wf), r.f(1.0, hf)),
                    paint,
                    ts: t,
                    mask,
                },
            });
        }
        push("mixed-random", format!("seed{seed}"), w, h, bg, ops);
    }

    // 13. Edge cases the oracle survives: big coordinates, clipping, tiny canvases.
    push(
        "edge-cases",
        "huge-coords".into(),
        100,
        100,
        white,
        vec![Op::Fill {
            cmds: vec![
                Cmd::M(-1.0e6, -1.0e6),
                Cmd::L(1.0e6, 50.0),
                Cmd::L(50.0, 1.0e6),
                Cmd::Z,
            ],
            paint: solid([0, 100, 0, 255], true),
            even_odd: false,
            ts: ID,
            mask: None,
        }],
    );
    push(
        "edge-cases",
        "huge-stroke".into(),
        100,
        100,
        white,
        vec![Op::Stroke {
            cmds: vec![Cmd::M(10.0, 10.0), Cmd::L(90.0, 90.0)],
            paint: solid([0, 0, 0, 200], true),
            stroke: StrokeSpec {
                width: 5000.0,
                miter: 4.0,
                cap: 2,
                join: 0,
                dash: None,
            },
            ts: ID,
            mask: None,
        }],
    );
    for (w, h) in [(1u32, 1u32), (1, 50), (50, 1), (3, 3), (17, 5)] {
        push(
            "edge-cases",
            format!("canvas{w}x{h}"),
            w,
            h,
            white,
            vec![Op::Fill {
                cmds: vec![Cmd::Circle(w as f32 / 2.0, h as f32 / 2.0, 3.0)],
                paint: solid([255, 0, 0, 200], true),
                even_odd: false,
                ts: ID,
                mask: None,
            }],
        );
    }
    push(
        "edge-cases",
        "wide-canvas".into(),
        1200,
        12,
        white,
        vec![Op::Stroke {
            cmds: vec![
                Cmd::M(0.0, 6.0),
                Cmd::C(400.0, -20.0, 800.0, 30.0, 1200.0, 6.0),
            ],
            paint: solid([0, 0, 0, 255], true),
            stroke: StrokeSpec {
                width: 2.0,
                miter: 4.0,
                cap: 1,
                join: 0,
                dash: Some((vec![10.0, 4.0], 0.0)),
            },
            ts: ID,
            mask: None,
        }],
    );

    out
}

// ---------------------------------------------------------------------------
// Comparison

struct Cmp {
    max_diff: u8,
    mismatch_pct: f64,
    psnr: f64,
}

fn compare(a: &[u8], b: &[u8]) -> Cmp {
    let mut max_diff = 0u8;
    let mut se = 0f64;
    let mut px_diff = 0usize;
    for (pa, pb) in a.chunks_exact(4).zip(b.chunks_exact(4)) {
        let mut any = false;
        for k in 0..4 {
            let d = pa[k].abs_diff(pb[k]);
            if d != 0 {
                any = true;
                max_diff = max_diff.max(d);
                se += f64::from(d) * f64::from(d);
            }
        }
        if any {
            px_diff += 1;
        }
    }
    let n_px = (a.len() / 4).max(1);
    let mse = se / a.len().max(1) as f64;
    let psnr = if mse == 0.0 {
        f64::INFINITY
    } else {
        10.0 * (255.0f64 * 255.0 / mse).log10()
    };
    Cmp {
        max_diff,
        mismatch_pct: 100.0 * px_diff as f64 / n_px as f64,
        psnr,
    }
}

fn dump_png(dir: &str, name: &str, w: u32, h: u32, data: Vec<u8>) {
    let file = format!("{dir}/{}.png", name.replace(['/', '='], "_"));
    if let Some(pm) =
        rusty_raster::Pixmap::from_vec(data, rusty_raster::IntSize::from_wh(w, h).unwrap())
    {
        let _ = pm.save_png(file);
    }
}

#[test]
fn oracle_corpus_vs_tiny_skia() {
    use std::fmt::Write as _;

    let scenes = corpus();
    let dump = std::env::var("RR_ORACLE_DUMP").ok();

    #[derive(Default)]
    struct Agg {
        n: usize,
        identical: usize,
        max_diff: u8,
        min_psnr: f64,
        sum_mismatch: f64,
        worst_mismatch: f64,
    }
    let mut cats: Vec<(&str, Agg)> = Vec::new();
    let mut total = Agg {
        min_psnr: f64::INFINITY,
        ..Default::default()
    };
    let mut diffs = String::new();
    let mut oracle_panics = 0usize;
    let mut both_none = 0usize;
    let mut differing: Vec<(&Scene, u8)> = Vec::new();

    for sc in &scenes {
        let ours = render_rr(sc);
        let theirs = std::panic::catch_unwind(|| render_ts(sc));
        let theirs = match theirs {
            Ok(t) => t,
            Err(_) => {
                oracle_panics += 1;
                writeln!(
                    diffs,
                    "  {}/{}: tiny-skia PANICKED (rusty_raster rendered)",
                    sc.cat, sc.name
                )
                .unwrap();
                continue;
            }
        };
        let (a, b) = match (ours, theirs) {
            (Some(a), Some(b)) => (a, b),
            (None, None) => {
                both_none += 1;
                continue;
            }
            (a, b) => panic!(
                "{}/{}: one side failed: ours={} theirs={}",
                sc.cat,
                sc.name,
                a.is_some(),
                b.is_some()
            ),
        };
        let c = compare(&a, &b);
        if c.max_diff != 0 {
            differing.push((sc, c.max_diff));
            writeln!(
                diffs,
                "  {}/{}: max {} mismatch {:.3}% PSNR {:.2} dB",
                sc.cat, sc.name, c.max_diff, c.mismatch_pct, c.psnr
            )
            .unwrap();
            if let Some(dir) = &dump {
                std::fs::create_dir_all(dir).unwrap();
                dump_png(
                    dir,
                    &format!("{}_{}_rr", sc.cat, sc.name),
                    sc.w,
                    sc.h,
                    a.clone(),
                );
                dump_png(
                    dir,
                    &format!("{}_{}_ts", sc.cat, sc.name),
                    sc.w,
                    sc.h,
                    b.clone(),
                );
            }
        }
        let idx = match cats.iter().position(|(k, _)| *k == sc.cat) {
            Some(i) => i,
            None => {
                cats.push((
                    sc.cat,
                    Agg {
                        min_psnr: f64::INFINITY,
                        ..Default::default()
                    },
                ));
                cats.len() - 1
            }
        };
        for agg in [&mut cats[idx].1, &mut total] {
            agg.n += 1;
            if c.max_diff == 0 {
                agg.identical += 1;
            }
            agg.max_diff = agg.max_diff.max(c.max_diff);
            agg.min_psnr = agg.min_psnr.min(c.psnr);
            agg.sum_mismatch += c.mismatch_pct;
            agg.worst_mismatch = agg.worst_mismatch.max(c.mismatch_pct);
        }
    }

    let fmt_psnr = |p: f64| {
        if p.is_infinite() {
            "inf".to_string()
        } else {
            format!("{p:.2}")
        }
    };
    println!(
        "\nrusty_raster vs tiny-skia 0.12.0 ({} scenes)",
        scenes.len()
    );
    println!(
        "| category | scenes | pixel-identical | max channel diff | worst mismatch % | mean mismatch % | min PSNR dB |"
    );
    println!("|---|---:|---:|---:|---:|---:|---:|");
    for (k, a) in cats.iter().chain(std::iter::once(&("TOTAL", total))) {
        println!(
            "| {} | {} | {} | {} | {:.3} | {:.4} | {} |",
            k,
            a.n,
            a.identical,
            a.max_diff,
            a.worst_mismatch,
            a.sum_mismatch / a.n.max(1) as f64,
            fmt_psnr(a.min_psnr)
        );
    }
    println!("scenes where both refused to render: {both_none}; tiny-skia panics: {oracle_panics}");
    if !diffs.is_empty() {
        println!("differing scenes:\n{diffs}");
    }

    let n: usize = cats.iter().map(|(_, a)| a.n).sum();
    assert!(n > 600, "corpus too small: {n}");
    assert_eq!(oracle_panics, 0);
    // The one documented divergence: `recip_fast` is exact here and `rcpps`
    // (a vendor-specific 12-bit approximation) in SSE2 tiny-skia, and only the
    // ColorDodge / ColorBurn blend modes use it. Every other scene must be
    // bit-identical, and those may differ by at most one level.
    for (sc, max_diff) in &differing {
        assert!(
            uses_dodge_or_burn(sc),
            "unexpected oracle difference in {}/{}",
            sc.cat,
            sc.name
        );
        assert!(
            *max_diff <= 1,
            "{}/{} differs by {max_diff}",
            sc.cat,
            sc.name
        );
    }
}

fn uses_dodge_or_burn(sc: &Scene) -> bool {
    // Indices 18 and 19 in the blend table are ColorDodge and ColorBurn.
    let is = |b: u8| matches!(b % 29, 18 | 19);
    sc.ops.iter().any(|op| match op {
        Op::Fill { paint, .. } | Op::Stroke { paint, .. } | Op::FillRect { paint, .. } => {
            is(paint.blend)
        }
        Op::Pixmap { blend, .. } => is(*blend),
    })
}
