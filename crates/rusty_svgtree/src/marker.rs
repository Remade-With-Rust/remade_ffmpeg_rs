//! Markers (`marker-start`, `marker-mid`, `marker-end`), following usvg's
//! placement and vertex-angle rules.

use rusty_raster::{NonZeroRect, PathSegment, Point, Size, Transform};

/// A flattened path segment (quads promoted to cubics).
#[derive(Copy, Clone, Debug)]
pub(crate) enum Segment {
    MoveTo(Point),
    LineTo(Point),
    CubicTo(Point, Point, Point),
    Close,
}

#[derive(Clone, Copy, PartialEq, Eq)]
pub(crate) enum MarkerKind {
    Start,
    Middle,
    End,
}

pub(crate) enum Orientation {
    Auto,
    AutoStartReverse,
    Angle(f32),
}

fn approx_eq(a: f32, b: f32) -> bool {
    if a == b {
        return true;
    }
    if a.is_sign_negative() != b.is_sign_negative() {
        return false;
    }
    (i64::from(a.to_bits() as i32) - i64::from(b.to_bits() as i32)).abs() <= 4
}

pub(crate) fn segments(path: &rusty_raster::Path) -> Vec<Segment> {
    let mut out = Vec::with_capacity(path.len());
    let mut prev = Point::zero();
    let mut prev_move = Point::zero();
    for seg in path.segments() {
        match seg {
            PathSegment::MoveTo(p) => {
                out.push(Segment::MoveTo(p));
                prev = p;
                prev_move = p;
            }
            PathSegment::LineTo(p) => {
                out.push(Segment::LineTo(p));
                prev = p;
            }
            PathSegment::QuadTo(p1, p) => {
                let c = |n1: f32, n2: f32| (n1 + n2 * 2.0) / 3.0;
                out.push(Segment::CubicTo(
                    Point::from_xy(c(prev.x, p1.x), c(prev.y, p1.y)),
                    Point::from_xy(c(p.x, p1.x), c(p.y, p1.y)),
                    p,
                ));
                prev = p;
            }
            PathSegment::CubicTo(p1, p2, p) => {
                out.push(Segment::CubicTo(p1, p2, p));
                prev = p;
            }
            PathSegment::Close => {
                out.push(Segment::Close);
                prev = prev_move;
            }
        }
    }
    out
}

/// Calls `draw(point, index)` for each vertex the marker kind applies to.
pub(crate) fn draw_markers(path: &[Segment], kind: MarkerKind, mut draw: impl FnMut(Point, usize)) {
    if path.is_empty() {
        return;
    }
    match kind {
        MarkerKind::Start => {
            if let Some(Segment::MoveTo(p)) = path.first().copied() {
                draw(p, 0);
            }
        }
        MarkerKind::Middle => {
            let total = path.len() - 1;
            let mut i = 1;
            while i < total {
                let p = match path[i] {
                    Segment::MoveTo(p) | Segment::LineTo(p) | Segment::CubicTo(_, _, p) => p,
                    Segment::Close => {
                        i += 1;
                        continue;
                    }
                };
                draw(p, i);
                i += 1;
            }
        }
        MarkerKind::End => {
            let idx = path.len() - 1;
            match path.last().copied() {
                Some(Segment::LineTo(p) | Segment::CubicTo(_, _, p)) => draw(p, idx),
                Some(Segment::Close) => draw(get_subpath_start(path, idx), idx),
                _ => {}
            }
        }
    }
}

