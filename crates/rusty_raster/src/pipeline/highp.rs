// Copyright 2018 Google Inc.
// Copyright 2020 Yevhenii Reizner
//
// Use of this source code is governed by a BSD-style license that can be
// found in the LICENSE file.

/*!
A high precision raster pipeline implementation.

Unlike lowp, this one implements all stages.

Just like Skia, this pipeline is implemented using f32x8.

For some reason, we are almost 2x slower. Maybe because Skia uses clang's vector extensions
and we're using a manual implementation.
*/

use crate::{SpreadMode, PixmapRef};

use crate::geom::ScreenIntRect;
use crate::pixmap::SubPixmapMut;
use crate::wide::{f32x8, i32x8, u32x8};

pub const STAGE_WIDTH: usize = 8;

pub type StageFn = fn(p: &mut Pipeline);

pub struct Pipeline<'a, 'b: 'a> {
    index: usize,
    functions: &'a [StageFn],
    pixmap_src: PixmapRef<'a>,
    pixmap_dst: &'a mut SubPixmapMut<'b>,
    ctx: &'a mut super::Context, // TODO: remove mut
    mask_ctx: super::MaskCtx<'a>,
    aa_mask_ctx: super::AAMaskCtx,
    r: f32x8,
    g: f32x8,
    b: f32x8,
    a: f32x8,
    dr: f32x8,
    dg: f32x8,
    db: f32x8,
    da: f32x8,
    tail: usize,
    dx: usize,
    dy: usize,
}

impl Pipeline<'_, '_> {
    #[inline(always)]
    fn next_stage(&mut self) {
        let next: fn(&mut Self) = self.functions[self.index];
        self.index += 1;
        next(self);
    }
}

// Must be in the same order as raster_pipeline::Stage
pub const STAGES: &[StageFn; super::STAGES_COUNT] = &[
    move_source_to_destination,
    move_destination_to_source,
    clamp_0,
    clamp_a,
    premultiply,
    uniform_color,
    seed_shader,
    load_dst,
    store,
    load_dst_u8,
    store_u8,
    gather,
    load_mask_u8,
    mask_u8,
    scale_u8,
    lerp_u8,
    scale_1_float,
    lerp_1_float,
    destination_atop,
    destination_in,
    destination_out,
    destination_over,
    source_atop,
    source_in,
    source_out,
    source_over,
    clear,
    modulate,
    multiply,
    plus,
    screen,
    xor,
    color_burn,
    color_dodge,
    darken,
    difference,
    exclusion,
    hard_light,
    lighten,
    overlay,
    soft_light,
    hue,
    saturation,
    color,
    luminosity,
    source_over_rgba,
    transform,
    reflect,
    repeat,
    bilinear,
    bicubic,
    pad_x1,
    reflect_x1,
    repeat_x1,
    gradient,
    evenly_spaced_2_stop_gradient,
    xy_to_unit_angle,
    xy_to_radius,
    xy_to_2pt_conical_focal_on_circle,
    xy_to_2pt_conical_well_behaved,
    xy_to_2pt_conical_smaller,
    xy_to_2pt_conical_greater,
    xy_to_2pt_conical_strip,
    mask_2pt_conical_nan,
    mask_2pt_conical_degenerates,
    apply_vector_mask,
    alter_2pt_conical_compensate_focal,
    alter_2pt_conical_unswap,
    negate_x,
    apply_concentric_scale_bias,
    gamma_expand_2,
    gamma_expand_dst_2,
    gamma_compress_2,
    gamma_expand_22,
    gamma_expand_dst_22,
    gamma_compress_22,
    gamma_expand_srgb,
    gamma_expand_dst_srgb,
    gamma_compress_srgb,
];

pub fn fn_ptr(f: StageFn) -> *const () {
    f as *const ()
}

#[inline(never)]
pub fn start(
    functions: &[StageFn],
    functions_tail: &[StageFn],
    rect: &ScreenIntRect,
    aa_mask_ctx: super::AAMaskCtx,
    mask_ctx: super::MaskCtx,
    ctx: &mut super::Context,
    pixmap_src: PixmapRef,
    pixmap_dst: &mut SubPixmapMut,
) {
    let mut p = Pipeline {
        index: 0,
        functions: &[],
        pixmap_src,
        pixmap_dst,
        mask_ctx,
        aa_mask_ctx,
        ctx,
        r: f32x8::default(),
        g: f32x8::default(),
        b: f32x8::default(),
        a: f32x8::default(),
        dr: f32x8::default(),
        dg: f32x8::default(),
        db: f32x8::default(),
        da: f32x8::default(),
        tail: 0,
        dx: 0,
        dy: 0,
    };

    for y in rect.y()..rect.bottom() {
        let mut x = rect.x() as usize;
        let end = rect.right() as usize;

        p.functions = functions;
        while x + STAGE_WIDTH <= end {
            p.index = 0;
            p.dx = x;
            p.dy = y as usize;
            p.tail = STAGE_WIDTH;
            p.next_stage();
            x += STAGE_WIDTH;
        }

        if x != end {
            p.index = 0;
            p.functions = functions_tail;
            p.dx = x;
            p.dy = y as usize;
            p.tail = end - x;
            p.next_stage();
        }
    }
}

fn move_source_to_destination(p: &mut Pipeline) {
    p.dr = p.r;
    p.dg = p.g;
    p.db = p.b;
    p.da = p.a;

    p.next_stage();
}

fn premultiply(p: &mut Pipeline) {
    p.r *= p.a;
    p.g *= p.a;
    p.b *= p.a;

    p.next_stage();
}

fn move_destination_to_source(p: &mut Pipeline) {
    p.r = p.dr;
    p.g = p.dg;
    p.b = p.db;
    p.a = p.da;

    p.next_stage();
}

fn clamp_0(p: &mut Pipeline) {
    p.r = p.r.max(f32x8::default());
    p.g = p.g.max(f32x8::default());
    p.b = p.b.max(f32x8::default());
    p.a = p.a.max(f32x8::default());

    p.next_stage();
}

fn clamp_a(p: &mut Pipeline) {
    p.r = p.r.min(f32x8::splat(1.0));
    p.g = p.g.min(f32x8::splat(1.0));
    p.b = p.b.min(f32x8::splat(1.0));
    p.a = p.a.min(f32x8::splat(1.0));

    p.next_stage();
}

fn uniform_color(p: &mut Pipeline) {
    let ctx = &p.ctx.uniform_color;
    p.r = f32x8::splat(ctx.r);
    p.g = f32x8::splat(ctx.g);
    p.b = f32x8::splat(ctx.b);
    p.a = f32x8::splat(ctx.a);

    p.next_stage();
}

fn seed_shader(p: &mut Pipeline) {
    let iota = f32x8::from([0.5, 1.5, 2.5, 3.5, 4.5, 5.5, 6.5, 7.5]);

    p.r = f32x8::splat(p.dx as f32) + iota;
    p.g = f32x8::splat(p.dy as f32 + 0.5);
    p.b = f32x8::splat(1.0);
    p.a = f32x8::default();

    p.dr = f32x8::default();
    p.dg = f32x8::default();
    p.db = f32x8::default();
    p.da = f32x8::default();

    p.next_stage();
}

pub fn load_dst(p: &mut Pipeline) {
    load_8888(p.pixmap_dst.slice4_at_xy(p.dx, p.dy), &mut p.dr, &mut p.dg, &mut p.db, &mut p.da);
    p.next_stage();
}

pub fn load_dst_tail(p: &mut Pipeline) {
    load_8888_tail(p.tail, p.pixmap_dst.slice_at_xy(p.dx, p.dy), &mut p.dr, &mut p.dg, &mut p.db, &mut p.da);
    p.next_stage();
}

