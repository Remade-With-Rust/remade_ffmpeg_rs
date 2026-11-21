//! Converts the resolved SVG document into the render tree, following usvg's
//! converter semantics (so resvg is a fair oracle).

use std::collections::HashMap;
use std::sync::Arc;

use rusty_raster::{
    BlendMode, FillRule, FilterQuality, LineCap, LineJoin, MaskType, NonZeroRect, PathBuilder,
    PathStroker, Rect, Size, SpreadMode, Transform,
};

use crate::svgtree::{AId, Document, EId, NodeId, SvgNode};
use crate::tree::{
    BaseGradient, ClipPath, Color, Fill, Group, Image, ImageKind, LinearGradient, Mask, Node,
    Paint, PaintOrder, Path, Pattern, RadialGradient, Stop, Stroke,
};
use crate::values::{self, Align, AspectRatio, Length, Seg, Unit};
use crate::{Error, Options};

/// Coordinate system of a paint server, clip path or mask.
#[derive(Clone, Copy, PartialEq, Eq, Debug)]
enum Units {
    UserSpaceOnUse,
    ObjectBoundingBox,
}

#[derive(Clone)]
struct State<'a> {
    in_clip_path: bool,
    view_box: NonZeroRect,
    use_size: (Option<f32>, Option<f32>),
    opt: &'a Options,
    /// Markers being instantiated (a marker cannot contain itself).
    parent_markers: Vec<NodeId>,
    /// The paints `context-fill` / `context-stroke` resolve to.
    context: Option<(Option<Fill>, Option<Stroke>)>,
}

#[derive(Default)]
struct Cache {
    clip_paths: HashMap<NodeId, Arc<ClipPath>>,
    masks: HashMap<NodeId, Arc<Mask>>,
    /// Clip paths / masks / patterns being converted: a reference back into
    /// one of them is a cycle and is treated as `none`.
    active: Vec<NodeId>,
    /// Elements converted so far, across every re-conversion of shared
    /// content (`objectBoundingBox` clip paths, patterns, markers): bounds
    /// the output tree, which bounds render work.
    work: usize,
    max_work: usize,
    /// The builder every shape is built in, kept with its capacity: a shape
    /// grows it only past the largest one before it, and leaves as an
    /// exactly sized copy (see `Cache::finish_path`).
    path_scratch: Option<PathBuilder>,
    /// The stroker the stroke bounding boxes are taken with, kept so its
    /// per-contour scratch keeps its capacity (it resets all of its state at
    /// the start of every stroke).
    stroker: PathStroker,
    /// The point count a huge path's builder was sized for (see
    /// `Cache::path_data_builder`).
    presized: Option<usize>,
}

/// A shape with more points than this keeps the scratch builder's buffers as
/// its own instead of being copied out of them (no second copy of a huge
/// path is ever held).
const SCRATCH_MAX_POINTS: usize = 1 << 16;

/// Path data at least this long is built in a builder sized by
/// `values::path_data_size_estimate`.
const PRESIZE_MIN_BYTES: usize = 1 << 16;

impl Cache {
    /// Spends one unit of conversion work; `false` once the budget is gone.
    fn spend(&mut self) -> bool {
        self.work += 1;
        self.work <= self.max_work
    }

    /// The (empty) builder to build a shape in.
    // Not `unwrap_or_default()`: `PathBuilder::default()` is not
    // `PathBuilder::new()` (it does not inject the initial move-to).
    #[allow(clippy::unwrap_or_default)]
    fn path_builder(&mut self) -> PathBuilder {
        self.path_scratch.take().unwrap_or_else(PathBuilder::new)
    }

    /// The builder to build the path data `d` in: for huge data, one of
    /// about its final size (instead of a chain of doubling copies, whose
    /// last step holds the old and the new buffers at once).
    fn path_data_builder(&mut self, d: &str) -> PathBuilder {
        if d.len() >= PRESIZE_MIN_BYTES {
            if let Some(b) = self.presized_builder(d) {
                return b;
            }
        }
        self.path_builder()
    }

    #[cold]
    #[inline(never)]
    fn presized_builder(&mut self, d: &str) -> Option<PathBuilder> {
        let size = values::path_data_size_estimate(d);
        self.presized = size.map(|(_, points)| points);
        size.map(|(verbs, points)| PathBuilder::with_capacity(verbs, points))
    }

    /// Finishes a shape built in `path_builder()`'s builder: the path is an
    /// exactly sized copy, and the builder goes back to the scratch slot.
    fn finish_path(&mut self, b: PathBuilder) -> Option<Arc<rusty_raster::Path>> {
        let path = b.finish()?;
        if path.points().len() > SCRATCH_MAX_POINTS {
            // A presized builder left more than half empty (data that stops
            // early, say) is not kept at that size.
            let sized = self.presized.take();
            if sized.is_some_and(|p| path.points().len() < p / 2) {
                return Some(Arc::new(path.clone()));
            }
            return Some(Arc::new(path));
        }
        let out = path.clone();
        self.path_scratch = Some(path.clear());
        Some(Arc::new(out))
    }
}

/// Maximum nesting of clip-path / mask / pattern references being resolved
/// at once (each level is a real reference, cycles are broken separately).
const MAX_REF_DEPTH: usize = 32;

fn ts_from_matrix(m: values::Matrix) -> Transform {
    Transform::from_row(
        m[0] as f32,
        m[1] as f32,
        m[2] as f32,
        m[3] as f32,
        m[4] as f32,
        m[5] as f32,
    )
}

impl<'a> SvgNode<'a> {
    fn length_attr(&self, name: AId) -> Option<Length> {
        values::length(self.attribute(name)?)
    }

    fn opacity_attr(&self, name: AId) -> Option<f32> {
        values::opacity(self.attribute(name)?)
    }

    fn convert_length(&self, name: AId, units: Units, state: &State, def: Length) -> f32 {
        convert_length(
            self.length_attr(name).unwrap_or(def),
            *self,
            name,
            units,
            state,
        )
    }

    fn convert_user_length(&self, name: AId, state: &State, def: Length) -> f32 {
        self.convert_length(name, Units::UserSpaceOnUse, state, def)
    }

    fn parse_viewbox(&self) -> Option<NonZeroRect> {
        let vb = values::view_box(self.attribute(AId::ViewBox)?)?;
        NonZeroRect::from_xywh(vb[0] as f32, vb[1] as f32, vb[2] as f32, vb[3] as f32)
    }

    fn aspect(&self) -> AspectRatio {
        self.attribute(AId::PreserveAspectRatio)
            .and_then(values::aspect_ratio)
            .unwrap_or_default()
    }

    /// A transform attribute; invalid (non-finite or degenerate) becomes
    /// identity, as usvg does. `transform-origin` is applied.
    fn resolve_transform(&self, name: AId, state: &State) -> Transform {
        let mut ts = match self.attribute(name).and_then(values::transform) {
            Some(m) => {
                let t = ts_from_matrix(m);
                if t.is_valid() {
                    t
                } else {
                    Transform::identity()
                }
            }
            None => Transform::identity(),
        };
        if let Some(origin) = self.attribute(AId::TransformOrigin) {
            if let Some((ox, oy)) = parse_transform_origin(origin) {
                let dx = convert_length(ox, *self, AId::Width, Units::UserSpaceOnUse, state);
                let dy = convert_length(oy, *self, AId::Height, Units::UserSpaceOnUse, state);
                ts = Transform::identity()
                    .pre_translate(dx, dy)
                    .pre_concat(ts)
                    .pre_translate(-dx, -dy);
            }
        }
        ts
    }

    /// `transform` that parses to a non-invertible matrix hides the element.
    fn has_valid_transform(&self, name: AId) -> bool {
        match self.attribute(name).and_then(values::transform) {
            Some(m) => ts_from_matrix(m).is_valid(),
            None => true,
        }
    }

    fn is_visible_element(&self, opt: &Options) -> bool {
        self.attribute(AId::Display).map(values::trim) != Some("none")
            && self.has_valid_transform(AId::Transform)
            && switch_condition_passed(*self, opt)
    }
}

fn parse_transform_origin(text: &str) -> Option<(Length, Length)> {
    let words: Vec<&str> = text.split_ascii_whitespace().collect();
    let kw = |w: &str| -> Option<Length> {
        Some(match w {
            "left" | "top" => Length::new(0.0, Unit::Percent),
            "center" => Length::new(50.0, Unit::Percent),
            "right" | "bottom" => Length::new(100.0, Unit::Percent),
            _ => values::length(w)?,
        })
    };
    match words.as_slice() {
        [x] => Some((kw(x)?, Length::new(50.0, Unit::Percent))),
        [x, y] | [x, y, _] => {
            if matches!(*x, "top" | "bottom") || matches!(*y, "left" | "right") {
                Some((kw(y)?, kw(x)?))
            } else {
                Some((kw(x)?, kw(y)?))
            }
        }
        _ => None,
    }
}

fn convert_length(length: Length, node: SvgNode, name: AId, units: Units, state: &State) -> f32 {
    let dpi = state.opt.dpi;
    let n = length.number as f32;
    match length.unit {
        Unit::None | Unit::Px => n,
        Unit::Em => n * resolve_font_size(node, state),
        Unit::Ex => n * resolve_font_size(node, state) / 2.0,
        Unit::In => n * dpi,
        Unit::Cm => n * dpi / 2.54,
        Unit::Mm => n * dpi / 25.4,
        Unit::Pt => n * dpi / 72.0,
        Unit::Pc => n * dpi / 6.0,
        Unit::Percent => {
            if units == Units::ObjectBoundingBox {
                n / 100.0
            } else {
                let vb = state.view_box;
                match name {
                    AId::Cx
                    | AId::Fx
                    | AId::MarkerWidth
                    | AId::RefX
                    | AId::Rx
                    | AId::Width
                    | AId::X
                    | AId::X1
                    | AId::X2 => vb.width() * n / 100.0,
                    AId::Cy
                    | AId::Fy
                    | AId::Height
                    | AId::MarkerHeight
                    | AId::RefY
                    | AId::Ry
                    | AId::Y
                    | AId::Y1
                    | AId::Y2 => vb.height() * n / 100.0,
                    _ => {
                        let len = ((vb.width().powi(2) + vb.height().powi(2)) / 2.0).sqrt();
                        len * n / 100.0
                    }
                }
            }
        }
    }
}

