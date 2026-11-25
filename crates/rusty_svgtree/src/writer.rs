//! Writes a tree back as SVG: every style resolved, as the tree has it. What
//! is written parses to a tree that draws the same.

use std::collections::{HashMap, HashSet};
use std::fmt::Write;
use std::sync::Arc;

use rusty_raster::{
    BlendMode, FillRule, FilterQuality, LineCap, LineJoin, MaskType, NonZeroRect, PathSegment,
    SpreadMode, Transform,
};

use crate::filter::{self, Filter, Input, Kind, LightSource, TransferFunction};
use crate::text::*;
use crate::tree::*;
use crate::Tree;

/// How [`Tree::to_svg_string`] writes.
#[derive(Clone, Copy, PartialEq, Eq, Debug, Default)]
pub struct WriteOptions {
    /// Write text as `<text>` even where it has been turned into paths.
    /// (Text that has not is always written as text.)
    pub preserve_text: bool,
}

pub(crate) fn write(tree: &Tree, opt: &WriteOptions) -> String {
    let mut w = Writer {
        opt: *opt,
        defs: String::new(),
        ids: HashMap::new(),
        taken: HashSet::new(),
        next: 0,
    };
    w.collect_ids(tree.root());
    let mut body = String::new();
    w.children(tree.root(), &mut body);
    let size = tree.size();
    let mut out = format!(
        r#"<svg xmlns="http://www.w3.org/2000/svg" width="{}" height="{}">"#,
        num(size.width()),
        num(size.height())
    );
    if !w.defs.is_empty() {
        let _ = write!(out, "<defs>{}</defs>", w.defs);
    }
    out.push_str(&body);
    out.push_str("</svg>");
    out
}

struct Writer {
    opt: WriteOptions,
    /// Finished definitions.
    defs: String,
    /// The ids of shared definitions, by address.
    ids: HashMap<usize, String>,
    /// Ids the document uses itself.
    taken: HashSet<String>,
    next: usize,
}

/// A number that parses back to the same `f32`.
fn num(v: f32) -> String {
    if v.is_finite() {
        format!("{v}")
    } else {
        "0".to_string()
    }
}

fn nums(list: &[f32]) -> String {
    let mut out = String::new();
    for v in list {
        if !out.is_empty() {
            out.push(' ');
        }
        out.push_str(&num(*v));
    }
    out
}

/// Escapes character data or an attribute value.
fn escape(text: &str) -> String {
    let mut out = String::with_capacity(text.len());
    for c in text.chars() {
        match c {
            '&' => out.push_str("&amp;"),
            '<' => out.push_str("&lt;"),
            '>' => out.push_str("&gt;"),
            '"' => out.push_str("&quot;"),
            // Not XML characters.
            '\u{0}'..='\u{8}' | '\u{b}' | '\u{c}' | '\u{e}'..='\u{1f}' => {}
            '\u{fffe}' | '\u{ffff}' => {}
            c => out.push(c),
        }
    }
    out
}

/// Appends ` name="value"`.
fn attr(out: &mut String, name: &str, value: &str) {
    let _ = write!(out, r#" {name}="{}""#, escape(value));
}

fn attr_num(out: &mut String, name: &str, value: f32) {
    attr(out, name, &num(value));
}

/// Appends a number attribute that is not its default.
fn attr_not(out: &mut String, name: &str, value: f32, default: f32) {
    if value.to_bits() != default.to_bits() {
        attr_num(out, name, value);
    }
}

fn attr_transform(out: &mut String, name: &str, t: Transform) {
    if !t.is_identity() {
        let m = nums(&[t.sx, t.ky, t.kx, t.sy, t.tx, t.ty]);
        attr(out, name, &format!("matrix({m})"));
    }
}

fn attr_rect(out: &mut String, r: NonZeroRect) {
    attr_num(out, "x", r.x());
    attr_num(out, "y", r.y());
    attr_num(out, "width", r.width());
    attr_num(out, "height", r.height());
}

fn color(c: Color) -> String {
    format!("#{:02x}{:02x}{:02x}", c.red, c.green, c.blue)
}

fn blend_mode(mode: BlendMode) -> &'static str {
    match mode {
        BlendMode::Multiply => "multiply",
        BlendMode::Screen => "screen",
        BlendMode::Overlay => "overlay",
        BlendMode::Darken => "darken",
        BlendMode::Lighten => "lighten",
        BlendMode::ColorDodge => "color-dodge",
        BlendMode::ColorBurn => "color-burn",
        BlendMode::HardLight => "hard-light",
        BlendMode::SoftLight => "soft-light",
        BlendMode::Difference => "difference",
        BlendMode::Exclusion => "exclusion",
        BlendMode::Hue => "hue",
        BlendMode::Saturation => "saturation",
        BlendMode::Color => "color",
        BlendMode::Luminosity => "luminosity",
        _ => "normal",
    }
}

