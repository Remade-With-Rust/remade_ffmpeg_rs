//! Applies filters to a group's layer.
//!
//! The pipeline (which input a primitive reads, colour spaces, region
//! clipping, the rounding of every operation) is derived from resvg 0.48.1
//! `filter/mod.rs` (Copyright 2018 the Resvg Authors, Apache-2.0 OR MIT; see
//! `NOTICE.md`), so that results match resvg's. What differs is bounded cost:
//! a result is kept only while a later primitive reads it, a primitive no
//! other one reads is skipped, scratch memory is reserved against the layer
//! budget and work against the filter budget before either is spent. A
//! filter that cannot run leaves its element transparent.

use std::collections::HashMap;
use std::rc::Rc;

use rusty_raster::{
    BlendMode, Color, FilterQuality, IntRect, NonZeroRect, Paint, Pattern, Pixmap, PixmapPaint,
    Rect, SpreadMode, Transform,
};

use crate::filter::*;
use crate::filter_kernels as kernels;
use crate::render::Context;
use kernels::{px, px_mut, Px};

/// The result of a primitive.
#[derive(Clone)]
struct Image {
    /// As large as the filter region, or as the whole layer when it comes
    /// from the source graphic and the region starts where the layer does
    /// (as in resvg: the region then misses at most the layer's last row and
    /// column, and where a recursive blur starts and ends shows in its last
    /// bit).
    pixmap: Rc<Pixmap>,
    /// The part that has data, in layer coordinates (`feTile` repeats it,
    /// `feOffset` inherits it).
    region: IntRect,
    space: ColorInterpolation,
}

impl Image {
    fn new(pixmap: Pixmap, region: IntRect, space: ColorInterpolation) -> Image {
        Image {
            // The pixels cover the whole filter region; its own coordinates
            // are resvg's `0,0,w,h`.
            region: IntRect::from_xywh(0, 0, pixmap.width(), pixmap.height()).unwrap_or(region),
            pixmap: Rc::new(pixmap),
            space,
        }
    }

    /// The pixels, converted to `space`, without a copy when nothing else
    /// holds them.
    fn take(self, space: ColorInterpolation) -> Pixmap {
        let mut p = Rc::try_unwrap(self.pixmap).unwrap_or_else(|rc| (*rc).clone());
        if self.space != space {
            kernels::convert_space(px_mut(&mut p), space);
        }
        p
    }

    /// `take`, cut to the size of the filter region.
    fn take_sized(self, space: ColorInterpolation, region: IntRect) -> Option<Pixmap> {
        let p = self.take(space);
        if p.width() == region.width() && p.height() == region.height() {
            return Some(p);
        }
        p.clone_rect(IntRect::from_xywh(0, 0, region.width(), region.height())?)
    }

    /// The pixels in `space`, shared when they already are.
    fn shared(self, space: ColorInterpolation) -> Rc<Pixmap> {
        if self.space == space {
            return self.pixmap;
        }
        Rc::new(self.take(space))
    }
}

/// Applies `filter` to `source`, the layer of the filtered group drawn with
/// `ts`.
pub(crate) fn apply(filter: &Filter, ctx: &mut Context, ts: Transform, source: &mut Pixmap) {
    if apply_inner(filter, ctx, ts, source).is_none() {
        source.fill(Color::TRANSPARENT);
    }
}

/// `NonZeroRect::to_int_rect` for any rectangle: huge coordinates clamp.
fn int_rect(r: NonZeroRect) -> Option<IntRect> {
    const MAX: f64 = (1 << 30) as f64;
    let side = |v: f32, len: f32| -> (i32, i32) {
        let a = f64::from(v.floor()).clamp(-MAX, MAX);
        let b = (a + f64::from(len.ceil()).max(1.0)).clamp(-MAX, MAX);
        (a as i32, b as i32)
    };
    let (l, rt) = side(r.x(), r.width());
    let (t, b) = side(r.y(), r.height());
    IntRect::from_ltrb(l, t, rt, b)
}

/// Which earlier result each `Reference` input reads: the latest of that
/// name. (The converter only leaves references that resolve.)
fn resolve<'a>(names: &HashMap<&'a str, usize>, input: &'a Input) -> Option<usize> {
    match input {
        Input::Reference(name) => names.get(name.as_str()).copied(),
        _ => None,
    }
}