pub fn store(p: &mut Pipeline) {
    store_8888(&p.r, &p.g, &p.b, &p.a, p.pixmap_dst.slice4_at_xy(p.dx, p.dy));
    p.next_stage();
}

pub fn store_tail(p: &mut Pipeline) {
    store_8888_tail(&p.r, &p.g, &p.b, &p.a, p.tail, p.pixmap_dst.slice_at_xy(p.dx, p.dy));
    p.next_stage();
}

// Currently, all mask/A8 pixmaps are handled by lowp.
pub fn load_dst_u8(_: &mut Pipeline) {
    // unreachable
}

pub fn load_dst_u8_tail(_: &mut Pipeline) {
    // unreachable
}

pub fn store_u8(_: &mut Pipeline) {
    // unreachable
}

pub fn store_u8_tail(_: &mut Pipeline) {
    // unreachable
}

pub fn gather(p: &mut Pipeline) {
    let ix = gather_ix(p.pixmap_src, p.r, p.g);
    load_8888(&p.pixmap_src.gather(ix), &mut p.r, &mut p.g, &mut p.b, &mut p.a);

    p.next_stage();
}

#[inline(always)]
fn gather_ix(pixmap: PixmapRef, mut x: f32x8, mut y: f32x8) -> u32x8 {
    // Exclusive -> inclusive.
    let w = ulp_sub(pixmap.width() as f32);
    let h = ulp_sub(pixmap.height() as f32);
    x = x.max(f32x8::default()).min(f32x8::splat(w));
    y = y.max(f32x8::default()).min(f32x8::splat(h));

    // The clamps leave every lane in `[0, limit)` (`max` turns NaN into 0), so
    // for any pixmap short of 2^23 pixels on a side the truncation needs no
    // float-to-int conversion.
    const NO_CONVERSION: u32 = 1 << 23;

    // And for a pixmap of at most 2^23 pixels in all, `row * width + column`
    // can be computed in `f32` too: the floors are integers, the product and
    // the sum stay below 2^23, so every step is exact. That keeps the whole
    // index in vector registers; an `i32` lane multiply has no SSE2
    // instruction and compiled to a scalar `imul` per lane, which dragged
    // the truncation into scalar code with it.
    if u64::from(pixmap.width()) * u64::from(pixmap.height()) <= u64::from(NO_CONVERSION) {
        let row = y.floor_below_2p23() * f32x8::splat(pixmap.width() as f32);
        return (row + x.floor_below_2p23()).integral_to_u32_below_2p23();
    }

    let (ix, iy) = if pixmap.width() <= NO_CONVERSION && pixmap.height() <= NO_CONVERSION {
        (x.trunc_int_below_2p23(), y.trunc_int_below_2p23())
    } else {
        (x.trunc_int(), y.trunc_int())
    };

    (iy * i32x8::splat(pixmap.width() as i32) + ix).to_u32x8_bitcast()
}

#[inline(always)]
fn ulp_sub(v: f32) -> f32 {
    // Somewhat similar to v - f32::EPSILON
    f32::from_bits(v.to_bits().wrapping_sub(1))
}

fn load_mask_u8(_: &mut Pipeline) {
    // unreachable
}

fn mask_u8(p: &mut Pipeline) {
    let offset = p.mask_ctx.offset(p.dx, p.dy);
    let mut c = [0.0; 8];
    for i in 0..p.tail {
        c[i] = p.mask_ctx.data[offset + i] as f32;
    }
    let c = f32x8::from(c) / f32x8::splat(255.0);

    if c == f32x8::default() {
        return;
    }

    p.r *= c;
    p.g *= c;
    p.b *= c;
    p.a *= c;

    p.next_stage();
}

fn scale_u8(p: &mut Pipeline) {
    // Load u8xTail and cast it to f32x8.
    let data = p.aa_mask_ctx.copy_at_xy(p.dx, p.dy, p.tail);
    let c = f32x8::from([data[0] as f32, data[1] as f32, 0.0, 0.0, 0.0, 0.0, 0.0, 0.0]);
    let c = c / f32x8::splat(255.0);

    p.r *= c;
    p.g *= c;
    p.b *= c;
    p.a *= c;

    p.next_stage();
}

fn lerp_u8(p: &mut Pipeline) {
    // Load u8xTail and cast it to f32x8.
    let data = p.aa_mask_ctx.copy_at_xy(p.dx, p.dy, p.tail);
    let c = f32x8::from([data[0] as f32, data[1] as f32, 0.0, 0.0, 0.0, 0.0, 0.0, 0.0]);
    let c = c / f32x8::splat(255.0);

    p.r = lerp(p.dr, p.r, c);
    p.g = lerp(p.dg, p.g, c);
    p.b = lerp(p.db, p.b, c);
    p.a = lerp(p.da, p.a, c);

    p.next_stage();
}

fn scale_1_float(p: &mut Pipeline) {
    let c = f32x8::splat(p.ctx.current_coverage);
    p.r *= c;
    p.g *= c;
    p.b *= c;
    p.a *= c;

    p.next_stage();
}

fn lerp_1_float(p: &mut Pipeline) {
    let c = f32x8::splat(p.ctx.current_coverage);
    p.r = lerp(p.dr, p.r, c);
    p.g = lerp(p.dg, p.g, c);
    p.b = lerp(p.db, p.b, c);
    p.a = lerp(p.da, p.a, c);

    p.next_stage();
}

macro_rules! blend_fn {
    ($name:ident, $f:expr) => {
        fn $name(p: &mut Pipeline) {
            p.r = $f(p.r, p.dr, p.a, p.da);
            p.g = $f(p.g, p.dg, p.a, p.da);
            p.b = $f(p.b, p.db, p.a, p.da);
            p.a = $f(p.a, p.da, p.a, p.da);

            p.next_stage();
        }
    };
}

blend_fn!(clear,            |_, _,  _,  _| f32x8::default());
blend_fn!(source_atop,      |s, d, sa, da| s * da + d * inv(sa));
blend_fn!(destination_atop, |s, d, sa, da| d * sa + s * inv(da));
blend_fn!(source_in,        |s, _,  _, da| s * da);
blend_fn!(destination_in,   |_, d, sa,  _| d * sa);
blend_fn!(source_out,       |s, _,  _, da| s * inv(da));
blend_fn!(destination_out,  |_, d, sa,  _| d * inv(sa));
blend_fn!(source_over,      |s, d, sa,  _| mad(d, inv(sa), s));
blend_fn!(destination_over, |s, d,  _, da| mad(s, inv(da), d));
blend_fn!(modulate,         |s, d,  _,  _| s * d);
blend_fn!(multiply,         |s, d, sa, da| s * inv(da) + d * inv(sa) + s * d);
blend_fn!(screen,           |s, d,  _,  _| s + d - s * d);
blend_fn!(xor,              |s, d, sa, da| s * inv(da) + d * inv(sa));

// Wants a type for some reason.
blend_fn!(plus, |s: f32x8, d: f32x8, _, _| (s + d).min(f32x8::splat(1.0)));

macro_rules! blend_fn2 {
    ($name:ident, $f:expr) => {
        fn $name(p: &mut Pipeline) {
            // The same logic applied to color, and source_over for alpha.
            p.r = $f(p.r, p.dr, p.a, p.da);
            p.g = $f(p.g, p.dg, p.a, p.da);
            p.b = $f(p.b, p.db, p.a, p.da);
            p.a = mad(p.da, inv(p.a), p.a);

            p.next_stage();
        }
    };
}

