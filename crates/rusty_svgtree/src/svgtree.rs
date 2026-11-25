//! The intermediate SVG document: the XML DOM reduced to SVG elements with
//! their attributes fully resolved (presentation attributes, `<style>` rules,
//! `style` attributes, `inherit`), and `use` references expanded into copies,
//! as usvg's `svgtree` does.
//!
//! Hostile-input bounds live here: element nesting depth, total node count
//! (which bounds `use` amplification) and reference-cycle breaking.

use std::collections::HashMap;

use rusty_xml::{NodeId as XId, NodeKind, XmlDoc};

use crate::css;
use crate::Error;

const SVG_NS: &str = "http://www.w3.org/2000/svg";
const XLINK_NS: &str = "http://www.w3.org/1999/xlink";

/// SVG element kinds we keep. Anything else is dropped with its subtree.
#[derive(Clone, Copy, PartialEq, Eq, Debug)]
pub(crate) enum EId {
    Svg,
    G,
    Defs,
    Symbol,
    Use,
    Switch,
    Path,
    Rect,
    Circle,
    Ellipse,
    Line,
    Polyline,
    Polygon,
    Image,
    Text,
    Tspan,
    Tref,
    TextPath,
    LinearGradient,
    RadialGradient,
    Stop,
    ClipPath,
    Mask,
    Pattern,
    Marker,
    Filter,
    FeBlend,
    FeColorMatrix,
    FeComponentTransfer,
    FeComposite,
    FeConvolveMatrix,
    FeDiffuseLighting,
    FeDisplacementMap,
    FeDistantLight,
    FeDropShadow,
    FeFlood,
    FeFuncA,
    FeFuncB,
    FeFuncG,
    FeFuncR,
    FeGaussianBlur,
    FeImage,
    FeMerge,
    FeMergeNode,
    FeMorphology,
    FeOffset,
    FePointLight,
    FeSpecularLighting,
    FeSpotLight,
    FeTile,
    FeTurbulence,
}

impl EId {
    fn from_name(name: &str) -> Option<EId> {
        Some(match name {
            "svg" => EId::Svg,
            "g" | "a" => EId::G,
            "defs" => EId::Defs,
            "symbol" => EId::Symbol,
            "use" => EId::Use,
            "switch" => EId::Switch,
            "path" => EId::Path,
            "rect" => EId::Rect,
            "circle" => EId::Circle,
            "ellipse" => EId::Ellipse,
            "line" => EId::Line,
            "polyline" => EId::Polyline,
            "polygon" => EId::Polygon,
            "image" => EId::Image,
            "text" => EId::Text,
            "tspan" => EId::Tspan,
            "tref" => EId::Tref,
            "textPath" => EId::TextPath,
            "linearGradient" => EId::LinearGradient,
            "radialGradient" => EId::RadialGradient,
            "stop" => EId::Stop,
            "clipPath" => EId::ClipPath,
            "mask" => EId::Mask,
            "pattern" => EId::Pattern,
            "marker" => EId::Marker,
            "filter" => EId::Filter,
            "feBlend" => EId::FeBlend,
            "feColorMatrix" => EId::FeColorMatrix,
            "feComponentTransfer" => EId::FeComponentTransfer,
            "feComposite" => EId::FeComposite,
            "feConvolveMatrix" => EId::FeConvolveMatrix,
            "feDiffuseLighting" => EId::FeDiffuseLighting,
            "feDisplacementMap" => EId::FeDisplacementMap,
            "feDistantLight" => EId::FeDistantLight,
            "feDropShadow" => EId::FeDropShadow,
            "feFlood" => EId::FeFlood,
            "feFuncA" => EId::FeFuncA,
            "feFuncB" => EId::FeFuncB,
            "feFuncG" => EId::FeFuncG,
            "feFuncR" => EId::FeFuncR,
            "feGaussianBlur" => EId::FeGaussianBlur,
            "feImage" => EId::FeImage,
            "feMerge" => EId::FeMerge,
            "feMergeNode" => EId::FeMergeNode,
            "feMorphology" => EId::FeMorphology,
            "feOffset" => EId::FeOffset,
            "fePointLight" => EId::FePointLight,
            "feSpecularLighting" => EId::FeSpecularLighting,
            "feSpotLight" => EId::FeSpotLight,
            "feTile" => EId::FeTile,
            "feTurbulence" => EId::FeTurbulence,
            _ => return None,
        })
    }

    /// Elements that draw (SVG "graphics elements" plus `use`).
    pub(crate) fn is_graphic(self) -> bool {
        matches!(
            self,
            EId::Circle
                | EId::Ellipse
                | EId::Image
                | EId::Line
                | EId::Path
                | EId::Polygon
                | EId::Polyline
                | EId::Rect
                | EId::Text
                | EId::Use
        )
    }

    pub(crate) fn is_gradient(self) -> bool {
        matches!(self, EId::LinearGradient | EId::RadialGradient)
    }
}

macro_rules! attribute_ids {
    ($($variant:ident = $name:literal,)*) => {
        /// The attributes we keep, by name: every presentation attribute and
        /// every other attribute the converter reads. An element's other
        /// attributes are dropped while the document is built (nothing could
        /// look them up).
        #[derive(Clone, Copy, PartialEq, Eq, Debug)]
        #[repr(u8)]
        pub(crate) enum AId {
            $($variant,)*
        }

        impl AId {
            /// How many attribute ids there are.
            const COUNT: usize = [$($name,)*].len();

            fn from_name(name: &str) -> Option<AId> {
                Some(match name {
                    $($name => AId::$variant,)*
                    _ => return None,
                })
            }
        }
    };
}

/// A set of attribute ids, one bit each.
#[derive(Clone, Copy, Debug, Default, PartialEq, Eq)]
struct AttrSet([u64; 3]);

const _: () = assert!(AId::COUNT <= 192);

impl AttrSet {
    #[inline]
    fn contains(self, id: AId) -> bool {
        let i = id as usize;
        self.0[i >> 6] & (1 << (i & 63)) != 0
    }

    #[inline]
    fn insert(&mut self, id: AId) {
        let i = id as usize;
        self.0[i >> 6] |= 1 << (i & 63);
    }

    #[inline]
    fn union(self, other: AttrSet) -> AttrSet {
        AttrSet([
            self.0[0] | other.0[0],
            self.0[1] | other.0[1],
            self.0[2] | other.0[2],
        ])
    }
}

