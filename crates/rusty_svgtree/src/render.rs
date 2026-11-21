//! Renders the tree with rusty_raster, mapping it onto the rasteriser the
//! way resvg maps usvg onto tiny-skia: isolated groups render into a layer
//! sized to their layer bounding box, clip paths and masks are applied to the
//! layer as alpha masks, and the layer is composited with `draw_pixmap`.
//!
//! Memory is bounded: layers are clipped to resvg's `(-2w, -2h, 5w, 5h)`
//! area (kept for pixel parity: the layer offset feeds the rasteriser's
//! sub-pixel positions), and the bytes held by live layers are capped; a
//! group that would exceed the cap is skipped instead of allocated.

use rusty_raster::{
    BlendMode, Color, FillRule, FilterQuality, IntRect, Mask, MaskType, Paint, PathBuilder, Pixmap,
    PixmapMut, PixmapPaint, SpreadMode, Transform,
};

use crate::tree::{self, ClipPath, Group, Node, PaintOrder};

pub(crate) struct Context {
    /// The area layers may cover, in destination pixels: resvg's
    /// `(-2w, -2h, 5w, 5h)`, so layer offsets (and therefore anti-aliasing)
    /// match resvg exactly. Memory stays bounded by `max_live_bytes`.
    max_bbox: IntRect,
    /// Destination size, for the pattern-tile cap.
    canvas: (u32, u32),
    /// Bytes currently held by layers, clip and mask scratch pixmaps.
    live_bytes: u64,
    max_live_bytes: u64,
    max_image_pixels: u64,
}

impl Context {
    pub(crate) fn new(
        width: u32,
        height: u32,
        max_live_bytes: u64,
        max_image_pixels: u64,
    ) -> Option<Self> {
        Some(Context {
            max_bbox: IntRect::from_xywh(
                i32::try_from(width).ok()?.saturating_mul(-2),
                i32::try_from(height).ok()?.saturating_mul(-2),
                width.saturating_mul(5),
                height.saturating_mul(5),
            )?,
            canvas: (width, height),
            live_bytes: 0,
            max_live_bytes,
            max_image_pixels,
        })
    }

    /// Reserves a `w x h` scratch pixmap against the budget.
    fn alloc(&mut self, w: u32, h: u32) -> Option<Pixmap> {
        let bytes = u64::from(w) * u64::from(h) * 4;
        if self.live_bytes.saturating_add(bytes) > self.max_live_bytes {
            return None;
        }
        let p = Pixmap::new(w, h)?;
        self.live_bytes += bytes;
        Some(p)
    }

    fn free(&mut self, p: &Pixmap) {
        let bytes = u64::from(p.width()) * u64::from(p.height()) * 4;
        self.live_bytes = self.live_bytes.saturating_sub(bytes);
    }

    /// Reserves bytes for a mask (1 byte per pixel).
    fn alloc_mask(&mut self, w: u32, h: u32) -> Option<Mask> {
        let bytes = u64::from(w) * u64::from(h);
        if self.live_bytes.saturating_add(bytes) > self.max_live_bytes {
            return None;
        }
        let m = Mask::new(w, h)?;
        self.live_bytes += bytes;
        Some(m)
    }

    fn free_mask(&mut self, w: u32, h: u32) {
        self.live_bytes = self.live_bytes.saturating_sub(u64::from(w) * u64::from(h));
    }
}

pub(crate) fn render_nodes(
    parent: &Group,
    ctx: &mut Context,
    ts: Transform,
    pixmap: &mut PixmapMut,
) {
    for node in &parent.children {
        render_node(node, ctx, ts, pixmap);
    }
}

fn render_node(node: &Node, ctx: &mut Context, ts: Transform, pixmap: &mut PixmapMut) {
    match node {
        Node::Group(g) => {
            render_group(g, ctx, ts, pixmap);
        }
        Node::Path(p) => render_path(p, BlendMode::SourceOver, ctx, ts, pixmap),
        Node::Image(i) => {
            render_image(i, ctx, ts, pixmap);
        }
    }
}