/// What the primitives of a filter need of each other.
struct Plan {
    /// How often each result is still to be read.
    uses: Vec<usize>,
    /// Whether a primitive contributes to the last one.
    needed: Vec<bool>,
    /// The most results alive at once.
    peak: usize,
}

fn plan(filter: &Filter) -> Plan {
    let n = filter.primitives.len();
    let mut names: HashMap<&str, usize> = HashMap::new();
    let mut reads: Vec<Vec<usize>> = Vec::with_capacity(n);
    for (i, p) in filter.primitives.iter().enumerate() {
        let mut r = Vec::new();
        p.kind
            .for_each_input(|input| r.extend(resolve(&names, input)));
        reads.push(r);
        names.insert(&p.result, i);
    }
    let mut needed = vec![false; n];
    let mut uses = vec![0usize; n];
    let mut last_use = vec![0usize; n];
    if let Some(l) = needed.last_mut() {
        *l = true;
    }
    for i in (0..n).rev() {
        if needed[i] {
            for &j in &reads[i] {
                needed[j] = true;
                uses[j] += 1;
                last_use[j] = last_use[j].max(i);
            }
        }
    }
    let mut freed = vec![0usize; n + 1];
    let (mut live, mut peak) = (0usize, 0usize);
    for i in 0..n {
        if needed[i] {
            live += 1;
            freed[last_use[i].max(i)] += 1;
        }
        peak = peak.max(live);
        live -= freed[i];
    }
    Plan { uses, needed, peak }
}

/// Scratch pixmaps a single primitive may need on top of the live results:
/// its source copy, its output, and a back buffer (two pixmaps' worth).
const SCRATCH_PIXMAPS: u64 = 4;

fn apply_inner(
    filter: &Filter,
    ctx: &mut Context,
    ts: Transform,
    source: &mut Pixmap,
) -> Option<()> {
    let source_rect = IntRect::from_xywh(0, 0, source.width(), source.height())?;
    // A filter function is not clipped to a region.
    let region = if filter.is_function {
        source_rect
    } else {
        crate::render::fit_to_rect(int_rect(filter.rect.transform(ts)?)?, source_rect)?
    };
    let mut plan = plan(filter);
    // Counted at the size of the layer (see `Image`).
    let pixels = u64::from(source.width()) * u64::from(source.height());
    let bytes = pixels
        .saturating_mul(4)
        .saturating_mul(plan.peak as u64 + SCRATCH_PIXMAPS);
    if !ctx.reserve(bytes) {
        return None;
    }
    let result = run(filter, ctx, ts, source, region, pixels, &mut plan);
    ctx.release(bytes);
    let result = result?.take(ColorInterpolation::SRgb);
    if result.width() == source.width() && result.height() == source.height() {
        *source = result;
    } else {
        source.fill(Color::TRANSPARENT);
        draw(
            source,
            region.x(),
            region.y(),
            &result,
            BlendMode::SourceOver,
        );
    }
    Some(())
}

/// The state primitives are run in.
struct Run<'a> {
    source: &'a Pixmap,
    region: IntRect,
    is_function: bool,
    /// `source` cropped to the region, once something reads it.
    graphic: Option<Image>,
    results: Vec<Option<Image>>,
    uses: &'a mut [usize],
    names: HashMap<&'a str, usize>,
}

impl<'a> Run<'a> {
    fn input(&mut self, input: &'a Input) -> Option<Image> {
        if let Some(j) = resolve(&self.names, input) {
            self.uses[j] = self.uses[j].saturating_sub(1);
            return if self.uses[j] == 0 {
                self.results[j].take()
            } else {
                self.results[j].clone()
            };
        }
        if self.graphic.is_none() {
            self.graphic = Some(Image {
                pixmap: Rc::new(if self.region.x() == 0 && self.region.y() == 0 {
                    self.source.clone()
                } else {
                    self.source.clone_rect(self.region)?
                }),
                region: self.region,
                space: ColorInterpolation::SRgb,
            });
        }
        let image = self.graphic.clone()?;
        if *input != Input::SourceAlpha {
            return Some(image);
        }
        let (space, region) = (image.space, image.region);
        let mut p = image.take(space);
        for c in px_mut(&mut p) {
            *c = [0, 0, 0, c[3]];
        }
        Some(Image {
            pixmap: Rc::new(p),
            region,
            space,
        })
    }

