//! Text nodes of the render tree.
//!
//! A [`Text`] keeps what the document says (chunks, spans, fonts, positions),
//! whether or not it can be drawn. With the `text` feature and
//! [`Options::fonts`](crate::Options) set it is also laid out, and
//! [`Text::flattened`] holds the paths that are drawn. The types mirror
//! usvg's (see `NOTICE.md`).

use std::sync::Arc;

use rusty_raster::{Rect, Transform};

use crate::tree::{Fill, Group, PaintOrder, Stroke};

/// A `<text>` element.
#[derive(Clone, Debug)]
#[non_exhaustive]
pub struct Text {
    /// The element id.
    pub id: String,
    /// The text, cut wherever an absolute position or a text path starts.
    pub chunks: Vec<TextChunk>,
    /// Relative x offsets, one per character of all chunks.
    pub dx: Vec<f32>,
    /// Relative y offsets, one per character.
    pub dy: Vec<f32>,
    /// Glyph rotations in degrees, one per character.
    pub rotate: Vec<f32>,
    /// `writing-mode`.
    pub writing_mode: WritingMode,
    /// `text-rendering` resolved to an anti-aliasing switch.
    pub anti_alias: bool,
    /// The transform relative to the root (see
    /// [`Group::abs_transform`]).
    pub abs_transform: Transform,
    pub(crate) bounding_box: Option<Rect>,
    pub(crate) stroke_bounding_box: Option<Rect>,
    pub(crate) flattened: Option<Box<Group>>,
    #[cfg(feature = "text")]
    pub(crate) glyph_runs: Option<Vec<GlyphRun>>,
}

impl Text {
    /// The box of the text's font metrics; `None` when it was not laid out.
    pub fn bounding_box(&self) -> Option<Rect> {
        self.bounding_box
    }

    /// The box of the drawn paths with their strokes; `None` when the text
    /// was not laid out.
    pub fn stroke_bounding_box(&self) -> Option<Rect> {
        self.stroke_bounding_box
    }

    /// The text as paths (and images, for colour glyphs); `None` when it was
    /// not laid out: no fonts, or the `text` feature is off.
    pub fn flattened(&self) -> Option<&Group> {
        self.flattened.as_deref()
    }

    /// The shaped glyphs, one run per laid-out span, in paint order; `None`
    /// when the text was not laid out. They draw what [`Text::flattened`]
    /// draws, for a writer that keeps text as text (a PDF): glyph ids of the
    /// [`FontProvider`] the tree was parsed with ([`Tree::fonts`]), where
    /// they are, and the text they stand for.
    ///
    /// [`Tree::fonts`]: crate::Tree::fonts
    #[cfg(feature = "text")]
    pub fn glyph_runs(&self) -> Option<&[GlyphRun]> {
        self.glyph_runs.as_deref()
    }
}

/// The glyphs of one span of a laid-out text (see [`Text::glyph_runs`]).
///
/// Its style is the span's, `Text::chunks[chunk].spans[span]`: font size,
/// fill and stroke (resolved to user space; a glyph fill always uses the
/// nonzero rule, whatever the span's `fill-rule`), paint order and
/// visibility. A run is drawn in this order: overline, underline, the
/// glyphs, line-through.
#[cfg(feature = "text")]
#[derive(Clone, Debug)]
#[non_exhaustive]
pub struct GlyphRun {
    /// The chunk, an index into [`Text::chunks`].
    pub chunk: usize,
    /// The span, an index into the chunk's [`TextChunk::spans`].
    pub span: usize,
    /// The axis values the glyphs were shaped and drawn with: the span's
    /// `font-variation-settings`, plus `opsz` set to `optical_size` when that
    /// is `Some`, the font has that axis and the list does not set it (the
    /// rule of [`ShapeRequest::optical_sizing`]).
    pub variations: Vec<FontVariation>,
    /// The font size, when `font-optical-sizing` is `auto`.
    pub optical_size: Option<f32>,
    /// The glyphs, in visual order. Fallback fonts make one run hold glyphs
    /// of several fonts.
    pub glyphs: Vec<PositionedGlyph>,
    /// The span's overline, a filled or stroked path in the text's
    /// coordinates.
    pub overline: Option<crate::tree::Path>,
    /// The span's underline.
    pub underline: Option<crate::tree::Path>,
    /// The span's line-through.
    pub line_through: Option<crate::tree::Path>,
}

