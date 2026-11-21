//! Hostile inputs: non-finite and huge coordinates, degenerate transforms,
//! extreme strokes and dashes, zero-size and oversized targets. Every call must
//! return (rendering nothing, or something clipped) without panicking, and in
//! bounded time and memory.
//!
//! `RR_FUZZ_ITERS=<n>` raises the iteration count of the random sweep;
//! `RR_FUZZ_TRACE=1` prints each iteration (and stroke parameters) to stderr.

use rusty_raster::*;

const NASTY: [f32; 14] = [
    f32::NAN,
    f32::INFINITY,
    f32::NEG_INFINITY,
    f32::MAX,
    f32::MIN,
    1.0e30,
    -1.0e30,
    1.0e15,
    1.0e-30,
    -1.0e-30,
    f32::MIN_POSITIVE,
    0.0,
    -0.0,
    1.0e7,
];

fn canvas() -> Pixmap {
    Pixmap::new(64, 64).unwrap()
}

fn paint() -> Paint<'static> {
    let mut p = Paint::default();
    p.set_color_rgba8(10, 200, 30, 255);
    p.anti_alias = true;
    p
}

fn ts(sx: f32, ky: f32, kx: f32, sy: f32, tx: f32, ty: f32) -> Transform {
    Transform::from_row(sx, ky, kx, sy, tx, ty)
}

fn shape() -> Path {
    let mut pb = PathBuilder::new();
    pb.move_to(5.0, 5.0);
    pb.cubic_to(60.0, 0.0, 0.0, 60.0, 55.0, 55.0);
    pb.quad_to(10.0, 60.0, 5.0, 30.0);
    pb.close();
    pb.push_circle(32.0, 32.0, 10.0);
    pb.finish().unwrap()
}

#[test]
fn zero_and_oversized_pixmaps() {
    assert!(Pixmap::new(0, 0).is_none());
    assert!(Pixmap::new(0, 10).is_none());
    assert!(Pixmap::new(10, 0).is_none());
    assert!(Pixmap::new(u32::MAX, u32::MAX).is_none());
    assert!(Pixmap::new(u32::MAX / 4, 1).is_none());
    // 2^31 x 2^31 pixels: rejected by the size arithmetic, no allocation attempted.
    assert!(Pixmap::new(1 << 31, 1 << 31).is_none());
    // A request the size arithmetic accepts but the allocator cannot satisfy
    // (5e8 x 5e8 x 4 bytes = 1e18) returns `None` instead of aborting.
    assert!(Pixmap::new(500_000_000, 500_000_000).is_none());
    assert!(Mask::new(2_000_000_000, 2_000_000_000).is_none());
    assert!(Mask::new(0, 5).is_none());
    assert!(Mask::new(1 << 30, 1 << 30).is_none());

    let mut empty: [u8; 0] = [];
    assert!(PixmapMut::from_bytes(&mut empty, 0, 0).is_none());
    let mut short = [0u8; 15];
    assert!(PixmapMut::from_bytes(&mut short, 2, 2).is_none());
    let mut exact = [0u8; 16];
    let mut pm = PixmapMut::from_bytes(&mut exact, 2, 2).unwrap();
    pm.fill_path(
        &shape(),
        &paint(),
        FillRule::Winding,
        Transform::identity(),
        None,
    );
}

#[test]
fn one_pixel_targets() {
    for (w, h) in [(1, 1), (1, 300), (300, 1)] {
        let mut pm = Pixmap::new(w, h).unwrap();
        pm.fill_path(
            &shape(),
            &paint(),
            FillRule::EvenOdd,
            ts(9.0, 0.0, 0.0, 9.0, -50.0, -50.0),
            None,
        );
        let s = Stroke {
            width: 3.0,
            ..Stroke::default()
        };
        pm.stroke_path(&shape(), &paint(), &s, Transform::identity(), None);
        pm.stroke_path(
            &shape(),
            &paint(),
            &Stroke::default(),
            Transform::identity(),
            None,
        );
    }
}