    fn empty(&self) -> Option<Pixmap> {
        Pixmap::new(self.region.width(), self.region.height())
    }

    /// `input`, as large as the region, for the kernels that pair pixels.
    fn sized(&mut self, input: &'a Input, space: Option<ColorInterpolation>) -> Option<Image> {
        let image = self.input(input)?;
        let (region, space) = (image.region, space.unwrap_or(image.space));
        let same = image.pixmap.width() == self.region.width()
            && image.pixmap.height() == self.region.height();
        Some(Image {
            pixmap: if same {
                image.shared(space)
            } else {
                Rc::new(image.take_sized(space, self.region)?)
            },
            region,
            space,
        })
    }
}

fn run<'a>(
    filter: &'a Filter,
    ctx: &mut Context,
    ts: Transform,
    source: &'a Pixmap,
    region: IntRect,
    pixels: u64,
    plan: &'a mut Plan,
) -> Option<Image> {
    let mut run = Run {
        source,
        region,
        is_function: filter.is_function,
        graphic: None,
        results: Vec::with_capacity(filter.primitives.len()),
        uses: &mut plan.uses,
        names: HashMap::new(),
    };
    for (i, primitive) in filter.primitives.iter().enumerate() {
        let image = if plan.needed[i] {
            if !ctx.spend_filter_work(work(&primitive.kind, pixels)) {
                return None;
            }
            Some(run_primitive(primitive, ctx, ts, &mut run)?)
        } else {
            None
        };
        run.results.push(image);
        run.names.insert(&primitive.result, i);
    }
    run.results.pop()?
}

/// An upper bound on what a primitive costs, in pixel operations.
fn work(kind: &Kind, pixels: u64) -> u64 {
    let per_pixel = match kind {
        Kind::GaussianBlur(_) | Kind::DropShadow(_) => 16,
        Kind::Merge(fe) => 4 * fe.inputs.len() as u64,
        Kind::ConvolveMatrix(fe) => 4 + fe.kernel.len() as u64,
        // The window is built up by doubling.
        Kind::Morphology(_) => 32,
        Kind::Turbulence(fe) => 4 + 16 * u64::from(fe.num_octaves.min(kernels::MAX_OCTAVES)),
        Kind::DiffuseLighting(_) | Kind::SpecularLighting(_) => 32,
        Kind::ComponentTransfer(_) | Kind::ColorMatrix(_) => 8,
        _ => 4,
    };
    pixels.saturating_mul(per_pixel).saturating_add(4096)
}