fn fit_to_rect(r: IntRect, bounds: IntRect) -> Option<IntRect> {
    IntRect::from_ltrb(
        r.left().max(bounds.left()),
        r.top().max(bounds.top()),
        r.right().min(bounds.right()),
        r.bottom().min(bounds.bottom()),
    )
}

fn render_group(
    group: &Group,
    ctx: &mut Context,
    ts: Transform,
    pixmap: &mut PixmapMut,
) -> Option<()> {
    let ts = ts.pre_concat(group.transform);
    if !group.should_isolate() {
        render_nodes(group, ctx, ts, pixmap);
        return Some(());
    }
    let bbox = group.layer_bounding_box.transform(ts)?;
    let fx = bbox.x().floor();
    let fy = bbox.y().floor();
    let cw = bbox.width().ceil();
    let ch = bbox.height().ceil();
    if !(fx.is_finite() && fy.is_finite() && cw.is_finite() && ch.is_finite()) {
        return None;
    }
    // Saturating float-to-int casts, then the resvg 2px padding.
    let ibbox = IntRect::from_xywh(
        (fx as i32).checked_sub(2)?,
        (fy as i32).checked_sub(2)?,
        (cw as u32).checked_add(4)?,
        (ch as u32).checked_add(4)?,
    )?;
    let ibbox = fit_to_rect(ibbox, ctx.max_bbox)?;

    let shift = Transform::from_translate(-ibbox.x() as f32, -ibbox.y() as f32);
    let ts = shift.pre_concat(ts);

    let mut sub = ctx.alloc(ibbox.width(), ibbox.height())?;
    render_nodes(group, ctx, ts, &mut sub.as_mut());
    if let Some(clip) = &group.clip_path {
        apply_clip(clip, ctx, ts, &mut sub);
    }
    if let Some(mask) = &group.mask {
        apply_mask(mask, ctx, ts, &mut sub);
    }
    let paint = PixmapPaint {
        opacity: group.opacity,
        blend_mode: group.blend_mode,
        quality: FilterQuality::Nearest,
    };
    pixmap.draw_pixmap(
        ibbox.x(),
        ibbox.y(),
        sub.as_ref(),
        &paint,
        Transform::identity(),
        None,
    );
    ctx.free(&sub);
    Some(())
}

// ----------------------------------------------------------------- paths --

fn render_path(
    path: &tree::Path,
    blend: BlendMode,
    ctx: &mut Context,
    ts: Transform,
    pixmap: &mut PixmapMut,
) {
    if !path.visible {
        return;
    }
    if path.paint_order == PaintOrder::FillAndStroke {
        fill_path(path, blend, ctx, ts, pixmap);
        stroke_path(path, blend, ctx, ts, pixmap);
    } else {
        stroke_path(path, blend, ctx, ts, pixmap);
        fill_path(path, blend, ctx, ts, pixmap);
    }
}

/// Builds a rasteriser paint. A pattern tile is rendered into `holder`,
/// which must outlive the paint.
fn make_paint<'a>(
    p: &tree::Paint,
    opacity: f32,
    ctx: &mut Context,
    ts: Transform,
    holder: &'a mut Option<Pixmap>,
) -> Option<Paint<'a>> {
    let mut paint = Paint::default();
    match p {
        tree::Paint::Color(c) => {
            paint.set_color_rgba8(c.red, c.green, c.blue, opacity_to_u8(opacity));
        }
        tree::Paint::LinearGradient(g) => {
            let (mode, stops) = convert_base(&g.base, opacity);
            paint.shader = rusty_raster::LinearGradient::new(
                (g.x1, g.y1).into(),
                (g.x2, g.y2).into(),
                stops,
                mode,
                g.base.transform,
            )?;
        }
        tree::Paint::RadialGradient(g) => {
            let (mode, stops) = convert_base(&g.base, opacity);
            paint.shader = rusty_raster::RadialGradient::new(
                (g.fx, g.fy).into(),
                g.fr,
                (g.cx, g.cy).into(),
                g.r,
                stops,
                mode,
                g.base.transform,
            )?;
        }
        tree::Paint::Pattern(pat) => {
            let (pix, pts) = render_pattern_pixmap(pat, ctx, ts)?;
            *holder = Some(pix);
            let pix = holder.as_ref()?;
            paint.shader = rusty_raster::Pattern::new(
                pix.as_ref(),
                SpreadMode::Repeat,
                FilterQuality::Bicubic,
                opacity,
                pts,
            );
        }
    }
    Some(paint)
}