blend_fn2!(darken,      |s: f32x8, d, sa, da: f32x8| s + d - (s * da).max(d * sa));
blend_fn2!(lighten,     |s: f32x8, d, sa, da: f32x8| s + d - (s * da).min(d * sa));
blend_fn2!(difference,  |s: f32x8, d, sa, da: f32x8| s + d - two((s * da).min(d * sa)));
blend_fn2!(exclusion,   |s: f32x8, d,  _,  _| s + d - two(s * d));

blend_fn2!(color_burn, |s: f32x8, d: f32x8, sa: f32x8, da: f32x8|
    d.cmp_eq(da).blend(
        d + s * inv(da),
        s.cmp_eq(f32x8::default()).blend(
            d * inv(sa),
            sa * (da - da.min((da - d) * sa * s.recip_fast())) + s * inv(da) + d * inv(sa)
        )
    )
);

blend_fn2!(color_dodge, |s: f32x8, d: f32x8, sa: f32x8, da: f32x8|
    d.cmp_eq(f32x8::default()).blend(
        s * inv(da),
        s.cmp_eq(sa).blend(
            s + d * inv(sa),
            sa * da.min((d * sa) * (sa - s).recip_fast()) + s * inv(da) + d * inv(sa)
        )
    )
);

blend_fn2!(hard_light, |s: f32x8, d: f32x8, sa, da|
    s * inv(da) + d * inv(sa) + two(s).cmp_le(sa).blend(
        two(s * d),
        sa * da - two((da - d) * (sa - s))
    )
);

blend_fn2!(overlay, |s: f32x8, d: f32x8, sa, da|
    s * inv(da) + d * inv(sa) + two(d).cmp_le(da).blend(
        two(s * d),
        sa * da - two((da - d) * (sa - s))
    )
);

blend_fn2!(soft_light, |s: f32x8, d: f32x8, sa: f32x8, da: f32x8| {
    let m  = da.cmp_gt(f32x8::default()).blend(d / da, f32x8::default());
    let s2 = two(s);
    let m4 = two(two(m));

    // The logic forks three ways:
    //    1. dark src?
    //    2. light src, dark dst?
    //    3. light src, light dst?
    let dark_src = d * (sa + (s2 - sa) * (f32x8::splat(1.0) - m));
    let dark_dst = (m4 * m4 + m4) * (m - f32x8::splat(1.0)) + f32x8::splat(7.0) * m;
    let lite_dst = m.sqrt() - m;
    let lite_src = d * sa + da * (s2 - sa)
        * two(two(d)).cmp_le(da).blend(dark_dst, lite_dst); // 2 or 3?

    s * inv(da) + d * inv(sa) + s2.cmp_le(sa).blend(dark_src, lite_src) // 1 or (2 or 3)?
});

// We're basing our implementation of non-separable blend modes on
//   https://www.w3.org/TR/compositing-1/#blendingnonseparable.
// and
//   https://www.khronos.org/registry/OpenGL/specs/es/3.2/es_spec_3.2.pdf
// They're equivalent, but ES' math has been better simplified.
//
// Anything extra we add beyond that is to make the math work with premul inputs.

macro_rules! blend_fn3 {
    ($name:ident, $f:expr) => {
        fn $name(p: &mut Pipeline) {
            let (tr, tg, tb, ta) = $f(p.r, p.g, p.b, p.a, p.dr, p.dg, p.db, p.da);
            p.r = tr;
            p.g = tg;
            p.b = tb;
            p.a = ta;

            p.next_stage();
        }
    };
}

blend_fn3!(hue, hue_k);

#[inline(always)]
fn hue_k(
    r: f32x8, g: f32x8, b: f32x8, a: f32x8,
    dr: f32x8, dg: f32x8, db: f32x8, da: f32x8,
) -> (f32x8, f32x8, f32x8, f32x8) {
    let rr = &mut (r * a);
    let gg = &mut (g * a);
    let bb = &mut (b * a);

    set_sat(rr, gg, bb, sat(dr, dg, db) * a);
    set_lum(rr, gg, bb, lum(dr, dg, db) * a);
    clip_color(rr, gg, bb, a * da);

    let r = r * inv(da) + dr * inv(a) + *rr;
    let g = g * inv(da) + dg * inv(a) + *gg;
    let b = b * inv(da) + db * inv(a) + *bb;
    let a = a + da - a * da;

    (r, g, b, a)
}

blend_fn3!(saturation, saturation_k);

#[inline(always)]
fn saturation_k(
    r: f32x8, g: f32x8, b: f32x8, a: f32x8,
    dr: f32x8, dg: f32x8, db: f32x8, da: f32x8,
) -> (f32x8, f32x8, f32x8, f32x8) {
    let rr = &mut (dr * a);
    let gg = &mut (dg * a);
    let bb = &mut (db * a);

    set_sat(rr, gg, bb, sat(r, g, b) * da);
    set_lum(rr, gg, bb, lum(dr, dg, db) * a); // (This is not redundant.)
    clip_color(rr, gg, bb, a * da);

    let r = r * inv(da) + dr * inv(a) + *rr;
    let g = g * inv(da) + dg * inv(a) + *gg;
    let b = b * inv(da) + db * inv(a) + *bb;
    let a = a + da - a * da;

    (r, g, b, a)
}

blend_fn3!(color, color_k);

#[inline(always)]
fn color_k(
    r: f32x8, g: f32x8, b: f32x8, a: f32x8,
    dr: f32x8, dg: f32x8, db: f32x8, da: f32x8,
) -> (f32x8, f32x8, f32x8, f32x8) {
    let rr = &mut (r * da);
    let gg = &mut (g * da);
    let bb = &mut (b * da);

    set_lum(rr, gg, bb, lum(dr, dg, db) * a);
    clip_color(rr, gg, bb, a * da);

    let r = r * inv(da) + dr * inv(a) + *rr;
    let g = g * inv(da) + dg * inv(a) + *gg;
    let b = b * inv(da) + db * inv(a) + *bb;
    let a = a + da - a * da;

    (r, g, b, a)
}

blend_fn3!(luminosity, luminosity_k);

#[inline(always)]
fn luminosity_k(
    r: f32x8, g: f32x8, b: f32x8, a: f32x8,
    dr: f32x8, dg: f32x8, db: f32x8, da: f32x8,
) -> (f32x8, f32x8, f32x8, f32x8) {
    let rr = &mut (dr * a);
    let gg = &mut (dg * a);
    let bb = &mut (db * a);

    set_lum(rr, gg, bb, lum(r, g, b) * da);
    clip_color(rr, gg, bb, a * da);

    let r = r * inv(da) + dr * inv(a) + *rr;
    let g = g * inv(da) + dg * inv(a) + *gg;
    let b = b * inv(da) + db * inv(a) + *bb;
    let a = a + da - a * da;

    (r, g, b, a)
}

#[inline(always)]
fn sat(r: f32x8, g: f32x8, b: f32x8) -> f32x8 {
    r.max(g.max(b)) - r.min(g.min(b))
}

#[inline(always)]
fn lum(r: f32x8, g: f32x8, b: f32x8) -> f32x8 {
    r * f32x8::splat(0.30) + g * f32x8::splat(0.59) + b * f32x8::splat(0.11)
}

#[inline(always)]
fn set_sat(r: &mut f32x8, g: &mut f32x8, b: &mut f32x8, s: f32x8) {
    let mn  = r.min(g.min(*b));
    let mx  = r.max(g.max(*b));
    let sat = mx - mn;

    // Map min channel to 0, max channel to s, and scale the middle proportionally.
    let scale = |c| sat.cmp_eq(f32x8::default())
                       .blend(f32x8::default(), (c - mn) * s / sat);

    *r = scale(*r);
    *g = scale(*g);
    *b = scale(*b);
}