fn resolve_font_size(node: SvgNode, state: &State) -> f32 {
    let nodes: Vec<SvgNode> = node.ancestors().collect();
    let mut font_size = state.opt.font_size;
    for n in nodes.iter().rev().skip(1) {
        let Some(v) = n.attribute(AId::FontSize) else {
            continue;
        };
        if let Some(length) = values::length(v) {
            let dpi = state.opt.dpi;
            let x = length.number as f32;
            font_size = match length.unit {
                Unit::None | Unit::Px => x,
                Unit::Em => x * font_size,
                Unit::Ex => x * font_size / 2.0,
                Unit::In => x * dpi,
                Unit::Cm => x * dpi / 2.54,
                Unit::Mm => x * dpi / 25.4,
                Unit::Pt => x * dpi / 72.0,
                Unit::Pc => x * dpi / 6.0,
                Unit::Percent => x * font_size * 0.01,
            };
        } else {
            let factor = match v.trim() {
                "xx-small" => -3,
                "x-small" => -2,
                "small" | "smaller" => -1,
                "large" | "larger" => 1,
                "x-large" => 2,
                "xx-large" => 3,
                _ => 0,
            };
            font_size *= 1.2f32.powi(factor);
        }
    }
    font_size
}

/// SVG 1.1 feature strings usvg reports as supported.
const FEATURES: &[&str] = &[
    "SVGDOM-static",
    "SVG-static",
    "CoreAttribute",
    "Structure",
    "BasicStructure",
    "ContainerAttribute",
    "ConditionalProcessing",
    "Image",
    "Style",
    "Shape",
    "Text",
    "BasicText",
    "PaintAttribute",
    "BasicPaintAttribute",
    "OpacityAttribute",
    "GraphicsAttribute",
    "BasicGraphicsAttribute",
    "Marker",
    "Gradient",
    "Pattern",
    "Clip",
    "BasicClip",
    "Mask",
    "Filter",
    "BasicFilter",
    "XlinkAttribute",
];

/// `switch` / conditional processing: `requiredExtensions` fails,
/// `systemLanguage` must match one of the configured languages.
fn switch_condition_passed(node: SvgNode, opt: &Options) -> bool {
    if node.has_attribute(AId::RequiredExtensions) {
        return false;
    }
    if let Some(features) = node.attribute(AId::RequiredFeatures) {
        for feature in features.split(' ') {
            let known = feature
                .strip_prefix("http://www.w3.org/TR/SVG11/feature#")
                .is_some_and(|f| FEATURES.contains(&f));
            if !known {
                return false;
            }
        }
    }
    if let Some(langs) = node.attribute(AId::SystemLanguage) {
        let mut ok = false;
        for lang in langs.split(',') {
            let lang = lang.trim();
            for want in &opt.languages {
                if lang == want {
                    ok = true;
                } else if let Some((prefix, _)) = lang.split_once('-') {
                    if prefix == want {
                        ok = true;
                    }
                }
            }
        }
        if !ok {
            return false;
        }
    }
    true
}

pub(crate) fn view_box_to_transform(vb: NonZeroRect, aspect: AspectRatio, size: Size) -> Transform {
    let sx = size.width() / vb.width();
    let sy = size.height() / vb.height();
    let (sx, sy) = if aspect.align == Align::None {
        (sx, sy)
    } else {
        let s = if aspect.slice {
            if sx < sy {
                sy
            } else {
                sx
            }
        } else if sx > sy {
            sy
        } else {
            sx
        };
        (s, s)
    };
    let x = -vb.x() * sx;
    let y = -vb.y() * sy;
    let w = size.width() - vb.width() * sx;
    let h = size.height() - vb.height() * sy;
    let (tx, ty) = aligned_pos(aspect.align, x, y, w, h);
    Transform::from_row(sx, 0.0, 0.0, sy, tx, ty)
}

fn aligned_pos(align: Align, x: f32, y: f32, w: f32, h: f32) -> (f32, f32) {
    match align {
        Align::None | Align::XMinYMin => (x, y),
        Align::XMidYMin => (x + w / 2.0, y),
        Align::XMaxYMin => (x + w, y),
        Align::XMinYMid => (x, y + h / 2.0),
        Align::XMidYMid => (x + w / 2.0, y + h / 2.0),
        Align::XMaxYMid => (x + w, y + h / 2.0),
        Align::XMinYMax => (x, y + h),
        Align::XMidYMax => (x + w / 2.0, y + h),
        Align::XMaxYMax => (x + w, y + h),
    }
}

/// The converted document: canvas size and the root group.
pub(crate) struct Converted {
    pub size: Size,
    pub root: Group,
}

pub(crate) fn convert_doc(doc: &Document, opt: &Options) -> Result<Converted, Error> {
    let svg = doc.root_element().ok_or(Error::NotAnSvg)?;
    let (size, restore_viewbox) = resolve_svg_size(svg, opt);
    let size = size?;
    let view_box = svg
        .parse_viewbox()
        .unwrap_or_else(|| size.to_non_zero_rect(0.0, 0.0));
    let aspect = svg.aspect();

    let mut root = Group::default();
    if !svg.is_visible_element(opt) {
        return Ok(Converted { size, root });
    }
    let state = State {
        in_clip_path: false,
        parent_markers: Vec::new(),
        context: None,
        view_box,
        use_size: (None, None),
        opt,
    };
    let mut cache = Cache {
        max_work: opt.max_nodes.saturating_mul(8),
        ..Cache::default()
    };
    let root_ts = view_box_to_transform(view_box, aspect, size);
    // usvg honours a non-standard `background-color` on the root element.
    let background = svg
        .attribute(AId::BackgroundColor)
        .and_then(values::paint)
        .and_then(|p| match p {
            values::Paint::Color(c) => Some(c),
            _ => None,
        });
    if root_ts.is_identity() && background.is_none() {
        convert_children(doc.root(), &state, &mut cache, &mut root);
    } else {
        let mut g = Group {
            transform: root_ts,
            ..Group::default()
        };
        if let Some(c) = background {
            let mut bg = clip_element(view_box, Transform::identity());
            if let Some(clip) = bg.clip_path.take() {
                if let Some(Node::Path(p)) = clip.root.children.first() {
                    let mut p = (**p).clone();
                    if let Some(f) = p.fill.as_mut() {
                        f.paint = Paint::Color(Color {
                            red: c.r,
                            green: c.g,
                            blue: c.b,
                        });
                        f.opacity = f32::from(c.a) / 255.0;
                    }
                    g.children.push(Node::Path(Box::new(p)));
                }
            }
        }
        convert_children(doc.root(), &state, &mut cache, &mut g);
        g.calculate_bounding_boxes();
        root.children.push(Node::Group(Box::new(g)));
    }
    root.calculate_bounding_boxes();
    let mut size = size;
    if restore_viewbox {
        if let Some(bbox) = root.bounding_box {
            if let Some(s) = Size::from_wh(bbox.right(), bbox.bottom()) {
                size = s;
            }
        }
    }
    Ok(Converted { size, root })
}

fn resolve_svg_size(svg: SvgNode, opt: &Options) -> (Result<Size, Error>, bool) {
    let mut state = State {
        in_clip_path: false,
        parent_markers: Vec::new(),
        context: None,
        view_box: NonZeroRect::from_xywh(0.0, 0.0, 100.0, 100.0).unwrap_or_else(|| unreachable!()),
        use_size: (None, None),
        opt,
    };
    let def = Length::new(100.0, Unit::Percent);
    let mut width = svg.length_attr(AId::Width).unwrap_or(def);
    let mut height = svg.length_attr(AId::Height).unwrap_or(def);
    let view_box = svg.parse_viewbox();
    let restore_viewbox =
        if (width.unit == Unit::Percent || height.unit == Unit::Percent) && view_box.is_none() {
            if width.unit == Unit::Percent {
                width = Length::new(
                    (width.number / 100.0) * f64::from(opt.default_size.width()),
                    Unit::None,
                );
            }
            if height.unit == Unit::Percent {
                height = Length::new(
                    (height.number / 100.0) * f64::from(opt.default_size.height()),
                    Unit::None,
                );
            }
            true
        } else {
            false
        };
    let size = if let Some(vbox) = view_box {
        state.view_box = vbox;
        let w = if width.unit == Unit::Percent {
            vbox.width() * (width.number as f32 / 100.0)
        } else {
            convert_length(width, svg, AId::Width, Units::UserSpaceOnUse, &state)
        };
        let h = if height.unit == Unit::Percent {
            vbox.height() * (height.number as f32 / 100.0)
        } else {
            convert_length(height, svg, AId::Height, Units::UserSpaceOnUse, &state)
        };
        match (svg.length_attr(AId::Width), svg.length_attr(AId::Height)) {
            (Some(_), None) => Size::from_wh(w, vbox.height() * w / vbox.width()),
            (None, Some(_)) => Size::from_wh(vbox.width() * h / vbox.height(), h),
            _ => Size::from_wh(w, h),
        }
    } else {
        Size::from_wh(
            convert_length(width, svg, AId::Width, Units::UserSpaceOnUse, &state),
            convert_length(height, svg, AId::Height, Units::UserSpaceOnUse, &state),
        )
    };
    let size = size.filter(|s| s.width().is_finite() && s.height().is_finite());
    (size.ok_or(Error::InvalidSize), restore_viewbox)
}