#[test]
fn non_finite_coordinates_never_make_a_path() {
    for &v in &NASTY[..5] {
        let mut pb = PathBuilder::new();
        pb.move_to(0.0, 0.0);
        pb.line_to(v, 10.0);
        pb.line_to(10.0, 10.0);
        if !v.is_finite() {
            assert!(pb.finish().is_none(), "non-finite point {v} accepted");
        }
        // A non-finite FIRST point slipped past tiny-skia's bounds check.
        let mut pb = PathBuilder::new();
        pb.move_to(v, 51.0);
        pb.close();
        pb.move_to(-19.9, 587886.0);
        pb.line_to(25.0, 24.7);
        if !v.is_finite() {
            assert!(pb.finish().is_none(), "non-finite first point {v} accepted");
        }
        assert!(Rect::from_xywh(v, 0.0, 10.0, 10.0).is_none() || v.is_finite());
    }
}

#[test]
fn nasty_transforms_on_every_draw_call() {
    let path = shape();
    let mut src = Pixmap::new(8, 8).unwrap();
    src.fill(Color::from_rgba8(255, 0, 0, 128));
    let stops = || {
        vec![
            GradientStop::new(0.0, Color::BLACK),
            GradientStop::new(1.0, Color::WHITE),
        ]
    };
    let mut calls = 0usize;
    for &a in &NASTY {
        for &b in &NASTY {
            for t in [
                ts(a, 0.0, 0.0, b, 0.0, 0.0),
                ts(1.0, a, b, 1.0, 0.0, 0.0),
                ts(1.0, 0.0, 0.0, 1.0, a, b),
                ts(a, b, b, a, a, b),
            ] {
                let mut pm = canvas();
                pm.fill_path(&path, &paint(), FillRule::Winding, t, None);
                pm.fill_rect(
                    Rect::from_xywh(1.0, 1.0, 30.0, 30.0).unwrap(),
                    &paint(),
                    t,
                    None,
                );
                for width in [0.0, 1.0, 7.0] {
                    let s = Stroke {
                        width,
                        line_join: LineJoin::Round,
                        ..Stroke::default()
                    };
                    pm.stroke_path(&path, &paint(), &s, t, None);
                }
                pm.draw_pixmap(3, 3, src.as_ref(), &PixmapPaint::default(), t, None);
                let q = PixmapPaint {
                    quality: FilterQuality::Bicubic,
                    ..PixmapPaint::default()
                };
                pm.draw_pixmap(3, 3, src.as_ref(), &q, t, None);
                for shader in [
                    LinearGradient::new(
                        Point::from_xy(0.0, 0.0),
                        Point::from_xy(40.0, 9.0),
                        stops(),
                        SpreadMode::Repeat,
                        t,
                    ),
                    RadialGradient::new(
                        Point::from_xy(20.0, 20.0),
                        2.0,
                        Point::from_xy(30.0, 25.0),
                        20.0,
                        stops(),
                        SpreadMode::Reflect,
                        t,
                    ),
                    SweepGradient::new(
                        Point::from_xy(32.0, 32.0),
                        0.0,
                        270.0,
                        stops(),
                        SpreadMode::Pad,
                        t,
                    ),
                    Some(Pattern::new(
                        src.as_ref(),
                        SpreadMode::Repeat,
                        FilterQuality::Bilinear,
                        0.7,
                        t,
                    )),
                ]
                .into_iter()
                .flatten()
                {
                    let p = Paint {
                        shader,
                        anti_alias: true,
                        ..Paint::default()
                    };
                    pm.fill_path(&path, &p, FillRule::EvenOdd, Transform::identity(), None);
                }
                let mut mask = Mask::new(64, 64).unwrap();
                mask.fill_path(&path, FillRule::Winding, true, t);
                mask.intersect_path(&path, FillRule::EvenOdd, false, t);
                calls += 13;
            }
        }
    }
    assert!(calls > 10_000);
}