#[inline(always)]
fn set_lum(r: &mut f32x8, g: &mut f32x8, b: &mut f32x8, l: f32x8) {
    let diff = l - lum(*r, *g, *b);
    *r += diff;
    *g += diff;
    *b += diff;
}

#[inline(always)]
fn clip_color(r: &mut f32x8, g: &mut f32x8, b: &mut f32x8, a: f32x8) {
    let mn = r.min(g.min(*b));
    let mx = r.max(g.max(*b));
    let l  = lum(*r, *g, *b);

    let clip = |mut c| {
        c = mx.cmp_ge(f32x8::default()).blend(c, l + (c - l) * l / (l - mn));
        c = mx.cmp_gt(a).blend(l + (c - l) * (a - l) / (mx - l), c);
        c = c.max(f32x8::default()); // Sometimes without this we may dip just a little negative.
        c
    };

    *r = clip(*r);
    *g = clip(*g);
    *b = clip(*b);
}

pub fn source_over_rgba(p: &mut Pipeline) {
    let pixels = p.pixmap_dst.slice4_at_xy(p.dx, p.dy);
    load_8888(pixels, &mut p.dr, &mut p.dg, &mut p.db, &mut p.da);
    p.r = mad(p.dr, inv(p.a), p.r);
    p.g = mad(p.dg, inv(p.a), p.g);
    p.b = mad(p.db, inv(p.a), p.b);
    p.a = mad(p.da, inv(p.a), p.a);
    store_8888(&p.r, &p.g, &p.b, &p.a, pixels);

    p.next_stage();
}

pub fn source_over_rgba_tail(p: &mut Pipeline) {
    let pixels = p.pixmap_dst.slice_at_xy(p.dx, p.dy);
    load_8888_tail(p.tail, pixels, &mut p.dr, &mut p.dg, &mut p.db, &mut p.da);
    p.r = mad(p.dr, inv(p.a), p.r);
    p.g = mad(p.dg, inv(p.a), p.g);
    p.b = mad(p.db, inv(p.a), p.b);
    p.a = mad(p.da, inv(p.a), p.a);
    store_8888_tail(&p.r, &p.g, &p.b, &p.a, p.tail, pixels);

    p.next_stage();
}

/// One row of a sprite: premultiplied RGBA8 `src` composited source-over onto
/// `dst`, pixel for pixel. `opacity` is the pattern opacity when it is not 1.
///
/// This is `draw_pixmap` without a transform. The pipeline it would otherwise
/// build is `seed_shader, transform, gather, [scale_1_float], source_over_rgba`,
/// and for an integer translation the first three stages only compute "source
/// pixel `(dx - x, dy - y)`", in floats, for every pixel. Here the caller
/// hands over the two rows and the arithmetic that is left is the pipeline's
/// own, operation for operation (`sprite_source_over_8`), so the bytes are
/// the same.
///
/// Two shortcuts skip that arithmetic for a group of eight source pixels:
///
/// - all zero: `d * (1 - 0) + 0` is `d`, and a byte survives the trip to
///   `f32` and back, so the destination keeps its bytes;
/// - all opaque, at full opacity: alpha 255 loads as exactly 1.0, so the
///   result is `d * 0 + s`, the source's own bytes.
///
/// Both rest on facts about `load_8888`/`store_8888` that the tests below
/// check for every byte value.
pub fn sprite_source_over_row(src: &[u8], dst: &mut [u8], opacity: Option<f32>) {
    const GROUP: usize = STAGE_WIDTH * 4;
    let mut src_groups = src.chunks_exact(GROUP);
    let mut dst_groups = dst.chunks_exact_mut(GROUP);
    for (s, d) in (&mut src_groups).zip(&mut dst_groups) {
        let (Ok(s), Ok(d)) = (<&[u8; GROUP]>::try_from(s), <&mut [u8; GROUP]>::try_from(d)) else {
            continue; // chunks_exact yields exactly GROUP bytes
        };
        let mut any = 0u32;
        let mut all = u32::MAX;
        for px in s.chunks_exact(4) {
            let px = u32::from_le_bytes([px[0], px[1], px[2], px[3]]);
            any |= px;
            all &= px;
        }
        if any == 0 {
            continue;
        }
        if all >> 24 == 0xff && opacity.is_none() {
            *d = *s;
            continue;
        }
        sprite_source_over_8(s, d, opacity);
    }

    // The tail, as the pipeline does it: missing lanes are zero and are not
    // written back. Lanes do not interact.
    let (s, d) = (src_groups.remainder(), dst_groups.into_remainder());
    let n = s.len().min(d.len());
    if n != 0 {
        let mut s8 = [0u8; GROUP];
        let mut d8 = [0u8; GROUP];
        s8[..n].copy_from_slice(&s[..n]);
        d8[..n].copy_from_slice(&d[..n]);
        sprite_source_over_8(&s8, &mut d8, opacity);
        d[..n].copy_from_slice(&d8[..n]);
    }
}

/// `gather` (the load), `scale_1_float` and `source_over_rgba` for eight pixels.
#[inline(always)]
fn sprite_source_over_8(
    src: &[u8; STAGE_WIDTH * 4], dst: &mut [u8; STAGE_WIDTH * 4], opacity: Option<f32>,
) {
    let (mut r, mut g, mut b, mut a) =
        (f32x8::default(), f32x8::default(), f32x8::default(), f32x8::default());
    load_8888(src, &mut r, &mut g, &mut b, &mut a);

    if let Some(c) = opacity {
        let c = f32x8::splat(c);
        r *= c;
        g *= c;
        b *= c;
        a *= c;
    }

    let (mut dr, mut dg, mut db, mut da) =
        (f32x8::default(), f32x8::default(), f32x8::default(), f32x8::default());
    load_8888(dst, &mut dr, &mut dg, &mut db, &mut da);
    r = mad(dr, inv(a), r);
    g = mad(dg, inv(a), g);
    b = mad(db, inv(a), b);
    a = mad(da, inv(a), a);
    store_8888(&r, &g, &b, &a, dst);
}

fn transform(p: &mut Pipeline) {
    let ts = &p.ctx.transform;

    let tr = mad(p.r, f32x8::splat(ts.sx), mad(p.g, f32x8::splat(ts.kx), f32x8::splat(ts.tx)));
    let tg = mad(p.r, f32x8::splat(ts.ky), mad(p.g, f32x8::splat(ts.sy), f32x8::splat(ts.ty)));
    p.r = tr;
    p.g = tg;

    p.next_stage();
}

// Tile x or y to [0,limit) == [0,limit - 1 ulp] (think, sampling from images).
// The gather stages will hard clamp the output of these stages to [0,limit)...
// we just need to do the basic repeat or mirroring.

fn reflect(p: &mut Pipeline) {
    let ctx = &p.ctx.limit_x;
    p.r = exclusive_reflect(p.r, ctx.scale, ctx.inv_scale);

    let ctx = &p.ctx.limit_y;
    p.g = exclusive_reflect(p.g, ctx.scale, ctx.inv_scale);

    p.next_stage();
}

#[inline(always)]
fn exclusive_reflect(v: f32x8, limit: f32, inv_limit: f32) -> f32x8 {
    let limit = f32x8::splat(limit);
    let inv_limit = f32x8::splat(inv_limit);
    ((v - limit) - (limit + limit)
        * ((v - limit) * (inv_limit * f32x8::splat(0.5))).floor() - limit).abs()
}