fn opacity_to_u8(o: f32) -> u8 {
    // strict-num `NormalizedF32::to_u8`, as resvg uses.
    (o * 255.0 + 0.5) as u8
}

fn convert_base(
    g: &tree::BaseGradient,
    opacity: f32,
) -> (SpreadMode, Vec<rusty_raster::GradientStop>) {
    let stops = g
        .stops
        .iter()
        .map(|s| {
            let a = s.opacity * opacity;
            let c = Color::from_rgba8(s.color.red, s.color.green, s.color.blue, opacity_to_u8(a));
            rusty_raster::GradientStop::new(s.offset, c)
        })
        .collect();
    (g.spread, stops)
}

fn render_pattern_pixmap(
    pat: &tree::Pattern,
    ctx: &mut Context,
    ts: Transform,
) -> Option<(Pixmap, Transform)> {
    let (sx, sy) = ts.pre_concat(pat.transform).get_scale();
    let rect = pat.rect;
    let w = (rect.width() * sx).round();
    let h = (rect.height() * sy).round();
    if !(w.is_finite() && h.is_finite() && w >= 1.0 && h >= 1.0) {
        return None;
    }
    // A pattern tile larger than the destination is never needed in full
    // detail and is the classic memory bomb; refuse it.
    let (w, h) = (w as u32, h as u32);
    if u64::from(w) * u64::from(h) > u64::from(ctx.canvas.0) * u64::from(ctx.canvas.1) * 4 + 4096 {
        return None;
    }
    let mut pixmap = ctx.alloc(w, h)?;
    let tile_ts = Transform::from_scale(sx, sy);
    render_nodes(&pat.root, ctx, tile_ts, &mut pixmap.as_mut());
    ctx.free(&pixmap);
    let pts = Transform::identity()
        .pre_concat(pat.transform)
        .pre_translate(rect.x(), rect.y())
        .pre_scale(1.0 / sx, 1.0 / sy);
    Some((pixmap, pts))
}

pub(crate) fn fill_path(
    path: &tree::Path,
    blend: BlendMode,
    ctx: &mut Context,
    ts: Transform,
    pixmap: &mut PixmapMut,
) -> Option<()> {
    let fill = path.fill.as_ref()?;
    let b = path.data.bounds();
    if b.width() == 0.0 || b.height() == 0.0 {
        return None;
    }
    let mut holder = None;
    let mut paint = make_paint(&fill.paint, fill.opacity, ctx, ts, &mut holder)?;
    paint.anti_alias = path.anti_alias;
    paint.blend_mode = blend;
    pixmap.fill_path(&path.data, &paint, fill.rule, ts, None);
    Some(())
}

fn stroke_path(
    path: &tree::Path,
    blend: BlendMode,
    ctx: &mut Context,
    ts: Transform,
    pixmap: &mut PixmapMut,
) -> Option<()> {
    let stroke = path.stroke.as_ref()?;
    let mut holder = None;
    let mut paint = make_paint(&stroke.paint, stroke.opacity, ctx, ts, &mut holder)?;
    paint.anti_alias = path.anti_alias;
    paint.blend_mode = blend;
    pixmap.stroke_path(&path.data, &paint, &stroke.to_raster(), ts, None);
    Some(())
}

// ------------------------------------------------------------ clip, mask --

fn apply_clip(clip: &ClipPath, ctx: &mut Context, ts: Transform, pixmap: &mut Pixmap) {
    let (w, h) = (pixmap.width(), pixmap.height());
    let Some(mut clip_pixmap) = ctx.alloc(w, h) else {
        // Out of budget: clip everything rather than leak unclipped content.
        pixmap.fill(Color::TRANSPARENT);
        return;
    };
    clip_pixmap.fill(Color::BLACK);
    draw_clip_children(
        &clip.root,
        BlendMode::Clear,
        ts.pre_concat(clip.transform),
        ctx,
        &mut clip_pixmap.as_mut(),
    );
    if let Some(inner) = &clip.clip_path {
        apply_clip(inner, ctx, ts, pixmap);
    }
    let mut mask = Mask::from_pixmap(clip_pixmap.as_ref(), MaskType::Alpha);
    mask.invert();
    pixmap.apply_mask(&mask);
    ctx.free(&clip_pixmap);
}