#[test]
fn nasty_gradient_parameters() {
    let mut pm = canvas();
    let path = shape();
    for &v in &NASTY {
        let stops = vec![
            GradientStop::new(v, Color::BLACK),
            GradientStop::new(1.0, Color::WHITE),
        ];
        let shaders = [
            LinearGradient::new(
                Point::from_xy(v, 0.0),
                Point::from_xy(10.0, v),
                stops.clone(),
                SpreadMode::Pad,
                Transform::identity(),
            ),
            RadialGradient::new(
                Point::from_xy(v, v),
                v.abs(),
                Point::from_xy(10.0, 10.0),
                v.abs(),
                stops.clone(),
                SpreadMode::Repeat,
                Transform::identity(),
            ),
            RadialGradient::new(
                Point::from_xy(5.0, 5.0),
                0.0,
                Point::from_xy(5.0, 5.0),
                v,
                stops.clone(),
                SpreadMode::Reflect,
                Transform::identity(),
            ),
            SweepGradient::new(
                Point::from_xy(v, 3.0),
                0.0,
                v,
                stops.clone(),
                SpreadMode::Pad,
                Transform::identity(),
            ),
        ];
        for shader in shaders.into_iter().flatten() {
            let p = Paint {
                shader,
                anti_alias: true,
                ..Paint::default()
            };
            pm.fill_path(&path, &p, FillRule::Winding, Transform::identity(), None);
        }
    }
    // Many stops, unsorted and duplicated positions.
    let stops: Vec<GradientStop> = (0..10_000)
        .map(|i| {
            GradientStop::new(
                ((i * 7919) % 1000) as f32 / 999.0,
                Color::from_rgba8((i % 256) as u8, 0, 0, 255),
            )
        })
        .collect();
    if let Some(shader) = LinearGradient::new(
        Point::from_xy(0.0, 0.0),
        Point::from_xy(64.0, 0.0),
        stops,
        SpreadMode::Pad,
        Transform::identity(),
    ) {
        let p = Paint {
            shader,
            ..Paint::default()
        };
        pm.fill_path(&path, &p, FillRule::Winding, Transform::identity(), None);
    }
}

#[test]
fn nasty_strokes_and_dashes() {
    let path = shape();
    let mut pm = canvas();
    for &w in &NASTY {
        for &m in &[f32::NAN, 0.0, -1.0, 1.0, 1.0e30, f32::INFINITY] {
            for join in [
                LineJoin::Miter,
                LineJoin::MiterClip,
                LineJoin::Round,
                LineJoin::Bevel,
            ] {
                for cap in [LineCap::Butt, LineCap::Round, LineCap::Square] {
                    let s = Stroke {
                        width: w,
                        miter_limit: m,
                        line_cap: cap,
                        line_join: join,
                        dash: None,
                    };
                    pm.stroke_path(&path, &paint(), &s, Transform::identity(), None);
                }
            }
        }
    }
    // Dash arrays: zero, tiny, huge, non-finite offsets.
    for arr in [
        vec![0.0, 0.0],
        vec![1.0e-30, 1.0e-30],
        vec![1.0e-3, 1.0e-3],
        vec![1.0e30, 1.0],
        vec![f32::MAX, f32::MAX],
        vec![0.0, 1.0e-6],
    ] {
        for off in [0.0, -1.0e30, 1.0e30, 0.5] {
            if let Some(dash) = StrokeDash::new(arr.clone(), off) {
                let s = Stroke {
                    width: 2.0,
                    dash: Some(dash),
                    ..Stroke::default()
                };
                pm.stroke_path(&path, &paint(), &s, Transform::identity(), None);
            }
        }
    }
    assert!(StrokeDash::new(vec![f32::NAN, 1.0], 0.0).is_none());
    assert!(StrokeDash::new(vec![1.0, 1.0], f32::NAN).is_none());
    assert!(StrokeDash::new(vec![-1.0, 1.0], 0.0).is_none());
}