fn repeat(p: &mut Pipeline) {
    let ctx = &p.ctx.limit_x;
    p.r = exclusive_repeat(p.r, ctx.scale, ctx.inv_scale);

    let ctx = &p.ctx.limit_y;
    p.g = exclusive_repeat(p.g, ctx.scale, ctx.inv_scale);

    p.next_stage();
}

#[inline(always)]
fn exclusive_repeat(v: f32x8, limit: f32, inv_limit: f32) -> f32x8 {
    v - (v * f32x8::splat(inv_limit)).floor() * f32x8::splat(limit)
}

fn bilinear(p: &mut Pipeline) {
    let x = p.r;
    let fx = (x + f32x8::splat(0.5)).fract();
    let y = p.g;
    let fy = (y + f32x8::splat(0.5)).fract();
    let one = f32x8::splat(1.0);
    let wx = [one - fx, fx];
    let wy = [one - fy, fy];

    sampler_2x2(p.pixmap_src, &p.ctx.sampler, x, y, &wx, &wy, &mut p.r, &mut p.g, &mut p.b, &mut p.a);

    p.next_stage();
}

fn bicubic(p: &mut Pipeline) {
    let x = p.r;
    let fx = (x + f32x8::splat(0.5)).fract();
    let y = p.g;
    let fy = (y + f32x8::splat(0.5)).fract();
    let one = f32x8::splat(1.0);
    let wx = [bicubic_far(one - fx), bicubic_near(one - fx), bicubic_near(fx), bicubic_far(fx)];
    let wy = [bicubic_far(one - fy), bicubic_near(one - fy), bicubic_near(fy), bicubic_far(fy)];

    sampler_4x4(p.pixmap_src, &p.ctx.sampler, x, y, &wx, &wy, &mut p.r, &mut p.g, &mut p.b, &mut p.a);

    p.next_stage();
}

// In bicubic interpolation, the 16 pixels and +/- 0.5 and +/- 1.5 offsets from the sample
// pixel center are combined with a non-uniform cubic filter, with higher values near the center.
//
// We break this function into two parts, one for near 0.5 offsets and one for far 1.5 offsets.

#[inline(always)]
fn bicubic_near(t: f32x8) -> f32x8 {
    // 1/18 + 9/18t + 27/18t^2 - 21/18t^3 == t ( t ( -21/18t + 27/18) + 9/18) + 1/18
    mad(
        t,
        mad(t,
            mad(
                f32x8::splat(-21.0/18.0),
                t,
                f32x8::splat(27.0/18.0),
            ),
            f32x8::splat(9.0/18.0),
        ),
        f32x8::splat(1.0/18.0),
    )
}

#[inline(always)]
fn bicubic_far(t: f32x8) -> f32x8 {
    // 0/18 + 0/18*t - 6/18t^2 + 7/18t^3 == t^2 (7/18t - 6/18)
    (t * t) * mad(f32x8::splat(7.0/18.0), t, f32x8::splat(-6.0/18.0))
}

#[inline(always)]
fn sampler_2x2(
    pixmap: PixmapRef,
    ctx: &super::SamplerCtx,
    cx: f32x8, cy: f32x8,
    wx: &[f32x8; 2], wy: &[f32x8; 2],
    r: &mut f32x8, g: &mut f32x8, b: &mut f32x8, a: &mut f32x8,
) {
    *r = f32x8::default();
    *g = f32x8::default();
    *b = f32x8::default();
    *a = f32x8::default();

    let one = f32x8::splat(1.0);
    let start = -0.5;
    let mut y = cy + f32x8::splat(start);
    for j in 0..2 {
        let mut x = cx + f32x8::splat(start);
        for i in 0..2 {
            let mut rr = f32x8::default();
            let mut gg = f32x8::default();
            let mut bb = f32x8::default();
            let mut aa = f32x8::default();
            sample(pixmap, ctx, x,y, &mut rr, &mut gg, &mut bb, &mut aa);

            let w = wx[i] * wy[j];
            *r = mad(w, rr, *r);
            *g = mad(w, gg, *g);
            *b = mad(w, bb, *b);
            *a = mad(w, aa, *a);

            x += one;
        }

        y += one;
    }
}

#[inline(always)]
fn sampler_4x4(
    pixmap: PixmapRef,
    ctx: &super::SamplerCtx,
    cx: f32x8, cy: f32x8,
    wx: &[f32x8; 4], wy: &[f32x8; 4],
    r: &mut f32x8, g: &mut f32x8, b: &mut f32x8, a: &mut f32x8,
) {
    *r = f32x8::default();
    *g = f32x8::default();
    *b = f32x8::default();
    *a = f32x8::default();

    let one = f32x8::splat(1.0);
    let start = -1.5;
    let mut y = cy + f32x8::splat(start);
    for j in 0..4 {
        let mut x = cx + f32x8::splat(start);
        for i in 0..4 {
            let mut rr = f32x8::default();
            let mut gg = f32x8::default();
            let mut bb = f32x8::default();
            let mut aa = f32x8::default();
            sample(pixmap, ctx, x,y, &mut rr, &mut gg, &mut bb, &mut aa);

            let w = wx[i] * wy[j];
            *r = mad(w, rr, *r);
            *g = mad(w, gg, *g);
            *b = mad(w, bb, *b);
            *a = mad(w, aa, *a);

            x += one;
        }

        y += one;
    }
}

#[inline(always)]
fn sample(
    pixmap: PixmapRef, ctx: &super::SamplerCtx, mut x: f32x8, mut y: f32x8,
    r: &mut f32x8, g: &mut f32x8, b: &mut f32x8, a: &mut f32x8,
) {
    x = tile(x, ctx.spread_mode, pixmap.width() as f32, ctx.inv_width);
    y = tile(y, ctx.spread_mode, pixmap.height() as f32, ctx.inv_height);

    let ix = gather_ix(pixmap, x, y);
    load_8888(&pixmap.gather(ix), r, g, b, a);
}

#[inline(always)]
fn tile(v: f32x8, mode: SpreadMode, limit: f32, inv_limit: f32) -> f32x8 {
    match mode {
        SpreadMode::Pad => v,
        SpreadMode::Repeat => exclusive_repeat(v, limit, inv_limit),
        SpreadMode::Reflect => exclusive_reflect(v, limit, inv_limit),
    }
}

fn pad_x1(p: &mut Pipeline) {
    p.r = p.r.normalize();

    p.next_stage();
}

fn reflect_x1(p: &mut Pipeline) {
    p.r = (
        (p.r - f32x8::splat(1.0))
            - two(((p.r - f32x8::splat(1.0)) * f32x8::splat(0.5)).floor())
            - f32x8::splat(1.0)
    ).abs().normalize();

    p.next_stage();
}

fn repeat_x1(p: &mut Pipeline) {
    p.r = (p.r - p.r.floor()).normalize();

    p.next_stage();
}