fn path_data(path: &rusty_raster::Path) -> String {
    let mut d = String::new();
    for seg in path.segments() {
        let (cmd, p): (char, &[rusty_raster::Point]) = match &seg {
            PathSegment::MoveTo(p) => ('M', &[*p]),
            PathSegment::LineTo(p) => ('L', &[*p]),
            PathSegment::QuadTo(a, p) => ('Q', &[*a, *p]),
            PathSegment::CubicTo(a, b, p) => ('C', &[*a, *b, *p]),
            PathSegment::Close => ('Z', &[]),
        };
        d.push(cmd);
        for (i, p) in p.iter().enumerate() {
            if i > 0 {
                d.push(' ');
            }
            let _ = write!(d, "{} {}", num(p.x), num(p.y));
        }
    }
    d
}

fn base64(data: &[u8]) -> String {
    const DIGITS: &[u8; 64] = b"ABCDEFGHIJKLMNOPQRSTUVWXYZabcdefghijklmnopqrstuvwxyz0123456789+/";
    let mut out = String::with_capacity(data.len().div_ceil(3) * 4);
    for c in data.chunks(3) {
        let n = (0..3).fold(0u32, |n, i| n << 8 | c.get(i).map_or(0, |b| u32::from(*b)));
        for i in 0..4 {
            if i <= c.len() {
                out.push(char::from(DIGITS[(n >> (18 - 6 * i)) as usize & 63]));
            } else {
                out.push('=');
            }
        }
    }
    out
}

impl Writer {
    fn collect_ids(&mut self, group: &Group) {
        for c in &group.children {
            if !c.id().is_empty() {
                self.taken.insert(c.id().to_string());
            }
            if let Node::Group(g) = c {
                self.collect_ids(g);
            }
        }
    }

    fn new_id(&mut self, prefix: &str) -> String {
        loop {
            self.next += 1;
            let id = format!("{prefix}{}", self.next);
            if !self.taken.contains(&id) {
                return id;
            }
        }
    }