#[test]
fn dense_dash_on_a_long_path_is_bounded() {
    // 1e7-unit line with a 0.01 dash period would be 1e9 dashes; the dasher
    // gives up past 1M segments (Skia's bound) instead of exhausting memory.
    let mut pb = PathBuilder::new();
    pb.move_to(0.0, 10.0);
    pb.line_to(1.0e7, 10.0);
    let path = pb.finish().unwrap();
    let dash = StrokeDash::new(vec![0.005, 0.005], 0.0).unwrap();
    assert!(path.dash(&dash, 1.0).is_none());
    let mut pm = canvas();
    let s = Stroke {
        width: 1.0,
        dash: Some(dash),
        ..Stroke::default()
    };
    let t0 = std::time::Instant::now();
    pm.stroke_path(&path, &paint(), &s, Transform::identity(), None);
    assert!(t0.elapsed().as_secs() < 5);
}

#[test]
fn huge_and_tiny_geometry() {
    let mut pm = canvas();
    // Finite but enormous coordinates.
    for &v in &[1.0e7f32, 1.0e15, 1.0e30, 3.0e38] {
        let mut pb = PathBuilder::new();
        pb.move_to(-v, -v);
        pb.line_to(v, 0.0);
        pb.quad_to(v, v, 0.0, v);
        pb.cubic_to(-v, v, -v, -v, 10.0, 10.0);
        pb.close();
        if let Some(p) = pb.finish() {
            pm.fill_path(&p, &paint(), FillRule::Winding, Transform::identity(), None);
            pm.stroke_path(
                &p,
                &paint(),
                &Stroke::default(),
                Transform::identity(),
                None,
            );
            pm.stroke_path(
                &p,
                &paint(),
                &Stroke {
                    width: 4.0,
                    ..Stroke::default()
                },
                Transform::identity(),
                None,
            );
        }
    }
    // The milestone-2 repro: a 1e-31 circle scaled up by 1e32 (a giant/tiny
    // SVG viewBox), stroked as a hairline and as a wide stroke.
    let tiny = PathBuilder::from_circle(1.0e-31, 1.0e-31, 1.0e-31).unwrap();
    for scale in [1.0e30f32, 1.0e32, 1.0e33, 1.0e35] {
        let t = Transform::from_scale(scale, scale);
        pm.stroke_path(&tiny, &paint(), &Stroke::default(), t, None);
        pm.stroke_path(
            &tiny,
            &paint(),
            &Stroke {
                width: 1.0e-31,
                ..Stroke::default()
            },
            t,
            None,
        );
        pm.fill_path(&tiny, &paint(), FillRule::Winding, t, None);
        let mut nonaa = paint();
        nonaa.anti_alias = false;
        pm.stroke_path(&tiny, &nonaa, &Stroke::default(), t, None);
    }
    // Many-segment path: memory stays proportional to the input.
    let mut pb = PathBuilder::new();
    pb.move_to(0.0, 0.0);
    for i in 0..200_000 {
        let f = i as f32;
        pb.line_to((f * 0.37) % 64.0, (f * 0.73) % 64.0);
    }
    let big = pb.finish().unwrap();
    pm.fill_path(
        &big,
        &paint(),
        FillRule::EvenOdd,
        Transform::identity(),
        None,
    );
    pm.stroke_path(
        &big,
        &paint(),
        &Stroke::default(),
        Transform::identity(),
        None,
    );
}

#[test]
fn mismatched_masks_are_ignored() {
    let mut pm = canvas();
    let mask = Mask::new(10, 10).unwrap();
    pm.fill_path(
        &shape(),
        &paint(),
        FillRule::Winding,
        Transform::identity(),
        Some(&mask),
    );
    pm.stroke_path(
        &shape(),
        &paint(),
        &Stroke::default(),
        Transform::identity(),
        Some(&mask),
    );
    pm.fill_rect(
        Rect::from_xywh(0.0, 0.0, 9.0, 9.0).unwrap(),
        &paint(),
        Transform::identity(),
        Some(&mask),
    );
    let src = Pixmap::new(4, 4).unwrap();
    pm.draw_pixmap(
        0,
        0,
        src.as_ref(),
        &PixmapPaint::default(),
        Transform::identity(),
        Some(&mask),
    );
    pm.apply_mask(&mask);
    // The draw calls refuse a mask of the wrong size; the canvas stays empty.
    assert!(pm.data().iter().all(|&b| b == 0));
}

