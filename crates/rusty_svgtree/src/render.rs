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
    BlendMode, Color, FillRule, FilterQuality, IntRect, IntSize, Mask, MaskType, Paint,
    PathBuilder, Pixmap, PixmapMut, PixmapPaint, PixmapRef, SpreadMode, Transform,
};

use crate::tree::{self, ClipPath, Group, Node, PaintOrder};

pub(crate) struct Context {
    /// The area layers may cover, in destination pixels: resvg's
    /// `(-2w, -2h, 5w, 5h)`, so layer offsets (and therefore anti-aliasing)
    /// match resvg exactly. Memory stays bounded by `max_live_bytes`.
    max_bbox: IntRect,
    /// Destination size, for the pattern-tile cap.
    canvas: (u32, u32),
    /// Bytes currently held by layers, clip, mask and filter scratch.
    live_bytes: u64,
    max_live_bytes: u64,
    max_image_pixels: u64,
    /// Filter work left, in pixel operations.
    filter_work: u64,
}

/// The area layers may cover on a canvas.
fn max_bbox(width: u32, height: u32) -> Option<IntRect> {
    IntRect::from_xywh(
        i32::try_from(width).ok()?.saturating_mul(-2),
        i32::try_from(height).ok()?.saturating_mul(-2),
        width.saturating_mul(5),
        height.saturating_mul(5),
    )
}

impl Context {
    pub(crate) fn new(
        width: u32,
        height: u32,
        max_live_bytes: u64,
        max_image_pixels: u64,
        max_filter_work: u64,
    ) -> Option<Self> {
        Some(Context {
            max_bbox: max_bbox(width, height)?,
            canvas: (width, height),
            live_bytes: 0,
            max_live_bytes,
            max_image_pixels,
            filter_work: max_filter_work,
        })
    }

    /// Spends filter work; `false` once the budget is gone.
    pub(crate) fn spend_filter_work(&mut self, n: u64) -> bool {
        match self.filter_work.checked_sub(n) {
            Some(left) => {
                self.filter_work = left;
                true
            }
            None => {
                self.filter_work = 0;
                false
            }
        }
    }

    /// Reserves scratch bytes against the layer budget.
    pub(crate) fn reserve(&mut self, bytes: u64) -> bool {
        let ok = self.live_bytes.saturating_add(bytes) <= self.max_live_bytes;
        if ok {
            self.live_bytes += bytes;
        }
        ok
    }

    /// Gives `reserve`d bytes back.
    pub(crate) fn release(&mut self, bytes: u64) {
        self.live_bytes = self.live_bytes.saturating_sub(bytes);
    }

