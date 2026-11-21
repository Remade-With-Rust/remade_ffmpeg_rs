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
mod convert;
mod css;
mod image;
mod marker;
mod render;
mod svgtree;
mod tree;
mod values;

pub use rusty_raster;
pub use tree::{
    BaseGradient, ClipPath, Color, Fill, Group, Image, ImageKind, LinearGradient, Mask, Node,
    Paint, PaintOrder, Path, Pattern, RadialGradient, Stop, Stroke,
};

use rusty_raster::{PixmapMut, Size, Transform};

/// Parse errors.
#[derive(Clone, Debug, PartialEq, Eq)]
pub enum Error {
    /// The XML is malformed (the message comes from `rusty_xml`).
    ParsingFailed(String),
    /// The root element is not an SVG `<svg>`.
    NotAnSvg,
    /// `width`/`height`/`viewBox` do not give a finite, positive size.
    InvalidSize,
    /// The document exceeds [`Options::max_nodes`] or
    /// [`Options::max_depth`] (after `use` expansion).
    ElementsLimitReached,
}

impl std::fmt::Display for Error {
    fn fmt(&self, f: &mut std::fmt::Formatter<'_>) -> std::fmt::Result {
        match self {
            Error::ParsingFailed(m) => write!(f, "XML parsing failed: {m}"),
            Error::NotAnSvg => f.write_str("the root element is not <svg>"),
            Error::InvalidSize => f.write_str("the SVG has an invalid size"),
            Error::ElementsLimitReached => f.write_str("the SVG exceeds the element limits"),
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
    /// The size used when `width`/`height` are percentages without a
    /// `viewBox`.
    pub default_size: Size,
    /// Languages for `systemLanguage` matching.
    pub languages: Vec<String>,
    /// Maximum elements after `use` expansion. Bounds `use` amplification
    /// ("billion laughs" through references).
    pub max_nodes: usize,
    /// Maximum element nesting depth, counting `use` indirections.
    pub max_depth: u32,
    /// Largest embedded image decoded, in pixels.
    pub max_image_pixels: u64,
    /// Largest canvas `width x height` accepted, in pixels (`Tree::size` is
    /// checked against it; giant `width`/`height` attributes are an error,
    /// not an allocation).
    pub max_canvas_pixels: u64,
    /// Cap on bytes held at once by group layers, clip and mask scratch
    /// buffers while rendering. A group that would exceed it is skipped.
    pub max_layer_bytes: u64,
}

impl Default for Options {
    fn default() -> Self {
        Options {
            dpi: 96.0,
            font_size: 12.0,
            default_size: Size::from_wh(100.0, 100.0).unwrap_or_else(|| unreachable!()),
            languages: vec!["en".to_string()],
            max_nodes: 500_000,
            max_depth: 256,
            max_image_pixels: 64 * 1024 * 1024,
            max_canvas_pixels: 1 << 28,
            max_layer_bytes: 1 << 30,
        }
    }
}

/// A parsed SVG document.
#[derive(Clone, Debug)]
pub struct Tree {
    size: Size,
    root: Group,
    max_layer_bytes: u64,
    max_image_pixels: u64,
}

impl Tree {
    /// Parses SVG bytes (UTF-8 or any encoding `rusty_xml` detects).
    pub fn from_data(data: &[u8], opt: &Options) -> Result<Tree, Error> {
        let xml = svgtree::parse_xml(data)?;
        // The document borrows its attribute values from the XML DOM, which
        // therefore lives until the conversion is done.
        let doc = svgtree::build(
            &xml,
            svgtree::Limits {
                max_nodes: opt.max_nodes,
                max_depth: opt.max_depth,
            },
        )?;
        let converted = convert::convert_doc(&doc, opt)?;
        drop(doc);
        drop(xml);
        let s = converted.size;
        if f64::from(s.width()) * f64::from(s.height()) > opt.max_canvas_pixels as f64 {
            return Err(Error::InvalidSize);
        }
        Ok(Tree {
            size: s,
            root: converted.root,
            max_layer_bytes: opt.max_layer_bytes,
            max_image_pixels: opt.max_image_pixels,
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

    /// The root group.
    pub fn root(&self) -> &Group {
        &self.root
    }
}

/// Renders `tree` onto `pixmap` with `transform` applied to the canvas.
pub fn render(tree: &Tree, transform: Transform, pixmap: &mut PixmapMut) {
    let Some(mut ctx) = render::Context::new(
        pixmap.width(),
        pixmap.height(),
        tree.max_layer_bytes,
        tree.max_image_pixels,
    ) else {
        return;
    };
    render::render_nodes(&tree.root, &mut ctx, transform, pixmap);
}