attribute_ids! {
    AlignmentBaseline = "alignment-baseline",
    BaselineShift = "baseline-shift",
    BackgroundColor = "background-color",
    ClipPath = "clip-path",
    ClipRule = "clip-rule",
    Color = "color",
    ColorInterpolation = "color-interpolation",
    ColorInterpolationFilters = "color-interpolation-filters",
    ColorRendering = "color-rendering",
    Direction = "direction",
    Display = "display",
    DominantBaseline = "dominant-baseline",
    Fill = "fill",
    FillOpacity = "fill-opacity",
    FillRule = "fill-rule",
    Filter = "filter",
    FloodColor = "flood-color",
    FloodOpacity = "flood-opacity",
    FontFamily = "font-family",
    FontKerning = "font-kerning",
    FontSize = "font-size",
    FontSizeAdjust = "font-size-adjust",
    FontStretch = "font-stretch",
    FontStyle = "font-style",
    FontVariant = "font-variant",
    FontWeight = "font-weight",
    ImageRendering = "image-rendering",
    Isolation = "isolation",
    LetterSpacing = "letter-spacing",
    LightingColor = "lighting-color",
    MarkerEnd = "marker-end",
    MarkerMid = "marker-mid",
    MarkerStart = "marker-start",
    Mask = "mask",
    MaskType = "mask-type",
    MixBlendMode = "mix-blend-mode",
    Opacity = "opacity",
    Overflow = "overflow",
    PaintOrder = "paint-order",
    ShapeRendering = "shape-rendering",
    StopColor = "stop-color",
    StopOpacity = "stop-opacity",
    Stroke = "stroke",
    StrokeDasharray = "stroke-dasharray",
    StrokeDashoffset = "stroke-dashoffset",
    StrokeLinecap = "stroke-linecap",
    StrokeLinejoin = "stroke-linejoin",
    StrokeMiterlimit = "stroke-miterlimit",
    StrokeOpacity = "stroke-opacity",
    StrokeWidth = "stroke-width",
    TextAnchor = "text-anchor",
    TextDecoration = "text-decoration",
    TextRendering = "text-rendering",
    Transform = "transform",
    TransformOrigin = "transform-origin",
    UnicodeBidi = "unicode-bidi",
    VectorEffect = "vector-effect",
    Visibility = "visibility",
    WordSpacing = "word-spacing",
    WritingMode = "writing-mode",
    Id = "id",
    Href = "href",
    Width = "width",
    Height = "height",
    X = "x",
    Y = "y",
    Cx = "cx",
    Cy = "cy",
    R = "r",
    Rx = "rx",
    Ry = "ry",
    X1 = "x1",
    Y1 = "y1",
    X2 = "x2",
    Y2 = "y2",
    Fx = "fx",
    Fy = "fy",
    Fr = "fr",
    D = "d",
    Points = "points",
    ViewBox = "viewBox",
    PreserveAspectRatio = "preserveAspectRatio",
    Offset = "offset",
    GradientUnits = "gradientUnits",
    GradientTransform = "gradientTransform",
    SpreadMethod = "spreadMethod",
    PatternUnits = "patternUnits",
    PatternContentUnits = "patternContentUnits",
    PatternTransform = "patternTransform",
    ClipPathUnits = "clipPathUnits",
    MaskUnits = "maskUnits",
    MaskContentUnits = "maskContentUnits",
    MarkerUnits = "markerUnits",
    MarkerWidth = "markerWidth",
    MarkerHeight = "markerHeight",
    RefX = "refX",
    RefY = "refY",
    Orient = "orient",
    RequiredExtensions = "requiredExtensions",
    RequiredFeatures = "requiredFeatures",
    SystemLanguage = "systemLanguage",
    FontOpticalSizing = "font-optical-sizing",
    FontVariationSettings = "font-variation-settings",
    Kerning = "kerning",
    LengthAdjust = "lengthAdjust",
    Rotate = "rotate",
    Space = "space",
    StartOffset = "startOffset",
    TextLength = "textLength",
    FilterUnits = "filterUnits",
    PrimitiveUnits = "primitiveUnits",
    In = "in",
    In2 = "in2",
    Result = "result",
    Type = "type",
    Values = "values",
    TableValues = "tableValues",
    Slope = "slope",
    Intercept = "intercept",
    Amplitude = "amplitude",
    Exponent = "exponent",
    Mode = "mode",
    Operator = "operator",
    K1 = "k1",
    K2 = "k2",
    K3 = "k3",
    K4 = "k4",
    Order = "order",
    KernelMatrix = "kernelMatrix",
    Divisor = "divisor",
    Bias = "bias",
    TargetX = "targetX",
    TargetY = "targetY",
    EdgeMode = "edgeMode",
    PreserveAlpha = "preserveAlpha",
    StdDeviation = "stdDeviation",
    Dx = "dx",
    Dy = "dy",
    Radius = "radius",
    Scale = "scale",
    XChannelSelector = "xChannelSelector",
    YChannelSelector = "yChannelSelector",
    BaseFrequency = "baseFrequency",
    NumOctaves = "numOctaves",
    Seed = "seed",
    StitchTiles = "stitchTiles",
    SurfaceScale = "surfaceScale",
    DiffuseConstant = "diffuseConstant",
    SpecularConstant = "specularConstant",
    SpecularExponent = "specularExponent",
    Azimuth = "azimuth",
    Elevation = "elevation",
    Z = "z",
    PointsAtX = "pointsAtX",
    PointsAtY = "pointsAtY",
    PointsAtZ = "pointsAtZ",
    LimitingConeAngle = "limitingConeAngle",
}

impl AId {
    /// Presentation attributes: settable from CSS and the `style` attribute.
    fn is_presentation(self) -> bool {
        matches!(
            self,
            AId::AlignmentBaseline
                | AId::BaselineShift
                | AId::BackgroundColor
                | AId::ClipPath
                | AId::ClipRule
                | AId::Color
                | AId::ColorInterpolation
                | AId::ColorInterpolationFilters
                | AId::ColorRendering
                | AId::Direction
                | AId::Display
                | AId::DominantBaseline
                | AId::Fill
                | AId::FillOpacity
                | AId::FillRule
                | AId::Filter
                | AId::FloodColor
                | AId::FloodOpacity
                | AId::FontFamily
                | AId::FontKerning
                | AId::FontOpticalSizing
                | AId::FontSize
                | AId::FontSizeAdjust
                | AId::FontStretch
                | AId::FontStyle
                | AId::FontVariant
                | AId::FontVariationSettings
                | AId::FontWeight
                | AId::ImageRendering
                | AId::Isolation
                | AId::LetterSpacing
                | AId::LightingColor
                | AId::MarkerEnd
                | AId::MarkerMid
                | AId::MarkerStart
                | AId::Mask
                | AId::MaskType
                | AId::MixBlendMode
                | AId::Opacity
                | AId::Overflow
                | AId::PaintOrder
                | AId::ShapeRendering
                | AId::StopColor
                | AId::StopOpacity
                | AId::Stroke
                | AId::StrokeDasharray
                | AId::StrokeDashoffset
                | AId::StrokeLinecap
                | AId::StrokeLinejoin
                | AId::StrokeMiterlimit
                | AId::StrokeOpacity
                | AId::StrokeWidth
                | AId::TextAnchor
                | AId::TextDecoration
                | AId::TextRendering
                | AId::Transform
                | AId::TransformOrigin
                | AId::UnicodeBidi
                | AId::VectorEffect
                | AId::Visibility
                | AId::WordSpacing
                | AId::WritingMode
        )
    }

    fn is_non_inheritable(self) -> bool {
        matches!(
            self,
            AId::AlignmentBaseline
                | AId::BaselineShift
                | AId::ClipPath
                | AId::Display
                | AId::DominantBaseline
                | AId::Filter
                | AId::FloodColor
                | AId::FloodOpacity
                | AId::Mask
                | AId::Opacity
                | AId::Overflow
                | AId::LightingColor
                | AId::StopColor
                | AId::StopOpacity
                | AId::TextDecoration
                | AId::Transform
                | AId::TransformOrigin
        )
    }

