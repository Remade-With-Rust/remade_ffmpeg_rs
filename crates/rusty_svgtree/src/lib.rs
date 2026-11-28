//! `rusty_svgtree`: a pure-Rust SVG parser and renderer.
//!
//! [`Tree::from_data`] parses SVG (via the house `rusty_xml` parser) into a
//! simplified render tree in the shape of usvg's: every style resolved, every
//! length in user units, `use` expanded, paint servers in user space.
//! [`render`] draws the tree with the house `rusty_raster` rasteriser, the way
//! resvg draws usvg trees with tiny-skia.
//!
//! ```
//! use rusty_raster::{Pixmap, Transform};
//!
//! let svg = br#"<svg xmlns="http://www.w3.org/2000/svg" width="20" height="20">
//!     <circle cx="10" cy="10" r="8" fill="tomato"/></svg>"#;
//! let tree = rusty_svgtree::Tree::from_data(svg, &rusty_svgtree::Options::default()).unwrap();
//! let size = tree.size().to_int_size();
//! let mut pixmap = Pixmap::new(size.width(), size.height()).unwrap();
//! rusty_svgtree::render(&tree, Transform::identity(), &mut pixmap.as_mut());
//! assert_ne!(pixmap.data()[(10 * 20 + 10) * 4 + 3], 0);
//! ```
//!
//! Hostile input never panics and never allocates without bound: element
//! nesting depth, total node count (which bounds `use` amplification),
//! reference cycles, image pixel counts and the bytes held by render layers
//! are all limited (see [`Options`]).

#![forbid(unsafe_code)]

mod colors;
#[cfg(feature = "fonts")]
mod colr;
mod convert;
mod css;
pub mod filter;
mod filter_convert;
mod filter_kernels;
mod filter_render;
#[cfg(feature = "fonts")]
mod fonts;
mod image;
mod marker;
mod render;
mod svgtree;
pub mod text;
mod text_convert;
#[cfg(feature = "text")]
mod text_layout;
mod tree;
mod values;
mod writer;

#[cfg(feature = "fonts")]
pub use fonts::Fonts;
pub use rusty_raster;
pub use tree::{
    BaseGradient, ClipPath, Color, Fill, Group, Image, ImageKind, LinearGradient, Mask, Node,
    Paint, PaintOrder, Path, Pattern, RadialGradient, Stop, Stroke,
};

pub use image::{ImageLoad, ImageLoader, ImageRequest, ImageSource};
pub use writer::WriteOptions;

use rusty_raster::{PixmapMut, Size, Transform};

/// Parse errors.
#[derive(Clone, Debug, PartialEq, Eq)]
#[non_exhaustive]
pub enum Error {
    /// The XML is malformed (the message comes from `rusty_xml`).
    ParsingFailed(String),
    /// The root element is not an SVG `<svg>`.
    NotAnSvg,
    /// `width`/`height`/`viewBox` do not give a finite, positive size.
    InvalidSize,
    /// The data is compressed, and broken, larger than
    /// [`Options::max_svgz_bytes`], or the `svgz` feature is off.
    MalformedGzip,
    /// The document exceeds [`Options::max_nodes`] or
    /// [`Options::max_depth`] (after `use` expansion).
    ElementsLimitReached,
    /// The [`Options::image_loader`] answered [`ImageLoad::Fail`] for an
    /// image of the document (or of a document embedded in it).
    ImageRefused,
}

impl std::fmt::Display for Error {
    fn fmt(&self, f: &mut std::fmt::Formatter<'_>) -> std::fmt::Result {
        match self {
            Error::ParsingFailed(m) => write!(f, "XML parsing failed: {m}"),
            Error::NotAnSvg => f.write_str("the root element is not <svg>"),
            Error::InvalidSize => f.write_str("the SVG has an invalid size"),
            Error::MalformedGzip => f.write_str("the compressed data is not valid gzip"),
            Error::ElementsLimitReached => f.write_str("the SVG exceeds the element limits"),
            Error::ImageRefused => f.write_str("an image of the SVG was refused"),
        }
    }
}

impl std::error::Error for Error {}