/// A glyph of a [`GlyphRun`], placed.
#[cfg(feature = "text")]
#[derive(Clone, Debug)]
#[non_exhaustive]
pub struct PositionedGlyph {
    /// The font, of the provider the text was laid out with.
    pub font: FontId,
    /// The glyph in it (0 for a missing glyph, which draws `.notdef`).
    pub id: u32,
    /// The font's units per em.
    pub units_per_em: u16,
    /// The font size.
    pub font_size: f32,
    /// The bytes of the cluster the glyph belongs to, in the chunk's
    /// [`TextChunk::text`] (for a PDF's `ToUnicode` / `ActualText`).
    /// Glyphs of one cluster share it.
    pub text: std::ops::Range<usize>,
    /// From font units (y up, the glyph origin at `0,0`) to the text's
    /// coordinates, the coordinates of [`Text::flattened`]. Size, position,
    /// rotation, text-on-path and vertical writing are all in it.
    pub transform: Transform,
}

/// A run of text with one anchor.
#[derive(Clone, Debug)]
#[non_exhaustive]
pub struct TextChunk {
    /// Absolute x position; `None` continues after the previous chunk.
    pub x: Option<f32>,
    /// Absolute y position.
    pub y: Option<f32>,
    /// `text-anchor`.
    pub anchor: TextAnchor,
    /// The spans, in order; they cover the whole text.
    pub spans: Vec<TextSpan>,
    /// Along a line or a path.
    pub text_flow: TextFlow,
    /// The characters, white space resolved.
    pub text: String,
}

/// A run of text with one style.
#[derive(Clone, Debug)]
#[non_exhaustive]
pub struct TextSpan {
    /// First byte in the chunk's text.
    pub start: usize,
    /// One past the last byte.
    pub end: usize,
    /// The fill.
    pub fill: Option<Fill>,
    /// The stroke.
    pub stroke: Option<Stroke>,
    /// Fill then stroke, or stroke then fill.
    pub paint_order: PaintOrder,
    /// The font asked for.
    pub font: Font,
    /// `font-size`, positive.
    pub font_size: f32,
    /// `font-variant: small-caps`.
    pub small_caps: bool,
    /// Whether kerning applies.
    pub apply_kerning: bool,
    /// `font-optical-sizing: auto`.
    pub optical_sizing: bool,
    /// `text-decoration`.
    pub decoration: TextDecoration,
    /// `dominant-baseline`.
    pub dominant_baseline: Baseline,
    /// `alignment-baseline`.
    pub alignment_baseline: Baseline,
    /// `baseline-shift`, of this element and its ancestors, innermost first.
    pub baseline_shift: Vec<BaselineShift>,
    /// `visibility`.
    pub visible: bool,
    /// `letter-spacing`.
    pub letter_spacing: f32,
    /// `word-spacing`.
    pub word_spacing: f32,
    /// `textLength`, not negative.
    pub text_length: Option<f32>,
    /// `lengthAdjust="spacingAndGlyphs"`.
    pub adjust_glyphs: bool,
}

/// A font request.
#[derive(Clone, PartialEq, Eq, Hash, Debug)]
#[non_exhaustive]
pub struct Font {
    /// Families, in order of preference; never empty.
    pub families: Vec<FontFamily>,
    /// `font-style`.
    pub style: FontStyle,
    /// `font-stretch`.
    pub stretch: FontStretch,
    /// `font-weight`, 100 to 900.
    pub weight: u16,
    /// `font-variation-settings`, with the axes the other properties imply.
    pub variations: Vec<FontVariation>,
}

/// A font family name.
#[derive(Clone, PartialEq, Eq, Hash, Debug)]
pub enum FontFamily {
    /// `serif`.
    Serif,
    /// `sans-serif`.
    SansSerif,
    /// `cursive`.
    Cursive,
    /// `fantasy`.
    Fantasy,
    /// `monospace`.
    Monospace,
    /// A named family.
    Named(String),
}

impl std::fmt::Display for FontFamily {
    fn fmt(&self, f: &mut std::fmt::Formatter<'_>) -> std::fmt::Result {
        f.write_str(match self {
            FontFamily::Serif => "serif",
            FontFamily::SansSerif => "sans-serif",
            FontFamily::Cursive => "cursive",
            FontFamily::Fantasy => "fantasy",
            FontFamily::Monospace => "monospace",
            FontFamily::Named(s) => {
                // A name that holds one kind of quote takes the other.
                let q = if s.contains('\'') { '"' } else { '\'' };
                return write!(f, "{q}{s}{q}");
            }
        })
    }
}

/// `font-style`.
#[derive(Clone, Copy, PartialEq, Eq, Hash, Debug, Default)]
pub enum FontStyle {
    /// `normal`.
    #[default]
    Normal,
    /// `italic`.
    Italic,
    /// `oblique`.
    Oblique,
}