fn convert_children(parent_node: SvgNode, state: &State, cache: &mut Cache, parent: &mut Group) {
    for node in parent_node.children() {
        convert_element(node, state, cache, parent);
    }
}

fn convert_element(node: SvgNode, state: &State, cache: &mut Cache, parent: &mut Group) {
    let Some(tag) = node.tag_name() else {
        return;
    };
    if !cache.spend() {
        return;
    }
    if !tag.is_graphic() && !matches!(tag, EId::G | EId::Switch | EId::Svg) {
        return;
    }
    if !node.is_visible_element(state.opt) {
        return;
    }
    if tag == EId::Use {
        convert_use(node, state, cache, parent);
        return;
    }
    if tag == EId::Switch {
        let Some(child) = node
            .children()
            .find(|c| c.tag_name().is_some() && switch_condition_passed(*c, state.opt))
        else {
            return;
        };
        if let Some(g) = convert_group_into(node, state, false, cache, parent, &mut |cache, g| {
            convert_element(child, state, cache, g);
        }) {
            parent.children.push(Node::Group(Box::new(g)));
        }
        return;
    }
    if tag == EId::Svg && node.parent_element().is_some() {
        convert_nested_svg(node, state, cache, parent);
        return;
    }
    if let Some(g) = convert_group_into(node, state, false, cache, parent, &mut |cache, g| {
        convert_element_impl(tag, node, state, cache, g);
    }) {
        parent.children.push(Node::Group(Box::new(g)));
    }
}

fn convert_element_impl(
    tag: EId,
    node: SvgNode,
    state: &State,
    cache: &mut Cache,
    parent: &mut Group,
) {
    match tag {
        EId::Rect
        | EId::Circle
        | EId::Ellipse
        | EId::Line
        | EId::Polyline
        | EId::Polygon
        | EId::Path => {
            if let Some(path) = convert_shape(node, state, cache) {
                convert_path(node, path, state, cache, parent);
            }
        }
        EId::Image => convert_image(node, state, parent),
        EId::Svg | EId::G => convert_children(node, state, cache, parent),
        // Text is deferred (see README); filters are not graphics elements.
        _ => {}
    }
}

fn convert_clip_path_elements(clip: SvgNode, state: &State, cache: &mut Cache, parent: &mut Group) {
    for node in clip.children() {
        let Some(tag) = node.tag_name() else {
            continue;
        };
        if !cache.spend() {
            return;
        }
        if !tag.is_graphic() || !node.is_visible_element(state.opt) {
            continue;
        }
        if tag == EId::Use {
            convert_use(node, state, cache, parent);
            continue;
        }
        if let Some(g) = convert_group_into(node, state, false, cache, parent, &mut |cache, g| {
            if matches!(
                tag,
                EId::Rect | EId::Circle | EId::Ellipse | EId::Polyline | EId::Polygon | EId::Path
            ) {
                if let Some(path) = convert_shape(node, state, cache) {
                    convert_path(node, path, state, cache, g);
                }
            }
        }) {
            parent.children.push(Node::Group(Box::new(g)));
        }
    }
}

/// Converts `node` as a group whose content `collect` adds. A group that is
/// not required (no opacity, transform, blend mode, isolation, clip path or
/// mask of its own, and not a `g` or `use`) is never built: its content goes
/// straight into `parent`. A required group is returned for the caller to
/// place; `None` also means the element was dropped.
fn convert_group_into(
    node: SvgNode,
    state: &State,
    force: bool,
    cache: &mut Cache,
    parent: &mut Group,
    collect: &mut dyn FnMut(&mut Cache, &mut Group),
) -> Option<Group> {
    let opacity = if state.in_clip_path {
        1.0
    } else {
        node.opacity_attr(AId::Opacity).unwrap_or(1.0)
    };
    let transform = node.resolve_transform(AId::Transform, state);
    let blend_mode = node
        .attribute(AId::MixBlendMode)
        .and_then(parse_blend_mode)
        .unwrap_or(BlendMode::SourceOver);
    let isolate = node.attribute(AId::Isolation).map(values::trim) == Some("isolate");
    let is_g_or_use = matches!(node.tag_name(), Some(EId::G | EId::Use));
    let required = (opacity - 1.0).abs() > f32::EPSILON * 4.0
        || !transform.is_identity()
        || blend_mode != BlendMode::SourceOver
        || isolate
        || is_g_or_use
        || force;

    let clip_link = node.func_iri_attribute(AId::ClipPath);
    let mask_link = if state.in_clip_path {
        None
    } else {
        node.func_iri_attribute(AId::Mask)
    };
    // Filters are not implemented: a reference to a missing element
    // disables rendering (as the spec and usvg require); a valid filter
    // is ignored and the content drawn unfiltered (documented).
    let filter_missing = !state.in_clip_path
        && node.attribute(AId::Filter).is_some_and(|f| {
            f.trim() != "none"
                && values::func_iri(f).is_some()
                && node.func_iri_attribute(AId::Filter).is_none()
        });

    if !required && clip_link.is_none() && mask_link.is_none() && !filter_missing {
        // Nothing of its own to hold: the content is the parent's as it
        // stands, in the same order.
        collect(cache, parent);
        return None;
    }

    let mut g = Group {
        transform,
        opacity,
        blend_mode,
        isolate,
        ..Group::default()
    };
    collect(cache, &mut g);

    let mut clip_path = None;
    let mut mask = None;
    if clip_link.is_some() || mask_link.is_some() {
        let object_bbox = g.calculate_object_bbox();
        if let Some(link) = clip_link {
            clip_path = convert_clip_path(link, state, object_bbox, cache);
            clip_path.as_ref()?;
        }
        if let Some(link) = mask_link {
            mask = convert_mask(link, state, object_bbox, cache);
            mask.as_ref()?;
        }
    }
    if filter_missing {
        return None;
    }
    g.clip_path = clip_path;
    g.mask = mask;
    g.calculate_bounding_boxes();
    Some(g)
}

fn parse_blend_mode(v: &str) -> Option<BlendMode> {
    Some(match v.trim() {
        "normal" => BlendMode::SourceOver,
        "multiply" => BlendMode::Multiply,
        "screen" => BlendMode::Screen,
        "overlay" => BlendMode::Overlay,
        "darken" => BlendMode::Darken,
        "lighten" => BlendMode::Lighten,
        "color-dodge" => BlendMode::ColorDodge,
        "color-burn" => BlendMode::ColorBurn,
        "hard-light" => BlendMode::HardLight,
        "soft-light" => BlendMode::SoftLight,
        "difference" => BlendMode::Difference,
        "exclusion" => BlendMode::Exclusion,
        "hue" => BlendMode::Hue,
        "saturation" => BlendMode::Saturation,
        "color" => BlendMode::Color,
        "luminosity" => BlendMode::Luminosity,
        _ => return None,
    })
}

// ---------------------------------------------------------------- shapes --

fn convert_shape(
    node: SvgNode,
    state: &State,
    cache: &mut Cache,
) -> Option<Arc<rusty_raster::Path>> {
    match node.tag_name()? {
        EId::Rect => convert_rect(node, state, cache),
        EId::Circle => {
            let cx = node.convert_user_length(AId::Cx, state, Length::zero());
            let cy = node.convert_user_length(AId::Cy, state, Length::zero());
            let r = node.convert_user_length(AId::R, state, Length::zero());
            if !(r.is_finite() && r > 0.0) {
                return None;
            }
            ellipse_to_path(cx, cy, r, r, cache)
        }
        EId::Ellipse => {
            let cx = node.convert_user_length(AId::Cx, state, Length::zero());
            let cy = node.convert_user_length(AId::Cy, state, Length::zero());
            let (rx, ry) = resolve_rx_ry(node, state);
            if !(rx.is_finite() && rx > 0.0 && ry.is_finite() && ry > 0.0) {
                return None;
            }
            ellipse_to_path(cx, cy, rx, ry, cache)
        }
        EId::Line => {
            let x1 = node.convert_user_length(AId::X1, state, Length::zero());
            let y1 = node.convert_user_length(AId::Y1, state, Length::zero());
            let x2 = node.convert_user_length(AId::X2, state, Length::zero());
            let y2 = node.convert_user_length(AId::Y2, state, Length::zero());
            let mut b = cache.path_builder();
            b.move_to(x1, y1);
            b.line_to(x2, y2);
            cache.finish_path(b)
        }
        EId::Polyline | EId::Polygon => {
            let pts = values::points(node.attribute(AId::Points)?);
            if pts.len() < 2 {
                return None;
            }
            let mut b = cache.path_builder();
            for (i, (x, y)) in pts.into_iter().enumerate() {
                if i == 0 {
                    b.move_to(x as f32, y as f32);
                } else {
                    b.line_to(x as f32, y as f32);
                }
            }
            if node.tag_name() == Some(EId::Polygon) {
                b.close();
            }
            cache.finish_path(b)
        }
        EId::Path => {
            // Path data goes straight from the text into the builder.
            let d = node.attribute(AId::D)?;
            let mut b = cache.path_data_builder(d);
            values::path_data_with(d, &mut |s| match s {
                Seg::MoveTo(x, y) => b.move_to(x as f32, y as f32),
                Seg::LineTo(x, y) => b.line_to(x as f32, y as f32),
                Seg::Quad(x1, y1, x, y) => b.quad_to(x1 as f32, y1 as f32, x as f32, y as f32),
                Seg::Cubic(x1, y1, x2, y2, x, y) => b.cubic_to(
                    x1 as f32, y1 as f32, x2 as f32, y2 as f32, x as f32, y as f32,
                ),
                Seg::Close => b.close(),
            });
            cache.finish_path(b)
        }
        _ => None,
    }
}