/// The marker transform for a vertex (before the marker content's own
/// `refX/refY` translation is appended by the caller).
pub(crate) fn marker_transform(
    p: Point,
    idx: usize,
    segs: &[Segment],
    orient: &Orientation,
    rect: NonZeroRect,
    view_box: Option<(NonZeroRect, crate::values::AspectRatio)>,
    stroke_scale: f32,
) -> Option<Transform> {
    let mut ts = Transform::from_translate(p.x, p.y);
    let angle = match orient {
        Orientation::AutoStartReverse if idx == 0 => (calc_vertex_angle(segs, idx) + 180.0) % 360.0,
        Orientation::Auto | Orientation::AutoStartReverse => calc_vertex_angle(segs, idx),
        Orientation::Angle(a) => *a,
    };
    if !approx_eq(angle, 0.0) {
        ts = ts.pre_rotate(angle);
    }
    if let Some((vb, aspect)) = view_box {
        let size = Size::from_wh(rect.width() * stroke_scale, rect.height() * stroke_scale)?;
        let vbox_ts = crate::convert::view_box_to_transform(vb, aspect, size);
        let (sx, sy) = vbox_ts.get_scale();
        ts = ts.pre_scale(sx, sy);
    } else {
        ts = ts.pre_scale(stroke_scale, stroke_scale);
    }
    Some(ts.pre_translate(-rect.x(), -rect.y()))
}

fn calc_vertex_angle(path: &[Segment], idx: usize) -> f32 {
    if idx == 0 {
        if path.len() < 2 {
            return 0.0;
        }
        match (path[0], path[1]) {
            (Segment::MoveTo(pm), Segment::LineTo(p)) => calc_line_angle(pm.x, pm.y, p.x, p.y),
            (Segment::MoveTo(pm), Segment::CubicTo(p1, _, p)) => {
                if approx_eq(pm.x, p1.x) && approx_eq(pm.y, p1.y) {
                    calc_line_angle(pm.x, pm.y, p.x, p.y)
                } else {
                    calc_line_angle(pm.x, pm.y, p1.x, p1.y)
                }
            }
            _ => 0.0,
        }
    } else if idx == path.len() - 1 {
        match (path[idx - 1], path[idx]) {
            (_, Segment::MoveTo(_)) => 0.0,
            (_, Segment::LineTo(p)) => {
                let prev = get_prev_vertex(path, idx);
                calc_line_angle(prev.x, prev.y, p.x, p.y)
            }
            (_, Segment::CubicTo(p1, p2, p)) => {
                if approx_eq(p2.x, p.x) && approx_eq(p2.y, p.y) {
                    calc_line_angle(p1.x, p1.y, p.x, p.y)
                } else {
                    calc_line_angle(p2.x, p2.y, p.x, p.y)
                }
            }
            (Segment::LineTo(p), Segment::Close) => {
                let next = get_subpath_start(path, idx);
                calc_line_angle(p.x, p.y, next.x, next.y)
            }
            (Segment::CubicTo(_, p2, p), Segment::Close) => {
                let prev = get_prev_vertex(path, idx);
                let next = get_subpath_start(path, idx);
                calc_curves_angle(
                    prev.x, prev.y, p2.x, p2.y, p.x, p.y, next.x, next.y, next.x, next.y,
                )
            }
            (_, Segment::Close) => 0.0,
        }
    } else {
        match (path[idx], path[idx + 1]) {
            (Segment::MoveTo(pm), Segment::LineTo(p)) => calc_line_angle(pm.x, pm.y, p.x, p.y),
            (Segment::MoveTo(pm), Segment::CubicTo(p1, _, _)) => {
                calc_line_angle(pm.x, pm.y, p1.x, p1.y)
            }
            (Segment::LineTo(p1), Segment::LineTo(p2)) => {
                let prev = get_prev_vertex(path, idx);
                calc_angle(prev.x, prev.y, p1.x, p1.y, p1.x, p1.y, p2.x, p2.y)
            }
            (Segment::CubicTo(_, c1_p2, c1_p), Segment::CubicTo(c2_p1, _, c2_p)) => {
                let prev = get_prev_vertex(path, idx);
                calc_curves_angle(
                    prev.x, prev.y, c1_p2.x, c1_p2.y, c1_p.x, c1_p.y, c2_p1.x, c2_p1.y, c2_p.x,
                    c2_p.y,
                )
            }
            (Segment::LineTo(pl), Segment::CubicTo(p1, _, p)) => {
                let prev = get_prev_vertex(path, idx);
                calc_curves_angle(
                    prev.x, prev.y, prev.x, prev.y, pl.x, pl.y, p1.x, p1.y, p.x, p.y,
                )
            }
            (Segment::CubicTo(_, p2, p), Segment::LineTo(pl)) => {
                let prev = get_prev_vertex(path, idx);
                calc_curves_angle(prev.x, prev.y, p2.x, p2.y, p.x, p.y, pl.x, pl.y, pl.x, pl.y)
            }
            (Segment::LineTo(p), Segment::MoveTo(_)) => {
                let prev = get_prev_vertex(path, idx);
                calc_line_angle(prev.x, prev.y, p.x, p.y)
            }
            (Segment::CubicTo(_, p2, p), Segment::MoveTo(_)) => {
                if approx_eq(p.x, p2.x) && approx_eq(p.y, p2.y) {
                    let prev = get_prev_vertex(path, idx);
                    calc_line_angle(prev.x, prev.y, p.x, p.y)
                } else {
                    calc_line_angle(p2.x, p2.y, p.x, p.y)
                }
            }
            (Segment::LineTo(p), Segment::Close) => {
                let prev = get_prev_vertex(path, idx);
                let next = get_subpath_start(path, idx);
                calc_angle(prev.x, prev.y, p.x, p.y, p.x, p.y, next.x, next.y)
            }
            (_, Segment::Close) => {
                let prev = get_prev_vertex(path, idx);
                let next = get_subpath_start(path, idx);
                calc_line_angle(prev.x, prev.y, next.x, next.y)
            }
            (_, Segment::MoveTo(_)) | (Segment::Close, _) => 0.0,
        }
    }
}