/// `font-stretch`.
#[derive(Clone, Copy, PartialEq, Eq, PartialOrd, Ord, Hash, Debug, Default)]
#[allow(missing_docs)]
pub enum FontStretch {
    UltraCondensed,
    ExtraCondensed,
    Condensed,
    SemiCondensed,
    #[default]
    Normal,
    SemiExpanded,
    Expanded,
    ExtraExpanded,
    UltraExpanded,
}

/// A variable font axis and the value it is set to.
#[derive(Clone, Copy, Debug)]
pub struct FontVariation {
    /// The axis tag.
    pub tag: [u8; 4],
    /// The value.
    pub value: f32,
}

impl PartialEq for FontVariation {
    fn eq(&self, other: &Self) -> bool {
        self.tag == other.tag && self.value.to_bits() == other.value.to_bits()
    }
}

impl Eq for FontVariation {}

impl std::hash::Hash for FontVariation {
    fn hash<H: std::hash::Hasher>(&self, state: &mut H) {
        self.tag.hash(state);
        self.value.to_bits().hash(state);
    }
}

/// The lines drawn with a span.
#[derive(Clone, Debug, Default)]
#[non_exhaustive]
pub struct TextDecoration {
    /// `underline`.
    pub underline: Option<TextDecorationStyle>,
    /// `overline`.
    pub overline: Option<TextDecorationStyle>,
    /// `line-through`.
    pub line_through: Option<TextDecorationStyle>,
}

/// How a decoration is painted.
#[derive(Clone, Debug)]
#[non_exhaustive]
pub struct TextDecorationStyle {
    /// The fill.
    pub fill: Option<Fill>,
    /// The stroke.
    pub stroke: Option<Stroke>,
}

/// `dominant-baseline` and `alignment-baseline` (values no renderer tells
/// apart from `auto` are `Auto`).
#[derive(Clone, Copy, PartialEq, Eq, Debug, Default)]
#[allow(missing_docs)]
pub enum Baseline {
    #[default]
    Auto,
    Baseline,
    BeforeEdge,
    Middle,
    Central,
    AfterEdge,
    Ideographic,
    Alphabetic,
    Hanging,
    Mathematical,
}

/// `baseline-shift`.
#[derive(Clone, Copy, PartialEq, Debug)]
pub enum BaselineShift {
    /// `baseline`.
    Baseline,
    /// `sub`.
    Subscript,
    /// `super`.
    Superscript,
    /// A length, in user units.
    Number(f32),
}

/// `text-anchor`.
#[derive(Clone, Copy, PartialEq, Eq, Debug, Default)]
pub enum TextAnchor {
    /// `start`.
    #[default]
    Start,
    /// `middle`.
    Middle,
    /// `end`.
    End,
}

/// `writing-mode`.
#[derive(Clone, Copy, PartialEq, Eq, Debug, Default)]
pub enum WritingMode {
    /// Horizontal.
    #[default]
    LeftToRight,
    /// Vertical.
    TopToBottom,
}

/// What a chunk is set along.
#[derive(Clone, Debug)]
pub enum TextFlow {
    /// A straight line.
    Linear,
    /// A path (`textPath`).
    Path(Arc<TextPath>),
}

/// A `textPath`.
#[derive(Clone, Debug)]
#[non_exhaustive]
pub struct TextPath {
    /// The id of the path element.
    pub id: String,
    /// `startOffset`, in user units.
    pub start_offset: f32,
    /// The path.
    pub path: Arc<rusty_raster::Path>,
}

/// A font a [`FontProvider`] knows.
#[cfg(feature = "text")]
#[derive(Clone, Copy, PartialEq, Eq, Hash, Debug)]
pub struct FontId(pub u64);

/// Font metrics, in font units.
#[cfg(feature = "text")]
#[derive(Clone, Copy, PartialEq, Debug)]
#[allow(missing_docs)]
pub struct FontMetrics {
    /// Units per em, not zero.
    pub units_per_em: u16,
    pub ascent: i16,
    /// Negative below the baseline.
    pub descent: i16,
    pub x_height: u16,
    /// The middle of the underline, negative below the baseline.
    pub underline_position: i16,
    pub underline_thickness: u16,
    pub line_through_position: i16,
    pub subscript_offset: i16,
    pub superscript_offset: i16,
}

/// A run of one direction to shape.
#[cfg(feature = "text")]
#[derive(Clone, Copy, Debug)]
#[non_exhaustive]
pub struct ShapeRequest<'a> {
    /// The characters.
    pub text: &'a str,
    /// Right to left.
    pub rtl: bool,
    /// Use the `smcp` feature.
    pub small_caps: bool,
    /// Use kerning.
    pub kerning: bool,
    /// The font size.
    pub font_size: f32,
    /// Set the `opsz` axis to the font size if the font has one that
    /// `variations` does not set.
    pub optical_sizing: bool,
    /// Axis values.
    pub variations: &'a [FontVariation],
}

