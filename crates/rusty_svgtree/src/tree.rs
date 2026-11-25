//! The simplified render tree (usvg-shaped): groups, paths and images, with
//! every style resolved, every length in user units and every paint server in
//! user space.

use std::sync::Arc;

use rusty_raster::{
    BlendMode, FillRule, FilterQuality, LineCap, LineJoin, MaskType, NonZeroRect, Rect, Size,
    SpreadMode, Transform,
};

use crate::filter::Filter;

/// A node of the render tree.
#[derive(Clone, Debug)]
pub enum Node {
    /// A group (possibly isolated: opacity, clip, mask, blend).
    Group(Box<Group>),
    /// A filled and/or stroked path.
    Path(Box<Path>),
    /// An embedded raster image.
    Image(Box<Image>),
}

impl Node {
    /// The fill bounding box in the parent's coordinates (group transforms
    /// are not applied).
    pub(crate) fn bounding_box(&self) -> Option<Rect> {
        match self {
            Node::Group(g) => g.bounding_box,
            Node::Path(p) => Some(p.bounding_box),
            Node::Image(i) => i.size.to_rect(0.0, 0.0),
        }
    }

    pub(crate) fn stroke_bounding_box(&self) -> Option<Rect> {
        match self {
            Node::Group(g) => g.stroke_bounding_box,
            Node::Path(p) => Some(p.stroke_bounding_box),
            Node::Image(i) => i.size.to_rect(0.0, 0.0),
        }
    }
}

/// A group of nodes.
#[derive(Clone, Debug)]
pub struct Group {
    pub(crate) transform: Transform,
    pub(crate) opacity: f32,
    pub(crate) blend_mode: BlendMode,
    pub(crate) isolate: bool,
    pub(crate) clip_path: Option<Arc<ClipPath>>,
    pub(crate) mask: Option<Arc<Mask>>,
    pub(crate) filters: Vec<Arc<Filter>>,
    pub(crate) children: Vec<Node>,
    pub(crate) bounding_box: Option<Rect>,
    pub(crate) stroke_bounding_box: Option<Rect>,
    pub(crate) layer_bounding_box: NonZeroRect,
}

impl Default for Group {
    fn default() -> Self {
        Group {
            transform: Transform::identity(),
            opacity: 1.0,
            blend_mode: BlendMode::SourceOver,
            isolate: false,
            clip_path: None,
            mask: None,
            filters: Vec::new(),
            children: Vec::new(),
            bounding_box: None,
            stroke_bounding_box: None,
            layer_bounding_box: unit_rect(),
        }
    }
}

pub(crate) fn unit_rect() -> NonZeroRect {
    // 0,0,1,1 is a valid non-zero rect; `from_xywh` cannot fail here.
    NonZeroRect::from_xywh(0.0, 0.0, 1.0, 1.0).unwrap_or_else(|| unreachable!())
}

/// Expands an optional rect by another.
fn expand(a: Option<Rect>, b: Option<Rect>) -> Option<Rect> {
    match (a, b) {
        (Some(a), Some(b)) => Rect::from_ltrb(
            a.left().min(b.left()),
            a.top().min(b.top()),
            a.right().max(b.right()),
            a.bottom().max(b.bottom()),
        ),
        (Some(a), None) => Some(a),
        (None, b) => b,
    }
}

impl Group {
    /// The group's transform relative to its parent.
    pub fn transform(&self) -> Transform {
        self.transform
    }

    /// Group opacity, `0..=1`.
    pub fn opacity(&self) -> f32 {
        self.opacity
    }

    /// The blend mode used to composite the group.
    pub fn blend_mode(&self) -> BlendMode {
        self.blend_mode
    }

    /// The clip path, if any.
    pub fn clip_path(&self) -> Option<&ClipPath> {
        self.clip_path.as_deref()
    }

    /// The mask, if any.
    pub fn mask(&self) -> Option<&Mask> {
        self.mask.as_deref()
    }

    /// The filters, applied in order before the clip path and mask.
    pub fn filters(&self) -> &[Arc<Filter>] {
        &self.filters
    }

    /// Child nodes, in paint order.
    pub fn children(&self) -> &[Node] {
        &self.children
    }