#[test]
fn draw_pixmap_extreme_offsets() {
    let mut pm = canvas();
    let src = Pixmap::new(5, 5).unwrap();
    for (x, y) in [
        (i32::MIN, i32::MIN),
        (i32::MAX, i32::MAX),
        (i32::MIN, 0),
        (-4, -4),
        (63, 63),
    ] {
        pm.draw_pixmap(
            x,
            y,
            src.as_ref(),
            &PixmapPaint::default(),
            Transform::identity(),
            None,
        );
        pm.draw_pixmap(
            x,
            y,
            src.as_ref(),
            &PixmapPaint::default(),
            Transform::from_scale(3.0, 3.0),
            None,
        );
    }
}

/// Random sweep: random paths built from a mix of sane and nasty numbers, random
/// paints, strokes, dashes, transforms and masks, all through `catch_unwind`.
#[test]
fn random_hostile_sweep() {
    let iters: u64 = std::env::var("RR_FUZZ_ITERS")
        .ok()
        .and_then(|v| v.parse().ok())
        .unwrap_or(3000);
    let mut state = 0x2545_F491_4F6C_DD1Du64;
    let mut next = move || {
        state ^= state << 13;
        state ^= state >> 7;
        state ^= state << 17;
        state
    };
    let mut panics = Vec::new();
    for i in 0..iters {
        let seeds: Vec<u64> = (0..64).map(|_| next()).collect();
        if std::env::var_os("RR_FUZZ_TRACE").is_some() {
            eprintln!("iter {i}");
        }
        let r = std::panic::catch_unwind(|| run_one(&seeds));
        if r.is_err() {
            panics.push(i);
        }
    }
    assert!(
        panics.is_empty(),
        "{} of {iters} iterations panicked: {:?}",
        panics.len(),
        &panics[..panics.len().min(10)]
    );
}