fn run_primitive<'a>(
    primitive: &'a Primitive,
    ctx: &mut Context,
    ts: Transform,
    run: &mut Run<'a>,
) -> Option<Image> {
    let region = run.region;
    let mut subregion = if run.is_function {
        region
    } else {
        int_rect(primitive.rect.transform(ts)?)?
    };
    // `feOffset` takes its region from its input.
    if let Kind::Offset(fe) = &primitive.kind {
        if let Some(j) = resolve(&run.names, &fe.input) {
            if let Some(image) = &run.results[j] {
                subregion = image.region;
            }
        }
    }
    let cs = primitive.color_interpolation;
    let srgb = ColorInterpolation::SRgb;
    let out = |p: Pixmap, space| Some(Image::new(p, region, space));
    let (sx, sy) = ts.get_scale();
    let result = match &primitive.kind {
        Kind::Blend(fe) => {
            let input1 = run.input(&fe.input1)?;
            let mut p = run.input(&fe.input2)?.take_sized(cs, region)?;
            draw(&mut p, 0, 0, &input1.shared(cs), fe.mode);
            out(p, cs)
        }
        Kind::DropShadow(fe) => {
            let input = run.input(&fe.input)?;
            let empty = Pixmap::new(input.pixmap.width(), input.pixmap.height())?;
            drop_shadow(fe, cs, (sx, sy), input, empty).map(|p| Image::new(p, region, cs))
        }
        Kind::Flood(fe) => {
            let mut p = run.empty()?;
            p.fill(flood_color(fe.color, fe.opacity));
            out(p, srgb)
        }
        Kind::GaussianBlur(fe) => {
            let input = run.input(&fe.input)?;
            match std_devs(fe.std_dev_x, fe.std_dev_y, (sx, sy)) {
                // A zero deviation disables the primitive.
                None => Some(input),
                Some(dev) => {
                    let mut p = input.take(cs);
                    blur(&mut p, dev)?;
                    out(p, cs)
                }
            }
        }
        Kind::Offset(fe) => {
            let input = run.input(&fe.input)?;
            let (dx, dy) = (fe.dx * sx, fe.dy * sy);
            if near_zero(dx) && near_zero(dy) {
                Some(input)
            } else {
                let mut p = Pixmap::new(input.pixmap.width(), input.pixmap.height())?;
                draw(
                    &mut p,
                    dx as i32,
                    dy as i32,
                    &input.pixmap,
                    BlendMode::SourceOver,
                );
                out(p, input.space)
            }
        }
        Kind::Composite(fe) => {
            let a = run.sized(&fe.input1, Some(cs))?.pixmap;
            let mut p = run.input(&fe.input2)?.take_sized(cs, region)?;
            let mode = match fe.operator {
                CompositeOperator::Over => Some(BlendMode::SourceOver),
                CompositeOperator::In => Some(BlendMode::SourceIn),
                CompositeOperator::Out => Some(BlendMode::SourceOut),
                CompositeOperator::Atop => Some(BlendMode::SourceAtop),
                CompositeOperator::Xor => Some(BlendMode::Xor),
                CompositeOperator::Arithmetic { k1, k2, k3, k4 } => {
                    kernels::arithmetic([k1, k2, k3, k4], px(&a), px_mut(&mut p));
                    None
                }
            };
            if let Some(mode) = mode {
                draw(&mut p, 0, 0, &a, mode);
            }
            out(p, cs)
        }
        Kind::Merge(fe) => {
            let mut p: Option<Pixmap> = None;
            for input in &fe.inputs {
                let image = run.input(input)?;
                match p.as_mut() {
                    None => p = Some(image.take_sized(cs, region)?),
                    Some(p) => draw(p, 0, 0, &image.shared(cs), BlendMode::SourceOver),
                }
            }
            out(p.or_else(|| run.empty())?, cs)
        }
        Kind::Tile(fe) => {
            let input = run.input(&fe.input)?;
            out(tile(&input, region, run.empty()?)?, srgb)
        }
        Kind::Image(fe) => {
            let mut p = run.empty()?;
            let t =
                Transform::from_row(sx, 0.0, 0.0, sy, subregion.x() as f32, subregion.y() as f32);
            let bounds = IntRect::from_xywh(0, 0, region.width(), region.height())?;
            let old = ctx.swap_max_bbox(bounds);
            crate::render::render_nodes(&fe.root, ctx, t, &mut p.as_mut());
            ctx.swap_max_bbox(old);
            out(p, srgb)
        }
        Kind::ComponentTransfer(fe) => {
            let mut p = run.input(&fe.input)?.take(cs);
            kernels::component_transfer(fe, px_mut(&mut p));
            out(p, cs)
        }
        Kind::ColorMatrix(fe) => {
            let mut p = run.input(&fe.input)?.take(cs);
            kernels::color_matrix(&fe.kind, px_mut(&mut p));
            out(p, cs)
        }
        Kind::ConvolveMatrix(fe) => {
            let mut p = run.input(&fe.input)?.take(cs);
            let w = p.width() as usize;
            kernels::convolve(fe, w, px_mut(&mut p))?;
            out(p, cs)
        }
        Kind::Morphology(fe) => {
            let mut p = run.input(&fe.input)?.take(cs);
            let (rx, ry) = (fe.radius_x * sx, fe.radius_y * sy);
            if rx > 0.0 && ry > 0.0 {
                let w = p.width() as usize;
                let erode = fe.operator == MorphologyOperator::Erode;
                kernels::morphology(erode, rx, ry, w, px_mut(&mut p))?;
            } else {
                p.fill(Color::TRANSPARENT);
            }
            out(p, cs)
        }
        Kind::DisplacementMap(fe) => {
            // The colour space is that of the map; the moved image stays in
            // its own.
            let image = run.sized(&fe.input1, None)?;
            let map = run.sized(&fe.input2, Some(cs))?.pixmap;
            let mut p = run.empty()?;
            let w = p.width() as usize;
            let scale = (fe.scale * sx, fe.scale * sy);
            kernels::displacement(fe, scale, w, px(&image.pixmap), px(&map), px_mut(&mut p));
            out(p, image.space)
        }
        Kind::Turbulence(fe) => {
            let mut p = run.empty()?;
            if !(near_zero(sx) || near_zero(sy)) {
                let w = p.width() as usize;
                let offset = (
                    f64::from(region.x()) - f64::from(ts.tx),
                    f64::from(region.y()) - f64::from(ts.ty),
                );
                let scale = (f64::from(sx), f64::from(sy));
                kernels::turbulence(fe, offset, scale, w, px_mut(&mut p));
            }
            out(p, cs)
        }
        Kind::DiffuseLighting(fe) => {
            let input = run.sized(&fe.input, None)?;
            let mut p = run.empty()?;
            let light = transform_light(fe.light_source, region, ts);
            let w = p.width() as usize;
            kernels::diffuse_lighting(fe, light, w, px(&input.pixmap), px_mut(&mut p));
            out(p, cs)
        }
        Kind::SpecularLighting(fe) => {
            let input = run.sized(&fe.input, None)?;
            let mut p = run.empty()?;
            let light = transform_light(fe.light_source, region, ts);
            let w = p.width() as usize;
            kernels::specular_lighting(fe, light, w, px(&input.pixmap), px_mut(&mut p));
            out(p, cs)
        }
    }?;
    if region == subregion {
        return Some(result);
    }
    let space = result.space;
    let mut p = result.take(space);
    // `feOffset` is not clipped (its region is its input's).
    if !matches!(primitive.kind, Kind::Offset(_)) {
        clear_outside(&mut p, subregion, region);
    }
    Some(Image {
        pixmap: Rc::new(p),
        region: subregion,
        space,
    })
}