    /// Whether the value `inherit` is meaningful: for anything else it is an
    /// invalid value, kept as it stands (usvg's `allows_inherit_value`).
    fn allows_inherit(self) -> bool {
        self.is_presentation()
    }

    /// Whether the attribute is inherited by descendants.
    pub(crate) fn is_inheritable(self) -> bool {
        self.is_presentation() && !self.is_non_inheritable()
    }
}

/// An attribute value: borrowed from the XML DOM's text (attributes and
/// `style` declarations: no copy) or a fixed default, or owned (style-sheet
/// declarations, whose text the builder holds only while it runs).
#[derive(Clone, Debug)]
enum Value<'x> {
    Xml(&'x str),
    Own(Box<str>),
}

impl Value<'_> {
    fn as_str(&self) -> &str {
        match self {
            Value::Xml(s) => s,
            Value::Own(s) => s,
        }
    }
}

/// Where a value being applied comes from: the XML text (kept by
/// reference) or a style-sheet rule (copied, once it is known to be kept).
#[derive(Clone, Copy)]
enum Src<'x, 'v> {
    Xml(&'x str),
    Sheet(&'v str),
}

impl<'x> Src<'x, '_> {
    fn trimmed(self) -> Self {
        match self {
            Src::Xml(s) => Src::Xml(crate::values::trim(s)),
            Src::Sheet(s) => Src::Sheet(crate::values::trim(s)),
        }
    }

    fn as_str(&self) -> &str {
        match *self {
            Src::Xml(s) | Src::Sheet(s) => s,
        }
    }

    fn keep(self) -> Value<'x> {
        match self {
            Src::Xml(s) => Value::Xml(s),
            Src::Sheet(s) => Value::Own(s.into()),
        }
    }
}

#[derive(Clone, Debug)]
struct Attr<'x> {
    name: AId,
    important: bool,
    value: Value<'x>,
}

#[derive(Clone, Copy, PartialEq, Eq, Debug, Hash)]
pub(crate) struct NodeId(u32);

#[derive(Debug)]
struct NodeData {
    parent: Option<NodeId>,
    first_child: Option<NodeId>,
    last_child: Option<NodeId>,
    next_sibling: Option<NodeId>,
    /// `None` for the document root and for text.
    tag: Option<EId>,
    /// The attributes; of a text node, `start` is its index in
    /// `Document::texts` plus one, and there are none.
    attrs: std::ops::Range<u32>,
    /// The attributes this element has.
    own: AttrSet,
    /// The attributes this element or one of its ancestors has.
    chain: AttrSet,
}

/// The resolved SVG document. Attribute values borrow the XML DOM's text,
/// which therefore outlives it.
pub(crate) struct Document<'x> {
    nodes: Vec<NodeData>,
    attrs: Vec<Attr<'x>>,
    /// Character data of text content elements.
    texts: Vec<String>,
    /// Element ids, borrowed from the XML text like the attribute values.
    links: HashMap<&'x str, NodeId>,
}

/// A borrowed node handle.
#[derive(Clone, Copy)]
pub(crate) struct SvgNode<'a> {
    id: NodeId,
    doc: &'a Document<'a>,
}

impl PartialEq for SvgNode<'_> {
    fn eq(&self, other: &Self) -> bool {
        self.id == other.id && std::ptr::eq(self.doc, other.doc)
    }
}

impl std::fmt::Debug for SvgNode<'_> {
    fn fmt(&self, f: &mut std::fmt::Formatter<'_>) -> std::fmt::Result {
        write!(f, "SvgNode({:?}, {:?})", self.id, self.tag_name())
    }
}

impl<'a> SvgNode<'a> {
    fn d(&self) -> &'a NodeData {
        &self.doc.nodes[self.id.0 as usize]
    }

    pub(crate) fn id(&self) -> NodeId {
        self.id
    }

    pub(crate) fn document(&self) -> &'a Document<'a> {
        self.doc
    }

    pub(crate) fn tag_name(&self) -> Option<EId> {
        self.d().tag
    }

    /// The characters of a text node.
    pub(crate) fn text(&self) -> Option<&'a str> {
        let d = self.d();
        if d.tag.is_some() || d.attrs.start == 0 {
            return None;
        }
        self.doc
            .texts
            .get(d.attrs.start as usize - 1)
            .map(String::as_str)
    }

    pub(crate) fn element_id(&self) -> &'a str {
        self.attribute(AId::Id).unwrap_or("")
    }

    pub(crate) fn attribute(&self, name: AId) -> Option<&'a str> {
        let d = self.d();
        if !d.own.contains(name) {
            return None;
        }
        self.doc.attrs[d.attrs.start as usize..d.attrs.end as usize]
            .iter()
            .find(|a| a.name == name)
            .map(|a| a.value.as_str())
    }

    pub(crate) fn has_attribute(&self, name: AId) -> bool {
        self.d().own.contains(name)
    }

    /// The nearest of self and its ancestor elements that has `name`.
    pub(crate) fn ancestor_with_attribute(&self, name: AId) -> Option<SvgNode<'a>> {
        if !self.d().chain.contains(name) {
            return None;
        }
        self.ancestors().find(|n| n.has_attribute(name))
    }

    pub(crate) fn parent(&self) -> Option<SvgNode<'a>> {
        self.d().parent.map(|id| SvgNode { id, doc: self.doc })
    }

    /// The parent, if it is an element (not the document root).
    pub(crate) fn parent_element(&self) -> Option<SvgNode<'a>> {
        self.parent().filter(|p| p.tag_name().is_some())
    }

    pub(crate) fn first_child(&self) -> Option<SvgNode<'a>> {
        self.d().first_child.map(|id| SvgNode { id, doc: self.doc })
    }

    pub(crate) fn children(&self) -> Children<'a> {
        Children(self.first_child())
    }

    pub(crate) fn has_children(&self) -> bool {
        self.d().first_child.is_some()
    }

    /// Self, then each ancestor up to (excluding) the document root.
    pub(crate) fn ancestors(&self) -> Ancestors<'a> {
        Ancestors(Some(*self))
    }

    /// Pre-order traversal including `self`.
    pub(crate) fn descendants(&self) -> Descendants<'a> {
        Descendants {
            root: *self,
            next: Some(*self),
        }
    }

    /// Looks an attribute up on self or, if inheritable, on ancestors; a
    /// non-inheritable attribute is also taken from the direct parent (usvg's
    /// `find_attribute`).
    pub(crate) fn find_attribute_node(&self, name: AId) -> Option<SvgNode<'a>> {
        if name.is_inheritable() {
            self.ancestor_with_attribute(name)
        } else if self.has_attribute(name) {
            Some(*self)
        } else {
            self.parent_element().filter(|n| n.has_attribute(name))
        }
    }

    pub(crate) fn find_attribute(&self, name: AId) -> Option<&'a str> {
        self.find_attribute_node(name)?.attribute(name)
    }

    /// Resolves an `href` (`#id`) to a node.
    pub(crate) fn href(&self) -> Option<SvgNode<'a>> {
        let v = self.attribute(AId::Href)?;
        self.doc.element_by_id(crate::values::iri(v)?)
    }

    /// Resolves a `url(#id)` attribute to a node.
    pub(crate) fn func_iri_attribute(&self, name: AId) -> Option<SvgNode<'a>> {
        let v = self.attribute(name)?;
        self.doc.element_by_id(crate::values::func_iri(v)?)
    }

    /// The `href` chain starting at `self` (self first), cycle-safe and
    /// bounded.
    pub(crate) fn href_iter(&self) -> HrefIter<'a> {
        HrefIter {
            origin: self.id,
            curr: Some(*self),
            steps: 0,
        }
    }
}