fn draw_clip_children(
    parent: &Group,
    mode: BlendMode,
    ts: Transform,
    ctx: &mut Context,
    pixmap: &mut PixmapMut,
) {
    for child in &parent.children {
        match child {
            Node::Path(p) => {
                if !p.visible {
                    continue;
                }
                fill_path(p, mode, ctx, ts, pixmap);
            }
            Node::Group(g) => {
                let ts = ts.pre_concat(g.transform);
                if let Some(clip) = &g.clip_path {
                    clip_group(g, clip, ts, ctx, pixmap);
                } else {
                    draw_clip_children(g, mode, ts, ctx, pixmap);
                }
            }
            Node::Image(_) => {}
        }
    }
}

fn clip_group(
    children: &Group,
    clip: &ClipPath,
    ts: Transform,
    ctx: &mut Context,
    pixmap: &mut PixmapMut,
) -> Option<()> {
    let mut clip_pixmap = ctx.alloc(pixmap.width(), pixmap.height())?;
    draw_clip_children(
        children,
        BlendMode::SourceOver,
        ts,
        ctx,
        &mut clip_pixmap.as_mut(),
    );
    apply_clip(clip, ctx, ts, &mut clip_pixmap);
    let paint = PixmapPaint {
        blend_mode: BlendMode::Xor,
        ..PixmapPaint::default()
    };
    pixmap.draw_pixmap(
        0,
        0,
        clip_pixmap.as_ref(),
        &paint,
        Transform::identity(),
        None,
    );
    ctx.free(&clip_pixmap);
    Some(())
}

fn apply_mask(mask: &tree::Mask, ctx: &mut Context, ts: Transform, pixmap: &mut Pixmap) {
    if mask.root.children.is_empty() {
        pixmap.fill(Color::TRANSPARENT);
        return;
    }
    let (w, h) = (pixmap.width(), pixmap.height());
    let Some(mut mask_pixmap) = ctx.alloc(w, h) else {
        pixmap.fill(Color::TRANSPARENT);
        return;
    };
    {
        let Some(mut alpha_mask) = ctx.alloc_mask(w, h) else {
            ctx.free(&mask_pixmap);
            pixmap.fill(Color::TRANSPARENT);
            return;
        };
        alpha_mask.fill_path(
            &PathBuilder::from_rect(mask.rect.to_rect()),
            FillRule::Winding,
            true,
            ts,
        );
        render_nodes(&mask.root, ctx, ts, &mut mask_pixmap.as_mut());
        mask_pixmap.apply_mask(&alpha_mask);
        ctx.free_mask(w, h);
    }
    if let Some(inner) = &mask.mask {
        apply_mask(inner, ctx, ts, pixmap);
    }
    let m = Mask::from_pixmap(mask_pixmap.as_ref(), mask.kind);
    pixmap.apply_mask(&m);
    ctx.free(&mask_pixmap);
}

// ---------------------------------------------------------------- images --

fn render_image(
    image: &tree::Image,
    ctx: &mut Context,
    ts: Transform,
    pixmap: &mut PixmapMut,
) -> Option<()> {
    if !image.visible {
        return None;
    }
    let raster = crate::image::decode(&image.kind, ctx.max_image_pixels)?;
    let rect = rusty_raster::Size::from_wh(raster.width() as f32, raster.height() as f32)?
        .to_rect(0.0, 0.0)?;
    let pattern = rusty_raster::Pattern::new(
        raster.as_ref(),
        SpreadMode::Pad,
        image.quality,
        1.0,
        Transform::identity(),
    );
    let paint = Paint {
        shader: pattern,
        ..Paint::default()
    };
    pixmap.fill_rect(rect, &paint, ts, None);
    Some(())
}