/// Makes the pixels of `p` (placed at `region`) outside `keep` transparent.
fn clear_outside(p: &mut Pixmap, keep: IntRect, region: IntRect) {
    let (w, h) = (p.width() as i64, p.height() as i64);
    let local =
        |v: i32, origin: i32, max: i64| (i64::from(v) - i64::from(origin)).clamp(0, max) as usize;
    let (l, r) = (
        local(keep.left(), region.x(), w),
        local(keep.right(), region.x(), w),
    );
    let (t, b) = (
        local(keep.top(), region.y(), h),
        local(keep.bottom(), region.y(), h),
    );
    crate::render::clear_outside(p, (l, t, r, b));
}

fn draw(dst: &mut Pixmap, x: i32, y: i32, src: &Pixmap, blend_mode: BlendMode) {
    // Wholly outside: nothing to draw (and no arithmetic on huge offsets).
    let covers = |o: i32, len: u32, max: u32| {
        i64::from(o) < i64::from(max) && i64::from(o) + i64::from(len) > 0
    };
    if !(covers(x, src.width(), dst.width()) && covers(y, src.height(), dst.height())) {
        return;
    }
    let paint = PixmapPaint {
        blend_mode,
        ..PixmapPaint::default()
    };
    dst.draw_pixmap(x, y, src.as_ref(), &paint, Transform::identity(), None);
}

fn near_zero(v: f32) -> bool {
    crate::convert::approx_zero(v)
}

fn flood_color(c: crate::tree::Color, opacity: f32) -> Color {
    Color::from_rgba8(
        c.red,
        c.green,
        c.blue,
        crate::render::opacity_to_u8(opacity),
    )
}