/// Parse and render options.
#[derive(Clone, Debug)]
pub struct Options {
    /// Dots per inch for absolute units (`in`, `cm`, `mm`, `pt`, `pc`).
    pub dpi: f32,
    /// The default `font-size`, for `em`/`ex` units.
    pub font_size: f32,
    /// The default `font-family`.
    pub font_family: String,
    /// Where the fonts of text come from; with `None`, text is in the tree
    /// but draws nothing. Documents cannot load fonts.
    #[cfg(feature = "text")]
    pub fonts: Option<std::sync::Arc<dyn text::FontProvider>>,
    /// The size used when `width`/`height` are percentages without a
    /// `viewBox`.
    pub default_size: Size,
    /// Loads `<image>` and `feImage` references: linked files, URLs, and
    /// `data:` URIs under the application's rules (see [`ImageLoader`]).
    /// With `None`, only `data:` URIs are loaded.
    pub image_loader: Option<std::sync::Arc<dyn ImageLoader>>,
    /// Languages for `systemLanguage` matching.
    pub languages: Vec<String>,
    /// Maximum elements after `use` expansion. Bounds `use` amplification
    /// ("billion laughs" through references).
    pub max_nodes: usize,
    /// Maximum element nesting depth, counting `use` indirections (deeper is
    /// an error). Nesting through references (a mask in a mask) is held to
    /// the same depth: what is deeper is dropped. The default fits a 1 MiB
    /// stack in a release build; raise it only on a larger one.
    pub max_depth: u32,
    /// Largest embedded image decoded, in pixels.
    pub max_image_pixels: u64,
    /// Largest document `width x height` accepted, in pixels; by default any
    /// (a parsed document allocates nothing for its size, and `render` draws
    /// at the size of the caller's pixmap). Set it to reject documents too
    /// large for a caller that renders them at their own size.
    pub max_canvas_pixels: u64,
    /// Largest decompressed size of an svgz document, in bytes.
    pub max_svgz_bytes: u64,
    /// Cap on bytes held at once by group layers, clip, mask and filter
    /// scratch buffers while rendering. A group that would exceed it is
    /// skipped.
    pub max_layer_bytes: u64,
    /// Cap on filter work per `render` call, in pixel operations (a blur
    /// costs about 10 per pixel, a convolution its kernel's size, a
    /// turbulence 16 per octave; every primitive costs at least a few
    /// thousand). A filter past it leaves its element undrawn.
    pub max_filter_work: u64,
}

impl Default for Options {
    fn default() -> Self {
        Options {
            dpi: 96.0,
            font_size: 12.0,
            font_family: "Times New Roman".to_string(),
            #[cfg(feature = "text")]
            fonts: None,
            default_size: Size::from_wh(100.0, 100.0).unwrap_or_else(|| unreachable!()),
            image_loader: None,
            languages: vec!["en".to_string()],
            max_nodes: 500_000,
            max_depth: 1024,
            max_image_pixels: 64 * 1024 * 1024,
            max_canvas_pixels: u64::MAX,
            max_svgz_bytes: 1 << 28,
            max_layer_bytes: 1 << 30,
            max_filter_work: 1 << 33,
        }
    }
}

/// What was left out of a tree, so that a caller can say so or fall back.
#[derive(Clone, Copy, PartialEq, Eq, Debug, Default)]
#[non_exhaustive]
pub struct Skipped {
    /// `<text>` elements that draw nothing: there are no fonts, or a limit
    /// was reached. (They are in the tree.)
    pub text: usize,
    /// `<image>` and `feImage` references that were not loaded: not a
    /// `data:` URI, not a supported format, broken, or past a limit.
    pub images: usize,
    /// Elements dropped because a conversion limit was reached.
    pub over_limit: usize,
}

impl Skipped {
    /// Whether nothing was left out.
    pub fn is_empty(&self) -> bool {
        *self == Skipped::default()
    }
}

/// A parsed SVG document.
#[derive(Clone, Debug)]
pub struct Tree {
    size: Size,
    root: Group,
    skipped: Skipped,
    /// The elements of the document, `use` expanded.
    node_count: usize,
    max_layer_bytes: u64,
    max_image_pixels: u64,
    max_filter_work: u64,
    /// The provider the text was laid out with.
    #[cfg(feature = "text")]
    fonts: Option<std::sync::Arc<dyn text::FontProvider>>,
}

impl Tree {
    /// Parses SVG bytes (UTF-8 or any encoding `rusty_xml` detects), or
    /// gzip-compressed ones (svgz).
    pub fn from_data(data: &[u8], opt: &Options) -> Result<Tree, Error> {
        let mut limits = image::Nested {
            depth: 0,
            nodes_left: opt.max_nodes,
            failed: false,
        };
        Tree::parse(data, opt, &mut limits)
    }

    /// `limits` is what is left of the limits shared with the documents
    /// around this one; what it uses is taken from them.
    pub(crate) fn parse(
        data: &[u8],
        opt: &Options,
        limits: &mut image::Nested,
    ) -> Result<Tree, Error> {
        if data.starts_with(&[0x1f, 0x8b]) {
            #[cfg(feature = "svgz")]
            {
                let data = image::gunzip(data, opt.max_svgz_bytes).ok_or(Error::MalformedGzip)?;
                return Tree::parse_xml(&data, opt, limits);
            }
            #[cfg(not(feature = "svgz"))]
            return Err(Error::MalformedGzip);
        }
        Tree::parse_xml(data, opt, limits)
    }