fn calc_line_angle(x1: f32, y1: f32, x2: f32, y2: f32) -> f32 {
    calc_angle(x1, y1, x2, y2, x1, y1, x2, y2)
}

#[allow(clippy::too_many_arguments)]
fn calc_curves_angle(
    px: f32,
    py: f32,
    cx1: f32,
    cy1: f32,
    x: f32,
    y: f32,
    cx2: f32,
    cy2: f32,
    nx: f32,
    ny: f32,
) -> f32 {
    if approx_eq(cx1, x) && approx_eq(cy1, y) {
        calc_angle(px, py, x, y, x, y, cx2, cy2)
    } else if approx_eq(x, cx2) && approx_eq(y, cy2) {
        calc_angle(cx1, cy1, x, y, x, y, nx, ny)
    } else {
        calc_angle(cx1, cy1, x, y, x, y, cx2, cy2)
    }
}

#[allow(clippy::too_many_arguments)]
fn calc_angle(x1: f32, y1: f32, x2: f32, y2: f32, x3: f32, y3: f32, x4: f32, y4: f32) -> f32 {
    use std::f32::consts::{FRAC_PI_2, PI};
    fn normalize(rad: f32) -> f32 {
        let v = rad % (PI * 2.0);
        if v < 0.0 {
            v + PI * 2.0
        } else {
            v
        }
    }
    fn vector_angle(vx: f32, vy: f32) -> f32 {
        let rad = vy.atan2(vx);
        if rad.is_nan() {
            0.0
        } else {
            normalize(rad)
        }
    }
    let in_a = vector_angle(x2 - x1, y2 - y1);
    let out_a = vector_angle(x4 - x3, y4 - y3);
    let d = (out_a - in_a) * 0.5;
    let mut angle = in_a + d;
    if FRAC_PI_2 < d.abs() {
        angle -= PI;
    }
    normalize(angle).to_degrees()
}

fn get_subpath_start(segments: &[Segment], idx: usize) -> Point {
    let offset = segments.len() - idx;
    for seg in segments.iter().rev().skip(offset) {
        if let Segment::MoveTo(p) = *seg {
            return p;
        }
    }
    Point::zero()
}

fn get_prev_vertex(segments: &[Segment], idx: usize) -> Point {
    match segments[idx - 1] {
        Segment::MoveTo(p) | Segment::LineTo(p) | Segment::CubicTo(_, _, p) => p,
        Segment::Close => get_subpath_start(segments, idx),
    }
}