fn gradient(p: &mut Pipeline) {
    let ctx = &p.ctx.gradient;

    // N.B. The loop starts at 1 because idx 0 is the color to use before the first stop.
    let t: [f32; 8] = p.r.into();
    let mut idx = u32x8::default();
    for i in 1..ctx.len {
        let tt = ctx.t_values[i].get();
        let n: u32x8 = u32x8::from([
            (t[0] >= tt) as u32,
            (t[1] >= tt) as u32,
            (t[2] >= tt) as u32,
            (t[3] >= tt) as u32,
            (t[4] >= tt) as u32,
            (t[5] >= tt) as u32,
            (t[6] >= tt) as u32,
            (t[7] >= tt) as u32,
        ]);
        idx = idx + n;
    }

    // As in the lowp stage: when all 8 pixels sit between the same two stops
    // (the usual case), one factor and one bias serve every lane and the
    // 64 bounds-checked scalar loads of `gradient_lookup` are not needed.
    // The arithmetic per lane is the same `t * factor + bias`.
    let first = idx.0[0];
    let mut uniform = true;
    for i in 1..STAGE_WIDTH {
        uniform &= idx.0[i] == first;
    }
    match (uniform, ctx.factors.get(first as usize), ctx.biases.get(first as usize)) {
        (true, Some(factor), Some(bias)) => {
            let t = p.r;
            p.r = mad(t, f32x8::splat(factor.r), f32x8::splat(bias.r));
            p.g = mad(t, f32x8::splat(factor.g), f32x8::splat(bias.g));
            p.b = mad(t, f32x8::splat(factor.b), f32x8::splat(bias.b));
            p.a = mad(t, f32x8::splat(factor.a), f32x8::splat(bias.a));
        }
        _ => gradient_lookup(ctx, &idx, p.r, &mut p.r, &mut p.g, &mut p.b, &mut p.a),
    }

    p.next_stage();
}

fn gradient_lookup(
    ctx: &super::GradientCtx, idx: &u32x8, t: f32x8,
    r: &mut f32x8, g: &mut f32x8, b: &mut f32x8, a: &mut f32x8,
) {
    let idx: [u32; 8] = (*idx).into();

    macro_rules! gather {
        ($d:expr, $c:ident) => {
            // Surprisingly, but bound checking doesn't affect the performance.
            // And since `idx` can contain any number, we should leave it in place.
            f32x8::from([
                $d[idx[0] as usize].$c,
                $d[idx[1] as usize].$c,
                $d[idx[2] as usize].$c,
                $d[idx[3] as usize].$c,
                $d[idx[4] as usize].$c,
                $d[idx[5] as usize].$c,
                $d[idx[6] as usize].$c,
                $d[idx[7] as usize].$c,
            ])
        };
    }

    let fr = gather!(&ctx.factors, r);
    let fg = gather!(&ctx.factors, g);
    let fb = gather!(&ctx.factors, b);
    let fa = gather!(&ctx.factors, a);

    let br = gather!(&ctx.biases, r);
    let bg = gather!(&ctx.biases, g);
    let bb = gather!(&ctx.biases, b);
    let ba = gather!(&ctx.biases, a);

    *r = mad(t, fr, br);
    *g = mad(t, fg, bg);
    *b = mad(t, fb, bb);
    *a = mad(t, fa, ba);
}

fn evenly_spaced_2_stop_gradient(p: &mut Pipeline) {
    let ctx = &p.ctx.evenly_spaced_2_stop_gradient;

    let t = p.r;
    p.r = mad(t, f32x8::splat(ctx.factor.r), f32x8::splat(ctx.bias.r));
    p.g = mad(t, f32x8::splat(ctx.factor.g), f32x8::splat(ctx.bias.g));
    p.b = mad(t, f32x8::splat(ctx.factor.b), f32x8::splat(ctx.bias.b));
    p.a = mad(t, f32x8::splat(ctx.factor.a), f32x8::splat(ctx.bias.a));

    p.next_stage();
}

fn xy_to_unit_angle(p: &mut Pipeline) {
    let x = p.r;
    let y = p.g;
    let x_abs = x.abs();
    let y_abs = y.abs();
    let slope = x_abs.min(y_abs) / x_abs.max(y_abs);
    let s = slope * slope;
    // Use a 7th degree polynomial to approximate atan.
    // This was generated using sollya.gforge.inria.fr.
    // A float optimized polynomial was generated using the following command.
    // P1 = fpminimax((1/(2*Pi))*atan(x),[|1,3,5,7|],[|24...|],[2^(-40),1],relative);
    let phi = slope
        * (f32x8::splat(0.15912117063999176025390625)
           + s * (f32x8::splat(-5.185396969318389892578125e-2)
                  + s * (f32x8::splat(2.476101927459239959716796875e-2)
                         + s * (f32x8::splat(-7.0547382347285747528076171875e-3)))));
    let phi = x_abs.cmp_lt(y_abs).blend(f32x8::splat(0.25) - phi, phi);
    let phi = x
        .cmp_lt(f32x8::splat(0.0))
        .blend(f32x8::splat(0.5) - phi, phi);
    let phi = y
        .cmp_lt(f32x8::splat(0.0))
        .blend(f32x8::splat(1.0) - phi, phi);
    let phi = phi.cmp_ne(phi).blend(f32x8::splat(0.0), phi);
    p.r = phi;
    p.next_stage();
}

fn xy_to_radius(p: &mut Pipeline) {
    let x2 = p.r * p.r;
    let y2 = p.g * p.g;
    p.r = (x2 + y2).sqrt();

    p.next_stage();
}

fn xy_to_2pt_conical_focal_on_circle(p: &mut Pipeline) {
    let x = p.r;
    let y = p.g;
    p.r = x + y * y / x;

    p.next_stage();
}

fn xy_to_2pt_conical_well_behaved(p: &mut Pipeline) {
    let ctx = &p.ctx.two_point_conical_gradient;

    let x = p.r;
    let y = p.g;
    p.r = (x * x + y * y).sqrt() - x * f32x8::splat(ctx.p0);

    p.next_stage();
}

fn xy_to_2pt_conical_greater(p: &mut Pipeline) {
    let ctx = &p.ctx.two_point_conical_gradient;

    let x = p.r;
    let y = p.g;
    p.r = (x * x - y * y).sqrt() - x * f32x8::splat(ctx.p0);

    p.next_stage();
}

fn xy_to_2pt_conical_smaller(p: &mut Pipeline) {
    let ctx = &p.ctx.two_point_conical_gradient;

    let x = p.r;
    let y = p.g;
    p.r = -(x * x - y * y).sqrt() - x * f32x8::splat(ctx.p0);

    p.next_stage();
}

fn xy_to_2pt_conical_strip(p: &mut Pipeline) {
    let ctx = &p.ctx.two_point_conical_gradient;

    let x = p.r;
    let y = p.g;
    p.r = x + (f32x8::splat(ctx.p0) - y * y).sqrt();

    p.next_stage();
}

fn mask_2pt_conical_nan(p: &mut Pipeline) {
    let ctx = &mut p.ctx.two_point_conical_gradient;

    let t = p.r;
    let is_degenerate = t.cmp_ne(t);
    p.r = is_degenerate.blend(f32x8::default(), t);
    ctx.mask = cond_to_mask(!is_degenerate.to_u32x8_bitcast());

    p.next_stage();
}

fn mask_2pt_conical_degenerates(p: &mut Pipeline) {
    let ctx = &mut p.ctx.two_point_conical_gradient;

    let t = p.r;
    let is_degenerate = t.cmp_le(f32x8::default()) | t.cmp_ne(t);
    p.r = is_degenerate.blend(f32x8::default(), t);
    ctx.mask = cond_to_mask(!is_degenerate.to_u32x8_bitcast());

    p.next_stage();
}

fn apply_vector_mask(p: &mut Pipeline) {
    let ctx = &p.ctx.two_point_conical_gradient;

    p.r = (p.r.to_u32x8_bitcast() & ctx.mask).to_f32x8_bitcast();
    p.g = (p.g.to_u32x8_bitcast() & ctx.mask).to_f32x8_bitcast();
    p.b = (p.b.to_u32x8_bitcast() & ctx.mask).to_f32x8_bitcast();
    p.a = (p.a.to_u32x8_bitcast() & ctx.mask).to_f32x8_bitcast();

    p.next_stage();
}

fn alter_2pt_conical_compensate_focal(p: &mut Pipeline) {
    let ctx = &p.ctx.two_point_conical_gradient;

    p.r += f32x8::splat(ctx.p1);

    p.next_stage();
}

