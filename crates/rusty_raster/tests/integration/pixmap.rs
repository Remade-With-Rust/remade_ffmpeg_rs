use rusty_raster::*;

#[test]
fn clone_rect_1() {
    let mut pixmap = Pixmap::new(200, 200).unwrap();

    let mut paint = Paint::default();
    paint.set_color_rgba8(50, 127, 150, 200);
    paint.anti_alias = true;

    pixmap.fill_path(
        &PathBuilder::from_circle(100.0, 100.0, 80.0).unwrap(),
        &paint,
        FillRule::Winding,
        Transform::identity(),
        None,
    );

    let part = pixmap.as_ref().clone_rect(IntRect::from_xywh(10, 15, 80, 90).unwrap()).unwrap();

    let expected = Pixmap::load_png("tests/images/pixmap/clone-rect-1.png").unwrap();
    assert_eq!(part, expected);
}

#[test]
fn clone_rect_2() {
    let mut pixmap = Pixmap::new(200, 200).unwrap();

    let mut paint = Paint::default();
    paint.set_color_rgba8(50, 127, 150, 200);
    paint.anti_alias = true;

    pixmap.fill_path(
        &PathBuilder::from_circle(100.0, 100.0, 80.0).unwrap(),
        &paint,
        FillRule::Winding,
        Transform::identity(),
        None,
    );

    let part = pixmap.as_ref().clone_rect(IntRect::from_xywh(130, 120, 80, 90).unwrap()).unwrap();

    let expected = Pixmap::load_png("tests/images/pixmap/clone-rect-2.png").unwrap();
    assert_eq!(part, expected);
}

#[test]
fn clone_rect_out_of_bound() {
    let mut pixmap = Pixmap::new(200, 200).unwrap();

    let mut paint = Paint::default();
    paint.set_color_rgba8(50, 127, 150, 200);
    paint.anti_alias = true;

    pixmap.fill_path(
        &PathBuilder::from_circle(100.0, 100.0, 80.0).unwrap(),
        &paint,
        FillRule::Winding,
        Transform::identity(),
        None,
    );

    assert!(pixmap.as_ref().clone_rect(IntRect::from_xywh(250, 15, 80, 90).unwrap()).is_none());
    assert!(pixmap.as_ref().clone_rect(IntRect::from_xywh(10, 250, 80, 90).unwrap()).is_none());
    assert!(pixmap.as_ref().clone_rect(IntRect::from_xywh(10, -250, 80, 90).unwrap()).is_none());
}

#[test]
fn fill() {
    let c = Color::from_rgba8(50, 100, 150, 200);
    let mut pixmap = Pixmap::new(10, 10).unwrap();
    pixmap.fill(c);
    assert_eq!(pixmap.pixel(1, 1).unwrap(), c.premultiply().to_color_u8());
}

#[test]
fn draw_pixmap() {
    // Tests that painting algorithm will switch `Bicubic`/`Bilinear` to `Nearest`.
    // Otherwise we will get a blurry image.

    // A pixmap with the bottom half filled with solid color.
    let sub_pixmap = {
        let mut paint = Paint::default();
        paint.set_color_rgba8(50, 127, 150, 200);
        paint.anti_alias = false;

        let rect = Rect::from_xywh(0.0, 50.0, 100.0, 50.0).unwrap();

        let mut pixmap = Pixmap::new(100, 100).unwrap();
        pixmap.fill_rect(rect, &paint, Transform::identity(), None);
        pixmap
    };

    let mut paint = PixmapPaint::default();
    paint.quality = FilterQuality::Bicubic;

    let mut pixmap = Pixmap::new(200, 200).unwrap();
    pixmap.draw_pixmap(20, 20, sub_pixmap.as_ref(), &paint, Transform::identity(), None);

    let expected = Pixmap::load_png("tests/images/canvas/draw-pixmap.png").unwrap();
    assert_eq!(pixmap, expected);
}