/// usvg's `arc_to` on a builder: f32 endpoints widened to f64 for kurbo.
/// The cubics go straight into the builder.
fn builder_arc_to(b: &mut PathBuilder, rx: f32, ry: f32, x: f32, y: f32) {
    let Some(prev) = b.last_point() else {
        return;
    };
    values::arc_to_cubics(
        f64::from(prev.x),
        f64::from(prev.y),
        f64::from(rx),
        f64::from(ry),
        0.0,
        false,
        true,
        f64::from(x),
        f64::from(y),
        &mut |s| match s {
            Seg::Cubic(x1, y1, x2, y2, x, y) => b.cubic_to(
                x1 as f32, y1 as f32, x2 as f32, y2 as f32, x as f32, y as f32,
            ),
            Seg::LineTo(x, y) => b.line_to(x as f32, y as f32),
            _ => {}
        },
    );
}

fn convert_rect(
    node: SvgNode,
    state: &State,
    cache: &mut Cache,
) -> Option<Arc<rusty_raster::Path>> {
    let width = node.convert_user_length(AId::Width, state, Length::zero());
    let height = node.convert_user_length(AId::Height, state, Length::zero());
    if !(width.is_finite() && width > 0.0 && height.is_finite() && height > 0.0) {
        return None;
    }
    let x = node.convert_user_length(AId::X, state, Length::zero());
    let y = node.convert_user_length(AId::Y, state, Length::zero());
    let (mut rx, mut ry) = resolve_rx_ry(node, state);
    if rx > width / 2.0 {
        rx = width / 2.0;
    }
    if ry > height / 2.0 {
        ry = height / 2.0;
    }
    if rx.abs() <= f32::EPSILON * 4.0 {
        return Some(Arc::new(PathBuilder::from_rect(Rect::from_xywh(
            x, y, width, height,
        )?)));
    }
    let mut b = cache.path_builder();
    b.move_to(x + rx, y);
    b.line_to(x + width - rx, y);
    builder_arc_to(&mut b, rx, ry, x + width, y + ry);
    b.line_to(x + width, y + height - ry);
    builder_arc_to(&mut b, rx, ry, x + width - rx, y + height);
    b.line_to(x + rx, y + height);
    builder_arc_to(&mut b, rx, ry, x, y + height - ry);
    b.line_to(x, y + ry);
    builder_arc_to(&mut b, rx, ry, x + rx, y);
    b.close();
    cache.finish_path(b)
}

fn resolve_rx_ry(node: SvgNode, state: &State) -> (f32, f32) {
    let mut rx = node.length_attr(AId::Rx);
    let mut ry = node.length_attr(AId::Ry);
    if rx.is_some_and(|v| v.number.is_sign_negative()) {
        rx = None;
    }
    if ry.is_some_and(|v| v.number.is_sign_negative()) {
        ry = None;
    }
    let conv = |l: Length, name| convert_length(l, node, name, Units::UserSpaceOnUse, state);
    match (rx, ry) {
        (None, None) => (0.0, 0.0),
        (Some(rx), None) => {
            let v = conv(rx, AId::Rx);
            (v, v)
        }
        (None, Some(ry)) => {
            let v = conv(ry, AId::Ry);
            (v, v)
        }
        (Some(rx), Some(ry)) => (conv(rx, AId::Rx), conv(ry, AId::Ry)),
    }
}

fn ellipse_to_path(
    cx: f32,
    cy: f32,
    rx: f32,
    ry: f32,
    cache: &mut Cache,
) -> Option<Arc<rusty_raster::Path>> {
    let mut b = cache.path_builder();
    b.move_to(cx + rx, cy);
    builder_arc_to(&mut b, rx, ry, cx, cy + ry);
    builder_arc_to(&mut b, rx, ry, cx - rx, cy);
    builder_arc_to(&mut b, rx, ry, cx, cy - ry);
    builder_arc_to(&mut b, rx, ry, cx + rx, cy);
    b.close();
    cache.finish_path(b)
}

// ----------------------------------------------------------------- paths --

fn convert_path(
    node: SvgNode,
    data: Arc<rusty_raster::Path>,
    state: &State,
    cache: &mut Cache,
    parent: &mut Group,
) {
    if data.len() < 2 {
        return;
    }
    let bounds = data.bounds();
    let has_bbox = bounds.width() > 0.0 && bounds.height() > 0.0;
    let Some(bounding_box) = data.compute_tight_bounds() else {
        return;
    };
    let mut fill = resolve_fill(node, has_bbox, state, cache);
    let mut stroke = resolve_stroke(node, has_bbox, state, cache);
    let visible_attr = node
        .find_attribute(AId::Visibility)
        .map(values::trim)
        .unwrap_or("visible")
        == "visible";
    let mut visible = visible_attr;
    let anti_alias = !matches!(
        node.find_attribute(AId::ShapeRendering).map(values::trim),
        Some("optimizeSpeed" | "crispEdges")
    );
    let (paint_order, markers_pos) = node
        .find_attribute(AId::PaintOrder)
        .map(parse_paint_order)
        .unwrap_or((PaintOrder::FillAndStroke, 2));
    if fill.is_none() && stroke.is_none() {
        visible = false;
    }
    // objectBoundingBox paint servers to user space (usvg does this in a
    // second pass; the result is the same for paths).
    if let Some(f) = fill.as_mut() {
        if !paint_to_user_space(&mut f.paint, bounding_box) {
            fill = None;
        }
    }
    if let Some(s) = stroke.as_mut() {
        if !paint_to_user_space(&mut s.paint, bounding_box) {
            stroke = None;
        }
    }
    let stroke_bounding_box = stroke
        .as_ref()
        .and_then(|s| {
            let mut rs = s.to_raster();
            rs.dash = None;
            cache
                .stroker
                .stroke(&data, &rs, 1.0)?
                .compute_tight_bounds()
        })
        .unwrap_or(bounding_box);
    let markers = if visible_attr && !state.in_clip_path && has_markers(node) {
        let mut g = Group::default();
        let mut ms = state.clone();
        ms.context = Some((fill.clone(), stroke.clone()));
        convert_markers(node, &data, &ms, cache, &mut g);
        g.calculate_bounding_boxes();
        Some(g)
    } else {
        None
    };
    let path = Path {
        visible,
        fill,
        stroke,
        paint_order,
        anti_alias,
        data,
        bounding_box,
        stroke_bounding_box,
    };
    match (markers, markers_pos) {
        (Some(m), 0) => {
            parent.children.push(Node::Group(Box::new(m)));
            parent.children.push(Node::Path(Box::new(path)));
        }
        (Some(m), 1) => {
            let first_is_fill = paint_order == PaintOrder::FillAndStroke;
            let mut a = path.clone();
            let mut b = path;
            if first_is_fill {
                a.stroke = None;
                b.fill = None;
            } else {
                a.fill = None;
                b.stroke = None;
            }
            a.visible = a.visible && (a.fill.is_some() || a.stroke.is_some());
            b.visible = b.visible && (b.fill.is_some() || b.stroke.is_some());
            parent.children.push(Node::Path(Box::new(a)));
            parent.children.push(Node::Group(Box::new(m)));
            parent.children.push(Node::Path(Box::new(b)));
        }
        (Some(m), _) => {
            parent.children.push(Node::Path(Box::new(path)));
            parent.children.push(Node::Group(Box::new(m)));
        }
        (None, _) => parent.children.push(Node::Path(Box::new(path))),
    }
}

fn marker_link<'a>(node: SvgNode<'a>, name: AId) -> Option<SvgNode<'a>> {
    let n = node.find_attribute_node(name)?;
    n.func_iri_attribute(name)
}

fn has_markers(node: SvgNode) -> bool {
    if node
        .ancestors()
        .any(|n| n.tag_name() == Some(EId::ClipPath))
    {
        return false;
    }
    [AId::MarkerStart, AId::MarkerMid, AId::MarkerEnd]
        .iter()
        .any(|n| marker_link(node, *n).is_some())
}

fn convert_markers(
    node: SvgNode,
    data: &rusty_raster::Path,
    state: &State,
    cache: &mut Cache,
    parent: &mut Group,
) {
    use crate::marker::{self, MarkerKind, Orientation};
    let segs = marker::segments(data);
    for (attr, kind) in [
        (AId::MarkerStart, MarkerKind::Start),
        (AId::MarkerMid, MarkerKind::Middle),
        (AId::MarkerEnd, MarkerKind::End),
    ] {
        let Some(mnode) = marker_link(node, attr) else {
            continue;
        };
        if mnode.tag_name() != Some(EId::Marker) || state.parent_markers.contains(&mnode.id()) {
            continue;
        }
        if state.parent_markers.len() >= MAX_REF_DEPTH {
            continue;
        }
        let stroke_scale = match mnode.attribute(AId::MarkerUnits).map(values::trim) {
            Some("userSpaceOnUse") => 1.0,
            _ => resolve_length(node, AId::StrokeWidth, state, 1.0),
        };
        if !(stroke_scale.is_finite() && stroke_scale > 0.0) {
            continue;
        }
        let Some(rect) = NonZeroRect::from_xywh(
            mnode.convert_user_length(AId::RefX, state, Length::zero()),
            mnode.convert_user_length(AId::RefY, state, Length::zero()),
            mnode.convert_user_length(AId::MarkerWidth, state, Length::new(3.0, Unit::None)),
            mnode.convert_user_length(AId::MarkerHeight, state, Length::new(3.0, Unit::None)),
        ) else {
            continue;
        };
        let view_box = mnode.parse_viewbox().map(|vb| (vb, mnode.aspect()));
        let overflow = mnode.attribute(AId::Overflow).map(values::trim);
        let clip = if matches!(overflow, None | Some("hidden" | "scroll")) {
            let r = view_box.map_or_else(|| rect.size().to_non_zero_rect(0.0, 0.0), |v| v.0);
            clip_element(r, Transform::identity()).clip_path
        } else {
            None
        };
        let orient = match mnode.attribute(AId::Orient).map(values::trim) {
            Some("auto") => Orientation::Auto,
            Some("auto-start-reverse") => Orientation::AutoStartReverse,
            Some(v) => Orientation::Angle(parse_angle(v).unwrap_or(0.0)),
            None => Orientation::Angle(0.0),
        };
        let mut ms = state.clone();
        ms.parent_markers.push(mnode.id());
        marker::draw_markers(&segs, kind, |p, idx| {
            let Some(ts) =
                marker::marker_transform(p, idx, &segs, &orient, rect, view_box, stroke_scale)
            else {
                return;
            };
            let mut g = Group {
                transform: ts,
                clip_path: clip.clone(),
                ..Group::default()
            };
            convert_children(mnode, &ms, cache, &mut g);
            g.calculate_bounding_boxes();
            if !g.children.is_empty() {
                parent.children.push(Node::Group(Box::new(g)));
            }
        });
    }
}