pub(crate) struct Children<'a>(Option<SvgNode<'a>>);

impl<'a> Iterator for Children<'a> {
    type Item = SvgNode<'a>;
    fn next(&mut self) -> Option<Self::Item> {
        let n = self.0?;
        self.0 = n.d().next_sibling.map(|id| SvgNode { id, doc: n.doc });
        Some(n)
    }
}

pub(crate) struct Ancestors<'a>(Option<SvgNode<'a>>);

impl<'a> Iterator for Ancestors<'a> {
    type Item = SvgNode<'a>;
    fn next(&mut self) -> Option<Self::Item> {
        let n = self.0?;
        if n.tag_name().is_none() {
            self.0 = None;
            return None;
        }
        self.0 = n.parent();
        Some(n)
    }
}

pub(crate) struct Descendants<'a> {
    root: SvgNode<'a>,
    next: Option<SvgNode<'a>>,
}

impl<'a> Iterator for Descendants<'a> {
    type Item = SvgNode<'a>;
    fn next(&mut self) -> Option<Self::Item> {
        let n = self.next?;
        // Pre-order successor, never leaving `root`'s subtree.
        self.next = if let Some(c) = n.first_child() {
            Some(c)
        } else {
            let mut cur = Some(n);
            let mut succ = None;
            while let Some(c) = cur {
                if c == self.root {
                    break;
                }
                if let Some(s) = c.d().next_sibling {
                    succ = Some(SvgNode { id: s, doc: c.doc });
                    break;
                }
                cur = c.parent();
            }
            succ
        };
        Some(n)
    }
}

/// Maximum `href` hops followed (gradients, patterns): reference chains in
/// real files are 1-3 long; this bounds a hostile chain.
const MAX_HREF_HOPS: u32 = 64;

pub(crate) struct HrefIter<'a> {
    origin: NodeId,
    curr: Option<SvgNode<'a>>,
    steps: u32,
}

impl<'a> Iterator for HrefIter<'a> {
    type Item = SvgNode<'a>;
    fn next(&mut self) -> Option<Self::Item> {
        let n = self.curr?;
        self.steps += 1;
        self.curr = match n.href() {
            Some(link) if link.id != n.id && link.id != self.origin => {
                if self.steps >= MAX_HREF_HOPS {
                    None
                } else {
                    Some(link)
                }
            }
            _ => None,
        };
        Some(n)
    }
}

impl<'x> Document<'x> {
    /// How many elements there are.
    pub(crate) fn len(&self) -> usize {
        self.nodes.len()
    }

    pub(crate) fn root(&self) -> SvgNode<'_> {
        SvgNode {
            id: NodeId(0),
            doc: self,
        }
    }

    pub(crate) fn root_element(&self) -> Option<SvgNode<'_>> {
        self.root().first_child()
    }

    pub(crate) fn element_by_id(&self, id: &str) -> Option<SvgNode<'_>> {
        self.links.get(id).map(|&id| SvgNode { id, doc: self })
    }

    /// Appends an element whose attributes are `attrs`, which hold exactly
    /// the ids in `own` (or a text node, see `NodeData`).
    fn append(
        &mut self,
        parent: NodeId,
        tag: Option<EId>,
        attrs: std::ops::Range<u32>,
        own: AttrSet,
    ) -> NodeId {
        let id = NodeId(self.nodes.len() as u32);
        let p = parent.0 as usize;
        let chain = own.union(self.nodes[p].chain);
        self.nodes.push(NodeData {
            parent: Some(parent),
            first_child: None,
            last_child: None,
            next_sibling: None,
            tag,
            attrs,
            own,
            chain,
        });
        if let Some(last) = self.nodes[p].last_child {
            self.nodes[last.0 as usize].next_sibling = Some(id);
        } else {
            self.nodes[p].first_child = Some(id);
        }
        self.nodes[p].last_child = Some(id);
        id
    }

    fn set_attribute_value(&mut self, node: NodeId, name: AId, value: &'x str) {
        let r = self.nodes[node.0 as usize].attrs.clone();
        for a in &mut self.attrs[r.start as usize..r.end as usize] {
            if a.name == name {
                a.value = Value::Xml(value);
            }
        }
    }

    /// The value of `node`'s attribute `name`, if it has one.
    fn value(&self, node: NodeId, name: AId) -> Option<&Value<'x>> {
        let r = self.nodes[node.0 as usize].attrs.clone();
        self.attrs[r.start as usize..r.end as usize]
            .iter()
            .find(|a| a.name == name)
            .map(|a| &a.value)
    }
}

/// Compound-selector tests allowed for the whole document (about a second of
/// matching); past it, remaining style rules stop matching.
const CSS_MATCH_BUDGET: u64 = 50_000_000;

/// Bytes of text a document may have once `use` and `tref` are expanded.
const MAX_TEXT_BYTES: usize = 1 << 25;

/// Node visits allowed for the cycle-breaking passes below (each pass).
const FIXUP_BUDGET: usize = 20_000_000;

/// Limits applied while building the document.
#[derive(Clone, Copy)]
pub(crate) struct Limits {
    pub max_nodes: usize,
    pub max_depth: u32,
}

/// A rusty_xml element handle for CSS matching.
#[derive(Clone, Copy)]
struct XNode<'a> {
    doc: &'a XmlDoc,
    id: XId,
}

impl<'a> XNode<'a> {
    fn is_element(&self) -> bool {
        self.doc.kind(self.id) == NodeKind::Element
    }

    fn attr(&self, name: &str) -> Option<&'a str> {
        let mut a = self.doc.first_attr(self.id);
        while let Some(id) = a {
            if self.doc.prefix(id).is_none() && self.doc.name(id) == name {
                return Some(self.doc.content(id));
            }
            a = self.doc.next_sibling(id);
        }
        None
    }

    fn svg_tag(&self) -> Option<EId> {
        if !self.is_element() {
            return None;
        }
        match self.doc.ns_uri(self.id) {
            None | Some(SVG_NS) => {}
            Some(_) => return None,
        }
        // Prefixed elements are not SVG unless the prefix maps to SVG.
        if self.doc.prefix(self.id).is_some() && self.doc.ns_uri(self.id) != Some(SVG_NS) {
            return None;
        }
        EId::from_name(self.doc.name(self.id))
    }

    /// `href`, preferring the unprefixed form over `xlink:href`.
    fn href(&self) -> Option<&'a str> {
        let mut plain = None;
        let mut xlink = None;
        let mut a = self.doc.first_attr(self.id);
        while let Some(id) = a {
            if self.doc.name(id) == "href" {
                match self.doc.prefix(id) {
                    None => plain = Some(self.doc.content(id)),
                    Some(p) => {
                        let ns = self.doc.ns_uri(id);
                        if ns == Some(XLINK_NS) || (ns.is_none() && p == "xlink") {
                            xlink = Some(self.doc.content(id));
                        }
                    }
                }
            }
            a = self.doc.next_sibling(id);
        }
        plain.or(xlink)
    }
}

