//! `COLR` glyphs as SVG documents, as far as SVG can say them (no sweep
//! gradients, and only the blend modes CSS has).
//!
//! Derived from usvg 0.48.1 `text/colr.rs` (Copyright 2024 the Resvg Authors,
//! Apache-2.0 OR MIT; see `NOTICE.md`), writing the text itself and bounded.

use std::fmt::Write;

use skrifa::color::{Brush, ColorGlyph, ColorPainter, ColorStop, CompositeMode, Extend, Transform};
use skrifa::instance::LocationRef;
use skrifa::outline::{DrawSettings, OutlinePen};
use skrifa::prelude::Size;
use skrifa::raw::types::BoundingBox;
use skrifa::{FontRef, GlyphId, MetadataProvider};

/// `[left, top, right, bottom]`.
type Bounds = [f32; 4];

/// Nothing more is written past this; the glyph is then not drawn.
const MAX_SVG_BYTES: usize = 4 << 20;

/// The glyph as an SVG document.
pub(crate) fn paint(font: &FontRef, glyph: &ColorGlyph, location: LocationRef) -> Option<Vec<u8>> {
    let mut p = Painter {
        font,
        location,
        svg: String::from(r#"<svg xmlns="http://www.w3.org/2000/svg"><g>"#),
        path: String::new(),
        ids: 0,
        transform: Transform::default(),
        outline_transform: Transform::default(),
        transforms: Vec::new(),
        clips: Vec::new(),
        open: 0,
    };
    glyph.paint(location, &mut p).ok()?;
    if p.full() {
        return None;
    }
    // Every group opened is closed, whatever the font's paint table did.
    for _ in 0..p.open {
        p.svg.push_str("</g>");
    }
    p.svg.push_str("</g></svg>");
    Some(p.svg.into_bytes())
}

struct Painter<'a> {
    font: &'a FontRef<'a>,
    location: LocationRef<'a>,
    svg: String,
    /// Path data of the current outline.
    path: String,
    ids: usize,
    transform: Transform,
    outline_transform: Transform,
    transforms: Vec<Transform>,
    /// The box of every active clip, in root coordinates; `None` is empty.
    clips: Vec<Option<Bounds>>,
    /// Groups still open.
    open: usize,
}

/// Writes path data and its box (control points included).
struct Outline<'a> {
    path: &'a mut String,
    bounds: Option<Bounds>,
}

impl Outline<'_> {
    fn add(&mut self, points: &[(f32, f32)]) {
        for (x, y) in points {
            let b = self.bounds.get_or_insert([*x, *y, *x, *y]);
            *b = [b[0].min(*x), b[1].min(*y), b[2].max(*x), b[3].max(*y)];
        }
    }
}

impl OutlinePen for Outline<'_> {
    fn move_to(&mut self, x: f32, y: f32) {
        self.add(&[(x, y)]);
        let _ = write!(self.path, "M {x} {y} ");
    }

    fn line_to(&mut self, x: f32, y: f32) {
        self.add(&[(x, y)]);
        let _ = write!(self.path, "L {x} {y} ");
    }

    fn quad_to(&mut self, a: f32, b: f32, x: f32, y: f32) {
        self.add(&[(a, b), (x, y)]);
        let _ = write!(self.path, "Q {a} {b} {x} {y} ");
    }

    fn curve_to(&mut self, a: f32, b: f32, c: f32, d: f32, x: f32, y: f32) {
        self.add(&[(a, b), (c, d), (x, y)]);
        let _ = write!(self.path, "C {a} {b} {c} {d} {x} {y} ");
    }

    fn close(&mut self) {
        self.path.push_str("Z ");
    }
}

fn to_raster(t: Transform) -> rusty_raster::Transform {
    rusty_raster::Transform::from_row(t.xx, t.yx, t.xy, t.yy, t.dx, t.dy)
}

fn map_bounds(b: Bounds, t: Transform) -> Bounds {
    let mut points = [(b[0], b[1]), (b[2], b[1]), (b[0], b[3]), (b[2], b[3])]
        .map(|(x, y)| rusty_raster::Point::from_xy(x, y));
    to_raster(t).map_points(&mut points);
    let fold = |f: fn(f32, f32) -> f32, v: [f32; 4]| v.into_iter().fold(v[0], f);
    let (xs, ys) = (points.map(|p| p.x), points.map(|p| p.y));
    [
        fold(f32::min, xs),
        fold(f32::min, ys),
        fold(f32::max, xs),
        fold(f32::max, ys),
    ]
}