fn parse_angle(v: &str) -> Option<f32> {
    let mut s = values::Stream::new(v);
    let n = s.parse_number()?;
    let deg = match s.rest().trim() {
        "" | "deg" => n,
        "grad" => n * 180.0 / 200.0,
        "rad" => n.to_degrees(),
        "turn" => n * 360.0,
        _ => return None,
    };
    Some(deg as f32)
}

fn parse_paint_order(v: &str) -> (PaintOrder, usize) {
    let v = v.trim();
    let default = (PaintOrder::FillAndStroke, 2);
    if v == "normal" {
        return default;
    }
    let mut order: Vec<&str> = Vec::new();
    for w in v.split_ascii_whitespace() {
        if !matches!(w, "fill" | "stroke" | "markers") || order.contains(&w) {
            return default;
        }
        order.push(w);
    }
    for w in ["fill", "stroke", "markers"] {
        if !order.contains(&w) {
            order.push(w);
        }
    }
    let pos = |name: &str| order.iter().position(|w| *w == name).unwrap_or(0);
    let po = if pos("stroke") < pos("fill") {
        PaintOrder::StrokeAndFill
    } else {
        PaintOrder::FillAndStroke
    };
    (po, pos("markers"))
}

/// Folds an `objectBoundingBox` gradient/pattern into user space. Returns
/// `false` when the paint cannot apply (zero-size box).
fn paint_to_user_space(paint: &mut Paint, bbox: Rect) -> bool {
    let obb = match paint {
        Paint::Color(_) => return true,
        Paint::LinearGradient(g) => g.base.transform_is_obb(),
        Paint::RadialGradient(g) => g.base.transform_is_obb(),
        Paint::Pattern(p) => p.pending.is_some(),
    };
    if !obb {
        return true;
    }
    let Some(bbox) = bbox.to_non_zero_rect() else {
        return false;
    };
    let bts = Transform::from_bbox(bbox);
    match paint {
        Paint::LinearGradient(g) => {
            let mut ng = (**g).clone();
            ng.base.transform = ng.base.transform.post_concat(bts);
            ng.base.obb = false;
            *g = Arc::new(ng);
        }
        Paint::RadialGradient(g) => {
            let mut ng = (**g).clone();
            ng.base.transform = ng.base.transform.post_concat(bts);
            ng.base.obb = false;
            *g = Arc::new(ng);
        }
        Paint::Pattern(p) => {
            let Some(pend) = p.pending else {
                return true;
            };
            let mut np = (**p).clone();
            np.pending = None;
            if pend.obb_units {
                let Some(r) = bbox_transform(np.rect, bbox) else {
                    return false;
                };
                np.rect = r;
            }
            if pend.obb_content && pend.view_box.is_none() {
                push_pattern_transform(
                    &mut np.root,
                    Transform::from_scale(bbox.width(), bbox.height()),
                );
            }
            if let Some((vb, aspect)) = pend.view_box {
                push_pattern_transform(
                    &mut np.root,
                    view_box_to_transform(vb, aspect, np.rect.size()),
                );
            }
            *p = Arc::new(np);
        }
        Paint::Color(_) => {}
    }
    true
}

/// `NonZeroRect::bbox_transform` without its internal `unwrap` (it panics
/// when the result overflows, which hostile units can arrange).
fn bbox_transform(r: NonZeroRect, bbox: NonZeroRect) -> Option<NonZeroRect> {
    NonZeroRect::from_xywh(
        r.x() * bbox.width() + bbox.x(),
        r.y() * bbox.height() + bbox.y(),
        r.width() * bbox.width(),
        r.height() * bbox.height(),
    )
}

/// `Size::scale_to` (`expand == false`) / `expand_to` without the internal
/// `unwrap`.
fn fit_size(s1: Size, s2: Size, expand: bool) -> Option<Size> {
    let rw = s2.height() * s1.width() / s1.height();
    let with_h = if expand {
        rw <= s2.width()
    } else {
        rw >= s2.width()
    };
    if with_h {
        Size::from_wh(s2.width(), s2.width() * s1.height() / s1.width())
    } else {
        Size::from_wh(rw, s2.height())
    }
}

fn push_pattern_transform(root: &mut Group, transform: Transform) {
    let mut g = std::mem::take(root);
    g.transform = transform;
    root.children.push(Node::Group(Box::new(g)));
    root.calculate_bounding_boxes();
}

// ----------------------------------------------------------------- style --

fn resolve_fill(node: SvgNode, has_bbox: bool, state: &State, cache: &mut Cache) -> Option<Fill> {
    let rule = |n: SvgNode, attr: AId| match n.find_attribute(attr).map(values::trim) {
        Some("evenodd") => FillRule::EvenOdd,
        _ => FillRule::Winding,
    };
    if state.in_clip_path {
        return Some(Fill {
            paint: Paint::Color(Color {
                red: 0,
                green: 0,
                blue: 0,
            }),
            opacity: 1.0,
            rule: rule(node, AId::ClipRule),
        });
    }
    let mut sub_opacity = 1.0;
    let paint = if let Some(n) = node.ancestor_with_attribute(AId::Fill) {
        let value = n.attribute(AId::Fill)?;
        convert_paint(node, value, true, has_bbox, state, &mut sub_opacity, cache)?
    } else {
        Paint::Color(Color {
            red: 0,
            green: 0,
            blue: 0,
        })
    };
    let fill_opacity = node
        .find_attribute(AId::FillOpacity)
        .and_then(values::opacity)
        .unwrap_or(1.0);
    Some(Fill {
        paint,
        opacity: sub_opacity * fill_opacity,
        rule: rule(node, AId::FillRule),
    })
}

fn resolve_stroke(
    node: SvgNode,
    has_bbox: bool,
    state: &State,
    cache: &mut Cache,
) -> Option<Stroke> {
    if state.in_clip_path {
        return None;
    }
    let mut sub_opacity = 1.0;
    let n = node.ancestor_with_attribute(AId::Stroke)?;
    let value = n.attribute(AId::Stroke)?;
    let paint = convert_paint(node, value, false, has_bbox, state, &mut sub_opacity, cache)?;
    let width = resolve_length(node, AId::StrokeWidth, state, 1.0);
    if !(width.is_finite() && width > 0.0) {
        return None;
    }
    let miterlimit = node
        .find_attribute(AId::StrokeMiterlimit)
        .and_then(values::number)
        .map(|v| v as f32)
        .unwrap_or(4.0);
    let miterlimit = if miterlimit < 1.0 { 1.0 } else { miterlimit };
    let stroke_opacity = node
        .find_attribute(AId::StrokeOpacity)
        .and_then(values::opacity)
        .unwrap_or(1.0);
    let linecap = match node.find_attribute(AId::StrokeLinecap).map(values::trim) {
        Some("round") => LineCap::Round,
        Some("square") => LineCap::Square,
        _ => LineCap::Butt,
    };
    let linejoin = match node.find_attribute(AId::StrokeLinejoin).map(values::trim) {
        Some("round") => LineJoin::Round,
        Some("bevel") => LineJoin::Bevel,
        Some("miter-clip") => LineJoin::MiterClip,
        _ => LineJoin::Miter,
    };
    Some(Stroke {
        paint,
        opacity: sub_opacity * stroke_opacity,
        width,
        miterlimit,
        linecap,
        linejoin,
        dasharray: conv_dasharray(node, state),
        dashoffset: resolve_length(node, AId::StrokeDashoffset, state, 0.0),
    })
}

/// An inherited length (`stroke-width`, `stroke-dashoffset`).
fn resolve_length(node: SvgNode, name: AId, state: &State, def: f32) -> f32 {
    if let Some(n) = node.ancestor_with_attribute(name) {
        if let Some(l) = n.length_attr(name) {
            return convert_length(l, n, name, Units::UserSpaceOnUse, state);
        }
    }
    def
}

fn conv_dasharray(node: SvgNode, state: &State) -> Option<Vec<f32>> {
    let n = node.ancestor_with_attribute(AId::StrokeDasharray)?;
    let list = values::length_list(n.attribute(AId::StrokeDasharray)?)?;
    let list: Vec<f32> = list
        .into_iter()
        .map(|l| convert_length(l, n, AId::StrokeDasharray, Units::UserSpaceOnUse, state))
        .collect();
    if list.iter().any(|v| v.is_sign_negative() || !v.is_finite()) {
        return None;
    }
    let sum: f32 = list.iter().sum();
    if sum.abs() <= f32::EPSILON * 4.0 {
        return None;
    }
    if list.len() & 1 == 1 {
        let mut t = list.clone();
        t.extend_from_slice(&list);
        return Some(t);
    }
    Some(list)
}

fn current_color(node: SvgNode) -> values::Color {
    node.find_attribute(AId::Color)
        .and_then(values::color)
        .unwrap_or(values::Color::BLACK)
}

fn split(c: values::Color, opacity: &mut f32) -> Color {
    *opacity = f32::from(c.a) / 255.0;
    Color {
        red: c.r,
        green: c.g,
        blue: c.b,
    }
}