    /// Whether the group is rendered into its own layer.
    pub fn should_isolate(&self) -> bool {
        self.isolate
            || self.opacity != 1.0
            || self.clip_path.is_some()
            || self.mask.is_some()
            || !self.filters.is_empty()
            || self.blend_mode != BlendMode::SourceOver
    }

    /// The union of the children's fill boxes (the `objectBoundingBox`).
    pub(crate) fn calculate_object_bbox(&self) -> Option<NonZeroRect> {
        let mut bbox = None;
        for child in &self.children {
            let mut c = child.bounding_box();
            if let (Node::Group(g), Some(r)) = (child, c) {
                if let Some(t) = r.transform(g.transform) {
                    c = Some(t);
                }
            }
            bbox = expand(bbox, c);
        }
        bbox.and_then(|r| r.to_non_zero_rect())
    }

    /// Computes the fill, stroke and layer boxes, as usvg does.
    pub(crate) fn calculate_bounding_boxes(&mut self) {
        let mut bbox = None;
        let mut stroke_bbox = None;
        let mut layer_bbox = None;
        for child in &self.children {
            let tr = |r: Option<Rect>| -> Option<Rect> {
                if let (Node::Group(g), Some(r)) = (child, r) {
                    return r.transform(g.transform).or(Some(r));
                }
                r
            };
            bbox = expand(bbox, tr(child.bounding_box()));
            stroke_bbox = expand(stroke_bbox, tr(child.stroke_bounding_box()));
            if let Node::Group(g) = child {
                if let Some(r) = g.layer_bounding_box.transform(g.transform) {
                    layer_bbox = expand(layer_bbox, Some(r.to_rect()));
                }
            } else {
                layer_bbox = expand(layer_bbox, child.stroke_bounding_box());
            }
        }
        if bbox.is_some() {
            self.bounding_box = bbox;
            self.stroke_bounding_box = stroke_bbox;
        }
        // The filter regions take the place of the content's box.
        let filter_bbox = self
            .filters
            .iter()
            .fold(None, |b, f| expand(b, Some(f.rect.to_rect())));
        if let Some(r) = filter_bbox
            .or(layer_bbox)
            .and_then(|r| r.to_non_zero_rect())
        {
            self.layer_bounding_box = r;
        }
    }
}

/// How the fill and stroke of a path are ordered.
#[derive(Clone, Copy, PartialEq, Eq, Debug, Default)]
pub enum PaintOrder {
    /// Fill first (the default).
    #[default]
    FillAndStroke,
    /// Stroke first.
    StrokeAndFill,
}

/// A path with resolved paint.
#[derive(Clone, Debug)]
pub struct Path {
    pub(crate) visible: bool,
    pub(crate) fill: Option<Fill>,
    pub(crate) stroke: Option<Stroke>,
    pub(crate) paint_order: PaintOrder,
    pub(crate) anti_alias: bool,
    pub(crate) data: Arc<rusty_raster::Path>,
    pub(crate) bounding_box: Rect,
    pub(crate) stroke_bounding_box: Rect,
}

impl Path {
    /// Whether the path is drawn (`visibility`).
    pub fn is_visible(&self) -> bool {
        self.visible
    }

    /// The fill, if any.
    pub fn fill(&self) -> Option<&Fill> {
        self.fill.as_ref()
    }

    /// The stroke, if any.
    pub fn stroke(&self) -> Option<&Stroke> {
        self.stroke.as_ref()
    }

    /// The geometry, in the parent group's user space.
    pub fn data(&self) -> &rusty_raster::Path {
        &self.data
    }

    /// Fill then stroke, or stroke then fill.
    pub fn paint_order(&self) -> PaintOrder {
        self.paint_order
    }

    /// `shape-rendering` resolved to an anti-aliasing switch.
    pub fn anti_alias(&self) -> bool {
        self.anti_alias
    }
}

/// An sRGB colour, non-premultiplied, opaque (alpha lives in opacities).
#[derive(Clone, Copy, PartialEq, Eq, Debug)]
pub struct Color {
    /// Red.
    pub red: u8,
    /// Green.
    pub green: u8,
    /// Blue.
    pub blue: u8,
}