fn rect_path(b: Bounds) -> String {
    let [l, t, r, bt] = b;
    format!("M {l} {t} L {r} {t} L {r} {bt} L {l} {bt} Z")
}

fn matrix(name: &str, t: Transform) -> String {
    if t == Transform::default() {
        return String::new();
    }
    format!(
        r#" {name}="matrix({} {} {} {} {} {})""#,
        t.xx, t.yx, t.xy, t.yy, t.dx, t.dy
    )
}

fn spread(extend: Extend) -> &'static str {
    match extend {
        Extend::Repeat => r#" spreadMethod="repeat""#,
        Extend::Reflect => r#" spreadMethod="reflect""#,
        _ => "",
    }
}

impl Painter<'_> {
    fn full(&self) -> bool {
        self.svg.len() > MAX_SVG_BYTES
    }

    fn id(&mut self) -> String {
        self.ids += 1;
        format!("i{}", self.ids)
    }

    /// `rgb()` and opacity of a palette entry; the foreground is black.
    fn color(&self, index: u16, alpha: f32) -> (String, f32) {
        let palettes = self.font.color_palettes();
        let c = palettes
            .get(0)
            .filter(|_| index != u16::MAX)
            .and_then(|p| p.colors().get(usize::from(index)).copied())
            .map_or([0, 0, 0, 255], |c| [c.red, c.green, c.blue, c.alpha]);
        let a = (f32::from(c[3]) * alpha) as u8;
        (
            format!("rgb({}, {}, {})", c[0], c[1], c[2]),
            f32::from(a) / 255.0,
        )
    }

    /// Writes the outline of a glyph to `path`, placed by the current
    /// transform; returns its box in its own coordinates.
    fn outline(&mut self, glyph: GlyphId) -> Option<Bounds> {
        self.path.clear();
        self.outline_transform = self.transform;
        let mut pen = Outline {
            path: &mut self.path,
            bounds: None,
        };
        let settings = DrawSettings::unhinted(Size::unscaled(), self.location);
        let ok = self
            .font
            .outline_glyphs()
            .get(glyph)
            .is_some_and(|o| o.draw(settings, &mut pen).is_ok());
        let bounds = pen.bounds;
        if !ok {
            self.path.clear();
            return None;
        }
        bounds
    }

    fn draw_path(&mut self, paint: &str) {
        if self.full() {
            return;
        }
        let ts = matrix("transform", self.outline_transform);
        let _ = write!(
            self.svg,
            r#"<path {paint}{ts} d="{}"/>"#,
            self.path.trim_end()
        );
    }

    fn draw_gradient(&mut self, tag: &str, attrs: &str, stops: &[ColorStop], extend: Extend) {
        if self.full() {
            return;
        }
        let id = self.id();
        let to_outline = to_raster(self.outline_transform)
            .invert()
            .unwrap_or_default()
            .pre_concat(to_raster(self.transform));
        let ts = matrix(
            "gradientTransform",
            Transform {
                xx: to_outline.sx,
                yx: to_outline.ky,
                xy: to_outline.kx,
                yy: to_outline.sy,
                dx: to_outline.tx,
                dy: to_outline.ty,
            },
        );
        let _ = write!(
            self.svg,
            r#"<{tag} id="{id}" {attrs} gradientUnits="userSpaceOnUse"{}{ts}>"#,
            spread(extend)
        );
        for s in stops {
            let (color, opacity) = self.color(s.palette_index, s.alpha);
            let _ = write!(
                self.svg,
                r#"<stop offset="{}" stop-color="{color}" stop-opacity="{opacity}"/>"#,
                s.offset
            );
        }
        let _ = write!(self.svg, "</{tag}>");
        self.draw_path(&format!(r##"fill="url(#{id})""##));
    }

    fn brush(&mut self, brush: Brush<'_>) {
        match brush {
            Brush::Solid {
                palette_index,
                alpha,
            } => {
                let (color, opacity) = self.color(palette_index, alpha);
                self.draw_path(&format!(r#"fill="{color}" fill-opacity="{opacity}""#));
            }
            Brush::LinearGradient {
                p0,
                p1,
                color_stops,
                extend,
            } => {
                let attrs = format!(r#"x1="{}" y1="{}" x2="{}" y2="{}""#, p0.x, p0.y, p1.x, p1.y);
                self.draw_gradient("linearGradient", &attrs, color_stops, extend);
            }
            Brush::RadialGradient {
                c0,
                r0,
                c1,
                r1,
                color_stops,
                extend,
            } => {
                let attrs = format!(
                    r#"cx="{}" cy="{}" r="{r1}" fr="{r0}" fx="{}" fy="{}""#,
                    c1.x, c1.y, c0.x, c0.y
                );
                self.draw_gradient("radialGradient", &attrs, color_stops, extend);
            }
            // SVG has none.
            Brush::SweepGradient { .. } => {}
        }
    }

    fn open_group(&mut self, attrs: &str) {
        self.open += 1;
        let _ = write!(self.svg, "<g {attrs}>");
    }

    fn close_group(&mut self) {
        if self.open > 0 {
            self.open -= 1;
            self.svg.push_str("</g>");
        }
    }

    fn clip(&mut self, path: &str) {
        let id = self.id();
        let ts = matrix("transform", self.outline_transform);
        let _ = write!(
            self.svg,
            r#"<clipPath id="{id}"><path{ts} d="{path}"/></clipPath>"#
        );
        self.open_group(&format!(r##"clip-path="url(#{id})""##));
    }
}

impl ColorPainter for Painter<'_> {
    fn push_transform(&mut self, transform: Transform) {
        self.transforms.push(self.transform);
        self.transform *= transform;
    }

    fn pop_transform(&mut self) {
        if let Some(t) = self.transforms.pop() {
            self.transform = t;
        }
    }

    fn fill_glyph(&mut self, glyph: GlyphId, brush_transform: Option<Transform>, brush: Brush<'_>) {
        if self.full() {
            return;
        }
        self.outline(glyph);
        match brush_transform {
            Some(t) => {
                self.push_transform(t);
                self.brush(brush);
                self.pop_transform();
            }
            None => self.brush(brush),
        }
    }

    fn push_clip_glyph(&mut self, glyph: GlyphId) {
        // A clip is always opened: an outline that failed clips everything.
        let bounds = self.outline(glyph);
        let path = std::mem::take(&mut self.path);
        self.clip(path.trim_end());
        self.path = path;
        let t = self.outline_transform;
        self.clips.push(bounds.map(|b| map_bounds(b, t)));
    }

    fn push_clip_box(&mut self, b: BoundingBox<f32>) {
        let bounds = [b.x_min, b.y_min, b.x_max, b.y_max];
        self.outline_transform = self.transform;
        self.clip(&rect_path(bounds));
        self.clips
            .push(Some(map_bounds(bounds, self.outline_transform)));
    }

    fn pop_clip(&mut self) {
        self.close_group();
        self.clips.pop();
    }

    fn fill(&mut self, brush: Brush<'_>) {
        // A rectangle over what the clips leave; the groups shape it.
        let mut region: Option<Bounds> = None;
        for clip in &self.clips {
            let Some(c) = clip else {
                return;
            };
            region = Some(match region {
                Some(r) => [
                    r[0].max(c[0]),
                    r[1].max(c[1]),
                    r[2].min(c[2]),
                    r[3].min(c[3]),
                ],
                None => *c,
            });
        }
        let Some(r) = region.filter(|r| r[0] <= r[2] && r[1] <= r[3]) else {
            return;
        };
        self.path = rect_path(r);
        self.outline_transform = Transform::default();
        self.brush(brush);
    }

    fn push_layer(&mut self, mode: CompositeMode) {
        let mode = match mode {
            CompositeMode::Screen => "screen",
            CompositeMode::Overlay => "overlay",
            CompositeMode::Darken => "darken",
            CompositeMode::Lighten => "lighten",
            CompositeMode::ColorDodge => "color-dodge",
            CompositeMode::ColorBurn => "color-burn",
            CompositeMode::HardLight => "hard-light",
            CompositeMode::SoftLight => "soft-light",
            CompositeMode::Difference => "difference",
            CompositeMode::Exclusion => "exclusion",
            CompositeMode::Multiply => "multiply",
            CompositeMode::HslHue => "hue",
            CompositeMode::HslSaturation => "saturation",
            CompositeMode::HslColor => "color",
            CompositeMode::HslLuminosity => "luminosity",
            _ => "normal",
        };
        self.open_group(&format!(
            r#"style="mix-blend-mode: {mode}; isolation: isolate""#
        ));
    }

    fn pop_layer(&mut self) {
        self.close_group();
    }
}