impl css::Element for XNode<'_> {
    fn local_name(&self) -> &str {
        self.doc.name(self.id)
    }

    fn attribute(&self, name: &str) -> Option<&str> {
        self.attr(name)
    }

    fn parent_element(&self) -> Option<Self> {
        let p = self.doc.parent(self.id)?;
        let n = XNode {
            doc: self.doc,
            id: p,
        };
        n.is_element().then_some(n)
    }

    fn prev_sibling_element(&self) -> Option<Self> {
        let mut cur = self.doc.prev_sibling(self.id);
        while let Some(id) = cur {
            if self.doc.kind(id) == NodeKind::Element {
                return Some(XNode { doc: self.doc, id });
            }
            cur = self.doc.prev_sibling(id);
        }
        None
    }
}

struct Builder<'a> {
    xml: &'a XmlDoc,
    /// `use` targets currently being expanded: a target already on this
    /// stack is a reference cycle and is skipped (usvg instead recurses until
    /// its depth limit and fails the whole document).
    use_chain: Vec<XId>,
    /// Shared work budget for `use` recursion checks (sum over all uses).
    use_check_steps: usize,
    /// Work budget for style-sheet matching (compound tests).
    css_budget: u64,
    /// Bytes of text so far.
    text_bytes: usize,
    sheet: Vec<css::Rule>,
    /// `sheet` bucketed by what each rule's subject needs (built once the
    /// whole sheet is parsed, for a sheet long enough to gain from it), and
    /// the scratch list of rules to test.
    rule_index: Option<css::RuleIndex>,
    rule_scratch: Vec<u32>,
    id_map: HashMap<&'a str, XId>,
    doc: Document<'a>,
    limits: Limits,
}

/// Parses XML text into an XML DOM with the safe rusty_xml defaults plus
/// internal-entity substitution (Illustrator files declare their namespace
/// URIs as entities).
pub(crate) fn parse_xml(data: &[u8]) -> Result<XmlDoc, Error> {
    let opts = rusty_xml::XML_PARSE_NOENT
        | rusty_xml::XML_PARSE_NONET
        | rusty_xml::XML_PARSE_NO_XXE
        | rusty_xml::XML_PARSE_NOERROR
        | rusty_xml::XML_PARSE_NOWARNING;
    rusty_xml::xml_read_memory(data, None, None, opts)
        .map_err(|e| Error::ParsingFailed(e.to_string()))
}

/// Builds the resolved SVG document from the XML DOM.
pub(crate) fn build(xml: &XmlDoc, limits: Limits) -> Result<Document<'_>, Error> {
    let root_el = xml.xml_doc_get_root_element().ok_or(Error::NotAnSvg)?;
    let mut b = Builder {
        xml,
        use_chain: Vec::new(),
        use_check_steps: 0,
        css_budget: CSS_MATCH_BUDGET,
        text_bytes: 0,
        sheet: Vec::new(),
        rule_index: None,
        rule_scratch: Vec::new(),
        id_map: HashMap::new(),
        doc: Document {
            nodes: vec![NodeData {
                parent: None,
                first_child: None,
                last_child: None,
                next_sibling: None,
                tag: None,
                attrs: 0..0,
                own: AttrSet::default(),
                chain: AttrSet::default(),
            }],
            attrs: Vec::new(),
            texts: Vec::new(),
            links: HashMap::new(),
        },
        limits,
    };

    // Style sheets and the ids, in one bounded iterative walk. The ids are
    // listed first and the map built at its final size, so it never rehashes.
    let mut stack = vec![root_el];
    let mut ids: Vec<(&str, XId)> = Vec::new();
    let mut visited = 0usize;
    while let Some(id) = stack.pop() {
        visited += 1;
        if visited > limits.max_nodes.saturating_mul(4) {
            return Err(Error::ElementsLimitReached);
        }
        let n = XNode { doc: xml, id };
        if !n.is_element() {
            continue;
        }
        if let Some(v) = n.attr("id") {
            ids.push((v, id));
        }
        if xml.name(id) == "style"
            && matches!(xml.ns_uri(id), None | Some(SVG_NS))
            && matches!(n.attr("type"), None | Some("text/css") | Some(""))
        {
            let text = xml.xml_node_get_content(id);
            css::parse_sheet(&text, &mut b.sheet);
        }
        // Push the children and reverse them where they lie: they pop in
        // document order.
        let first = stack.len();
        let mut c = xml.first_child(id);
        while let Some(k) = c {
            stack.push(k);
            c = xml.next_sibling(k);
        }
        stack[first..].reverse();
    }
    b.id_map.reserve(ids.len());
    for (v, id) in ids {
        // The first element with an id owns it.
        b.id_map.entry(v).or_insert(id);
    }

    let root = XNode {
        doc: xml,
        id: root_el,
    };
    if root.svg_tag() != Some(EId::Svg) {
        return Err(Error::NotAnSvg);
    }
    b.rule_index = css::RuleIndex::new(&b.sheet);
    b.parse_node(root_el, None, NodeId(0), false, 0)?;

    // No more ids than the XML had: sized once. An `id` only ever comes from
    // an XML attribute (it is not a presentation attribute, and `inherit`
    // copies the parent's), so every one is borrowed text.
    let mut links = HashMap::with_capacity(b.id_map.len());
    for (i, n) in b.doc.nodes.iter().enumerate() {
        if n.tag.is_none() || !n.own.contains(AId::Id) {
            continue;
        }
        if let Some(Value::Xml(id)) = b.doc.value(NodeId(i as u32), AId::Id) {
            links.entry(*id).or_insert(NodeId(i as u32));
        }
    }
    b.doc.links = links;
    fix_recursive_links(&mut b.doc, EId::ClipPath, AId::ClipPath);
    fix_recursive_links(&mut b.doc, EId::Mask, AId::Mask);
    fix_recursive_fe_images(&mut b.doc);
    fix_recursive_patterns(&mut b.doc, AId::Fill);
    fix_recursive_patterns(&mut b.doc, AId::Stroke);
    Ok(b.doc)
}

impl<'a> Builder<'a> {
    fn check_nodes(&self) -> Result<(), Error> {
        if self.doc.nodes.len() > self.limits.max_nodes {
            Err(Error::ElementsLimitReached)
        } else {
            Ok(())
        }
    }

    /// `origin` is the `use` element whose copy we are inside, if any.
    fn parse_node(
        &mut self,
        xid: XId,
        origin: Option<XId>,
        parent: NodeId,
        ignore_ids: bool,
        depth: u32,
    ) -> Result<(), Error> {
        if depth > self.limits.max_depth {
            return Err(Error::ElementsLimitReached);
        }
        let x = XNode {
            doc: self.xml,
            id: xid,
        };
        let Some(tag) = x.svg_tag() else {
            return Ok(());
        };
        let node_id = self.parse_element(x, parent, tag, ignore_ids)?;
        if tag == EId::Use {
            self.parse_use(x, origin, node_id, depth + 1)?;
        } else if tag == EId::Text {
            self.parse_text(xid, node_id, ignore_ids, depth)?;
        } else {
            let mut c = self.xml.first_child(xid);
            while let Some(k) = c {
                self.parse_node(k, origin, node_id, ignore_ids, depth + 1)?;
                c = self.xml.next_sibling(k);
            }
        }
        Ok(())
    }