/// A shaped glyph, in visual order; distances in font units.
#[cfg(feature = "text")]
#[derive(Clone, Copy, PartialEq, Eq, Debug)]
pub struct ShapedGlyph {
    /// The glyph, 0 for a missing one.
    pub glyph: u32,
    /// The first byte of the cluster in [`ShapeRequest::text`].
    pub cluster: usize,
    /// Offset of the glyph.
    pub dx: i32,
    /// Offset of the glyph, positive up.
    pub dy: i32,
    /// Advance.
    pub advance: i32,
}

/// What a glyph draws.
#[cfg(feature = "text")]
#[derive(Clone, Debug)]
#[non_exhaustive]
pub enum GlyphData {
    /// An outline in font units, y up, painted with the span's paints.
    Outline(rusty_raster::Path),
    /// An SVG document, one user unit to the font unit.
    Svg {
        /// The document.
        data: Vec<u8>,
        /// The element to draw; `None` draws all of it.
        id: Option<String>,
        /// The document is y up (made from a `COLR` glyph), not y down.
        y_up: bool,
    },
    /// A PNG.
    Png(GlyphBitmap),
}

/// A glyph of a bitmap font.
#[cfg(feature = "text")]
#[derive(Clone, Debug)]
#[allow(missing_docs)]
pub struct GlyphBitmap {
    pub data: Vec<u8>,
    pub width: u32,
    pub height: u32,
    /// Bearings of the bitmap, in its pixels.
    pub x: f32,
    pub y: f32,
    pub pixels_per_em: f32,
    /// For `sbix`: the minimum of the outline's box, in font units.
    pub sbix_origin: Option<(f32, f32)>,
}

/// Where text layout gets its fonts from.
///
/// [`Fonts`](crate::Fonts) (the `fonts` feature) is one; an application with
/// its own font stack can provide another. What it returns is trusted to be
/// bounded, but not to be consistent: a glyph cluster outside the text, say,
/// is moved into it.
#[cfg(feature = "text")]
pub trait FontProvider: std::fmt::Debug + Send + Sync {
    /// The best font for a request.
    fn select(&self, font: &Font) -> Option<FontId>;

    /// A font that has `c` and is not in `used`, as close as possible to the
    /// first one of them.
    fn fallback(&self, c: char, used: &[FontId]) -> Option<FontId>;

    /// The metrics of a font.
    fn metrics(&self, font: FontId, variations: &[FontVariation]) -> Option<FontMetrics>;

    /// Shapes a run.
    fn shape(&self, font: FontId, request: &ShapeRequest) -> Vec<ShapedGlyph>;

    /// The glyph's drawing, if it has one. `optical_size` is the font size
    /// when `opsz` is to follow it (see [`ShapeRequest::optical_sizing`]).
    fn glyph(
        &self,
        font: FontId,
        glyph: u32,
        variations: &[FontVariation],
        optical_size: Option<f32>,
    ) -> Option<GlyphData>;

    /// The font file, to embed the font in a document (a PDF). `None` (the
    /// default) when the provider cannot give it.
    ///
    /// A provider's [`FontId`]s name its faces for as long as it lives, so
    /// they are what to de-duplicate on: the same provider and the same id
    /// are the same face in every tree parsed with it.
    fn font_data(&self, font: FontId) -> Option<FontData> {
        let _ = font;
        None
    }
}

/// A font file and the face in it (see [`FontProvider::font_data`]).
#[cfg(feature = "text")]
#[derive(Clone)]
#[non_exhaustive]
pub struct FontData {
    /// The file's bytes (a font or a collection).
    pub data: Arc<dyn AsRef<[u8]> + Send + Sync>,
    /// The face's index in a collection; 0 for a single font.
    pub index: u32,
}

#[cfg(feature = "text")]
impl FontData {
    /// The file's bytes and a face index.
    pub fn new(data: Arc<dyn AsRef<[u8]> + Send + Sync>, index: u32) -> FontData {
        FontData { data, index }
    }

    /// The file's bytes.
    pub fn bytes(&self) -> &[u8] {
        (*self.data).as_ref()
    }
}

#[cfg(feature = "text")]
impl std::fmt::Debug for FontData {
    fn fmt(&self, f: &mut std::fmt::Formatter<'_>) -> std::fmt::Result {
        f.debug_struct("FontData")
            .field("len", &self.bytes().len())
            .field("index", &self.index)
            .finish()
    }
}