/// The blur deviations in device pixels, and whether to blur with boxes
/// (from 2 pixels on either axis) rather than recursively; `None` when both
/// are zero.
fn std_devs(x: f32, y: f32, (sx, sy): (f32, f32)) -> Option<(f64, f64, bool)> {
    let (x, y) = (x * sx, y * sy);
    if near_zero(x) && near_zero(y) {
        return None;
    }
    // Tiny deviations are dropped: the recursive blur would lose the image.
    let floor = |v: f32| if v < 0.05 { 0.0 } else { v };
    let (x, y) = (floor(x), floor(y));
    Some((f64::from(x), f64::from(y), x >= 2.0 || y >= 2.0))
}

fn blur(p: &mut Pixmap, (x, y, boxes): (f64, f64, bool)) -> Option<()> {
    let w = p.width() as usize;
    if boxes {
        kernels::box_blur(x as f32, y as f32, w, px_mut(p))
    } else {
        kernels::iir_blur(x, y, w, px_mut(p))
    }
}

fn drop_shadow(
    fe: &DropShadow,
    cs: ColorInterpolation,
    scale: (f32, f32),
    input: Image,
    mut out: Pixmap,
) -> Option<Pixmap> {
    let input = input.shared(cs);
    let mut shadow = (*input).clone();
    if let Some(dev) = std_devs(fe.std_dev_x, fe.std_dev_y, scale) {
        blur(&mut shadow, dev)?;
    }
    // The flood colour under the shadow's alpha; as resvg, the result is
    // taken through the primitive's colour space conversion.
    let color = flood_color(fe.color, fe.opacity);
    let mut table: [Px; 256] = [[0; 4]; 256];
    for (a, t) in table.iter_mut().enumerate() {
        let mut c = color;
        c.apply_opacity(a as f32 / 255.0);
        let c = c.premultiply().to_color_u8();
        *t = [c.red(), c.green(), c.blue(), c.alpha()];
    }
    kernels::convert_space(&mut table, cs);
    for p in px_mut(&mut shadow) {
        *p = table[usize::from(p[3])];
    }
    let (dx, dy) = (fe.dx * scale.0, fe.dy * scale.1);
    draw(
        &mut out,
        dx as i32,
        dy as i32,
        &shadow,
        BlendMode::SourceOver,
    );
    draw(&mut out, 0, 0, &input, BlendMode::SourceOver);
    Some(out)
}

fn tile(input: &Image, region: IntRect, mut out: Pixmap) -> Option<Pixmap> {
    let sub = input
        .region
        .translate(region.x().checked_neg()?, region.y().checked_neg()?)?;
    let tile = input.pixmap.clone_rect(sub)?;
    let paint = Paint {
        shader: Pattern::new(
            tile.as_ref(),
            SpreadMode::Repeat,
            FilterQuality::Bicubic,
            1.0,
            Transform::from_translate(sub.x() as f32, sub.y() as f32),
        ),
        ..Paint::default()
    };
    let rect = Rect::from_xywh(0.0, 0.0, out.width() as f32, out.height() as f32)?;
    out.fill_rect(rect, &paint, Transform::identity(), None);
    Some(out)
}

/// A light in the filter region's pixels.
fn transform_light(light: LightSource, region: IntRect, ts: Transform) -> LightSource {
    let sz = (ts.sx * ts.sx + ts.sy * ts.sy).sqrt() / std::f32::consts::SQRT_2;
    let map = |x: f32, y: f32| {
        let mut p = rusty_raster::Point::from_xy(x, y);
        ts.map_point(&mut p);
        (p.x - region.x() as f32, p.y - region.y() as f32)
    };
    match light {
        LightSource::Distant { .. } => light,
        LightSource::Point { x, y, z } => {
            let (x, y) = map(x, y);
            LightSource::Point {
                x,
                y,
                z: z * (ts.sx * ts.sx + ts.sy * ts.sy).sqrt() / std::f32::consts::SQRT_2,
            }
        }
        LightSource::Spot(s) => {
            let (x, y) = map(s.x, s.y);
            let (px, py) = map(s.points_at_x, s.points_at_y);
            LightSource::Spot(SpotLight {
                x,
                y,
                z: s.z * sz,
                points_at_x: px,
                points_at_y: py,
                points_at_z: s.points_at_z * sz,
                ..s
            })
        }
    }
}