fn convert_paint(
    node: SvgNode,
    value: &str,
    is_fill: bool,
    has_bbox: bool,
    state: &State,
    opacity: &mut f32,
    cache: &mut Cache,
) -> Option<Paint> {
    let paint = match values::paint(value) {
        Some(p) => p,
        None => {
            if is_fill {
                values::Paint::Color(values::Color::BLACK)
            } else {
                return None;
            }
        }
    };
    match paint {
        values::Paint::None => None,
        // Context paints: solid colours are carried over; context gradients
        // and patterns are not (documented).
        values::Paint::ContextFill => match state.context.as_ref()?.0.as_ref()?.paint {
            Paint::Color(c) => Some(Paint::Color(c)),
            _ => None,
        },
        values::Paint::ContextStroke => match state.context.as_ref()?.1.as_ref()?.paint {
            Paint::Color(c) => Some(Paint::Color(c)),
            _ => None,
        },
        values::Paint::CurrentColor => Some(Paint::Color(split(current_color(node), opacity))),
        values::Paint::Color(c) => Some(Paint::Color(split(c, opacity))),
        values::Paint::Url(id, fallback) => {
            let Some(link) = node.document().element_by_id(id) else {
                return from_fallback(node, fallback, opacity);
            };
            let tag = link.tag_name()?;
            if !(tag.is_gradient() || tag == EId::Pattern) {
                return None;
            }
            match convert_paint_server(link, state, cache) {
                Some(ServerOrColor::Server(p, obb)) => {
                    if !has_bbox && obb {
                        from_fallback(node, fallback, opacity)
                    } else {
                        Some(p)
                    }
                }
                Some(ServerOrColor::Color(c, o)) => {
                    *opacity = o;
                    Some(Paint::Color(c))
                }
                None => from_fallback(node, fallback, opacity),
            }
        }
    }
}

fn from_fallback(
    node: SvgNode,
    fallback: Option<values::PaintFallback>,
    opacity: &mut f32,
) -> Option<Paint> {
    match fallback? {
        values::PaintFallback::None => None,
        values::PaintFallback::CurrentColor => {
            Some(Paint::Color(split(current_color(node), opacity)))
        }
        values::PaintFallback::Color(c) => Some(Paint::Color(split(c, opacity))),
    }
}

// ---------------------------------------------------------- paint servers --

enum ServerOrColor {
    /// The paint and whether it still needs the object bounding box.
    Server(Paint, bool),
    Color(Color, f32),
}

impl BaseGradient {
    pub(crate) fn transform_is_obb(&self) -> bool {
        self.obb
    }
}

fn units_attr(node: SvgNode, name: AId, def: Units) -> Units {
    let n = resolve_attr(node, name);
    match n.attribute(name).map(values::trim) {
        Some("userSpaceOnUse") => Units::UserSpaceOnUse,
        Some("objectBoundingBox") => Units::ObjectBoundingBox,
        _ => def,
    }
}

/// The node in the `href` chain that defines `name` (gradient attribute
/// inheritance).
fn resolve_attr<'a>(node: SvgNode<'a>, name: AId) -> SvgNode<'a> {
    if node.has_attribute(name) {
        return node;
    }
    let Some(tag) = node.tag_name() else {
        return node;
    };
    let shared = matches!(
        name,
        AId::GradientUnits | AId::SpreadMethod | AId::GradientTransform
    );
    for link in node.href_iter() {
        let Some(lt) = link.tag_name() else {
            return node;
        };
        let ok = match tag {
            EId::LinearGradient => {
                (matches!(name, AId::X1 | AId::Y1 | AId::X2 | AId::Y2) && lt == EId::LinearGradient)
                    || (shared && lt.is_gradient())
            }
            EId::RadialGradient => {
                (matches!(
                    name,
                    AId::Cx | AId::Cy | AId::R | AId::Fx | AId::Fy | AId::Fr
                ) && lt == EId::RadialGradient)
                    || (shared && lt.is_gradient())
            }
            EId::Pattern => lt == EId::Pattern,
            _ => false,
        };
        if !ok {
            break;
        }
        if link.has_attribute(name) {
            return link;
        }
    }
    node
}

fn resolve_number(node: SvgNode, name: AId, units: Units, state: &State, def: Length) -> f32 {
    resolve_attr(node, name).convert_length(name, units, state, def)
}

fn find_gradient_with_stops(node: SvgNode) -> Option<SvgNode> {
    for link in node.href_iter() {
        if !link.tag_name()?.is_gradient() {
            return None;
        }
        if link.children().any(|n| n.tag_name() == Some(EId::Stop)) {
            return Some(link);
        }
    }
    None
}

fn approx_eq(a: f32, b: f32) -> bool {
    // usvg's `approx_eq_ulps(.., 4)`.
    if a == b {
        return true;
    }
    if a.is_sign_negative() != b.is_sign_negative() {
        return false;
    }
    let ia = a.to_bits() as i64;
    let ib = b.to_bits() as i64;
    (ia - ib).abs() <= 4
}

fn convert_stops(grad: SvgNode) -> Vec<Stop> {
    let mut stops: Vec<Stop> = Vec::new();
    let mut prev_offset = 0.0f64;
    for stop in grad.children() {
        if stop.tag_name() != Some(EId::Stop) {
            continue;
        }
        let offset = match stop.length_attr(AId::Offset) {
            Some(l) => match l.unit {
                Unit::None => l.number,
                Unit::Percent => l.number / 100.0,
                _ => prev_offset,
            },
            None => prev_offset,
        };
        prev_offset = offset;
        let offset = (offset as f32).clamp(0.0, 1.0);
        let c = match stop.attribute(AId::StopColor).map(values::trim) {
            Some("currentColor") => current_color(stop),
            Some(v) => values::color(v).unwrap_or(values::Color::BLACK),
            None => values::Color::BLACK,
        };
        let stop_opacity = stop.opacity_attr(AId::StopOpacity).unwrap_or(1.0);
        stops.push(Stop {
            offset,
            color: Color {
                red: c.r,
                green: c.g,
                blue: c.b,
            },
            opacity: (f32::from(c.a) / 255.0) * stop_opacity,
        });
    }
    if stops.len() >= 3 {
        let mut i = 0;
        while i < stops.len() - 2 {
            let (o1, o2, o3) = (stops[i].offset, stops[i + 1].offset, stops[i + 2].offset);
            if approx_eq(o1, o2) && approx_eq(o2, o3) {
                stops.remove(i + 1);
            } else {
                i += 1;
            }
        }
    }
    if stops.len() >= 2 {
        let mut i = 0;
        while i < stops.len() - 1 {
            let (o1, o2) = (stops[i].offset, stops[i + 1].offset);
            if approx_eq(o1, 0.0) && approx_eq(o2, 0.0) {
                stops[i + 1].offset = (o1 + f32::EPSILON).clamp(0.0, 1.0);
            }
            i += 1;
        }
    }
    let mut i = 1;
    while i < stops.len() {
        let (o1, o2) = (stops[i - 1].offset, stops[i].offset);
        if o1 > o2 || approx_eq(o1, o2) {
            stops[i - 1].offset = (o1 - f32::EPSILON).clamp(0.0, 1.0);
            stops[i].offset = o1.clamp(0.0, 1.0);
        }
        i += 1;
    }
    stops
}

fn spread_attr(node: SvgNode) -> SpreadMode {
    match resolve_attr(node, AId::SpreadMethod)
        .attribute(AId::SpreadMethod)
        .map(values::trim)
    {
        Some("reflect") => SpreadMode::Reflect,
        Some("repeat") => SpreadMode::Repeat,
        _ => SpreadMode::Pad,
    }
}

fn convert_paint_server(node: SvgNode, state: &State, cache: &mut Cache) -> Option<ServerOrColor> {
    match node.tag_name()? {
        EId::LinearGradient | EId::RadialGradient => convert_gradient(node, state),
        EId::Pattern => convert_pattern(node, state, cache),
        _ => None,
    }
}

fn convert_gradient(node: SvgNode, state: &State) -> Option<ServerOrColor> {
    let stops = convert_stops(find_gradient_with_stops(node)?);
    if stops.len() < 2 {
        let s = stops.first()?;
        return Some(ServerOrColor::Color(s.color, s.opacity));
    }
    let units = units_attr(node, AId::GradientUnits, Units::ObjectBoundingBox);
    let transform =
        resolve_attr(node, AId::GradientTransform).resolve_transform(AId::GradientTransform, state);
    let base = BaseGradient {
        transform,
        spread: spread_attr(node),
        stops,
        obb: units == Units::ObjectBoundingBox,
    };
    let obb = base.obb;
    if node.tag_name() == Some(EId::LinearGradient) {
        let g = LinearGradient {
            x1: resolve_number(node, AId::X1, units, state, Length::zero()),
            y1: resolve_number(node, AId::Y1, units, state, Length::zero()),
            x2: resolve_number(
                node,
                AId::X2,
                units,
                state,
                Length::new(100.0, Unit::Percent),
            ),
            y2: resolve_number(node, AId::Y2, units, state, Length::zero()),
            base,
        };
        return Some(ServerOrColor::Server(
            Paint::LinearGradient(Arc::new(g)),
            obb,
        ));
    }
    let r = resolve_number(node, AId::R, units, state, Length::new(50.0, Unit::Percent));
    if !(r.is_finite() && r > 0.0) {
        let s = base.stops.last()?;
        return Some(ServerOrColor::Color(s.color, s.opacity));
    }
    let fr = resolve_number(node, AId::Fr, units, state, Length::zero());
    let cx = resolve_number(
        node,
        AId::Cx,
        units,
        state,
        Length::new(50.0, Unit::Percent),
    );
    let cy = resolve_number(
        node,
        AId::Cy,
        units,
        state,
        Length::new(50.0, Unit::Percent),
    );
    let fx = resolve_number(
        node,
        AId::Fx,
        units,
        state,
        Length::new(f64::from(cx), Unit::None),
    );
    let fy = resolve_number(
        node,
        AId::Fy,
        units,
        state,
        Length::new(f64::from(cy), Unit::None),
    );
    let g = RadialGradient {
        cx,
        cy,
        r,
        fx,
        fy,
        fr: if fr.is_finite() && fr > 0.0 { fr } else { 0.0 },
        base,
    };
    Some(ServerOrColor::Server(
        Paint::RadialGradient(Arc::new(g)),
        obb,
    ))
}