/// `draw_pixmap` without a transform takes the sprite path. It has to give the
/// bytes of the general path, which is what filling the same rect with the
/// same `Pattern` runs: every offset, clip, opacity and kind of source pixel.
#[test]
fn draw_pixmap_sprite_matches_pipeline() {
    let mut seed = 0x9e37_79b9_7f4a_7c15u64;
    let mut next = move || {
        seed ^= seed << 13;
        seed ^= seed >> 7;
        seed ^= seed << 17;
        seed
    };

    for case in 0..160 {
        let (sw, sh) = (1 + (next() % 70) as u32, 1 + (next() % 40) as u32);
        let mut data = vec![0u8; (sw * sh * 4) as usize];
        for (i, px) in data.chunks_exact_mut(4).enumerate() {
            // Runs of transparent, opaque, translucent and (deliberately)
            // not premultiplied pixels, long enough in most cases to fill
            // whole groups of eight.
            let v = next().to_le_bytes();
            match (i / (1 + case % 29)) % 5 {
                0 | 1 => px.fill(0),
                2 => px.copy_from_slice(&[v[0], v[1], v[2], 255]),
                3 => {
                    let a = v[3];
                    let m = |c: u8| (u16::from(c) * u16::from(a) / 255) as u8;
                    px.copy_from_slice(&[m(v[0]), m(v[1]), m(v[2]), a]);
                }
                _ => px.copy_from_slice(&[v[0], v[1], v[2], v[3]]),
            }
        }
        let src = Pixmap::from_vec(data, IntSize::from_wh(sw, sh).unwrap()).unwrap();

        let (dw, dh) = (1 + (next() % 90) as u32, 1 + (next() % 50) as u32);
        let mut canvas = Pixmap::new(dw, dh).unwrap();
        for b in canvas.data_mut() {
            *b = next() as u8;
        }

        let x = (next() % 200) as i32 - 100;
        let y = (next() % 120) as i32 - 60;
        let (x, y) = if case % 7 == 0 { (0, 0) } else { (x, y) };
        let opacity = [1.0, 1.0, 0.7, 0.0, 0.31, 2.5, f32::NAN][case % 7];
        let quality = [FilterQuality::Nearest, FilterQuality::Bilinear, FilterQuality::Bicubic][case % 3];

        let mut sprite = canvas.clone();
        let paint = PixmapPaint { opacity, blend_mode: BlendMode::SourceOver, quality };
        sprite.draw_pixmap(x, y, src.as_ref(), &paint, Transform::identity(), None);

        let mut general = canvas.clone();
        let pattern = Paint {
            shader: Pattern::new(
                src.as_ref(),
                SpreadMode::Pad,
                quality,
                opacity,
                Transform::from_translate(x as f32, y as f32),
            ),
            blend_mode: BlendMode::SourceOver,
            anti_alias: false,
            force_hq_pipeline: false,
            colorspace: ColorSpace::default(),
        };
        let rect = IntRect::from_xywh(x, y, sw, sh).unwrap().to_rect();
        general.fill_rect(rect, &pattern, Transform::identity(), None);

        assert!(
            sprite == general,
            "case {case}: {sw}x{sh} at ({x}, {y}) on {dw}x{dh}, opacity {opacity}"
        );
    }
}

#[test]
fn draw_pixmap_ts() {
    let triangle = {
        let mut paint = Paint::default();
        paint.set_color_rgba8(50, 127, 150, 200);
        paint.anti_alias = true;

        let mut pb = PathBuilder::new();
        pb.move_to(0.0, 100.0);
        pb.line_to(100.0, 100.0);
        pb.line_to(50.0, 0.0);
        pb.close();
        let path = pb.finish().unwrap();

        let mut pixmap = Pixmap::new(100, 100).unwrap();
        pixmap.fill_path(&path, &paint, FillRule::Winding, Transform::identity(), None);
        pixmap
    };

    let mut paint = PixmapPaint::default();
    paint.quality = FilterQuality::Bicubic;

    let mut pixmap = Pixmap::new(200, 200).unwrap();
    pixmap.draw_pixmap(
        5, 10,
        triangle.as_ref(),
        &paint,
        Transform::from_row(1.2, 0.5, 0.5, 1.2, 0.0, 0.0),
        None,
    );

    let expected = Pixmap::load_png("tests/images/canvas/draw-pixmap-ts.png").unwrap();
    assert_eq!(pixmap, expected);
}

#[test]
fn draw_pixmap_opacity() {
    let triangle = {
        let mut paint = Paint::default();
        paint.set_color_rgba8(50, 127, 150, 200);
        paint.anti_alias = true;

        let mut pb = PathBuilder::new();
        pb.move_to(0.0, 100.0);
        pb.line_to(100.0, 100.0);
        pb.line_to(50.0, 0.0);
        pb.close();
        let path = pb.finish().unwrap();

        let mut pixmap = Pixmap::new(100, 100).unwrap();
        pixmap.fill_path(&path, &paint, FillRule::Winding, Transform::identity(), None);
        pixmap
    };

    let mut paint = PixmapPaint::default();
    paint.quality = FilterQuality::Bicubic;
    paint.opacity = 0.5;

    let mut pixmap = Pixmap::new(200, 200).unwrap();
    pixmap.draw_pixmap(
        5, 10,
        triangle.as_ref(),
        &paint,
        Transform::from_row(1.2, 0.5, 0.5, 1.2, 0.0, 0.0),
        None,
    );

    let expected = Pixmap::load_png("tests/images/canvas/draw-pixmap-opacity.png").unwrap();
    assert_eq!(pixmap, expected);
}