fn run_one(seeds: &[u64]) {
    let mut k = 0usize;
    let mut take = || {
        let v = seeds[k % seeds.len()].rotate_left(k as u32 * 7);
        k += 1;
        v
    };
    let num = |take: &mut dyn FnMut() -> u64| -> f32 {
        let v = take();
        match v % 8 {
            0 => NASTY[(v >> 8) as usize % NASTY.len()],
            1 => ((v >> 8) % 2_000_000) as f32 - 1_000_000.0,
            _ => ((v >> 8) % 1000) as f32 / 10.0 - 20.0,
        }
    };
    let w = 1 + (take() % 90) as u32;
    let h = 1 + (take() % 90) as u32;
    let mut pm = Pixmap::new(w, h).unwrap();
    let mut pb = PathBuilder::new();
    for _ in 0..1 + take() % 10 {
        match take() % 6 {
            0 => pb.move_to(num(&mut take), num(&mut take)),
            1 | 2 => pb.line_to(num(&mut take), num(&mut take)),
            3 => pb.quad_to(
                num(&mut take),
                num(&mut take),
                num(&mut take),
                num(&mut take),
            ),
            4 => pb.cubic_to(
                num(&mut take),
                num(&mut take),
                num(&mut take),
                num(&mut take),
                num(&mut take),
                num(&mut take),
            ),
            _ => pb.close(),
        }
    }
    let path = match pb.finish() {
        Some(p) => p,
        None => PathBuilder::from_circle(10.0, 10.0, 8.0).unwrap(),
    };
    let t = Transform::from_row(
        num(&mut take),
        num(&mut take),
        num(&mut take),
        num(&mut take),
        num(&mut take),
        num(&mut take),
    );
    let t = if take() % 3 == 0 {
        t
    } else {
        Transform::from_scale(num(&mut take).abs().clamp(0.01, 50.0), 1.0)
    };
    let stops = vec![
        GradientStop::new(
            num(&mut take),
            Color::from_rgba8(take() as u8, take() as u8, take() as u8, take() as u8),
        ),
        GradientStop::new(
            num(&mut take),
            Color::from_rgba8(take() as u8, 0, 255, take() as u8),
        ),
    ];
    let shader = match take() % 4 {
        0 => LinearGradient::new(
            Point::from_xy(num(&mut take), num(&mut take)),
            Point::from_xy(num(&mut take), num(&mut take)),
            stops,
            SpreadMode::Reflect,
            t,
        ),
        1 => RadialGradient::new(
            Point::from_xy(num(&mut take), num(&mut take)),
            num(&mut take).abs(),
            Point::from_xy(num(&mut take), num(&mut take)),
            num(&mut take).abs(),
            stops,
            SpreadMode::Repeat,
            t,
        ),
        2 => SweepGradient::new(
            Point::from_xy(num(&mut take), num(&mut take)),
            num(&mut take),
            num(&mut take),
            stops,
            SpreadMode::Pad,
            t,
        ),
        _ => None,
    }
    .unwrap_or(Shader::SolidColor(Color::from_rgba8(1, 2, 3, 200)));
    let blend = [
        BlendMode::SourceOver,
        BlendMode::Multiply,
        BlendMode::ColorDodge,
        BlendMode::Hue,
        BlendMode::Xor,
        BlendMode::Clear,
    ][(take() % 6) as usize];
    let paint = Paint {
        shader,
        blend_mode: blend,
        anti_alias: take() % 4 != 0,
        ..Paint::default()
    };
    let mask = if take() % 4 == 0 {
        let mut m = Mask::new(w, h).unwrap();
        m.fill_path(&path, FillRule::EvenOdd, true, t);
        Some(m)
    } else {
        None
    };
    match take() % 3 {
        0 => pm.fill_path(
            &path,
            &paint,
            if take() % 2 == 0 {
                FillRule::Winding
            } else {
                FillRule::EvenOdd
            },
            t,
            mask.as_ref(),
        ),
        1 => {
            let dash = if take() % 3 == 0 {
                StrokeDash::new(
                    vec![num(&mut take).abs(), num(&mut take).abs()],
                    num(&mut take),
                )
            } else {
                None
            };
            let s = Stroke {
                width: num(&mut take),
                miter_limit: num(&mut take),
                line_cap: [LineCap::Butt, LineCap::Round, LineCap::Square][(take() % 3) as usize],
                line_join: [
                    LineJoin::Miter,
                    LineJoin::MiterClip,
                    LineJoin::Round,
                    LineJoin::Bevel,
                ][(take() % 4) as usize],
                dash,
            };
            if std::env::var_os("RR_FUZZ_TRACE").is_some() {
                eprintln!("stroke {s:?} ts {t:?} path {:?}", path);
            }
            pm.stroke_path(&path, &paint, &s, t, mask.as_ref());
        }
        _ => {
            let mut src = Pixmap::new(1 + (take() % 20) as u32, 1 + (take() % 20) as u32).unwrap();
            src.fill(Color::from_rgba8(200, 100, 50, 180));
            let q = [
                FilterQuality::Nearest,
                FilterQuality::Bilinear,
                FilterQuality::Bicubic,
            ][(take() % 3) as usize];
            let pp = PixmapPaint {
                opacity: num(&mut take),
                blend_mode: blend,
                quality: q,
            };
            pm.draw_pixmap(
                (take() % 200) as i32 - 100,
                (take() % 200) as i32 - 100,
                src.as_ref(),
                &pp,
                t,
                mask.as_ref(),
            );
        }
    }
}