fn convert_pattern(node: SvgNode, state: &State, cache: &mut Cache) -> Option<ServerOrColor> {
    if cache.active.contains(&node.id()) || cache.active.len() >= MAX_REF_DEPTH {
        return None;
    }
    let with_children = node
        .href_iter()
        .take_while(|l| l.tag_name() == Some(EId::Pattern))
        .find(|l| l.has_children())?;
    let units = units_attr(node, AId::PatternUnits, Units::ObjectBoundingBox);
    let content_units = units_attr(node, AId::PatternContentUnits, Units::UserSpaceOnUse);
    let transform =
        resolve_attr(node, AId::PatternTransform).resolve_transform(AId::PatternTransform, state);
    let rect = NonZeroRect::from_xywh(
        resolve_number(node, AId::X, units, state, Length::zero()),
        resolve_number(node, AId::Y, units, state, Length::zero()),
        resolve_number(node, AId::Width, units, state, Length::zero()),
        resolve_number(node, AId::Height, units, state, Length::zero()),
    )?;
    let vb_node = resolve_attr(node, AId::ViewBox);
    let view_box = vb_node.parse_viewbox();
    let aspect = resolve_attr(node, AId::PreserveAspectRatio).aspect();
    cache.active.push(node.id());
    let mut root = Group::default();
    let obb = units == Units::ObjectBoundingBox || content_units == Units::ObjectBoundingBox;
    if let (Some(vb), false) = (view_box, obb) {
        let mut g = Group {
            transform: view_box_to_transform(vb, aspect, rect.size()),
            ..Group::default()
        };
        convert_children(with_children, state, cache, &mut g);
        if !g.children.is_empty() {
            g.calculate_bounding_boxes();
            root.children.push(Node::Group(Box::new(g)));
        }
    } else {
        convert_children(with_children, state, cache, &mut root);
    }
    cache.active.pop();
    if root.children.is_empty() {
        return None;
    }
    root.calculate_bounding_boxes();
    let pending = obb.then_some(crate::tree::PatternPending {
        obb_units: units == Units::ObjectBoundingBox,
        obb_content: content_units == Units::ObjectBoundingBox,
        view_box: view_box.map(|vb| (vb, aspect)),
    });
    Some(ServerOrColor::Server(
        Paint::Pattern(Arc::new(Pattern {
            transform,
            rect,
            root,
            pending,
        })),
        obb,
    ))
}

// ------------------------------------------------------ clip paths, masks --

fn from_bbox(b: NonZeroRect) -> Transform {
    Transform::from_bbox(b)
}

fn convert_clip_path(
    node: SvgNode,
    state: &State,
    object_bbox: Option<NonZeroRect>,
    cache: &mut Cache,
) -> Option<Arc<ClipPath>> {
    if node.tag_name() != Some(EId::ClipPath) {
        return None;
    }
    // A cycle the svgtree pass did not break: treat the reference as absent.
    if cache.active.contains(&node.id()) || cache.active.len() >= MAX_REF_DEPTH {
        return None;
    }
    // Only a parseable-but-invalid transform disables the clip.
    let mut transform = match node.attribute(AId::Transform) {
        None => Transform::identity(),
        Some(v) => {
            let m = values::transform(v)?;
            if !ts_from_matrix(m).is_valid() {
                return None;
            }
            node.resolve_transform(AId::Transform, state)
        }
    };
    let units = match node.attribute(AId::ClipPathUnits).map(values::trim) {
        Some("objectBoundingBox") => Units::ObjectBoundingBox,
        _ => Units::UserSpaceOnUse,
    };
    let cacheable = units == Units::UserSpaceOnUse;
    if cacheable {
        if let Some(c) = cache.clip_paths.get(&node.id()) {
            return Some(c.clone());
        }
    }
    if units == Units::ObjectBoundingBox {
        transform = transform.pre_concat(from_bbox(object_bbox?));
    }
    cache.active.push(node.id());
    let mut clip_path = None;
    let mut ok = true;
    if let Some(link) = node.func_iri_attribute(AId::ClipPath) {
        clip_path = convert_clip_path(link, state, object_bbox, cache);
        ok = clip_path.is_some();
    }
    let mut root = Group::default();
    if ok {
        let mut cs = state.clone();
        cs.in_clip_path = true;
        convert_clip_path_elements(node, &cs, cache, &mut root);
    }
    cache.active.pop();
    if !ok || root.children.is_empty() {
        return None;
    }
    root.calculate_bounding_boxes();
    let clip = Arc::new(ClipPath {
        transform,
        clip_path,
        root,
    });
    if cacheable {
        cache.clip_paths.insert(node.id(), clip.clone());
    }
    Some(clip)
}

fn convert_mask(
    node: SvgNode,
    state: &State,
    object_bbox: Option<NonZeroRect>,
    cache: &mut Cache,
) -> Option<Arc<Mask>> {
    if node.tag_name() != Some(EId::Mask) {
        return None;
    }
    if cache.active.contains(&node.id()) || cache.active.len() >= MAX_REF_DEPTH {
        return None;
    }
    let units = match node.attribute(AId::MaskUnits).map(values::trim) {
        Some("userSpaceOnUse") => Units::UserSpaceOnUse,
        _ => Units::ObjectBoundingBox,
    };
    let content_units = match node.attribute(AId::MaskContentUnits).map(values::trim) {
        Some("objectBoundingBox") => Units::ObjectBoundingBox,
        _ => Units::UserSpaceOnUse,
    };
    let cacheable = units == Units::UserSpaceOnUse && content_units == Units::UserSpaceOnUse;
    if cacheable {
        if let Some(m) = cache.masks.get(&node.id()) {
            return Some(m.clone());
        }
    }
    let rect = NonZeroRect::from_xywh(
        node.convert_length(AId::X, units, state, Length::new(-10.0, Unit::Percent)),
        node.convert_length(AId::Y, units, state, Length::new(-10.0, Unit::Percent)),
        node.convert_length(AId::Width, units, state, Length::new(120.0, Unit::Percent)),
        node.convert_length(AId::Height, units, state, Length::new(120.0, Unit::Percent)),
    )?;
    let mut rect = rect;
    let mut mask_all = false;
    if units == Units::ObjectBoundingBox {
        match object_bbox {
            Some(b) => rect = bbox_transform(rect, b)?,
            None => mask_all = true,
        }
    }
    let kind = if node.attribute(AId::MaskType).map(values::trim) == Some("alpha") {
        MaskType::Alpha
    } else {
        MaskType::Luminance
    };
    if mask_all {
        let m = Arc::new(Mask {
            rect,
            kind: MaskType::Luminance,
            mask: None,
            root: Group::default(),
        });
        if cacheable {
            cache.masks.insert(node.id(), m.clone());
        }
        return Some(m);
    }
    cache.active.push(node.id());
    let mut sub_mask = None;
    let mut ok = true;
    if let Some(link) = node.func_iri_attribute(AId::Mask) {
        sub_mask = convert_mask(link, state, object_bbox, cache);
        ok = sub_mask.is_some();
    }
    let mut root = Group::default();
    if ok {
        if content_units == Units::ObjectBoundingBox {
            match object_bbox {
                Some(b) => {
                    let mut g = Group {
                        transform: from_bbox(b),
                        ..Group::default()
                    };
                    convert_children(node, state, cache, &mut g);
                    if g.children.is_empty() {
                        ok = false;
                    } else {
                        g.calculate_bounding_boxes();
                        root.children.push(Node::Group(Box::new(g)));
                    }
                }
                None => ok = false,
            }
        } else {
            convert_children(node, state, cache, &mut root);
            if root.children.is_empty() {
                ok = false;
            }
        }
    }
    cache.active.pop();
    if !ok {
        return None;
    }
    root.calculate_bounding_boxes();
    let m = Arc::new(Mask {
        rect,
        kind,
        mask: sub_mask,
        root,
    });
    if cacheable {
        cache.masks.insert(node.id(), m.clone());
    }
    Some(m)
}

// ------------------------------------------------------------ use / svg --