/// A paint source.
#[derive(Clone, Debug)]
pub enum Paint {
    /// A solid colour.
    Color(Color),
    /// A linear gradient.
    LinearGradient(Arc<LinearGradient>),
    /// A radial (two-point conical) gradient.
    RadialGradient(Arc<RadialGradient>),
    /// A tiled pattern.
    Pattern(Arc<Pattern>),
}

/// A fill.
#[derive(Clone, Debug)]
pub struct Fill {
    pub(crate) paint: Paint,
    pub(crate) opacity: f32,
    pub(crate) rule: FillRule,
}

impl Fill {
    /// The paint.
    pub fn paint(&self) -> &Paint {
        &self.paint
    }

    /// `fill-opacity` (times any colour alpha), `0..=1`.
    pub fn opacity(&self) -> f32 {
        self.opacity
    }

    /// The fill rule.
    pub fn rule(&self) -> FillRule {
        self.rule
    }
}

/// A stroke.
#[derive(Clone, Debug)]
pub struct Stroke {
    pub(crate) paint: Paint,
    pub(crate) opacity: f32,
    pub(crate) width: f32,
    pub(crate) miterlimit: f32,
    pub(crate) linecap: LineCap,
    pub(crate) linejoin: LineJoin,
    pub(crate) dasharray: Option<Vec<f32>>,
    pub(crate) dashoffset: f32,
}

impl Stroke {
    /// The paint.
    pub fn paint(&self) -> &Paint {
        &self.paint
    }

    /// `stroke-opacity` (times any colour alpha), `0..=1`.
    pub fn opacity(&self) -> f32 {
        self.opacity
    }

    /// The stroke width in user units (always `> 0`).
    pub fn width(&self) -> f32 {
        self.width
    }

    /// Converts to a rasteriser stroke, with the dash pattern.
    pub fn to_raster(&self) -> rusty_raster::Stroke {
        let mut s = rusty_raster::Stroke {
            width: self.width,
            miter_limit: self.miterlimit,
            line_cap: self.linecap,
            line_join: self.linejoin,
            dash: None,
        };
        if let Some(list) = &self.dasharray {
            s.dash = rusty_raster::StrokeDash::new(list.clone(), self.dashoffset);
        }
        s
    }
}

/// A gradient stop.
#[derive(Clone, Copy, Debug)]
pub struct Stop {
    pub(crate) offset: f32,
    pub(crate) color: Color,
    pub(crate) opacity: f32,
}

impl Stop {
    /// Offset, `0..=1`.
    pub fn offset(&self) -> f32 {
        self.offset
    }

    /// Colour.
    pub fn color(&self) -> Color {
        self.color
    }

    /// `stop-opacity` times any colour alpha.
    pub fn opacity(&self) -> f32 {
        self.opacity
    }
}

/// The parts linear and radial gradients share.
#[derive(Clone, Debug)]
pub struct BaseGradient {
    pub(crate) transform: Transform,
    pub(crate) spread: SpreadMode,
    pub(crate) stops: Vec<Stop>,
    /// Still in `objectBoundingBox` units (only before conversion ends).
    pub(crate) obb: bool,
}

impl BaseGradient {
    /// Gradient-to-user-space transform (`objectBoundingBox` folded in).
    pub fn transform(&self) -> Transform {
        self.transform
    }

    /// Spread method.
    pub fn spread_method(&self) -> SpreadMode {
        self.spread
    }

    /// The stops, sorted, at least two.
    pub fn stops(&self) -> &[Stop] {
        &self.stops
    }
}

/// A linear gradient in user space.
#[derive(Clone, Debug)]
pub struct LinearGradient {
    pub(crate) x1: f32,
    pub(crate) y1: f32,
    pub(crate) x2: f32,
    pub(crate) y2: f32,
    pub(crate) base: BaseGradient,
}

impl LinearGradient {
    /// Start and end points.
    pub fn points(&self) -> (f32, f32, f32, f32) {
        (self.x1, self.y1, self.x2, self.y2)
    }