fn alter_2pt_conical_unswap(p: &mut Pipeline) {
    p.r = f32x8::splat(1.0) - p.r;

    p.next_stage();
}

fn negate_x(p: &mut Pipeline) {
    p.r = -p.r;

    p.next_stage();
}

fn apply_concentric_scale_bias(p: &mut Pipeline) {
    let ctx = &p.ctx.two_point_conical_gradient;

    // Apply t = t * scale + bias for concentric gradients
    let x = p.r;
    p.r = x * f32x8::splat(ctx.p0) + f32x8::splat(ctx.p1);

    p.next_stage();
}

fn gamma_expand_2(p: &mut Pipeline) {
    p.r = p.r * p.r;
    p.g = p.g * p.g;
    p.b = p.b * p.b;

    p.next_stage();
}

fn gamma_expand_dst_2(p: &mut Pipeline) {
    p.dr = p.dr * p.dr;
    p.dg = p.dg * p.dg;
    p.db = p.db * p.db;

    p.next_stage();
}

fn gamma_compress_2(p: &mut Pipeline) {
    p.r = p.r.sqrt();
    p.g = p.g.sqrt();
    p.b = p.b.sqrt();

    p.next_stage();
}

fn gamma_expand_22(p: &mut Pipeline) {
    p.r = p.r.powf(2.2);
    p.g = p.g.powf(2.2);
    p.b = p.b.powf(2.2);

    p.next_stage();
}

fn gamma_expand_dst_22(p: &mut Pipeline) {
    p.dr = p.dr.powf(2.2);
    p.dg = p.dg.powf(2.2);
    p.db = p.db.powf(2.2);

    p.next_stage();
}

fn gamma_compress_22(p: &mut Pipeline) {
    p.r = p.r.powf(0.45454545);
    p.g = p.g.powf(0.45454545);
    p.b = p.b.powf(0.45454545);

    p.next_stage();
}

fn srgb_expand(x: f32x8) -> f32x8 {
    let small = x.cmp_le(f32x8::splat(0.04045));
    let linear = x / f32x8::splat(12.92);
    let exp = ((x + f32x8::splat(0.055)) / f32x8::splat(1.055)).powf(2.4);
    small.blend(linear, exp)
}

fn srgb_compress(x: f32x8) -> f32x8 {
    let small = x.cmp_le(f32x8::splat(0.0031308));
    let linear = x * f32x8::splat(12.92);
    let exp = x.powf(0.416666666) * f32x8::splat(1.055) - f32x8::splat(0.055);
    small.blend(linear, exp)
}

fn gamma_expand_srgb(p: &mut Pipeline) {
    p.r = srgb_expand(p.r);
    p.g = srgb_expand(p.g);
    p.b = srgb_expand(p.b);

    p.next_stage();
}

fn gamma_expand_dst_srgb(p: &mut Pipeline) {
    p.dr = srgb_expand(p.dr);
    p.dg = srgb_expand(p.dg);
    p.db = srgb_expand(p.db);

    p.next_stage();
}

fn gamma_compress_srgb(p: &mut Pipeline) {
    p.r = srgb_compress(p.r);
    p.g = srgb_compress(p.g);
    p.b = srgb_compress(p.b);

    p.next_stage();
}

pub fn just_return(_: &mut Pipeline) {
    // Ends the loop.
}

#[inline(always)]
fn cond_to_mask(cond: u32x8) -> u32x8 {
    let cond: [u32; 8] = cond.into();
    u32x8::from([
        if cond[0] != 0 { !0 } else { 0 },
        if cond[1] != 0 { !0 } else { 0 },
        if cond[2] != 0 { !0 } else { 0 },
        if cond[3] != 0 { !0 } else { 0 },
        if cond[4] != 0 { !0 } else { 0 },
        if cond[5] != 0 { !0 } else { 0 },
        if cond[6] != 0 { !0 } else { 0 },
        if cond[7] != 0 { !0 } else { 0 },
    ])
}

#[inline(always)]
fn load_8888(
    data: &[u8; STAGE_WIDTH * 4],
    r: &mut f32x8, g: &mut f32x8, b: &mut f32x8, a: &mut f32x8,
) {
    // Surprisingly, `f32 * FACTOR` is way faster than `f32x8 * f32x8::splat(FACTOR)`.
    const FACTOR: f32 = 1.0 / 255.0;
    // Each pixel is read as one little-endian word and its channels are
    // shifted and masked out of it, which is vector code (two 16-byte loads,
    // `psrld`/`pand`, `cvtdq2ps`). Indexing the 32 bytes one by one compiled
    // to 32 `movzbl` loads and 24 `pinsrw`. The values are the same integers
    // either way, so the floats are too.
    let mut px = [0u32; STAGE_WIDTH];
    for (p, bytes) in px.iter_mut().zip(data.chunks_exact(4)) {
        *p = u32::from_le_bytes([bytes[0], bytes[1], bytes[2], bytes[3]]);
    }
    for i in 0..STAGE_WIDTH {
        r.0[i] = (px[i] & 0xff) as i32 as f32 * FACTOR;
        g.0[i] = ((px[i] >> 8) & 0xff) as i32 as f32 * FACTOR;
        b.0[i] = ((px[i] >> 16) & 0xff) as i32 as f32 * FACTOR;
        a.0[i] = (px[i] >> 24) as i32 as f32 * FACTOR;
    }
}

#[inline(always)]
fn load_8888_tail(
    tail: usize, data: &[u8],
    r: &mut f32x8, g: &mut f32x8, b: &mut f32x8, a: &mut f32x8,
) {
    // Fill a dummy array with `tail` values. `tail` is always in a 1..STAGE_WIDTH-1 range.
    // This way we can reuse the `load_8888_` method and remove any branches.
    let mut tmp = [0u8; STAGE_WIDTH * 4];
    let n = tail.min(STAGE_WIDTH) * 4;
    tmp[0..n].copy_from_slice(&data[0..n]);
    load_8888(&tmp, r, g, b, a);
}

#[inline(always)]
fn store_8888(
    r: &f32x8, g: &f32x8, b: &f32x8, a: &f32x8,
    data: &mut [u8; STAGE_WIDTH * 4],
) {
    for (px, v) in data.chunks_exact_mut(4).zip(pack_8888(r, g, b, a)) {
        px.copy_from_slice(&v.to_le_bytes());
    }
}

#[inline(always)]
fn store_8888_tail(
    r: &f32x8, g: &f32x8, b: &f32x8, a: &f32x8,
    tail: usize, data: &mut [u8],
) {
    for (px, v) in data.chunks_exact_mut(4).zip(pack_8888(r, g, b, a)).take(tail) {
        px.copy_from_slice(&v.to_le_bytes());
    }
}

/// The eight pixels as `r | g << 8 | b << 16 | a << 24`, i.e. the pixmap's
/// RGBA byte order read as a little-endian word, so a store is one 4-byte
/// write per pixel instead of four bounds-checked byte writes.
#[inline(always)]
fn pack_8888(r: &f32x8, g: &f32x8, b: &f32x8, a: &f32x8) -> [u32; 8] {
    let (r, g, b, a) = (unnorm(r), unnorm(g), unnorm(b), unnorm(a));
    let mut out = [0u32; 8];
    for i in 0..STAGE_WIDTH {
        out[i] = r.0[i] | (g.0[i] << 8) | (b.0[i] << 16) | (a.0[i] << 24);
    }
    out
}