    /// The content of a `text` element: its character data and its text
    /// content elements.
    #[inline(never)]
    fn parse_text(
        &mut self,
        xid: XId,
        node_id: NodeId,
        ignore_ids: bool,
        depth: u32,
    ) -> Result<(), Error> {
        let space = SvgNode {
            id: node_id,
            doc: &self.doc,
        }
        .ancestors()
        .find_map(|n| n.attribute(AId::Space))
            == Some("preserve");
        let first = self.doc.nodes.len();
        self.parse_text_content(xid, node_id, space, ignore_ids, true, depth)?;
        trim_text_nodes(&mut self.doc, node_id, first, space);
        Ok(())
    }

    fn parse_text_content(
        &mut self,
        xid: XId,
        parent: NodeId,
        space: bool,
        ignore_ids: bool,
        is_root: bool,
        depth: u32,
    ) -> Result<(), Error> {
        if depth > self.limits.max_depth {
            return Err(Error::ElementsLimitReached);
        }
        let mut child = self.xml.first_child(xid);
        while let Some(id) = child {
            child = self.xml.next_sibling(id);
            let is_text = |k: XId| matches!(self.xml.kind(k), NodeKind::Text | NodeKind::CData);
            if is_text(id) {
                // Adjacent character data is one text.
                let mut text = std::borrow::Cow::Borrowed(self.xml.content(id));
                while let Some(next) = child.filter(|k| is_text(*k)) {
                    text.to_mut().push_str(self.xml.content(next));
                    child = self.xml.next_sibling(next);
                }
                self.append_text(parent, &text, space)?;
                continue;
            }
            let x = XNode { doc: self.xml, id };
            let is_a = x.is_element() && self.xml.name(id) == "a";
            let tag = match x.svg_tag() {
                // Links are plain text.
                Some(EId::G) if is_a => EId::Tspan,
                Some(t @ (EId::Tspan | EId::Tref)) => t,
                // A text path is a direct child of `text`.
                Some(EId::TextPath) if is_root => EId::TextPath,
                _ => continue,
            };
            // A `tref` becomes a `tspan` with the text it references.
            let own = if tag == EId::TextPath {
                tag
            } else {
                EId::Tspan
            };
            let node = self.parse_element(x, parent, own, ignore_ids)?;
            let own_space = match (SvgNode {
                id: node,
                doc: &self.doc,
            })
            .attribute(AId::Space)
            {
                Some(v) => v == "preserve",
                None => space,
            };
            if tag == EId::Tref {
                if let Some(link) = self.resolve_href(x) {
                    let linked = XNode {
                        doc: self.xml,
                        id: link,
                    };
                    if linked.is_element() && linked.svg_tag().is_some() {
                        let room = self.text_room();
                        let text = character_data(self.xml, link, room);
                        self.append_text(node, &text, own_space)?;
                    }
                }
            } else {
                self.parse_text_content(id, node, own_space, ignore_ids, false, depth + 1)?;
            }
        }
        Ok(())
    }

    /// How many more bytes of text the document may have (`use` and `tref`
    /// copy text).
    fn text_room(&self) -> usize {
        MAX_TEXT_BYTES.saturating_sub(self.text_bytes)
    }

    #[inline(never)]
    fn append_text(&mut self, parent: NodeId, text: &str, space: bool) -> Result<(), Error> {
        self.check_nodes()?;
        if text.len() > self.text_room() {
            return Err(Error::ElementsLimitReached);
        }
        self.text_bytes += text.len();
        // White space becomes spaces, and one space unless it is preserved.
        let mut out = String::with_capacity(text.len());
        let mut prev = '0';
        for c in text.chars() {
            let c = match c {
                '\r' | '\n' | '\t' => ' ',
                c => c,
            };
            if !space && c == ' ' && prev == ' ' {
                continue;
            }
            prev = c;
            out.push(c);
        }
        self.doc.texts.push(out);
        let index = self.doc.texts.len() as u32;
        self.doc
            .append(parent, None, index..index, AttrSet::default());
        Ok(())
    }

    fn resolve_href(&self, x: XNode) -> Option<XId> {
        let v = x.href()?;
        let id = crate::values::iri(v)?;
        self.id_map.get(id).copied()
    }

    #[inline(never)]
    fn parse_use(
        &mut self,
        x: XNode,
        origin: Option<XId>,
        node_id: NodeId,
        depth: u32,
    ) -> Result<(), Error> {
        let Some(link) = self.resolve_href(x) else {
            return Ok(());
        };
        if link == x.id || Some(link) == origin || self.use_chain.contains(&link) {
            return Ok(());
        }
        let lx = XNode {
            doc: self.xml,
            id: link,
        };
        if lx.svg_tag().is_none() {
            return Ok(());
        }
        // A `use` inside the target pointing back at us or at the target.
        let mut stack = vec![link];
        while let Some(id) = stack.pop() {
            self.use_check_steps += 1;
            if self.use_check_steps > self.limits.max_nodes.saturating_mul(4) {
                return Err(Error::ElementsLimitReached);
            }
            let n = XNode { doc: self.xml, id };
            if id != link && n.svg_tag() == Some(EId::Use) {
                if let Some(l2) = self.resolve_href(n) {
                    if l2 == x.id || l2 == link {
                        return Ok(());
                    }
                }
            }
            let mut c = self.xml.first_child(id);
            while let Some(k) = c {
                if self.xml.kind(k) == NodeKind::Element {
                    stack.push(k);
                }
                c = self.xml.next_sibling(k);
            }
        }
        self.use_chain.push(link);
        let r = self.parse_node(link, Some(x.id), node_id, true, depth + 1);
        self.use_chain.pop();
        r
    }