    /// Shared gradient parameters.
    pub fn base(&self) -> &BaseGradient {
        &self.base
    }
}

/// A radial gradient in user space.
#[derive(Clone, Debug)]
pub struct RadialGradient {
    pub(crate) cx: f32,
    pub(crate) cy: f32,
    pub(crate) r: f32,
    pub(crate) fx: f32,
    pub(crate) fy: f32,
    pub(crate) fr: f32,
    pub(crate) base: BaseGradient,
}

impl RadialGradient {
    /// Centre and radius.
    pub fn circle(&self) -> (f32, f32, f32) {
        (self.cx, self.cy, self.r)
    }

    /// Focal point and radius.
    pub fn focal(&self) -> (f32, f32, f32) {
        (self.fx, self.fy, self.fr)
    }

    /// Shared gradient parameters.
    pub fn base(&self) -> &BaseGradient {
        &self.base
    }
}

/// A pattern tile in user space.
#[derive(Clone, Debug)]
pub struct Pattern {
    pub(crate) transform: Transform,
    pub(crate) rect: NonZeroRect,
    pub(crate) root: Group,
    /// Pending `objectBoundingBox` resolution (only during conversion).
    pub(crate) pending: Option<PatternPending>,
}

/// What a pattern still needs from the painted shape's bounding box.
#[derive(Clone, Copy, Debug)]
pub(crate) struct PatternPending {
    pub obb_units: bool,
    pub obb_content: bool,
    pub view_box: Option<(NonZeroRect, crate::values::AspectRatio)>,
}

impl Pattern {
    /// Pattern-to-user transform.
    pub fn transform(&self) -> Transform {
        self.transform
    }

    /// The tile rectangle.
    pub fn rect(&self) -> NonZeroRect {
        self.rect
    }

    /// The tile content.
    pub fn root(&self) -> &Group {
        &self.root
    }
}

/// A clip path.
#[derive(Clone, Debug)]
pub struct ClipPath {
    pub(crate) transform: Transform,
    pub(crate) clip_path: Option<Arc<ClipPath>>,
    pub(crate) root: Group,
}

impl ClipPath {
    /// Clip-content transform (`clipPathUnits` folded in).
    pub fn transform(&self) -> Transform {
        self.transform
    }

    /// A clip path applied to this clip path.
    pub fn clip_path(&self) -> Option<&ClipPath> {
        self.clip_path.as_deref()
    }

    /// The clip content.
    pub fn root(&self) -> &Group {
        &self.root
    }
}

/// A mask.
#[derive(Clone, Debug)]
pub struct Mask {
    pub(crate) rect: NonZeroRect,
    pub(crate) kind: MaskType,
    pub(crate) mask: Option<Arc<Mask>>,
    pub(crate) root: Group,
}

impl Mask {
    /// The mask region in user space.
    pub fn rect(&self) -> NonZeroRect {
        self.rect
    }

    /// Luminance or alpha.
    pub fn kind(&self) -> MaskType {
        self.kind
    }

    /// A mask applied to this mask.
    pub fn mask(&self) -> Option<&Mask> {
        self.mask.as_deref()
    }

    /// The mask content.
    pub fn root(&self) -> &Group {
        &self.root
    }
}

/// Encoded image data; decoded at render time with a pixel-count limit.
#[derive(Clone, Debug)]
pub enum ImageKind {
    /// PNG bytes.
    Png(Arc<Vec<u8>>),
    /// JPEG bytes.
    Jpeg(Arc<Vec<u8>>),
}

/// An embedded raster image, drawn at `0,0` with its intrinsic size (the
/// parent group carries the placement transform).
#[derive(Clone, Debug)]
pub struct Image {
    pub(crate) visible: bool,
    pub(crate) size: Size,
    pub(crate) quality: FilterQuality,
    pub(crate) kind: ImageKind,
}

impl Image {
    /// Intrinsic size in pixels.
    pub fn size(&self) -> Size {
        self.size
    }

    /// The encoded data.
    pub fn kind(&self) -> &ImageKind {
        &self.kind
    }

    /// Whether the image is drawn (`visibility`).
    pub fn is_visible(&self) -> bool {
        self.visible
    }
}