    /// Replaces the area layers may cover, returning the old one.
    pub(crate) fn swap_max_bbox(&mut self, r: IntRect) -> IntRect {
        std::mem::replace(&mut self.max_bbox, r)
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
    nodes: &[Node],
    ctx: &mut Context,
    ts: Transform,
    pixmap: &mut PixmapMut,
) {
    for node in nodes {
        render_node(node, ctx, ts, pixmap);
    }
}

pub(crate) fn render_node(node: &Node, ctx: &mut Context, ts: Transform, pixmap: &mut PixmapMut) {
    match node {
        Node::Group(g) => {
            render_group(g, ctx, ts, pixmap);
        }
        Node::Path(p) => render_path(p, BlendMode::SourceOver, ctx, ts, pixmap),
        Node::Image(i) => {
            render_image(i, ctx, ts, pixmap);
        }
        Node::Text(t) => {
            if let Some(g) = &t.flattened {
                render_nodes(&g.children, ctx, ts, pixmap);
            }
        }
    }
}

pub(crate) fn fit_to_rect(r: IntRect, bounds: IntRect) -> Option<IntRect> {
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
        render_nodes(&group.children, ctx, ts, pixmap);
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
    // Saturating float-to-int casts, then the resvg 2px padding (a filter
    // region clips, so it gets none).
    let ibbox = if group.filters.is_empty() {
        IntRect::from_xywh(
            (fx as i32).checked_sub(2)?,
            (fy as i32).checked_sub(2)?,
            (cw as u32).checked_add(4)?,
            (ch as u32).checked_add(4)?,
        )?
    } else {
        IntRect::from_xywh(fx as i32, fy as i32, cw.max(1.0) as u32, ch.max(1.0) as u32)?
    };
    let ibbox = fit_to_rect(ibbox, ctx.max_bbox)?;

    let shift = Transform::from_translate(-ibbox.x() as f32, -ibbox.y() as f32);
    let ts = shift.pre_concat(ts);

    let mut sub = ctx.alloc(ibbox.width(), ibbox.height())?;
    render_nodes(&group.children, ctx, ts, &mut sub.as_mut());
    for filter in &group.filters {
        crate::filter_render::apply(filter, ctx, ts, &mut sub);
    }
    // A layer nothing was drawn into stays empty whatever clips or masks it,
    // and under source-over composites to nothing: it is done.
    if group.blend_mode == BlendMode::SourceOver
        && (group.clip_path.is_some() || group.mask.is_some())
        && is_blank(sub.data())
    {
        ctx.free(&sub);
        return Some(());
    }
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
    if !(group.blend_mode == BlendMode::SourceOver
        && draw_layer_content(&sub, ibbox, &paint, ctx, pixmap))
    {
        pixmap.draw_pixmap(
            ibbox.x(),
            ibbox.y(),
            sub.as_ref(),
            &paint,
            Transform::identity(),
            None,
        );
    }
    ctx.free(&sub);
    Some(())
}

/// A layer is composited without its blank columns (from a copy) only when
/// they are at least `1 / COLUMN_TRIM_SHARE` of its width.
const COLUMN_TRIM_SHARE: usize = 8;

/// Whether every byte is zero (every pixel transparent black).
fn is_blank(bytes: &[u8]) -> bool {
    let mut words = bytes.chunks_exact(8);
    words.all(|c| c == [0u8; 8]) && words.remainder().iter().all(|&b| b == 0)
}

/// The rectangle `(left, top, right, bottom)` of an RGBA buffer outside of
/// which every pixel is transparent black; `None` when they all are. The
/// columns are widened to the whole width once too few of them are blank to
/// be dropped (see `COLUMN_TRIM_SHARE`).
fn content_bounds(data: &[u8], w: usize, h: usize) -> Option<(usize, usize, usize, usize)> {
    let stride = w * 4;
    if stride == 0 || data.len() != stride * h {
        // Not a `w x h` buffer: nothing is known to be blank.
        return Some((0, 0, w, h));
    }
    let row = |y: usize| &data[y * stride..(y + 1) * stride];
    let mut top = 0;
    while top < h && is_blank(row(top)) {
        top += 1;
    }
    if top == h {
        return None;
    }
    let mut bottom = h;
    while is_blank(row(bottom - 1)) {
        bottom -= 1;
    }
    // Only the part of each row outside the columns found so far is read.
    let (mut left, mut right) = (w, 0);
    for y in top..bottom {
        if (left + (w - right)) * COLUMN_TRIM_SHARE < w {
            // Too few columns are left to drop for a copy without them to
            // be worth making (`draw_layer_content`): the rows are drawn
            // whole, so the columns need not be looked at any more.
            return Some((0, top, w, bottom));
        }
        let row = row(y);
        if let Some(i) = row[..left * 4].chunks_exact(4).position(|p| p != [0u8; 4]) {
            left = i;
        }
        if let Some(i) = row[right * 4..]
            .chunks_exact(4)
            .rposition(|p| p != [0u8; 4])
        {
            right += i + 1;
        }
    }
    Some((left, top, right, bottom))
}

/// Composites a source-over layer `sub` (placed at `ibbox`) by drawing only
/// the rectangle of it that holds anything. Under source-over a transparent
/// source pixel leaves the destination pixel as it is (the test
/// `transparent_source_leaves_destination` holds the rasteriser to that), and
/// the rectangle keeps its integer position, so the pixels are those of
/// compositing the whole layer. Returns `false` when the whole layer is to
/// be drawn after all (nothing to trim).
fn draw_layer_content(
    sub: &Pixmap,
    ibbox: IntRect,
    paint: &PixmapPaint,
    ctx: &Context,
    pixmap: &mut PixmapMut,
) -> bool {
    let (w, h) = (sub.width() as usize, sub.height() as usize);
    let stride = w * 4;
    let data = sub.data();
    let Some((left, top, right, bottom)) = content_bounds(data, w, h) else {
        // Nothing in the layer: nothing to draw.
        return true;
    };
    let (cw, ch) = (right - left, bottom - top);
    if cw == w && ch == h {
        return false;
    }
    let offset = |base: i32, by: usize| i32::try_from(by).ok().and_then(|v| base.checked_add(v));
    let (Some(x), Some(y)) = (offset(ibbox.x(), left), offset(ibbox.y(), top)) else {
        return false;
    };
    let (Ok(cw32), Ok(ch32)) = (u32::try_from(cw), u32::try_from(ch)) else {
        return false;
    };
    let rows = &data[top * stride..bottom * stride];
    // Whole rows are drawn where they are. Dropping columns takes a copy of
    // what is left, made when it is worth one and the layer budget has room.
    if (w - cw) * COLUMN_TRIM_SHARE >= w {
        let len = cw * ch * 4;
        let mut part: Vec<u8> = Vec::new();
        if ctx.live_bytes.saturating_add(len as u64) <= ctx.max_live_bytes
            && part.try_reserve_exact(len).is_ok()
        {
            for row in rows.chunks_exact(stride) {
                part.extend_from_slice(&row[left * 4..right * 4]);
            }
            if let Some(part) = IntSize::from_wh(cw32, ch32).and_then(|s| Pixmap::from_vec(part, s))
            {
                pixmap.draw_pixmap(x, y, part.as_ref(), paint, Transform::identity(), None);
                return true;
            }
        }
    }
    let Some(part) = PixmapRef::from_bytes(rows, sub.width(), ch32) else {
        return false;
    };
    pixmap.draw_pixmap(ibbox.x(), y, part, paint, Transform::identity(), None);
    true
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

pub(crate) fn opacity_to_u8(o: f32) -> u8 {
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
    render_nodes(&pat.root.children, ctx, tile_ts, &mut pixmap.as_mut());
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
    let Some((path, path_ts)) = single_clip_path(clip, ts) else {
        apply_clip_masked(clip, ctx, ts, pixmap);
        return;
    };
    // One filled path: what `apply_clip_masked` computes, without its RGBA
    // scratch pixmap (the tests `pixel_rect_clip_matches_the_mask` and
    // `single_path_clip_matches_the_mask` hold the two to equal bytes). Its
    // bytes stay reserved meanwhile, so the layer budget decides exactly as
    // it does there.
    let bytes = u64::from(w) * u64::from(h) * 4;
    if ctx.live_bytes.saturating_add(bytes) > ctx.max_live_bytes {
        pixmap.fill(Color::TRANSPARENT);
        return;
    }
    ctx.live_bytes += bytes;
    if let Some(rect) = pixel_rect(path, path_ts, w, h) {
        // A rectangle on pixel edges: every pixel fully inside it or fully
        // outside it, mask 255 or 0, which `apply_mask` turns into
        // "unchanged" or "transparent". Masks of 0 and 255 commute with the
        // inner clip's, so this one goes first.
        clear_outside(pixmap, rect);
        if let Some(inner) = &clip.clip_path {
            apply_clip(inner, ctx, ts, pixmap);
        }
    } else if let Some(mut mask) = Mask::new(w, h) {
        // The path's coverage, straight into a mask: drawing it with `Clear`
        // into opaque black leaves `255 - coverage`, which the inverted
        // alpha mask turns back into the coverage. Applied after the inner
        // clip, in the same order as there.
        let fill = path.fill.as_ref().map_or(FillRule::Winding, |f| f.rule);
        mask.fill_path(&path.data, fill, path.anti_alias, path_ts);
        if let Some(inner) = &clip.clip_path {
            apply_clip(inner, ctx, ts, pixmap);
        }
        pixmap.apply_mask(&mask);
    } else {
        pixmap.fill(Color::TRANSPARENT);
    }
    ctx.live_bytes -= bytes;
}

/// The one path a clip path draws, with the transform it is drawn with, when
/// it is a single visible, colour-filled path with an area (possibly under
/// transform-only groups); `None` for anything else.
fn single_clip_path(clip: &ClipPath, ts: Transform) -> Option<(&tree::Path, Transform)> {
    let mut ts = ts.pre_concat(clip.transform);
    let mut g = &clip.root;
    let path = loop {
        let [child] = g.children.as_slice() else {
            return None;
        };
        match child {
            Node::Path(p) => break p,
            // `draw_clip_children` reads only the transform of such a group.
            Node::Group(c) if c.clip_path.is_none() => {
                ts = ts.pre_concat(c.transform);
                g = c;
            }
            _ => return None,
        }
    };
    let fill = path.fill.as_ref()?;
    if !path.visible || !matches!(fill.paint, tree::Paint::Color(_)) {
        return None;
    }
    let b = path.data.bounds();
    if b.width() == 0.0 || b.height() == 0.0 {
        return None;
    }
    Some((path, ts))
}

/// The pixel rectangle `(left, top, right, bottom)` (clamped to `w x h`) a
/// clip path drawn with `ts` covers, when it is a rectangle whose corners
/// land on whole pixels; `None` for any other shape.
fn pixel_rect(
    path: &tree::Path,
    ts: Transform,
    w: u32,
    h: u32,
) -> Option<(usize, usize, usize, usize)> {
    let mut segs = path.data.segments();
    let (
        Some(rusty_raster::PathSegment::MoveTo(_)),
        Some(rusty_raster::PathSegment::LineTo(_)),
        Some(rusty_raster::PathSegment::LineTo(_)),
        Some(rusty_raster::PathSegment::LineTo(_)),
        Some(rusty_raster::PathSegment::Close),
        None,
    ) = (
        segs.next(),
        segs.next(),
        segs.next(),
        segs.next(),
        segs.next(),
        segs.next(),
    )
    else {
        return None;
    };
    let mut p: [rusty_raster::Point; 4] = path.data.points().try_into().ok()?;
    // The rasteriser transforms the points with this same call.
    ts.map_points(&mut p);
    let whole = |v: f32| v.is_finite() && v.fract() == 0.0 && v.abs() <= 16_777_216.0;
    if !p.iter().all(|q| whole(q.x) && whole(q.y)) {
        return None;
    }
    let axis = (p[0].y == p[1].y && p[1].x == p[2].x && p[2].y == p[3].y && p[3].x == p[0].x)
        || (p[0].x == p[1].x && p[1].y == p[2].y && p[2].x == p[3].x && p[3].y == p[0].y);
    if !axis {
        return None;
    }
    let (mut l, mut t, mut r, mut bo) = (p[0].x, p[0].y, p[0].x, p[0].y);
    for q in &p[1..] {
        l = l.min(q.x);
        t = t.min(q.y);
        r = r.max(q.x);
        bo = bo.max(q.y);
    }
    let clamp = |v: f32, max: u32| v.clamp(0.0, max as f32) as usize;
    Some((clamp(l, w), clamp(t, h), clamp(r, w), clamp(bo, h)))
}

/// Makes every pixel outside `(left, top, right, bottom)` transparent.
pub(crate) fn clear_outside(pixmap: &mut Pixmap, (l, t, r, b): (usize, usize, usize, usize)) {
    let stride = pixmap.width() as usize * 4;
    let (l, r) = (l.min(r), r);
    let data = pixmap.data_mut();
    for (y, row) in data.chunks_exact_mut(stride).enumerate() {
        if y < t || y >= b || l >= r {
            row.fill(0);
        } else {
            row[..l * 4].fill(0);
            row[r * 4..].fill(0);
        }
    }
}

/// Clips through a mask: the clip path drawn into a scratch pixmap.
fn apply_clip_masked(clip: &ClipPath, ctx: &mut Context, ts: Transform, pixmap: &mut Pixmap) {
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
            Node::Text(t) => {
                if let Some(g) = &t.flattened {
                    draw_clip_children(g, mode, ts, ctx, pixmap);
                }
            }
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
        render_nodes(&mask.root.children, ctx, ts, &mut mask_pixmap.as_mut());
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

/// An embedded document: drawn into a layer of its own.
fn render_document(
    tree: &crate::Tree,
    ctx: &mut Context,
    ts: Transform,
    pixmap: &mut PixmapMut,
) -> Option<()> {
    let mut sub = ctx.alloc(pixmap.width(), pixmap.height())?;
    let old = ctx.swap_max_bbox(max_bbox(sub.width(), sub.height())?);
    render_nodes(&tree.root().children, ctx, ts, &mut sub.as_mut());
    ctx.swap_max_bbox(old);
    pixmap.draw_pixmap(
        0,
        0,
        sub.as_ref(),
        &PixmapPaint::default(),
        Transform::identity(),
        None,
    );
    ctx.free(&sub);
    Some(())
}

fn render_image(
    image: &tree::Image,
    ctx: &mut Context,
    ts: Transform,
    pixmap: &mut PixmapMut,
) -> Option<()> {
    if !image.visible {
        return None;
    }
    if let tree::ImageKind::Svg(tree) = &image.kind {
        return render_document(tree, ctx, ts, pixmap);
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

#[cfg(test)]
mod tests {
    use super::*;

    /// What `draw_layer_content` relies on: compositing a transparent source
    /// pixel with source-over leaves the destination pixel as it was, for
    /// every premultiplied destination pixel.
    #[test]
    fn transparent_source_leaves_destination() {
        let mut dst = Pixmap::new(256, 256).unwrap();
        for (i, p) in dst.data_mut().chunks_exact_mut(4).enumerate() {
            // Every premultiplied (colour, alpha) pair, each channel on a
            // different colour.
            let (c, a) = ((i % 256) as u32, (i / 256) as u32);
            let v = (c * a + 127) / 255;
            p.copy_from_slice(&[v as u8, (v / 2) as u8, (a - v) as u8, a as u8]);
        }
        let before = dst.data().to_vec();
        let src = Pixmap::new(256, 256).unwrap();
        for opacity in [1.0, 0.5, 0.25, 0.0] {
            let paint = PixmapPaint {
                opacity,
                blend_mode: BlendMode::SourceOver,
                quality: FilterQuality::Nearest,
            };
            dst.as_mut()
                .draw_pixmap(0, 0, src.as_ref(), &paint, Transform::identity(), None);
            assert!(dst.data() == &before[..], "opacity {opacity}");
        }
    }

    /// `apply_clip` (which takes the pixel-rectangle path where it can)
    /// against the mask path on its own, byte for byte: rectangles on whole
    /// pixels and off them, inside, across and outside the layer, both
    /// windings, under scales, flips, 90-degree turns and transform-only
    /// groups, with and without anti-aliasing, and with an inner clip.
    #[test]
    fn pixel_rect_clip_matches_the_mask() {
        use std::sync::Arc;

        let mut x = 0x2545_f491_4f6c_dd1du64;
        let mut next = move || {
            x ^= x << 13;
            x ^= x >> 7;
            x ^= x << 17;
            x
        };
        let rect_path = |l: f32, t: f32, r: f32, b: f32, ccw: bool, aa: bool| {
            let mut pb = PathBuilder::new();
            pb.move_to(l, t);
            if ccw {
                pb.line_to(l, b);
                pb.line_to(r, b);
                pb.line_to(r, t);
            } else {
                pb.line_to(r, t);
                pb.line_to(r, b);
                pb.line_to(l, b);
            }
            pb.close();
            let data = Arc::new(pb.finish().unwrap());
            let bbox = data.bounds();
            Node::Path(Box::new(tree::Path {
                id: String::new(),
                abs_transform: Transform::identity(),
                visible: true,
                fill: Some(tree::Fill {
                    paint: tree::Paint::Color(tree::Color {
                        red: 255,
                        green: 255,
                        blue: 255,
                    }),
                    opacity: 1.0,
                    rule: FillRule::Winding,
                    context: None,
                }),
                stroke: None,
                paint_order: PaintOrder::FillAndStroke,
                anti_alias: aa,
                data,
                bounding_box: bbox,
                stroke_bounding_box: bbox,
            }))
        };
        let mut fast_rounds = 0;
        for round in 0..3000 {
            let (w, h) = (1 + (next() % 40) as u32, 1 + (next() % 40) as u32);
            let mut layer = Pixmap::new(w, h).unwrap();
            for p in layer.data_mut().chunks_exact_mut(4) {
                let a = (next() % 256) as u32;
                let c = (next() % 256) as u32 * a / 255;
                p.copy_from_slice(&[c as u8, (c / 2) as u8, (a - c) as u8, a as u8]);
            }
            // Device rectangle, mostly on whole pixels.
            let mut dev = || (next() % 60) as f32 - 10.0;
            let (mut l, t, mut r, mut b) = (dev(), dev(), dev(), dev());
            if next() % 5 == 0 {
                l += 0.25;
            }
            if next() % 7 == 0 {
                b -= 0.5;
            }
            if l == r {
                r += 1.0;
            }
            if t == b {
                b += 1.0;
            }
            // A transform the rectangle is mapped through, and its inverse
            // applied to the rectangle's corners.
            let s = [1.0f32, 2.0, 4.0, 0.5, 0.25, -1.0, -2.0][(next() % 7) as usize];
            let (tx, ty) = ((next() % 9) as f32 - 4.0, (next() % 9) as f32 - 4.0);
            let turn = next() % 4 == 0;
            let ts = if turn {
                // (x, y) -> (-s y + tx, s x + ty)
                Transform::from_row(0.0, s, -s, 0.0, tx, ty)
            } else {
                Transform::from_row(s, 0.0, 0.0, s, tx, ty)
            };
            let inv = |px: f32, py: f32| {
                if turn {
                    ((py - ty) / s, -(px - tx) / s)
                } else {
                    ((px - tx) / s, (py - ty) / s)
                }
            };
            let (ul, ut) = inv(l, t);
            let (ur, ub) = inv(r, b);
            let aa = next() % 4 != 0;
            let mut node = rect_path(
                ul.min(ur),
                ut.min(ub),
                ul.max(ur),
                ut.max(ub),
                next() % 2 == 0,
                aa,
            );
            if next() % 3 == 0 {
                // Under a transform-only group (a translation by 2 of user space).
                let g = Group {
                    transform: Transform::from_translate(-2.0, 0.0),
                    children: vec![match node {
                        Node::Path(mut p) => {
                            let moved = p
                                .data
                                .as_ref()
                                .clone()
                                .transform(Transform::from_translate(2.0, 0.0));
                            p.data = Arc::new(moved.unwrap());
                            Node::Path(p)
                        }
                        other => other,
                    }],
                    ..Group::default()
                };
                node = Node::Group(Box::new(g));
            }
            let inner = (next() % 4 == 0).then(|| {
                Arc::new(ClipPath {
                    transform: Transform::identity(),
                    clip_path: None,
                    root: Group {
                        children: vec![rect_path(1.5, 0.0, 30.0, 25.0, false, true)],
                        ..Group::default()
                    },
                })
            });
            let clip = ClipPath {
                transform: Transform::identity(),
                clip_path: inner,
                root: Group {
                    children: vec![node],
                    ..Group::default()
                },
            };
            if single_clip_path(&clip, ts)
                .and_then(|(p, pts)| pixel_rect(p, pts, w, h))
                .is_some()
            {
                fast_rounds += 1;
            }
            let mut want = layer.clone();
            let mut ctx = Context::new(64, 64, 1 << 30, 1 << 20, 1 << 30).unwrap();
            apply_clip_masked(&clip, &mut ctx, ts, &mut want);
            let mut got = layer.clone();
            let mut ctx = Context::new(64, 64, 1 << 30, 1 << 20, 1 << 30).unwrap();
            apply_clip(&clip, &mut ctx, ts, &mut got);
            assert_eq!(ctx.live_bytes, 0);
            assert!(want.data() == got.data(), "round {round}");
        }
        // The fast path was taken in most rounds (and declined in others).
        assert!((1000..2900).contains(&fast_rounds), "{fast_rounds}");
    }

    /// `apply_clip` against the mask path on its own, byte for byte, for
    /// clips of one path of any shape: polygons and curves (rounded corners,
    /// circles), both fill rules, with and without anti-aliasing, under
    /// fractional scales, rotations and skews, partly or wholly outside the
    /// layer, and with an inner clip.
    #[test]
    fn single_path_clip_matches_the_mask() {
        use std::sync::Arc;

        let mut x = 0x9e37_79b9_7f4a_7c15u64;
        let mut next = move || {
            x ^= x << 13;
            x ^= x >> 7;
            x ^= x << 17;
            x
        };
        let mut coord = || (next() % 4400) as f32 / 100.0 - 6.0;
        let mut shapes = Vec::new();
        for _ in 0..3000 {
            let mut pb = PathBuilder::new();
            pb.move_to(coord(), coord());
            for _ in 0..2 + (coord() as i32).rem_euclid(5) {
                if coord() > 15.0 {
                    let (a, b, c, d, e, f) = (coord(), coord(), coord(), coord(), coord(), coord());
                    pb.cubic_to(a, b, c, d, e, f);
                } else {
                    pb.line_to(coord(), coord());
                }
            }
            pb.close();
            if coord() > 30.0 {
                // A second contour (both fill rules matter then).
                pb.move_to(coord(), coord());
                pb.line_to(coord(), coord());
                pb.line_to(coord(), coord());
                pb.close();
            }
            shapes.push(pb.finish());
        }
        // Rounded rectangles and circles, as icon clip paths have.
        for i in 0..400 {
            let r = 1.0 + (i % 7) as f32;
            let (l, t) = ((i % 9) as f32 - 2.0, (i % 5) as f32 - 1.0);
            let mut pb = PathBuilder::new();
            pb.push_rect(rusty_raster::Rect::from_xywh(l, t, 20.0 + r, 14.0).unwrap());
            shapes.push(PathBuilder::from_circle(l + 10.0, t + 8.0, r * 2.0));
            let (rr, w, h) = (r, 24.0, 16.0);
            pb.move_to(l + rr, t);
            pb.line_to(l + w - rr, t);
            pb.quad_to(l + w, t, l + w, t + rr);
            pb.line_to(l + w, t + h - rr);
            pb.cubic_to(l + w, t + h, l + w, t + h, l + w - rr, t + h);
            pb.line_to(l + rr, t + h);
            pb.quad_to(l, t + h, l, t + h - rr);
            pb.line_to(l, t + rr);
            pb.quad_to(l, t, l + rr, t);
            pb.close();
            shapes.push(pb.finish());
        }
        let mut next = move || {
            x = x
                .wrapping_mul(6364136223846793005)
                .wrapping_add(1442695040888963407);
            x >> 33
        };
        let mut taken = 0;
        for (round, data) in shapes.into_iter().flatten().enumerate() {
            let (w, h) = (1 + (next() % 48) as u32, 1 + (next() % 40) as u32);
            let mut layer = Pixmap::new(w, h).unwrap();
            for p in layer.data_mut().chunks_exact_mut(4) {
                let a = (next() % 256) as u32;
                let c = (next() % 256) as u32 * a / 255;
                p.copy_from_slice(&[c as u8, (c / 3) as u8, (a - c) as u8, a as u8]);
            }
            let f = |n: u64| (n % 1000) as f32 / 250.0 - 2.0;
            let ts = match next() % 4 {
                0 => Transform::from_scale(2.0, 2.0),
                1 => {
                    Transform::from_row(1.0 + f(next()) / 4.0, 0.0, 0.0, 1.5, f(next()), f(next()))
                }
                2 => Transform::from_row(0.8, 0.3, -0.2, 1.1, 3.0 * f(next()), f(next())),
                _ => Transform::from_row(f(next()), f(next()), f(next()), f(next()), 5.0, 5.0),
            };
            let bbox = data.bounds();
            let path = tree::Path {
                id: String::new(),
                abs_transform: Transform::identity(),
                visible: true,
                fill: Some(tree::Fill {
                    paint: tree::Paint::Color(tree::Color {
                        red: 0,
                        green: 0,
                        blue: 0,
                    }),
                    opacity: [1.0, 0.5][(next() % 2) as usize],
                    rule: [FillRule::Winding, FillRule::EvenOdd][(next() % 2) as usize],
                    context: None,
                }),
                stroke: None,
                paint_order: PaintOrder::FillAndStroke,
                anti_alias: next() % 5 != 0,
                data: Arc::new(data),
                bounding_box: bbox,
                stroke_bounding_box: bbox,
            };
            let inner = (next() % 5 == 0).then(|| {
                let mut pb = PathBuilder::new();
                pb.push_oval(rusty_raster::Rect::from_xywh(2.5, 1.5, 30.0, 20.0).unwrap());
                let data = Arc::new(pb.finish().unwrap());
                let mut p = path.clone();
                p.data = data;
                Arc::new(ClipPath {
                    transform: Transform::identity(),
                    clip_path: None,
                    root: Group {
                        children: vec![Node::Path(Box::new(p))],
                        ..Group::default()
                    },
                })
            });
            let clip = ClipPath {
                transform: Transform::from_translate(f(next()), 0.0),
                clip_path: inner,
                root: Group {
                    children: vec![Node::Path(Box::new(path))],
                    ..Group::default()
                },
            };
            if single_clip_path(&clip, ts).is_some() {
                taken += 1;
            }
            let mut want = layer.clone();
            let mut ctx = Context::new(64, 64, 1 << 30, 1 << 20, 1 << 30).unwrap();
            apply_clip_masked(&clip, &mut ctx, ts, &mut want);
            let mut got = layer.clone();
            let mut ctx = Context::new(64, 64, 1 << 30, 1 << 20, 1 << 30).unwrap();
            apply_clip(&clip, &mut ctx, ts, &mut got);
            assert_eq!(ctx.live_bytes, 0);
            assert!(want.data() == got.data(), "round {round}");
        }
        assert!(taken > 3000, "{taken}");
    }

    /// The trimmed composite against compositing the whole layer, on layers
    /// with content in every position.
    #[test]
    fn trimmed_layer_composites_like_the_whole() {
        let mut x = 0x0123_4567_89ab_cdefu64;
        let mut next = move || {
            x ^= x << 13;
            x ^= x >> 7;
            x ^= x << 17;
            x
        };
        let ctx = Context::new(64, 64, 1 << 30, 1 << 20, 1 << 30).unwrap();
        for round in 0..400 {
            let (w, h) = (1 + (next() % 40) as u32, 1 + (next() % 40) as u32);
            let mut sub = Pixmap::new(w, h).unwrap();
            // Content inside a random rectangle (sometimes none at all).
            let (l, t) = (
                (next() % u64::from(w)) as u32,
                (next() % u64::from(h)) as u32,
            );
            let (r, b) = (
                l + (next() % u64::from(w - l + 1)) as u32,
                t + (next() % u64::from(h - t + 1)) as u32,
            );
            for py in t..b {
                for px in l..r {
                    if next() % 4 != 0 {
                        let a = (next() % 256) as u32;
                        let c = (next() % 256) as u32 * a / 255;
                        let i = ((py * w + px) * 4) as usize;
                        sub.data_mut()[i..i + 4].copy_from_slice(&[
                            c as u8,
                            (c / 3) as u8,
                            0,
                            a as u8,
                        ]);
                    }
                }
            }
            let mut whole = Pixmap::new(64, 64).unwrap();
            for (i, p) in whole.data_mut().chunks_exact_mut(4).enumerate() {
                let a = (i * 7 + round) % 256;
                p.copy_from_slice(&[(a / 2) as u8, (a / 5) as u8, a as u8, a as u8]);
            }
            let mut trimmed = whole.clone();
            let ibbox =
                IntRect::from_xywh((next() % 30) as i32 - 5, (next() % 30) as i32 - 5, w, h)
                    .unwrap();
            let paint = PixmapPaint {
                opacity: [1.0, 0.6][(next() % 2) as usize],
                blend_mode: BlendMode::SourceOver,
                quality: FilterQuality::Nearest,
            };
            whole.as_mut().draw_pixmap(
                ibbox.x(),
                ibbox.y(),
                sub.as_ref(),
                &paint,
                Transform::identity(),
                None,
            );
            if !draw_layer_content(&sub, ibbox, &paint, &ctx, &mut trimmed.as_mut()) {
                trimmed.as_mut().draw_pixmap(
                    ibbox.x(),
                    ibbox.y(),
                    sub.as_ref(),
                    &paint,
                    Transform::identity(),
                    None,
                );
            }
            assert!(whole.data() == trimmed.data(), "round {round}");
        }
    }
}