    #[inline(never)]
    fn parse_element(
        &mut self,
        x: XNode<'a>,
        parent: NodeId,
        tag: EId,
        ignore_ids: bool,
    ) -> Result<NodeId, Error> {
        self.check_nodes()?;
        let start = self.doc.attrs.len();
        // The ids of the attributes appended so far.
        let mut own = AttrSet::default();

        // 1. XML attributes, in one pass: `href` (the unprefixed form
        // preferred over `xlink:href`, as `XNode::href` does) and `style`
        // are picked up on the way and applied after the others.
        let (mut plain_href, mut xlink_href, mut style) = (None, None, None);
        let mut a = self.xml.first_attr(x.id);
        while let Some(aid) = a {
            a = self.xml.next_sibling(aid);
            let name = self.xml.name(aid);
            if let Some(prefix) = self.xml.prefix(aid) {
                if name == "href" {
                    let ns = self.xml.ns_uri(aid);
                    if ns == Some(XLINK_NS) || (ns.is_none() && prefix == "xlink") {
                        xlink_href = Some(self.xml.content(aid));
                    }
                } else if name == "space" && prefix == "xml" {
                    append_attribute(
                        &mut self.doc,
                        &mut own,
                        parent,
                        start,
                        AId::Space,
                        Src::Xml(self.xml.content(aid)),
                        false,
                        false,
                    );
                }
                continue;
            }
            // One lookup of the name. An attribute nothing reads is not
            // kept (`class` is read from the XML by the style matcher).
            let Some(name) = AId::from_name(name) else {
                if style.is_none() && name == "style" {
                    style = Some(self.xml.content(aid));
                }
                continue;
            };
            match name {
                AId::Href => {
                    plain_href = Some(self.xml.content(aid));
                    continue;
                }
                AId::Id if ignore_ids => continue,
                // usvg honours these only from CSS.
                AId::MixBlendMode | AId::Isolation | AId::FontKerning => continue,
                _ => {}
            }
            let value = self.xml.content(aid);
            append_attribute(
                &mut self.doc,
                &mut own,
                parent,
                start,
                name,
                Src::Xml(value),
                false,
                false,
            );
        }
        if let Some(h) = plain_href.or(xlink_href) {
            append_attribute(
                &mut self.doc,
                &mut own,
                parent,
                start,
                AId::Href,
                Src::Xml(h),
                false,
                false,
            );
        }

        // 2. Style sheet rules (sorted by specificity), then 3. `style`: each
        // declaration is applied where it is found, in that order.
        if !self.sheet.is_empty() {
            let doc = &mut self.doc;
            css::for_each_match(
                &self.sheet,
                self.rule_index.as_ref(),
                &mut self.rule_scratch,
                x,
                &mut self.css_budget,
                |rule| {
                    for d in &rule.declarations {
                        let (name, value) = (&d.name, Src::Sheet(&d.value));
                        apply_declaration(doc, &mut own, parent, start, name, value, d.important);
                    }
                },
            );
        }
        if let Some(style) = style {
            let doc = &mut self.doc;
            css::for_each_declaration(style, |name, value, important| {
                let value = match &value {
                    std::borrow::Cow::Borrowed(v) => Src::Xml(v),
                    // Comments were stripped: a copy.
                    std::borrow::Cow::Owned(v) => Src::Sheet(v),
                };
                apply_declaration(doc, &mut own, parent, start, name, value, important);
            });
        }

        let end = self.doc.attrs.len();
        let id = self
            .doc
            .append(parent, Some(tag), start as u32..end as u32, own);
        Ok(id)
    }
}

/// Applies one style declaration (from a style sheet rule or a `style`
/// attribute) to the element being built, whose attributes start at `start`.
fn apply_declaration<'x>(
    doc: &mut Document<'x>,
    own: &mut AttrSet,
    parent: NodeId,
    start: usize,
    name: &str,
    value: Src<'x, '_>,
    important: bool,
) {
    if name == "marker" {
        for n in [AId::MarkerStart, AId::MarkerMid, AId::MarkerEnd] {
            append_attribute(doc, own, parent, start, n, value, important, true);
        }
    } else if name == "font" {
        apply_font(doc, own, parent, start, value, important);
    } else if let Some(name) = AId::from_name(name).filter(|n| n.is_presentation()) {
        append_attribute(doc, own, parent, start, name, value, important, true);
    }
}

/// The `font` shorthand: what it does not name is reset.
fn apply_font<'x>(
    doc: &mut Document<'x>,
    own: &mut AttrSet,
    parent: NodeId,
    start: usize,
    value: Src<'x, '_>,
    important: bool,
) {
    let Some(f) = crate::values::font_shorthand(value.as_str()) else {
        return;
    };
    // A part of the value, kept as the value itself is.
    let part = |p: &str| -> Src<'x, '_> {
        match value {
            Src::Xml(all) => {
                let at = p.as_ptr() as usize - all.as_ptr() as usize;
                Src::Xml(all.get(at..at + p.len()).unwrap_or(""))
            }
            Src::Sheet(all) => {
                let at = p.as_ptr() as usize - all.as_ptr() as usize;
                Src::Sheet(all.get(at..at + p.len()).unwrap_or(""))
            }
        }
    };
    let mut set = |name: AId, value: Src<'x, '_>| {
        append_attribute(doc, own, parent, start, name, value, important, true);
    };
    for (name, p) in [
        (AId::FontStyle, f.style),
        (AId::FontVariant, f.variant),
        (AId::FontWeight, f.weight),
        (AId::FontStretch, f.stretch),
    ] {
        set(name, p.map_or(Src::Xml("normal"), part));
    }
    set(AId::FontSizeAdjust, Src::Xml("none"));
    set(AId::FontKerning, Src::Xml("auto"));
    set(AId::FontSize, part(f.size));
    set(AId::FontFamily, part(f.family));
}

/// Appends (or, with `replace`, overrides) an attribute of the element
/// being built, whose attributes start at `start` and have the ids in `own`.
#[allow(clippy::too_many_arguments)]
fn append_attribute<'x>(
    doc: &mut Document<'x>,
    own: &mut AttrSet,
    parent: NodeId,
    start: usize,
    name: AId,
    value: Src<'x, '_>,
    important: bool,
    replace: bool,
) {
    let value = value.trimmed();
    let resolved: Option<(Value<'x>, bool)> =
        if value.as_str() == "inherit" && name.allows_inherit() {
            resolve_inherit(doc, parent, name)
        } else {
            Some((value.keep(), important))
        };
    let Some((value, important)) = resolved else {
        return;
    };
    if own.contains(name) {
        if let Some(existing) = doc.attrs[start..].iter_mut().find(|a| a.name == name) {
            if replace && !existing.important {
                existing.value = value;
                existing.important = important;
            }
        }
        return;
    }
    own.insert(name);
    doc.attrs.push(Attr {
        name,
        important,
        value,
    });
}

fn resolve_inherit<'x>(doc: &Document<'x>, parent: NodeId, name: AId) -> Option<(Value<'x>, bool)> {
    let p = SvgNode { id: parent, doc };
    let found = if name.is_inheritable() {
        p.ancestor_with_attribute(name)
    } else {
        Some(p).filter(|n| n.tag_name().is_some() && n.has_attribute(name))
    }
    .map(|n| n.id);
    if let Some(n) = found {
        return doc.value(n, name).map(|v| (v.clone(), false));
    }
    let def = match name {
        AId::ImageRendering | AId::ShapeRendering | AId::TextRendering => "auto",
        AId::ClipPath
        | AId::Filter
        | AId::MarkerEnd
        | AId::MarkerMid
        | AId::MarkerStart
        | AId::Mask
        | AId::Stroke
        | AId::StrokeDasharray
        | AId::TextDecoration => "none",
        AId::FontStretch
        | AId::FontStyle
        | AId::FontVariant
        | AId::FontWeight
        | AId::LetterSpacing
        | AId::WordSpacing => "normal",
        AId::Fill | AId::FloodColor | AId::StopColor => "black",
        AId::FillOpacity
        | AId::FloodOpacity
        | AId::Opacity
        | AId::StopOpacity
        | AId::StrokeOpacity => "1",
        AId::ClipRule | AId::FillRule => "nonzero",
        AId::ColorInterpolationFilters => "linearRGB",
        AId::Display => "inline",
        AId::Overflow => "visible",
        AId::StrokeDashoffset => "0",
        AId::StrokeLinecap => "butt",
        AId::StrokeLinejoin => "miter",
        AId::StrokeMiterlimit => "4",
        AId::StrokeWidth => "1",
        AId::Visibility => "visible",
        _ => return None,
    };
    Some((Value::Xml(def), false))
}