fn convert_use(node: SvgNode, state: &State, cache: &mut Cache, parent: &mut Group) {
    let Some(child) = node.first_child() else {
        return;
    };
    if state.in_clip_path && child.tag_name() == Some(EId::Symbol) {
        return;
    }
    let mut use_state = state.clone();
    use_state.context = Some((
        resolve_fill(node, true, state, cache),
        resolve_stroke(node, true, state, cache),
    ));
    let mut orig_ts = node.resolve_transform(AId::Transform, state);
    let mut new_ts = Transform::identity();
    {
        let x = node.convert_user_length(AId::X, &use_state, Length::zero());
        let y = node.convert_user_length(AId::Y, &use_state, Length::zero());
        new_ts = new_ts.pre_translate(x, y);
    }
    let linked_to_symbol = child.tag_name() == Some(EId::Symbol);
    if linked_to_symbol {
        use_state.view_box = {
            let def = Length::new(100.0, Unit::Percent);
            let x = use_state.view_box.x();
            let y = use_state.view_box.y();
            let w = if node.has_attribute(AId::Width) {
                node.convert_user_length(AId::Width, &use_state, def)
            } else {
                use_state.view_box.width()
            };
            let h = if node.has_attribute(AId::Height) {
                node.convert_user_length(AId::Height, &use_state, def)
            } else {
                use_state.view_box.height()
            };
            NonZeroRect::from_xywh(x, y, w, h).unwrap_or(use_state.view_box)
        };
        if let Some(ts) = viewbox_transform(node, child, &use_state) {
            new_ts = new_ts.pre_concat(ts);
        }
        if let Some(clip_rect) = get_clip_rect(node, child, &use_state) {
            let mut g = clip_element(clip_rect, orig_ts);
            if let Some(mut g2) =
                convert_group_into(node, &use_state, true, cache, &mut g, &mut |cache, g2| {
                    use_convert_children(child, new_ts, &use_state, cache, g2);
                })
            {
                g2.transform = Transform::identity();
                g.children.push(Node::Group(Box::new(g2)));
            }
            if g.children.is_empty() {
                return;
            }
            g.calculate_bounding_boxes();
            parent.children.push(Node::Group(Box::new(g)));
            return;
        }
    }
    orig_ts = orig_ts.pre_concat(new_ts);
    if linked_to_symbol {
        if let Some(mut g) =
            convert_group_into(node, &use_state, false, cache, parent, &mut |cache, g| {
                use_convert_children(child, orig_ts, &use_state, cache, g);
            })
        {
            g.transform = Transform::identity();
            parent.children.push(Node::Group(Box::new(g)));
        }
    } else {
        if child.tag_name() == Some(EId::Svg) {
            let def = Length::new(100.0, Unit::Percent);
            use_state.use_size = (None, None);
            if node.has_attribute(AId::Width) {
                use_state.use_size.0 = Some(node.convert_user_length(AId::Width, &use_state, def));
            }
            if node.has_attribute(AId::Height) {
                use_state.use_size.1 = Some(node.convert_user_length(AId::Height, &use_state, def));
            }
        }
        use_convert_children(node, orig_ts, &use_state, cache, parent);
    }
}

fn convert_nested_svg(node: SvgNode, state: &State, cache: &mut Cache, parent: &mut Group) {
    let mut orig_ts = node.resolve_transform(AId::Transform, state);
    let mut new_ts = Transform::identity();
    let x = node.convert_user_length(AId::X, state, Length::zero());
    let y = node.convert_user_length(AId::Y, state, Length::zero());
    new_ts = new_ts.pre_translate(x, y);
    if let Some(ts) = viewbox_transform(node, node, state) {
        new_ts = new_ts.pre_concat(ts);
    }
    let mut new_state = state.clone();
    new_state.view_box = if let Some(vb) = node.parse_viewbox() {
        vb
    } else {
        let (mut w, mut h) = use_node_size(node, state);
        w = state.use_size.0.unwrap_or(w);
        h = state.use_size.1.unwrap_or(h);
        NonZeroRect::from_xywh(x, y, w, h).unwrap_or(state.view_box)
    };
    if let Some(clip_rect) = get_clip_rect(node, node, state) {
        let mut g = clip_element(clip_rect, orig_ts);
        use_convert_children(node, new_ts, &new_state, cache, &mut g);
        g.calculate_bounding_boxes();
        parent.children.push(Node::Group(Box::new(g)));
    } else {
        orig_ts = orig_ts.pre_concat(new_ts);
        use_convert_children(node, orig_ts, &new_state, cache, parent);
    }
}

fn clip_element(clip_rect: NonZeroRect, transform: Transform) -> Group {
    let data = Arc::new(PathBuilder::from_rect(clip_rect.to_rect()));
    let bbox = clip_rect.to_rect();
    let path = Path {
        visible: true,
        fill: Some(Fill {
            paint: Paint::Color(Color {
                red: 0,
                green: 0,
                blue: 0,
            }),
            opacity: 1.0,
            rule: FillRule::Winding,
        }),
        stroke: None,
        paint_order: PaintOrder::FillAndStroke,
        anti_alias: true,
        data,
        bounding_box: bbox,
        stroke_bounding_box: bbox,
    };
    let mut root = Group::default();
    root.children.push(Node::Path(Box::new(path)));
    root.calculate_bounding_boxes();
    Group {
        transform,
        clip_path: Some(Arc::new(ClipPath {
            transform: Transform::identity(),
            clip_path: None,
            root,
        })),
        ..Group::default()
    }
}

/// usvg `use_node::convert_children`.
fn use_convert_children(
    node: SvgNode,
    transform: Transform,
    state: &State,
    cache: &mut Cache,
    parent: &mut Group,
) {
    let required = !transform.is_identity();
    let r = convert_group_into(node, state, required, cache, parent, &mut |cache, g| {
        if state.in_clip_path {
            convert_clip_path_elements(node, state, cache, g);
        } else {
            convert_children(node, state, cache, g);
        }
    });
    if let Some(mut g) = r {
        // `convert_group_into` took the transform from the attribute; the
        // group carries the `use` transform instead.
        g.transform = transform;
        g.calculate_bounding_boxes();
        parent.children.push(Node::Group(Box::new(g)));
    }
}

fn get_clip_rect(use_node: SvgNode, symbol: SvgNode, state: &State) -> Option<NonZeroRect> {
    if matches!(
        symbol.attribute(AId::Overflow).map(values::trim),
        Some("visible" | "auto")
    ) {
        return None;
    }
    if use_node.tag_name() == Some(EId::Svg)
        && state.use_size.0.is_none()
        && state.use_size.1.is_none()
        && !(use_node.has_attribute(AId::Width) && use_node.has_attribute(AId::Height))
    {
        return None;
    }
    let x = use_node.convert_user_length(AId::X, state, Length::zero());
    let y = use_node.convert_user_length(AId::Y, state, Length::zero());
    let (mut w, mut h) = use_node_size(use_node, state);
    if use_node.tag_name() == Some(EId::Svg) {
        w = state.use_size.0.unwrap_or(w);
        h = state.use_size.1.unwrap_or(h);
    }
    if !(w.is_finite() && w > 0.0 && h.is_finite() && h > 0.0) {
        return None;
    }
    NonZeroRect::from_xywh(x, y, w, h)
}

fn use_node_size(node: SvgNode, state: &State) -> (f32, f32) {
    let def = Length::new(100.0, Unit::Percent);
    (
        node.convert_user_length(AId::Width, state, def),
        node.convert_user_length(AId::Height, state, def),
    )
}

fn viewbox_transform(node: SvgNode, linked: SvgNode, state: &State) -> Option<Transform> {
    let (mut w, mut h) = use_node_size(node, state);
    if node.tag_name() == Some(EId::Svg) {
        w = state.use_size.0.unwrap_or(w);
        h = state.use_size.1.unwrap_or(h);
    }
    let size = Size::from_wh(w, h)?;
    let rect = linked.parse_viewbox()?;
    Some(view_box_to_transform(rect, linked.aspect(), size))
}

// ---------------------------------------------------------------- images --

fn convert_image(node: SvgNode, state: &State, parent: &mut Group) {
    let Some(href) = node.attribute(AId::Href) else {
        return;
    };
    let Some((kind, actual)) = crate::image::load_href(href, state.opt) else {
        return;
    };
    let visible = node
        .find_attribute(AId::Visibility)
        .map(values::trim)
        .unwrap_or("visible")
        == "visible";
    let quality = match node.find_attribute(AId::ImageRendering).map(values::trim) {
        Some("optimizeSpeed" | "crisp-edges" | "pixelated") => FilterQuality::Nearest,
        Some("smooth") => FilterQuality::Bilinear,
        _ => FilterQuality::Bicubic,
    };
    let x = node.convert_user_length(AId::X, state, Length::zero());
    let y = node.convert_user_length(AId::Y, state, Length::zero());
    let mut width = node.convert_user_length(
        AId::Width,
        state,
        Length::new(f64::from(actual.width()), Unit::None),
    );
    let mut height = node.convert_user_length(
        AId::Height,
        state,
        Length::new(f64::from(actual.height()), Unit::None),
    );
    match (node.length_attr(AId::Width), node.length_attr(AId::Height)) {
        (Some(_), None) => height = actual.height() * (width / actual.width()),
        (None, Some(_)) => width = actual.width() * (height / actual.height()),
        _ => {}
    }
    let aspect = node.aspect();
    let Some(rect) = NonZeroRect::from_xywh(x, y, width, height) else {
        return;
    };
    let aligned = {
        let s = rect.size();
        if aspect.align == Align::None {
            Some(s)
        } else {
            fit_size(actual, s, aspect.slice)
        }
    };
    let Some(aligned) = aligned else {
        return;
    };
    let (ax, ay) = aligned_pos(
        aspect.align,
        rect.x(),
        rect.y(),
        rect.width() - aligned.width(),
        rect.height() - aligned.height(),
    );
    let Some(vb) = NonZeroRect::from_xywh(ax, ay, aligned.width(), aligned.height()) else {
        return;
    };
    let image_ts = Transform::from_row(
        vb.width() / actual.width(),
        0.0,
        0.0,
        vb.height() / actual.height(),
        vb.x(),
        vb.y(),
    );
    let mut g = Group {
        transform: image_ts,
        ..Group::default()
    };
    g.children.push(Node::Image(Box::new(Image {
        visible,
        size: actual,
        quality,
        kind,
    })));
    g.calculate_bounding_boxes();
    if aspect.slice {
        let mut g2 = clip_element(rect, Transform::identity());
        g2.children.push(Node::Group(Box::new(g)));
        g2.calculate_bounding_boxes();
        parent.children.push(Node::Group(Box::new(g2)));
    } else {
        parent.children.push(Node::Group(Box::new(g)));
    }
}

/// Image kinds the loader can produce.
pub(crate) fn image_kind_png(data: Vec<u8>) -> ImageKind {
    ImageKind::Png(Arc::new(data))
}

/// Image kinds the loader can produce.
pub(crate) fn image_kind_jpeg(data: Vec<u8>) -> ImageKind {
    ImageKind::Jpeg(Arc::new(data))
}