/// `[0, 1]` to a byte: clamp (NaN becomes 0), scale, round half to even.
///
/// tiny-skia rounds with `round_int` (`cvtps2dq`) here; `round_to_u8` gives the
/// same byte on the clamped range, proven over every `f32` in
/// `wide::tests::lane_conversions_match_libm_exhaustive`.
#[inline(always)]
fn unnorm(v: &f32x8) -> u32x8 {
    (v.max(f32x8::default()).min(f32x8::splat(1.0)) * f32x8::splat(255.0)).round_to_u8()
}

#[inline(always)]
fn inv(v: f32x8) -> f32x8 {
    f32x8::splat(1.0) - v
}

#[inline(always)]
fn two(v: f32x8) -> f32x8 {
    v + v
}

#[inline(always)]
fn mad(f: f32x8, m: f32x8, a: f32x8) -> f32x8 {
    f * m + a
}

#[inline(always)]
fn lerp(from: f32x8, to: f32x8, t: f32x8) -> f32x8 {
    mad(to - from, t, from)
}

#[cfg(test)]
mod tests {
    use super::*;

    /// `gather_ix` takes the float route for pixmaps of at most 2^23 pixels
    /// and the integer route above that. Both must give `row * width +
    /// column` of the clamped, truncated coordinates.
    #[test]
    fn gather_index_is_row_times_width_plus_column() {
        let mut seed = 0x1234_5678_9abc_def1u64;
        let mut next = move || {
            seed ^= seed << 13;
            seed ^= seed >> 7;
            seed ^= seed << 17;
            seed
        };
        // Sizes on both sides of the 2^23-pixel switch, including the extremes
        // of the float route (2^23 x 1, 1 x 2^23, 2896 x 2896).
        let sizes = [
            (1u32, 1u32), (7, 5), (160, 120), (2896, 2896), (8_388_608, 1), (1, 8_388_608),
            (4096, 2048), (4096, 2049), (2897, 2896), (8_388_609, 1), (3, 2_796_203),
        ];
        for (w, h) in sizes {
            // About 32 MB of zeros each; `gather_ix` only reads the size.
            let data = vec![0u8; w as usize * h as usize * 4];
            let pixmap = PixmapRef::from_bytes(&data, w, h).unwrap();
            for round in 0..2000 {
                let mut xs = [0.0f32; 8];
                let mut ys = [0.0f32; 8];
                for i in 0..8 {
                    let (a, b) = (next(), next());
                    // Mostly inside, sometimes far outside, on the last
                    // pixel, or NaN.
                    xs[i] = match a % 16 {
                        0 => -3.5,
                        1 => w as f32 + 9.0,
                        2 => w as f32 - 0.25,
                        3 => f32::NAN,
                        4 => f32::INFINITY,
                        _ => (a >> 16) as f32 / (1u64 << 48) as f32 * w as f32,
                    };
                    ys[i] = match b % 16 {
                        0 => -0.0,
                        1 => h as f32 + 1.0,
                        2 => h as f32 - 0.5,
                        3 => f32::NAN,
                        _ => (b >> 16) as f32 / (1u64 << 48) as f32 * h as f32,
                    };
                }
                let ix = gather_ix(pixmap, f32x8::from(xs), f32x8::from(ys));
                for i in 0..8 {
                    let clamp = |v: f32, limit: u32| {
                        let v = if v > 0.0 { v } else { 0.0 };
                        let top = ulp_sub(limit as f32);
                        if v < top { v } else { top }
                    };
                    let (cx, cy) = (clamp(xs[i], w), clamp(ys[i], h));
                    let expected = (cy as i32).wrapping_mul(w as i32).wrapping_add(cx as i32) as u32;
                    assert_eq!(ix.0[i], expected, "{w}x{h} round {round}: ({}, {})", xs[i], ys[i]);
                }
            }
        }
    }

    fn splat_px(px: [u8; 4]) -> [u8; STAGE_WIDTH * 4] {
        let mut out = [0u8; STAGE_WIDTH * 4];
        for o in out.chunks_exact_mut(4) {
            o.copy_from_slice(&px);
        }
        out
    }

    /// What the sprite shortcuts rest on: a byte survives `load_8888` then
    /// `store_8888`, and 255 loads as exactly 1.0.
    #[test]
    fn bytes_survive_load_and_store() {
        for v in 0..=255u8 {
            let src = splat_px([v, v, v, v]);
            let (mut r, mut g, mut b, mut a) =
                (f32x8::default(), f32x8::default(), f32x8::default(), f32x8::default());
            load_8888(&src, &mut r, &mut g, &mut b, &mut a);
            if v == 255 {
                assert_eq!(a.0[0].to_bits(), 1.0f32.to_bits());
            }
            let mut out = [0u8; STAGE_WIDTH * 4];
            store_8888(&r, &g, &b, &a, &mut out);
            assert_eq!(out, src, "byte {v}");
        }
    }

    /// The arithmetic agrees with both shortcuts for every source and
    /// destination byte: a transparent source leaves the destination alone
    /// at any opacity, and an opaque one replaces it at full opacity.
    #[test]
    fn sprite_shortcuts_are_what_the_arithmetic_gives() {
        for d in 0..=255u8 {
            for opacity in [None, Some(0.0), Some(0.3), Some(0.999)] {
                let before = splat_px([d, d / 2, 255 - d, d]);
                let mut dst = before;
                sprite_source_over_8(&[0u8; STAGE_WIDTH * 4], &mut dst, opacity);
                assert_eq!(dst, before, "transparent over {d} at {opacity:?}");
            }
            for s in 0..=255u8 {
                let src = splat_px([s, 255 - s, s / 3, 255]);
                let mut dst = splat_px([d, d, 255 - d, d]);
                sprite_source_over_8(&src, &mut dst, None);
                assert_eq!(dst, src, "opaque {s} over {d}");
            }
        }
    }

    /// The row function (shortcuts, groups of eight, tail) against the plain
    /// arithmetic one pixel at a time.
    #[test]
    fn sprite_row_matches_per_pixel_arithmetic() {
        let mut seed = 0x2545_f491_4f6c_dd1du64;
        let mut next = move || {
            seed ^= seed << 13;
            seed ^= seed >> 7;
            seed ^= seed << 17;
            seed
        };
        for len in [1usize, 3, 7, 8, 9, 16, 23, 64, 67] {
            for opacity in [None, Some(0.0), Some(0.25), Some(0.7)] {
                let mut src = vec![0u8; len * 4];
                let mut dst = vec![0u8; len * 4];
                for (i, px) in src.chunks_exact_mut(4).enumerate() {
                    // Runs of 19 transparent, opaque and mixed pixels (so
                    // whole groups of eight take each shortcut), some of
                    // them not premultiplied: the arithmetic does not care.
                    let kind = i / 19 + len;
                    let v = next().to_le_bytes();
                    match kind % 4 {
                        0 => px.fill(0),
                        1 => px.copy_from_slice(&[v[0], v[1], v[2], 255]),
                        _ => px.copy_from_slice(&[v[0], v[1], v[2], v[3]]),
                    }
                }
                for b in dst.iter_mut() {
                    *b = next() as u8;
                }

                let mut expected = dst.clone();
                for (s, d) in src.chunks_exact(4).zip(expected.chunks_exact_mut(4)) {
                    let mut s8 = [0u8; STAGE_WIDTH * 4];
                    let mut d8 = [0u8; STAGE_WIDTH * 4];
                    s8[..4].copy_from_slice(s);
                    d8[..4].copy_from_slice(d);
                    sprite_source_over_8(&s8, &mut d8, opacity);
                    d.copy_from_slice(&d8[..4]);
                }

                sprite_source_over_row(&src, &mut dst, opacity);
                assert_eq!(dst, expected, "len {len} opacity {opacity:?}");
            }
        }
    }
}