/// Breaks 1- and 2-step reference cycles through `attr` (`clip-path` on a
/// `clipPath`'s content pointing back at it), exactly as usvg does: the
/// offending attribute becomes `none`, and is `none` from then on (a cycle
/// of two is broken at one of its two ends, not at both). Longer cycles are
/// caught by the converter's recursion guard.
fn fix_recursive_links(doc: &mut Document, eid: EId, attr: AId) {
    let d = &*doc;
    let fixes = find_cycles(d, eid, |n| n.func_iri_attribute(attr));
    for id in fixes {
        doc.set_attribute_value(id, attr, "none");
    }
}

/// Pattern content painting with its own pattern: the paint becomes `none`.
fn fix_recursive_patterns(doc: &mut Document, attr: AId) {
    let d = &*doc;
    let fixes = find_cycles(d, EId::Pattern, |n| {
        match crate::values::paint(n.attribute(attr)?)? {
            crate::values::Paint::Url(id, _) => d.element_by_id(id),
            _ => None,
        }
    });
    for id in fixes {
        doc.set_attribute_value(id, attr, "none");
    }
}

/// The nodes whose reference (`link`) is to be dropped so that no element of
/// kind `eid` is reached from its own content in one or two steps.
fn find_cycles<'a>(
    doc: &'a Document<'a>,
    eid: EId,
    link: impl Fn(&SvgNode<'a>) -> Option<SvgNode<'a>>,
) -> std::collections::HashSet<NodeId> {
    let mut fixes = std::collections::HashSet::new();
    let mut steps = 0usize;
    for node in doc.root().descendants() {
        if node.tag_name() != Some(eid) {
            continue;
        }
        for child in node.descendants() {
            steps += 1;
            if steps > FIXUP_BUDGET {
                return fixes;
            }
            if fixes.contains(&child.id) {
                continue;
            }
            let Some(target) = link(&child) else {
                continue;
            };
            if target == node {
                fixes.insert(child.id);
                continue;
            }
            for n2 in target.descendants() {
                steps += 1;
                if steps > FIXUP_BUDGET {
                    return fixes;
                }
                if !fixes.contains(&n2.id) && link(&n2) == Some(node) {
                    fixes.insert(n2.id);
                }
            }
        }
    }
    fixes
}

/// An `feImage` referencing an element that is filtered by the same filter:
/// the element's `filter` becomes `none`, as in usvg. Longer cycles are
/// caught by the converter's recursion guard.
fn fix_recursive_fe_images(doc: &mut Document) {
    let mut fixes: Vec<NodeId> = Vec::new();
    {
        let d = &*doc;
        for fe in d
            .root()
            .descendants()
            .filter(|n| n.tag_name() == Some(EId::FeImage))
        {
            let Some(link) = fe.href() else {
                continue;
            };
            let (Some(value), Some(filter)) = (link.attribute(AId::Filter), fe.parent()) else {
                continue;
            };
            let (funcs, _) = crate::values::filter_functions(value);
            let recursive = funcs.iter().any(
                |f| matches!(f, crate::values::FilterFunc::Url(id) if *id == filter.element_id()),
            );
            if recursive {
                fixes.push(link.id);
            }
        }
    }
    for id in fixes {
        doc.set_attribute_value(id, AId::Filter, "none");
    }
}

/// The text of an element and everything in it, at most `max` bytes.
fn character_data(xml: &XmlDoc, root: XId, max: usize) -> String {
    let mut out = String::new();
    let mut stack = vec![root];
    while let Some(id) = stack.pop() {
        match xml.kind(id) {
            NodeKind::Text | NodeKind::CData => {
                out.push_str(xml.content(id));
                if out.len() > max {
                    break;
                }
            }
            NodeKind::Element => {
                let first = stack.len();
                let mut c = xml.first_child(id);
                while let Some(k) = c {
                    stack.push(k);
                    c = xml.next_sibling(k);
                }
                stack[first..].reverse();
            }
            _ => {}
        }
    }
    out
}

/// Trims the spaces at both ends of a `text` element, and those doubled where
/// its text nodes meet, the way Chrome does (a port of usvg's
/// `trim_text_nodes`; `space` is `xml:space="preserve"`).
fn trim_text_nodes(doc: &mut Document, text: NodeId, first: usize, space: bool) {
    // The text nodes, how deep each is and whether it keeps its spaces.
    let mut nodes: Vec<(usize, usize, bool)> = Vec::new();
    for i in first..doc.nodes.len() {
        let n = &doc.nodes[i];
        if n.tag.is_some() || n.attrs.start == 0 {
            continue;
        }
        let node = SvgNode {
            id: NodeId(i as u32),
            doc,
        };
        let mut depth = 0;
        let mut up = node.parent();
        while let Some(p) = up.filter(|p| p.id != text) {
            depth += 1;
            up = p.parent();
        }
        let own = node.parent().and_then(|p| p.attribute(AId::Space));
        let keeps = own.map_or(space, |v| v == "preserve");
        nodes.push((n.attrs.start as usize - 1, depth, keeps));
    }
    let texts = &mut doc.texts;
    let first_is = |t: &String| t.starts_with(' ');
    let last_is = |t: &String| t.ends_with(' ');
    fn drop_first(t: &mut String) {
        if t.starts_with(' ') {
            t.remove(0);
        }
    }
    fn drop_last(t: &mut String) {
        if t.ends_with(' ') {
            t.pop();
        }
    }
    if let [(only, _, _)] = nodes[..] {
        if !space {
            let t = &mut texts[only];
            if t.len() == 1 {
                drop_first(t);
            } else {
                drop_first(t);
                drop_last(t);
            }
        }
        return;
    }
    let pairs = nodes.len().saturating_sub(1);
    let mut last_non_empty: Option<(usize, bool)> = None;
    for i in 0..pairs {
        let (mut one, depth1, mut keeps1) = nodes[i];
        let (two, depth2, keeps2) = nodes[i + 1];
        if texts[one].is_empty() {
            if let Some((n, k)) = last_non_empty {
                one = n;
                keeps1 = k;
            }
        }
        let c1 = first_is(&texts[one]);
        let c2 = last_is(&texts[one]);
        let c3 = first_is(&texts[two]);
        let c4 = last_is(&texts[two]);
        if depth1 < depth2 {
            // `Text <tspan> text` is `Text <tspan>text`.
            if c3 && !keeps2 {
                drop_first(&mut texts[two]);
            }
        } else if c2 && c3 {
            if !keeps1 && !keeps2 {
                drop_last(&mut texts[one]);
            } else if keeps1 && !keeps2 {
                drop_first(&mut texts[two]);
            }
        }
        let is_first = i == 0;
        let is_last = i == pairs - 1;
        if is_first && c1 && !keeps1 && !texts[one].is_empty() {
            drop_first(&mut texts[one]);
        } else if is_last && c4 && !texts[two].is_empty() && !keeps2 {
            drop_last(&mut texts[two]);
        }
        if is_last && c2 && texts[two].is_empty() && last_is(&texts[one]) {
            drop_last(&mut texts[one]);
        }
        if !texts[one].trim().is_empty() {
            last_non_empty = Some((one, keeps1));
        }
    }
}