    /// The id of a shared definition, which `make` writes the first time (the
    /// part of the element after its id attribute).
    fn shared<T>(
        &mut self,
        item: &Arc<T>,
        prefix: &str,
        tag: &str,
        make: impl FnOnce(&mut Writer, &mut String),
    ) -> String {
        // The trees being written outlive the writer: an address is not
        // reused.
        let key = Arc::as_ptr(item) as *const u8 as usize;
        if let Some(id) = self.ids.get(&key) {
            return id.clone();
        }
        let id = self.new_id(prefix);
        self.ids.insert(key, id.clone());
        let mut rest = String::new();
        make(self, &mut rest);
        // Written when complete, so that what it uses itself comes first.
        let _ = write!(self.defs, r#"<{tag} id="{id}"{rest}</{tag}>"#);
        id
    }

    fn children(&mut self, group: &Group, out: &mut String) {
        for c in &group.children {
            self.node(c, out);
        }
    }

    fn node(&mut self, node: &Node, out: &mut String) {
        match node {
            Node::Group(g) => self.group(g, out),
            Node::Path(p) => self.path(p, "", out),
            Node::Image(i) => self.image(i, out),
            Node::Text(t) => match &t.flattened {
                Some(g) if !self.opt.preserve_text => {
                    out.push_str("<g");
                    if !t.id.is_empty() {
                        attr(out, "id", &t.id);
                    }
                    out.push('>');
                    self.children(g, out);
                    out.push_str("</g>");
                }
                _ => self.text(t, out),
            },
        }
    }

    fn group(&mut self, g: &Group, out: &mut String) {
        out.push_str("<g");
        if !g.id.is_empty() {
            attr(out, "id", &g.id);
        }
        attr_transform(out, "transform", g.transform);
        attr_not(out, "opacity", g.opacity, 1.0);
        if let Some(c) = &g.clip_path {
            let id = self.clip_path(c);
            attr(out, "clip-path", &format!("url(#{id})"));
        }
        if let Some(m) = &g.mask {
            let id = self.mask(m);
            attr(out, "mask", &format!("url(#{id})"));
        }
        if !g.filters.is_empty() {
            let list: Vec<String> = g
                .filters
                .iter()
                .map(|f| format!("url(#{})", self.filter(f)))
                .collect();
            attr(out, "filter", &list.join(" "));
        }
        let mut style = String::new();
        if g.blend_mode != BlendMode::SourceOver {
            let _ = write!(style, "mix-blend-mode:{};", blend_mode(g.blend_mode));
        }
        if g.isolate {
            style.push_str("isolation:isolate;");
        }
        if !style.is_empty() {
            attr(out, "style", &style);
        }
        out.push('>');
        self.children(g, out);
        out.push_str("</g>");
    }

    fn path(&mut self, p: &Path, attrs: &str, out: &mut String) {
        out.push_str("<path");
        if !p.id.is_empty() {
            attr(out, "id", &p.id);
        }
        out.push_str(attrs);
        if !p.visible {
            attr(out, "visibility", "hidden");
        }
        self.fill(p.fill.as_ref(), out);
        self.stroke(p.stroke.as_ref(), out);
        if p.paint_order == PaintOrder::StrokeAndFill {
            attr(out, "paint-order", "stroke");
        }
        if !p.anti_alias {
            attr(out, "shape-rendering", "crispEdges");
        }
        attr(out, "d", &path_data(&p.data));
        out.push_str("/>");
    }

    fn fill(&mut self, fill: Option<&Fill>, out: &mut String) {
        let Some(f) = fill else {
            attr(out, "fill", "none");
            return;
        };
        let paint = self.paint(&f.paint);
        if paint != "#000000" {
            attr(out, "fill", &paint);
        }
        attr_not(out, "fill-opacity", f.opacity, 1.0);
        if f.rule == FillRule::EvenOdd {
            attr(out, "fill-rule", "evenodd");
        }
    }

    fn stroke(&mut self, stroke: Option<&Stroke>, out: &mut String) {
        let Some(s) = stroke else {
            return;
        };
        let paint = self.paint(&s.paint);
        attr(out, "stroke", &paint);
        attr_not(out, "stroke-opacity", s.opacity, 1.0);
        attr_not(out, "stroke-width", s.width, 1.0);
        attr_not(out, "stroke-miterlimit", s.miterlimit, 4.0);
        match s.linecap {
            LineCap::Butt => {}
            LineCap::Round => attr(out, "stroke-linecap", "round"),
            LineCap::Square => attr(out, "stroke-linecap", "square"),
        }
        match s.linejoin {
            LineJoin::Miter => {}
            LineJoin::MiterClip => attr(out, "stroke-linejoin", "miter-clip"),
            LineJoin::Round => attr(out, "stroke-linejoin", "round"),
            LineJoin::Bevel => attr(out, "stroke-linejoin", "bevel"),
        }
        if let Some(d) = &s.dasharray {
            attr(out, "stroke-dasharray", &nums(d));
        }
        attr_not(out, "stroke-dashoffset", s.dashoffset, 0.0);
    }

    fn paint(&mut self, paint: &Paint) -> String {
        let id = match paint {
            Paint::Color(c) => return color(*c),
            Paint::LinearGradient(g) => self.shared(g, "lg", "linearGradient", |_, out| {
                attr_num(out, "x1", g.x1);
                attr_num(out, "y1", g.y1);
                attr_num(out, "x2", g.x2);
                attr_num(out, "y2", g.y2);
                gradient(&g.base, out);
            }),
            Paint::RadialGradient(g) => self.shared(g, "rg", "radialGradient", |_, out| {
                attr_num(out, "cx", g.cx);
                attr_num(out, "cy", g.cy);
                attr_num(out, "r", g.r);
                attr_num(out, "fx", g.fx);
                attr_num(out, "fy", g.fy);
                attr_not(out, "fr", g.fr, 0.0);
                gradient(&g.base, out);
            }),
            Paint::Pattern(p) => self.shared(p, "pt", "pattern", |w, out| {
                attr_rect(out, p.rect);
                attr(out, "patternUnits", "userSpaceOnUse");
                attr_transform(out, "patternTransform", p.transform);
                out.push('>');
                w.children(&p.root, out);
            }),
        };
        format!("url(#{id})")
    }

    fn clip_path(&mut self, c: &Arc<ClipPath>) -> String {
        self.shared(c, "cp", "clipPath", |w, out| {
            w.clip_path_content(c, Transform::identity(), out);
        })
    }

    /// `before` takes coordinates of where the clip path is defined to those
    /// of the element it is written for.
    fn clip_path_content(&mut self, c: &ClipPath, before: Transform, out: &mut String) {
        if let Some(inner) = &c.clip_path {
            // `clip-path` on a `clipPath` is in the same coordinates.
            let id = if before.is_identity() {
                self.clip_path(inner)
            } else {
                self.moved_clip(inner, before)
            };
            attr(out, "clip-path", &format!("url(#{id})"));
        }
        attr_transform(out, "transform", before.pre_concat(c.transform));
        out.push('>');
        self.clip_children(&c.root, Transform::identity(), None, out);
    }

    fn moved_clip(&mut self, c: &ClipPath, before: Transform) -> String {
        let id = self.new_id("cp");
        let mut rest = String::new();
        self.clip_path_content(c, before, &mut rest);
        let _ = write!(self.defs, r#"<clipPath id="{id}"{rest}</clipPath>"#);
        id
    }

    /// A `clipPath` holds shapes only: groups come down to the transform and
    /// clip path of each of their shapes. (Of clip paths on nested groups,
    /// the innermost is kept.) `clip` is in the coordinates `ts` leads to.
    fn clip_children(
        &mut self,
        group: &Group,
        ts: Transform,
        clip: Option<(&Arc<ClipPath>, Transform)>,
        out: &mut String,
    ) {
        for c in &group.children {
            match c {
                Node::Path(p) => {
                    if !p.visible || p.fill.is_none() {
                        continue;
                    }
                    let mut a = String::new();
                    attr_transform(&mut a, "transform", ts);
                    if let Some((c, since)) = clip {
                        let id = match since.invert() {
                            _ if since.is_identity() => self.clip_path(c),
                            Some(back) => self.moved_clip(c, back),
                            None => continue,
                        };
                        attr(&mut a, "clip-path", &format!("url(#{id})"));
                    }
                    if p.fill.as_ref().is_some_and(|f| f.rule == FillRule::EvenOdd) {
                        attr(&mut a, "clip-rule", "evenodd");
                    }
                    if !p.anti_alias {
                        attr(&mut a, "shape-rendering", "crispEdges");
                    }
                    let _ = write!(out, r#"<path{a} d="{}"/>"#, path_data(&p.data));
                }
                Node::Group(g) => {
                    let inner_ts = ts.pre_concat(g.transform);
                    let inner_clip = match &g.clip_path {
                        Some(c) => Some((c, Transform::identity())),
                        None => clip.map(|(c, since)| (c, since.pre_concat(g.transform))),
                    };
                    self.clip_children(g, inner_ts, inner_clip, out);
                }
                Node::Text(t) => {
                    if let Some(g) = &t.flattened {
                        self.clip_children(g, ts, clip, out);
                    }
                }
                Node::Image(_) => {}
            }
        }
    }

    fn mask(&mut self, m: &Arc<Mask>) -> String {
        self.shared(m, "mk", "mask", |w, out| {
            attr_rect(out, m.rect);
            attr(out, "maskUnits", "userSpaceOnUse");
            if m.kind == MaskType::Alpha {
                attr(out, "mask-type", "alpha");
            }
            if let Some(inner) = &m.mask {
                let id = w.mask(inner);
                attr(out, "mask", &format!("url(#{id})"));
            }
            out.push('>');
            w.children(&m.root, out);
        })
    }

    fn image(&mut self, i: &Image, out: &mut String) {
        let (mime, data) = match &i.kind {
            ImageKind::Png(d) => ("png", base64(d)),
            ImageKind::Jpeg(d) => ("jpeg", base64(d)),
            ImageKind::Gif(d) => ("gif", base64(d)),
            ImageKind::WebP(d) => ("webp", base64(d)),
            ImageKind::Svg(t) => ("svg+xml", base64(write(t, &self.opt).as_bytes())),
        };
        out.push_str("<image");
        if !i.id.is_empty() {
            attr(out, "id", &i.id);
        }
        attr_num(out, "width", i.size.width());
        attr_num(out, "height", i.size.height());
        if !i.visible {
            attr(out, "visibility", "hidden");
        }
        match i.quality {
            FilterQuality::Nearest => attr(out, "image-rendering", "optimizeSpeed"),
            // Only a style may say this.
            FilterQuality::Bilinear => attr(out, "style", "image-rendering:smooth"),
            FilterQuality::Bicubic => {}
        }
        let _ = write!(out, r#" href="data:image/{mime};base64,{data}"/>"#);
    }

    // -------------------------------------------------------------- filters

    fn filter(&mut self, f: &Arc<Filter>) -> String {
        self.shared(f, "fl", "filter", |w, out| {
            attr(out, "filterUnits", "userSpaceOnUse");
            attr_rect(out, f.rect);
            out.push('>');
            for p in &f.primitives {
                w.primitive(p, out);
            }
        })
    }

    fn primitive(&mut self, p: &filter::Primitive, out: &mut String) {
        let mut a = String::new();
        attr_rect(&mut a, p.rect);
        if p.color_interpolation == filter::ColorInterpolation::SRgb {
            attr(&mut a, "color-interpolation-filters", "sRGB");
        }
        attr(&mut a, "result", &p.result);
        let input = |a: &mut String, name: &str, input: &Input| {
            let v = match input {
                Input::SourceGraphic => "SourceGraphic",
                Input::SourceAlpha => "SourceAlpha",
                Input::Reference(r) => r,
            };
            attr(a, name, v);
        };
        let mut body = String::new();
        let tag = match &p.kind {
            Kind::Blend(fe) => {
                input(&mut a, "in", &fe.input1);
                input(&mut a, "in2", &fe.input2);
                attr(&mut a, "mode", blend_mode(fe.mode));
                "feBlend"
            }
            Kind::ColorMatrix(fe) => {
                input(&mut a, "in", &fe.input);
                let (kind, values) = match fe.kind {
                    filter::ColorMatrixKind::Matrix(m) => ("matrix", nums(&m)),
                    // Only a filter function can ask for more than 1.
                    filter::ColorMatrixKind::Saturate(v) if v > 1.0 => {
                        let mut m = [0.0f32; 20];
                        for (row, (r, g, b)) in [
                            (0.213 + 0.787 * v, 0.715 - 0.715 * v, 0.072 - 0.072 * v),
                            (0.213 - 0.213 * v, 0.715 + 0.285 * v, 0.072 - 0.072 * v),
                            (0.213 - 0.213 * v, 0.715 - 0.715 * v, 0.072 + 0.928 * v),
                        ]
                        .into_iter()
                        .enumerate()
                        {
                            m[row * 5..row * 5 + 3].copy_from_slice(&[r, g, b]);
                        }
                        m[18] = 1.0;
                        ("matrix", nums(&m))
                    }
                    filter::ColorMatrixKind::Saturate(v) => ("saturate", num(v)),
                    filter::ColorMatrixKind::HueRotate(v) => ("hueRotate", num(v)),
                    filter::ColorMatrixKind::LuminanceToAlpha => {
                        ("luminanceToAlpha", String::new())
                    }
                };
                attr(&mut a, "type", kind);
                if !values.is_empty() {
                    attr(&mut a, "values", &values);
                }
                "feColorMatrix"
            }
            Kind::ComponentTransfer(fe) => {
                input(&mut a, "in", &fe.input);
                for (tag, func) in [
                    ("feFuncR", &fe.func_r),
                    ("feFuncG", &fe.func_g),
                    ("feFuncB", &fe.func_b),
                    ("feFuncA", &fe.func_a),
                ] {
                    transfer_function(tag, func, &mut body);
                }
                "feComponentTransfer"
            }
            Kind::Composite(fe) => {
                input(&mut a, "in", &fe.input1);
                input(&mut a, "in2", &fe.input2);
                use filter::CompositeOperator as Op;
                let op = match fe.operator {
                    Op::Over => "over",
                    Op::In => "in",
                    Op::Out => "out",
                    Op::Atop => "atop",
                    Op::Xor => "xor",
                    Op::Arithmetic { k1, k2, k3, k4 } => {
                        for (name, k) in [("k1", k1), ("k2", k2), ("k3", k3), ("k4", k4)] {
                            attr_num(&mut a, name, k);
                        }
                        "arithmetic"
                    }
                };
                attr(&mut a, "operator", op);
                "feComposite"
            }
            Kind::ConvolveMatrix(fe) => {
                input(&mut a, "in", &fe.input);
                attr(&mut a, "order", &format!("{} {}", fe.columns, fe.rows));
                attr(&mut a, "kernelMatrix", &nums(&fe.kernel));
                attr_num(&mut a, "divisor", fe.divisor);
                attr_num(&mut a, "bias", fe.bias);
                attr(&mut a, "targetX", &fe.target_x.to_string());
                attr(&mut a, "targetY", &fe.target_y.to_string());
                let edge = match fe.edge_mode {
                    filter::EdgeMode::None => "none",
                    filter::EdgeMode::Duplicate => "duplicate",
                    filter::EdgeMode::Wrap => "wrap",
                };
                attr(&mut a, "edgeMode", edge);
                if fe.preserve_alpha {
                    attr(&mut a, "preserveAlpha", "true");
                }
                "feConvolveMatrix"
            }
            Kind::DiffuseLighting(fe) => {
                input(&mut a, "in", &fe.input);
                attr_num(&mut a, "surfaceScale", fe.surface_scale);
                attr_num(&mut a, "diffuseConstant", fe.diffuse_constant);
                attr(&mut a, "lighting-color", &color(fe.lighting_color));
                light(&fe.light_source, &mut body);
                "feDiffuseLighting"
            }
            Kind::SpecularLighting(fe) => {
                input(&mut a, "in", &fe.input);
                attr_num(&mut a, "surfaceScale", fe.surface_scale);
                attr_num(&mut a, "specularConstant", fe.specular_constant);
                attr_num(&mut a, "specularExponent", fe.specular_exponent);
                attr(&mut a, "lighting-color", &color(fe.lighting_color));
                light(&fe.light_source, &mut body);
                "feSpecularLighting"
            }
            Kind::DisplacementMap(fe) => {
                input(&mut a, "in", &fe.input1);
                input(&mut a, "in2", &fe.input2);
                attr_num(&mut a, "scale", fe.scale);
                let channel = |c: filter::ColorChannel| match c {
                    filter::ColorChannel::R => "R",
                    filter::ColorChannel::G => "G",
                    filter::ColorChannel::B => "B",
                    filter::ColorChannel::A => "A",
                };
                attr(&mut a, "xChannelSelector", channel(fe.x_channel_selector));
                attr(&mut a, "yChannelSelector", channel(fe.y_channel_selector));
                "feDisplacementMap"
            }
            Kind::DropShadow(fe) => {
                input(&mut a, "in", &fe.input);
                attr_num(&mut a, "dx", fe.dx);
                attr_num(&mut a, "dy", fe.dy);
                attr(&mut a, "stdDeviation", &nums(&[fe.std_dev_x, fe.std_dev_y]));
                attr(&mut a, "flood-color", &color(fe.color));
                attr_num(&mut a, "flood-opacity", fe.opacity);
                "feDropShadow"
            }
            Kind::Flood(fe) => {
                attr(&mut a, "flood-color", &color(fe.color));
                attr_num(&mut a, "flood-opacity", fe.opacity);
                "feFlood"
            }
            Kind::GaussianBlur(fe) => {
                input(&mut a, "in", &fe.input);
                attr(&mut a, "stdDeviation", &nums(&[fe.std_dev_x, fe.std_dev_y]));
                "feGaussianBlur"
            }
            Kind::Image(fe) => {
                let id = self.new_id("im");
                let mut g = String::new();
                self.children(&fe.root, &mut g);
                let _ = write!(self.defs, r#"<g id="{id}">{g}</g>"#);
                attr(&mut a, "href", &format!("#{id}"));
                "feImage"
            }
            Kind::Merge(fe) => {
                for i in &fe.inputs {
                    let mut n = String::from("<feMergeNode");
                    input(&mut n, "in", i);
                    let _ = write!(body, "{n}/>");
                }
                "feMerge"
            }
            Kind::Morphology(fe) => {
                input(&mut a, "in", &fe.input);
                if fe.operator == filter::MorphologyOperator::Dilate {
                    attr(&mut a, "operator", "dilate");
                }
                attr(&mut a, "radius", &nums(&[fe.radius_x, fe.radius_y]));
                "feMorphology"
            }
            Kind::Offset(fe) => {
                input(&mut a, "in", &fe.input);
                attr_num(&mut a, "dx", fe.dx);
                attr_num(&mut a, "dy", fe.dy);
                "feOffset"
            }
            Kind::Tile(fe) => {
                input(&mut a, "in", &fe.input);
                "feTile"
            }
            Kind::Turbulence(fe) => {
                attr(
                    &mut a,
                    "baseFrequency",
                    &nums(&[fe.base_frequency_x, fe.base_frequency_y]),
                );
                attr(&mut a, "numOctaves", &fe.num_octaves.to_string());
                attr(&mut a, "seed", &fe.seed.to_string());
                if fe.stitch_tiles {
                    attr(&mut a, "stitchTiles", "stitch");
                }
                if fe.fractal_noise {
                    attr(&mut a, "type", "fractalNoise");
                }
                "feTurbulence"
            }
        };
        if body.is_empty() {
            let _ = write!(out, "<{tag}{a}/>");
        } else {
            let _ = write!(out, "<{tag}{a}>{body}</{tag}>");
        }
    }

    // ----------------------------------------------------------------- text

    fn text(&mut self, t: &Text, out: &mut String) {
        out.push_str("<text");
        if !t.id.is_empty() {
            attr(out, "id", &t.id);
        }
        // The characters are as they are to be drawn.
        attr(out, "xml:space", "preserve");
        if t.writing_mode == WritingMode::TopToBottom {
            attr(out, "writing-mode", "tb");
        }
        if !t.anti_alias {
            attr(out, "text-rendering", "optimizeSpeed");
        }
        for (name, list) in [("dx", &t.dx), ("dy", &t.dy), ("rotate", &t.rotate)] {
            if list.iter().any(|v| *v != 0.0) {
                attr(out, name, &nums(list));
            }
        }
        out.push('>');
        for chunk in &t.chunks {
            let on_path = match &chunk.text_flow {
                TextFlow::Linear => false,
                TextFlow::Path(p) => {
                    let id = self.shared(&p.path, "tp", "path", |_, out| {
                        attr(out, "d", &path_data(&p.path));
                        out.push('>');
                    });
                    out.push_str("<textPath");
                    attr(out, "href", &format!("#{id}"));
                    attr_not(out, "startOffset", p.start_offset, 0.0);
                    out.push('>');
                    true
                }
            };
            out.push_str("<tspan");
            if let Some(x) = chunk.x {
                attr_num(out, "x", x);
            }
            if let Some(y) = chunk.y {
                attr_num(out, "y", y);
            }
            match chunk.anchor {
                TextAnchor::Start => {}
                TextAnchor::Middle => attr(out, "text-anchor", "middle"),
                TextAnchor::End => attr(out, "text-anchor", "end"),
            }
            out.push('>');
            for span in &chunk.spans {
                self.span(span, &chunk.text, out);
            }
            out.push_str(if on_path {
                "</tspan></textPath>"
            } else {
                "</tspan>"
            });
        }
        out.push_str("</text>");
    }

    fn span(&mut self, s: &TextSpan, text: &str, out: &mut String) {
        // Each decoration is painted as the element that sets it.
        let mut close = 0;
        let d = &s.decoration;
        for (kind, style) in [
            ("underline", &d.underline),
            ("overline", &d.overline),
            ("line-through", &d.line_through),
        ] {
            if let Some(style) = style {
                out.push_str("<tspan");
                attr(out, "text-decoration", kind);
                self.fill(style.fill.as_ref(), out);
                self.stroke(style.stroke.as_ref(), out);
                out.push('>');
                close += 1;
            }
        }
        // Innermost first; a shift applies to what is inside it.
        for shift in s.baseline_shift.iter().rev() {
            let v = match shift {
                BaselineShift::Baseline => "baseline".to_string(),
                BaselineShift::Subscript => "sub".to_string(),
                BaselineShift::Superscript => "super".to_string(),
                BaselineShift::Number(n) => num(*n),
            };
            out.push_str("<tspan");
            attr(out, "baseline-shift", &v);
            out.push('>');
            close += 1;
        }
        out.push_str("<tspan");
        self.fill(s.fill.as_ref(), out);
        match &s.stroke {
            None => attr(out, "stroke", "none"),
            s => self.stroke(s.as_ref(), out),
        }
        if s.paint_order == PaintOrder::StrokeAndFill {
            attr(out, "paint-order", "stroke");
        }
        let families: Vec<String> = s.font.families.iter().map(ToString::to_string).collect();
        attr(out, "font-family", &families.join(", "));
        attr_num(out, "font-size", s.font_size);
        match s.font.style {
            FontStyle::Normal => {}
            FontStyle::Italic => attr(out, "font-style", "italic"),
            FontStyle::Oblique => attr(out, "font-style", "oblique"),
        }
        if s.font.weight != 400 {
            attr(out, "font-weight", &s.font.weight.to_string());
        }
        let stretch = match s.font.stretch {
            FontStretch::UltraCondensed => "ultra-condensed",
            FontStretch::ExtraCondensed => "extra-condensed",
            FontStretch::Condensed => "condensed",
            FontStretch::SemiCondensed => "semi-condensed",
            FontStretch::Normal => "",
            FontStretch::SemiExpanded => "semi-expanded",
            FontStretch::Expanded => "expanded",
            FontStretch::ExtraExpanded => "extra-expanded",
            FontStretch::UltraExpanded => "ultra-expanded",
        };
        if !stretch.is_empty() {
            attr(out, "font-stretch", stretch);
        }
        let axes: Vec<String> = s
            .font
            .variations
            .iter()
            .filter_map(|v| {
                let tag = std::str::from_utf8(&v.tag).ok()?;
                Some(format!("'{tag}' {}", num(v.value)))
            })
            .collect();
        if !axes.is_empty() {
            attr(out, "font-variation-settings", &axes.join(", "));
        }
        if s.small_caps {
            attr(out, "font-variant", "small-caps");
        }
        if !s.apply_kerning {
            attr(out, "style", "font-kerning:none");
        }
        if !s.optical_sizing {
            attr(out, "font-optical-sizing", "none");
        }
        for (name, b) in [
            ("dominant-baseline", s.dominant_baseline),
            ("alignment-baseline", s.alignment_baseline),
        ] {
            let v = match b {
                Baseline::Auto => continue,
                Baseline::Baseline => "baseline",
                Baseline::BeforeEdge => "text-before-edge",
                Baseline::Middle => "middle",
                Baseline::Central => "central",
                Baseline::AfterEdge => "text-after-edge",
                Baseline::Ideographic => "ideographic",
                Baseline::Alphabetic => "alphabetic",
                Baseline::Hanging => "hanging",
                Baseline::Mathematical => "mathematical",
            };
            attr(out, name, v);
        }
        attr_not(out, "letter-spacing", s.letter_spacing, 0.0);
        attr_not(out, "word-spacing", s.word_spacing, 0.0);
        if let Some(l) = s.text_length {
            attr_num(out, "textLength", l);
        }
        if s.adjust_glyphs {
            attr(out, "lengthAdjust", "spacingAndGlyphs");
        }
        if !s.visible {
            attr(out, "visibility", "hidden");
        }
        out.push('>');
        out.push_str(&escape(text.get(s.start..s.end).unwrap_or("")));
        out.push_str("</tspan>");
        for _ in 0..close {
            out.push_str("</tspan>");
        }
    }
}

fn gradient(g: &BaseGradient, out: &mut String) {
    attr(out, "gradientUnits", "userSpaceOnUse");
    attr_transform(out, "gradientTransform", g.transform);
    match g.spread {
        SpreadMode::Pad => {}
        SpreadMode::Reflect => attr(out, "spreadMethod", "reflect"),
        SpreadMode::Repeat => attr(out, "spreadMethod", "repeat"),
    }
    out.push('>');
    for s in &g.stops {
        out.push_str("<stop");
        attr_num(out, "offset", s.offset);
        attr(out, "stop-color", &color(s.color));
        attr_not(out, "stop-opacity", s.opacity, 1.0);
        out.push_str("/>");
    }
}

fn transfer_function(tag: &str, func: &TransferFunction, out: &mut String) {
    let mut a = String::new();
    match func {
        TransferFunction::Identity => return,
        TransferFunction::Table(v) => {
            attr(&mut a, "type", "table");
            attr(&mut a, "tableValues", &nums(v));
        }
        TransferFunction::Discrete(v) => {
            attr(&mut a, "type", "discrete");
            attr(&mut a, "tableValues", &nums(v));
        }
        TransferFunction::Linear { slope, intercept } => {
            attr(&mut a, "type", "linear");
            attr_num(&mut a, "slope", *slope);
            attr_num(&mut a, "intercept", *intercept);
        }
        TransferFunction::Gamma {
            amplitude,
            exponent,
            offset,
        } => {
            attr(&mut a, "type", "gamma");
            attr_num(&mut a, "amplitude", *amplitude);
            attr_num(&mut a, "exponent", *exponent);
            attr_num(&mut a, "offset", *offset);
        }
    }
    let _ = write!(out, "<{tag}{a}/>");
}

fn light(l: &LightSource, out: &mut String) {
    let mut a = String::new();
    let tag = match l {
        LightSource::Distant { azimuth, elevation } => {
            attr_num(&mut a, "azimuth", *azimuth);
            attr_num(&mut a, "elevation", *elevation);
            "feDistantLight"
        }
        LightSource::Point { x, y, z } => {
            for (name, v) in [("x", x), ("y", y), ("z", z)] {
                attr_num(&mut a, name, *v);
            }
            "fePointLight"
        }
        LightSource::Spot(s) => {
            for (name, v) in [
                ("x", s.x),
                ("y", s.y),
                ("z", s.z),
                ("pointsAtX", s.points_at_x),
                ("pointsAtY", s.points_at_y),
                ("pointsAtZ", s.points_at_z),
                ("specularExponent", s.specular_exponent),
            ] {
                attr_num(&mut a, name, v);
            }
            if let Some(v) = s.limiting_cone_angle {
                attr_num(&mut a, "limitingConeAngle", v);
            }
            "feSpotLight"
        }
    };
    let _ = write!(out, "<{tag}{a}/>");
}