    fn parse_xml(data: &[u8], opt: &Options, limits: &mut image::Nested) -> Result<Tree, Error> {
        let xml = svgtree::parse_xml(data)?;
        // The document borrows its attribute values from the XML DOM, which
        // therefore lives until the conversion is done.
        let doc = svgtree::build(
            &xml,
            svgtree::Limits {
                max_nodes: limits.nodes_left,
                max_depth: opt.max_depth,
            },
        )?;
        let node_count = doc.len();
        limits.nodes_left = limits.nodes_left.saturating_sub(node_count);
        let converted = convert::convert_doc(&doc, opt, limits)?;
        if limits.failed {
            return Err(Error::ImageRefused);
        }
        drop(doc);
        drop(xml);
        let s = converted.size;
        if f64::from(s.width()) * f64::from(s.height()) > opt.max_canvas_pixels as f64 {
            return Err(Error::InvalidSize);
        }
        Ok(Tree {
            size: s,
            root: converted.root,
            skipped: converted.skipped,
            node_count,
            max_layer_bytes: opt.max_layer_bytes,
            max_image_pixels: opt.max_image_pixels,
            max_filter_work: opt.max_filter_work,
            #[cfg(feature = "text")]
            fonts: opt.fonts.clone(),
        })
    }

    /// Parses an SVG string.
    #[allow(clippy::should_implement_trait)]
    pub fn from_str(text: &str, opt: &Options) -> Result<Tree, Error> {
        Tree::from_data(text.as_bytes(), opt)
    }

    /// The canvas size in user units (after `viewBox` resolution).
    pub fn size(&self) -> Size {
        self.size
    }

    /// What the document has that the tree does not.
    pub fn skipped(&self) -> Skipped {
        self.skipped
    }

    /// The tree as SVG, with every style resolved. It parses to a tree that
    /// draws the same (a filter function becomes a `<filter>`, which has a
    /// region).
    pub fn to_svg_string(&self, opt: &WriteOptions) -> String {
        writer::write(self, opt)
    }

    /// The first node with an id, in paint order (also inside groups, not in
    /// clip paths, masks or patterns).
    pub fn node_by_id(&self, id: &str) -> Option<&Node> {
        if id.is_empty() {
            return None;
        }
        let mut stack: Vec<&Node> = self.root.children.iter().rev().collect();
        while let Some(n) = stack.pop() {
            if n.id() == id {
                return Some(n);
            }
            if let Node::Group(g) = n {
                stack.extend(g.children.iter().rev());
            }
        }
        None
    }

    /// The root group.
    pub fn root(&self) -> &Group {
        &self.root
    }

    /// The font provider the tree's text was laid out with
    /// ([`Options::fonts`]): the one the [`FontId`](text::FontId)s of
    /// [`Text::glyph_runs`](text::Text::glyph_runs) belong to.
    #[cfg(feature = "text")]
    pub fn fonts(&self) -> Option<&std::sync::Arc<dyn text::FontProvider>> {
        self.fonts.as_ref()
    }
}

/// Renders `tree` onto `pixmap` with `transform` applied to the canvas.
pub fn render(tree: &Tree, transform: Transform, pixmap: &mut PixmapMut) {
    let Some(mut ctx) = render::Context::new(
        pixmap.width(),
        pixmap.height(),
        tree.max_layer_bytes,
        tree.max_image_pixels,
        tree.max_filter_work,
    ) else {
        return;
    };
    render::render_nodes(&tree.root.children, &mut ctx, transform, pixmap);
}

/// Renders one node of `tree` (or of a tree inside it: a clip path's, a
/// mask's, a pattern's or an embedded document's) onto `pixmap`, with the
/// limits `tree` was parsed with. As `resvg::render_node`: the node's own
/// transform, opacity, blend mode, clip path, mask and filters apply, its
/// parents' do not, and `transform` maps the parent's coordinates to the
/// pixmap.
///
/// To rasterise a filtered group at scale `s` (a PDF writer drawing what it
/// cannot express): take its [`Group::layer_bounding_box`] through its
/// [`Group::transform`], size a pixmap to that box times `s`, and render
/// with `Transform::from_scale(s, s).pre_translate(-box.x(), -box.y())`.
pub fn render_node(tree: &Tree, node: &Node, transform: Transform, pixmap: &mut PixmapMut) {
    let Some(mut ctx) = render::Context::new(
        pixmap.width(),
        pixmap.height(),
        tree.max_layer_bytes,
        tree.max_image_pixels,
        tree.max_filter_work,
    ) else {
        return;
    };
    render::render_nodes(std::slice::from_ref(node), &mut ctx, transform, pixmap);
}
