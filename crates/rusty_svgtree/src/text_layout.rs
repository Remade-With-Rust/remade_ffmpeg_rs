//! Lays text out and turns it into paths.
//!
//! Derived from usvg 0.48.1 `text/layout.rs` and `text/flatten.rs` (Copyright
//! 2022 the Resvg Authors, Apache-2.0 OR MIT; see `NOTICE.md`): the bidi
//! reordering, font fallback, cluster merging, spacing, baselines,
//! decorations and text-on-path are usvg's. Fonts come from a
//! [`FontProvider`] instead of a font database, lookups that were linear per
//! character are not, and the work is bounded.

use std::collections::{HashMap, HashSet};
use std::sync::Arc;

use kurbo::{ParamCurve, ParamCurveArclen, ParamCurveDeriv};
use rusty_raster::{FilterQuality, NonZeroRect, PathBuilder, Rect, Size, Transform};
use unicode_script::UnicodeScript;

use crate::convert::{approx_zero, Cache, State};
use crate::text::*;
use crate::tree::{Group, Image, ImageKind, Node, PaintOrder, Path};

/// What became of a text.
pub(crate) enum Outcome {
    /// It is laid out.
    Drawn,
    /// It has nothing to draw.
    Empty,
    /// It is too much work.
    OverLimit,
}

/// Byte-spans a document's texts may shape (each span shapes its whole
/// chunk): some seconds' worth.
pub(crate) const MAX_SHAPE_WORK: u64 = 1 << 24;

pub(crate) fn layout(
    text: &mut Text,
    fonts: &dyn FontProvider,
    state: &State,
    cache: &mut Cache,
) -> Outcome {
    let work: u64 = text
        .chunks
        .iter()
        .map(|c| c.text.len() as u64 * c.spans.len() as u64 + 64)
        .sum();
    if !cache.spend_text_work(work) {
        crate::text_layout::drop_paints(text);
        return Outcome::OverLimit;
    }
    let mut l = Layout {
        fonts,
        resolved: HashMap::new(),
    };
    let Some((spans, bbox)) = l.layout_text(text) else {
        return Outcome::Empty;
    };
    text.bounding_box = Some(bbox.to_rect());
    let Some(mut group) = flatten(text, &spans, fonts, state, cache) else {
        return Outcome::Empty;
    };
    // Paint servers take the box of the whole text.
    let bbox = bbox.to_rect();
    let abs = text.abs_transform;
    for child in &mut group.children {
        if let Node::Path(p) = child {
            cache.resolve_path_paints(&mut p.fill, &mut p.stroke, bbox, abs);
        }
    }
    for span in text.chunks.iter_mut().flat_map(|c| &mut c.spans) {
        cache.resolve_path_paints(&mut span.fill, &mut span.stroke, bbox, abs);
        let d = &mut span.decoration;
        for s in [&mut d.underline, &mut d.overline, &mut d.line_through]
            .into_iter()
            .flatten()
        {
            cache.resolve_path_paints(&mut s.fill, &mut s.stroke, bbox, abs);
        }
    }
    group.calculate_bounding_boxes();
    let Some(stroke_bbox) = group.stroke_bounding_box.and_then(|r| r.to_non_zero_rect()) else {
        return Outcome::Empty;
    };
    text.stroke_bounding_box = Some(stroke_bbox.to_rect());
    text.flattened = Some(Box::new(group));
    Outcome::Drawn
}

fn drop_paints(text: &mut Text) {
    for span in text.chunks.iter_mut().flat_map(|c| &mut c.spans) {
        if span.fill.as_ref().is_some_and(|f| f.paint.needs_box()) {
            span.fill = None;
        }
        if span.stroke.as_ref().is_some_and(|s| s.paint.needs_box()) {
            span.stroke = None;
        }
    }
}

/// A font with the metrics of one set of axis values.
#[derive(Clone, Copy, Debug)]
struct Resolved {
    id: FontId,
    m: FontMetrics,
}

impl Resolved {
    fn load(
        fonts: &dyn FontProvider,
        id: FontId,
        variations: &[FontVariation],
    ) -> Option<Resolved> {
        let m = fonts.metrics(id, variations)?;
        (m.units_per_em != 0).then_some(Resolved { id, m })
    }

    fn scale(&self, size: f32) -> f32 {
        size / f32::from(self.m.units_per_em)
    }

    fn ascent(&self, size: f32) -> f32 {
        f32::from(self.m.ascent) * self.scale(size)
    }

    fn descent(&self, size: f32) -> f32 {
        f32::from(self.m.descent) * self.scale(size)
    }

    fn height(&self, size: f32) -> f32 {
        self.ascent(size) - self.descent(size)
    }

    fn units(&self, v: i16, size: f32) -> f32 {
        f32::from(v) * self.scale(size)
    }

    /// Where a baseline is, above the alphabetic one. Like Chrome: from fixed
    /// shares of the metrics, not from the font's baseline tables.
    fn baseline(&self, baseline: Baseline, size: f32) -> f32 {
        match baseline {
            Baseline::Auto | Baseline::Baseline | Baseline::Alphabetic => 0.0,
            Baseline::BeforeEdge => self.ascent(size),
            Baseline::Middle => f32::from(self.m.x_height) * self.scale(size) * 0.5,
            Baseline::Central => self.ascent(size) - self.height(size) * 0.5,
            Baseline::AfterEdge | Baseline::Ideographic => self.descent(size),
            Baseline::Hanging => self.ascent(size) * 0.8,
            Baseline::Mathematical => self.ascent(size) * 0.5,
        }
    }
}

/// A glyph placed in its span.
#[derive(Clone, Debug)]
struct Positioned {
    /// Of the glyph in its cluster.
    glyph_ts: Transform,
    /// Of the cluster in its span.
    cluster_ts: Transform,
    /// Of the span.
    span_ts: Transform,
    units_per_em: u16,
    font_size: f32,
    id: u32,
    font: FontId,
}

impl Positioned {
    fn transform(&self) -> Transform {
        let sx = self.font_size / f32::from(self.units_per_em);
        self.span_ts
            .pre_concat(self.cluster_ts)
            .pre_concat(Transform::from_scale(sx, sx))
            .pre_concat(self.glyph_ts)
    }

    /// Outlines are y up.
    fn outline_transform(&self) -> Transform {
        self.transform()
            .pre_concat(Transform::from_scale(1.0, -1.0))
    }

    fn bitmap_transform(&self, b: &GlyphBitmap) -> Transform {
        let em = f32::from(self.units_per_em);
        let s = em / b.pixels_per_em;
        match b.sbix_origin {
            None => self
                .transform()
                .pre_concat(Transform::from_scale(s, s))
                .pre_translate(b.x, -b.y),
            Some((x_min, y_min)) => {
                // Apple's emoji font has no shift of its own, but is drawn
                // with one.
                let y_shift = if approx_zero(y_min) {
                    0.128 * em
                } else {
                    -y_min
                };
                self.transform()
                    .pre_concat(Transform::from_translate(-x_min, y_shift))
                    .pre_concat(Transform::from_scale(s, s))
                    .pre_translate(b.x, -(b.height as f32) - b.y)
            }
        }
    }
}

/// Glyphs that share a style, placed.
struct LaidSpan {
    span: TextSpan,
    glyphs: Vec<Positioned>,
    underline: Option<rusty_raster::Path>,
    overline: Option<rusty_raster::Path>,
    line_through: Option<rusty_raster::Path>,
}

#[derive(Clone, Debug)]
struct Cluster {
    byte: usize,
    codepoint: char,
    width: f32,
    advance: f32,
    ascent: f32,
    descent: f32,
    has_relative_shift: bool,
    glyphs: Vec<Positioned>,
    transform: Transform,
    path_transform: Transform,
    visible: bool,
}

impl Cluster {
    fn transform(&self) -> Transform {
        self.path_transform.post_concat(self.transform)
    }
}

/// A shaped glyph of a chunk.
#[derive(Clone, Copy)]
struct Glyph {
    id: u32,
    byte: usize,
    /// Bytes of the chunk's text in the cluster.
    cluster_len: usize,
    dx: i32,
    dy: i32,
    width: i32,
    font: Resolved,
}

/// Counts code points by byte position.
struct CodePoints(Vec<usize>);

impl CodePoints {
    fn new(text: &str) -> CodePoints {
        CodePoints(text.char_indices().map(|(i, _)| i).collect())
    }

    fn at(&self, byte: usize) -> usize {
        self.0.partition_point(|b| *b < byte)
    }
}

/// The span a byte is in (spans are in order and do not overlap).
fn span_at(chunk: &TextChunk, byte: usize) -> Option<&TextSpan> {
    let i = chunk.spans.partition_point(|s| s.end <= byte);
    chunk.spans.get(i).filter(|s| contains(s, byte))
}

fn contains(span: &TextSpan, byte: usize) -> bool {
    byte >= span.start && byte < span.end
}

/// A box grown by rectangles.
#[derive(Default)]
struct BBox(Option<(f32, f32, f32, f32)>);

impl BBox {
    fn add(&mut self, r: Rect) {
        let (l, t, rt, b) = self.0.unwrap_or((r.left(), r.top(), r.right(), r.bottom()));
        self.0 = Some((
            l.min(r.left()),
            t.min(r.top()),
            rt.max(r.right()),
            b.max(r.bottom()),
        ));
    }

    fn to_rect(&self) -> Option<NonZeroRect> {
        let (l, t, r, b) = self.0?;
        NonZeroRect::from_ltrb(l, t, r, b)
    }
}

struct Layout<'a> {
    fonts: &'a dyn FontProvider,
    resolved: HashMap<Font, Option<Resolved>>,
}

impl Layout<'_> {
    fn font(&self, span: &TextSpan) -> Option<Resolved> {
        self.resolved.get(&span.font).copied().flatten()
    }

    fn layout_text(&mut self, text: &Text) -> Option<(Vec<LaidSpan>, NonZeroRect)> {
        for span in text.chunks.iter().flat_map(|c| &c.spans) {
            if !self.resolved.contains_key(&span.font) {
                let font = self
                    .fonts
                    .select(&span.font)
                    .and_then(|id| Resolved::load(self.fonts, id, &span.font.variations));
                self.resolved.insert(span.font.clone(), font);
            }
        }
        let vertical = text.writing_mode == WritingMode::TopToBottom;
        let mut spans = Vec::new();
        let mut char_offset = 0;
        let (mut last_x, mut last_y) = (0.0, 0.0);
        let mut bbox = BBox::default();
        for chunk in &text.chunks {
            let linear = matches!(chunk.text_flow, TextFlow::Linear);
            let (x, y) = if linear {
                (chunk.x.unwrap_or(last_x), chunk.y.unwrap_or(last_y))
            } else {
                (0.0, 0.0)
            };
            let chars = CodePoints::new(&chunk.text);
            let mut clusters = self.process_chunk(chunk);
            if clusters.is_empty() {
                char_offset += chars.0.len();
                continue;
            }
            apply_writing_mode(text.writing_mode, &mut clusters);
            apply_letter_spacing(chunk, &mut clusters);
            apply_word_spacing(chunk, &mut clusters);
            apply_length_adjust(chunk, &mut clusters);
            let mut end = match &chunk.text_flow {
                TextFlow::Linear => place_on_line(text, chunk, char_offset, &chars, &mut clusters),
                TextFlow::Path(path) => {
                    self.place_on_path(text, chunk, char_offset, &chars, path, &mut clusters)
                }
            };
            let mut text_ts = Transform::identity();
            if vertical && linear {
                text_ts = text_ts.pre_rotate_at(90.0, x, y);
            }
            for span in &chunk.spans {
                let Some(font) = self.font(span) else {
                    continue;
                };
                let size = span.font_size;
                let lines = decoration_lines(span, &clusters);
                let mut span_ts = text_ts.pre_translate(x, y);
                if linear {
                    // The span moves and not its clusters, which can be
                    // rotated.
                    let shift = resolve_baseline(span, &font, text.writing_mode);
                    span_ts = span_ts.pre_translate(0.0, shift);
                }
                let mut decorate = |wanted: bool, offset: f32| {
                    if !wanted {
                        return None;
                    }
                    let path = decoration_path(offset, span, &font, &lines, span_ts)?;
                    bbox.add(path.bounds());
                    Some(path)
                };
                let d = &span.decoration;
                let half = font.height(size) / 2.0;
                let pick = |h: f32, v: f32| if vertical { v } else { h };
                let underline = decorate(
                    d.underline.is_some(),
                    pick(-font.units(font.m.underline_position, size), half),
                );
                let overline = decorate(
                    d.overline.is_some(),
                    pick(-font.ascent(size), -font.height(size) / 2.0),
                );
                let line_through = decorate(
                    d.line_through.is_some(),
                    pick(-font.units(font.m.line_through_position, size), 0.0),
                );
                let Some((glyphs, span_bbox)) = collect_span(span, &clusters, span_ts) else {
                    continue;
                };
                bbox.add(span_bbox.to_rect());
                let mut span = span.clone();
                if let Some(f) = span.fill.as_mut() {
                    // `fill-rule` does not apply to text.
                    f.rule = rusty_raster::FillRule::Winding;
                }
                spans.push(LaidSpan {
                    span,
                    glyphs,
                    underline,
                    overline,
                    line_through,
                });
            }
            char_offset += chars.0.len();
            if vertical && linear {
                std::mem::swap(&mut end.0, &mut end.1);
            }
            last_x = x + end.0;
            last_y = y + end.1;
        }
        Some((spans, bbox.to_rect()?))
    }

    /// Shapes a chunk into clusters, all still at the origin.
    ///
    /// The whole chunk is shaped with the font of each of its spans, so that
    /// reordering and ligatures across spans come out right; then each span's
    /// glyphs replace the first span's, cluster by cluster.
    fn process_chunk(&self, chunk: &TextChunk) -> Vec<Cluster> {
        let mut glyphs: Vec<Glyph> = Vec::new();
        for span in &chunk.spans {
            let Some(font) = self.font(span) else {
                continue;
            };
            let new = self.shape(&chunk.text, font, span);
            if glyphs.is_empty() {
                glyphs = new;
                continue;
            }
            merge_span(span, new, &mut glyphs);
        }
        let mut clusters = Vec::new();
        let mut i = 0;
        while i < glyphs.len() {
            let byte = glyphs[i].byte;
            let n = glyphs[i..].iter().take_while(|g| g.byte == byte).count();
            if let Some(span) = span_at(chunk, byte) {
                clusters.push(form_cluster(&glyphs[i..i + n], &chunk.text, span.font_size));
            }
            i += n;
        }
        clusters
    }

    /// Shapes with fallback fonts for what the first lacks.
    fn shape(&self, text: &str, font: Resolved, span: &TextSpan) -> Vec<Glyph> {
        let variations = &span.font.variations;
        let mut glyphs = self.shape_with(text, font, span);
        let mut used = vec![font.id];
        // A font per missing character is as many as can be of use.
        while used.len() < 64 {
            let Some(missing) = glyphs.iter().find(|g| g.id == 0) else {
                break;
            };
            let Some(c) = text.get(missing.byte..).and_then(|t| t.chars().next()) else {
                break;
            };
            let Some(next) = self
                .fonts
                .fallback(c, &used)
                .filter(|id| !used.contains(id))
                .and_then(|id| Resolved::load(self.fonts, id, variations))
            else {
                break;
            };
            let other = self.shape_with(text, next, span);
            if other.iter().all(|g| g.id != 0) {
                glyphs = other;
                break;
            }
            // Glyph by glyph, as long as both fonts make as many.
            if glyphs.len() != other.len() {
                break;
            }
            for (g, o) in glyphs.iter_mut().zip(&other) {
                if g.id == 0 && o.id != 0 {
                    *g = *o;
                }
            }
            used.push(next.id);
        }
        glyphs
    }

    fn shape_with(&self, text: &str, font: Resolved, span: &TextSpan) -> Vec<Glyph> {
        let bidi = unicode_bidi::BidiInfo::new(text, Some(unicode_bidi::Level::ltr()));
        let Some(paragraph) = bidi.paragraphs.first() else {
            return Vec::new();
        };
        let (levels, runs) = bidi.visual_runs(paragraph, paragraph.range.clone());
        let mut out = Vec::new();
        for run in runs {
            let Some(sub) = text.get(run.clone()).filter(|s| !s.is_empty()) else {
                continue;
            };
            let ltr = levels.get(run.start).is_none_or(|l| l.is_ltr());
            let mut shaped = self.fonts.shape(
                font.id,
                &ShapeRequest {
                    text: sub,
                    rtl: !ltr,
                    small_caps: span.small_caps,
                    kerning: span.apply_kerning,
                    font_size: span.font_size,
                    optical_sizing: span.optical_sizing,
                    variations: &span.font.variations,
                },
            );
            for g in &mut shaped {
                // A provider may answer with anything.
                let mut c = g.cluster.min(sub.len() - 1);
                while !sub.is_char_boundary(c) {
                    c -= 1;
                }
                g.cluster = c;
            }
            for (i, g) in shaped.iter().enumerate() {
                let next = if ltr {
                    shaped.get(i + 1)
                } else {
                    i.checked_sub(1).and_then(|i| shaped.get(i))
                };
                let end = next.map_or(sub.len(), |g| g.cluster);
                out.push(Glyph {
                    id: g.glyph,
                    byte: run.start + g.cluster,
                    cluster_len: end.saturating_sub(g.cluster),
                    dx: g.dx,
                    dy: g.dy,
                    width: g.advance,
                    font,
                });
            }
        }
        out
    }

    fn place_on_path(
        &self,
        text: &Text,
        chunk: &TextChunk,
        char_offset: usize,
        chars: &CodePoints,
        path: &TextPath,
        clusters: &mut [Cluster],
    ) -> (f32, f32) {
        let (mut last_x, mut last_y) = (0.0, 0.0);
        let mut dy = 0.0;
        // A position is an offset along the path.
        let chunk_offset = match text.writing_mode {
            WritingMode::LeftToRight => chunk.x,
            WritingMode::TopToBottom => chunk.y,
        }
        .unwrap_or(0.0);
        let start =
            chunk_offset + path.start_offset + anchor_shift(chunk.anchor, total_advance(clusters));
        let normals = collect_normals(text, clusters, &path.path, char_offset, chars, start);
        for (cluster, normal) in clusters.iter_mut().zip(normals) {
            let Some((x, y, angle)) = normal else {
                // Off the path.
                cluster.visible = false;
                continue;
            };
            // Decorations are broken at every cluster.
            cluster.has_relative_shift = true;
            let original = cluster.transform;
            let half = cluster.width / 2.0;
            cluster.transform = Transform::identity()
                .pre_translate(x - half, y)
                .pre_rotate_at(angle, half, 0.0);
            let cp = char_offset + chars.at(cluster.byte);
            dy += text.dy.get(cp).copied().unwrap_or(0.0);
            let baseline = span_at(chunk, cluster.byte)
                .and_then(|span| {
                    let font = self.font(span)?;
                    Some(-resolve_baseline(span, &font, text.writing_mode))
                })
                .unwrap_or(0.0);
            if !approx_zero(dy) || !approx_zero(baseline) {
                cluster.transform = cluster
                    .transform
                    .pre_translate(0.0, f64::from(dy - baseline) as f32);
            }
            if let Some(angle) = text.rotate.get(cp).filter(|a| !approx_zero(**a)) {
                cluster.transform = cluster.transform.pre_rotate(*angle);
            }
            // What `lengthAdjust` did comes after.
            cluster.transform = cluster.transform.pre_concat(original);
            last_x = x + cluster.advance;
            last_y = y;
        }
        (last_x, last_y)
    }
}

/// Puts the glyphs `new` has for `span` in the place of those in `glyphs`.
fn merge_span(span: &TextSpan, new: Vec<Glyph>, glyphs: &mut Vec<Glyph>) {
    // First glyph by byte; made again after the glyphs move.
    let index = |glyphs: &[Glyph]| {
        let mut m: HashMap<usize, usize> = HashMap::with_capacity(glyphs.len());
        for (i, g) in glyphs.iter().enumerate() {
            m.entry(g.byte).or_insert(i);
        }
        m
    };
    let mut first = index(glyphs);
    // A split glyph repeats its byte: only the first one replaces.
    let mut done = HashSet::new();
    let mut iter = new.into_iter();
    while let Some(g) = iter.next() {
        if !contains(span, g.byte) {
            continue;
        }
        let Some(at) = first.get(&g.byte).copied() else {
            continue;
        };
        if !done.insert(at) {
            continue;
        }
        let old_len = glyphs[at].cluster_len;
        let before = glyphs.len();
        if old_len < g.cluster_len {
            // Fewer glyphs for the same text.
            let end = (at + g.cluster_len).min(glyphs.len());
            glyphs.drain(at + 1..end.max(at + 1));
        } else if old_len > g.cluster_len {
            // More of them.
            let more: Vec<Glyph> = iter.by_ref().take(old_len - 1).collect();
            let to = (at + 1).min(glyphs.len());
            glyphs.splice(to..to, more);
        }
        glyphs[at] = g;
        if glyphs.len() != before {
            first = index(glyphs);
        }
    }
}

fn form_cluster(glyphs: &[Glyph], text: &str, font_size: f32) -> Cluster {
    let (mut x, mut width, mut advance) = (0.0f32, 0.0f32, 0.0f32);
    let mut positioned = Vec::with_capacity(glyphs.len());
    for g in glyphs {
        let sx = g.font.scale(font_size);
        // Offsets are from where the last glyph ended.
        positioned.push(Positioned {
            glyph_ts: Transform::from_translate(x + g.dx as f32, -(g.dy as f32)),
            cluster_ts: Transform::identity(),
            span_ts: Transform::identity(),
            units_per_em: g.font.m.units_per_em,
            font_size,
            font: g.font.id,
            id: g.id,
        });
        x += g.width as f32;
        let w = g.width as f32 * sx;
        advance += w;
        if w > width {
            width = w;
        }
    }
    let g = &glyphs[0];
    Cluster {
        byte: g.byte,
        codepoint: text
            .get(g.byte..)
            .and_then(|t| t.chars().next())
            .unwrap_or(' '),
        width,
        advance,
        ascent: g.font.ascent(font_size),
        descent: g.font.descent(font_size),
        has_relative_shift: false,
        glyphs: positioned,
        transform: Transform::identity(),
        path_transform: Transform::identity(),
        visible: true,
    }
}

/// Upright characters stand up in vertical text, the others lie.
fn apply_writing_mode(mode: WritingMode, clusters: &mut [Cluster]) {
    if mode != WritingMode::TopToBottom {
        return;
    }
    for c in clusters {
        let middle = (c.ascent + c.descent) / 2.0;
        if unicode_vo::char_orientation(c.codepoint) == unicode_vo::Orientation::Upright {
            c.path_transform = Transform::identity()
                .pre_translate(0.0, middle)
                .pre_rotate_at(-90.0, c.width / 2.0, -middle);
            // The "baseline" in the middle, as high as wide.
            c.ascent = c.width / 2.0;
            c.descent = -c.width / 2.0;
        } else {
            c.transform = c.transform.pre_translate(0.0, middle);
        }
    }
}

/// Cursive scripts are not spaced.
fn takes_letter_spacing(script: unicode_script::Script) -> bool {
    use unicode_script::Script;
    !matches!(
        script,
        Script::Arabic
            | Script::Syriac
            | Script::Nko
            | Script::Manichaean
            | Script::Psalter_Pahlavi
            | Script::Mandaic
            | Script::Mongolian
            | Script::Phags_Pa
            | Script::Devanagari
            | Script::Bengali
            | Script::Gurmukhi
            | Script::Modi
            | Script::Sharada
            | Script::Syloti_Nagri
            | Script::Tirhuta
            | Script::Ogham
    )
}

fn apply_letter_spacing(chunk: &TextChunk, clusters: &mut [Cluster]) {
    if chunk.spans.iter().all(|s| approx_zero(s.letter_spacing)) {
        return;
    }
    let n = clusters.len();
    for (i, c) in clusters.iter_mut().enumerate() {
        if !takes_letter_spacing(c.codepoint.script()) {
            continue;
        }
        let Some(span) = span_at(chunk, c.byte) else {
            continue;
        };
        // Not after the last one.
        if i != n - 1 {
            c.advance += span.letter_spacing;
        }
        if !(c.advance > 0.0 && c.advance.is_finite()) {
            // Squeezed away, as in Chrome.
            c.width = 0.0;
            c.advance = 0.0;
            c.glyphs.clear();
        }
    }
}

fn apply_word_spacing(chunk: &TextChunk, clusters: &mut [Cluster]) {
    if chunk.spans.iter().all(|s| approx_zero(s.word_spacing)) {
        return;
    }
    for c in clusters {
        // CSS word separators.
        if matches!(
            c.codepoint as u32,
            0x0020 | 0x00A0 | 0x1361 | 0x0001_0100 | 0x0001_0101 | 0x0001_039F | 0x0001_091F
        ) {
            if let Some(span) = span_at(chunk, c.byte) {
                c.advance += span.word_spacing;
            }
        }
    }
}

fn apply_length_adjust(chunk: &TextChunk, clusters: &mut [Cluster]) {
    let linear = matches!(chunk.text_flow, TextFlow::Linear);
    if chunk.spans.iter().all(|s| s.text_length.is_none()) {
        return;
    }
    let mut by_byte: HashMap<usize, usize> = HashMap::with_capacity(clusters.len());
    for (i, c) in clusters.iter().enumerate() {
        by_byte.entry(c.byte).or_insert(i);
    }
    for span in &chunk.spans {
        let Some(target) = span.text_length else {
            continue;
        };
        let mut indexes: Vec<usize> = (span.start..span.end)
            .filter_map(|b| by_byte.get(&b).copied())
            .collect();
        indexes.sort_unstable();
        indexes.dedup();
        if indexes.is_empty() {
            continue;
        }
        // Of the glyphs: word and letter spacing do not count.
        let width = indexes.iter().fold(0.0, |w, i| w + clusters[*i].width);
        if !span.adjust_glyphs {
            let factor = if indexes.len() > 1 {
                (target - width) / (indexes.len() - 1) as f32
            } else {
                0.0
            };
            for i in indexes {
                clusters[i].advance = clusters[i].width + factor;
            }
        } else {
            let factor = target / width;
            if factor.is_nan() || factor < 0.001 {
                continue;
            }
            for i in indexes {
                let c = &mut clusters[i];
                c.transform = c.transform.pre_scale(factor, 1.0);
                if !linear {
                    c.advance *= factor;
                    c.width *= factor;
                }
            }
        }
    }
}

fn total_advance(clusters: &[Cluster]) -> f32 {
    clusters.iter().fold(0.0, |w, c| w + c.advance)
}

fn anchor_shift(anchor: TextAnchor, width: f32) -> f32 {
    match anchor {
        TextAnchor::Start => 0.0,
        TextAnchor::Middle => -width / 2.0,
        TextAnchor::End => -width,
    }
}

/// Places clusters one after the other; returns where the next one would be.
fn place_on_line(
    text: &Text,
    chunk: &TextChunk,
    offset: usize,
    chars: &CodePoints,
    clusters: &mut [Cluster],
) -> (f32, f32) {
    let mut x = anchor_shift(chunk.anchor, total_advance(clusters));
    let mut y = 0.0;
    for c in clusters {
        let cp = offset + chars.at(c.byte);
        if let (Some(dx), Some(dy)) = (text.dx.get(cp), text.dy.get(cp)) {
            if text.writing_mode == WritingMode::LeftToRight {
                x += dx;
                y += dy;
            } else {
                y -= dx;
                x += dy;
            }
            c.has_relative_shift = !approx_zero(*dx) || !approx_zero(*dy);
        }
        c.transform = c.transform.pre_translate(x, y);
        if let Some(angle) = text.rotate.get(cp).filter(|a| !approx_zero(**a)) {
            c.transform = c.transform.pre_rotate(*angle);
            c.has_relative_shift = true;
        }
        x += c.advance;
    }
    (x, y)
}

fn resolve_baseline(span: &TextSpan, font: &Resolved, mode: WritingMode) -> f32 {
    let size = span.font_size;
    let mut up = 0.0;
    for b in span.baseline_shift.iter().rev() {
        match b {
            BaselineShift::Baseline => {}
            BaselineShift::Subscript => up -= font.units(font.m.subscript_offset, size),
            BaselineShift::Superscript => up += font.units(font.m.superscript_offset, size),
            BaselineShift::Number(n) => up += n,
        }
    }
    let mut shift = -up;
    if mode == WritingMode::LeftToRight {
        let own = span.alignment_baseline;
        let b = if matches!(own, Baseline::Auto | Baseline::Baseline) {
            span.dominant_baseline
        } else {
            own
        };
        shift += font.baseline(b, size);
    }
    shift
}

/// The position and angle on a path of each cluster's middle; `None` for
/// those off its ends.
fn collect_normals(
    text: &Text,
    clusters: &[Cluster],
    path: &rusty_raster::Path,
    char_offset: usize,
    chars: &CodePoints,
    offset: f32,
) -> Vec<Option<(f32, f32, f32)>> {
    let mut offsets: Vec<f64> = Vec::with_capacity(clusters.len());
    let mut normals = Vec::with_capacity(clusters.len());
    let mut advance = offset;
    for c in clusters {
        let half = c.width / 2.0;
        let cp = char_offset + chars.at(c.byte);
        advance += text.dx.get(cp).copied().unwrap_or(0.0);
        let at = advance + half;
        if at < 0.0 {
            normals.push(None);
        }
        offsets.push(f64::from(at));
        advance += c.advance;
    }
    // The larger the text is drawn, the more exact.
    let (sx, sy) = text.abs_transform.get_scale();
    let accuracy = f64::from(0.5 / (sx * sy).sqrt().max(1.0));
    let mut length = 0.0f64;
    crate::text_convert::for_each_curve(path, |curve| {
        let len = curve.arclen(accuracy);
        for offset in &offsets[normals.len().min(offsets.len())..] {
            if *offset >= length && *offset <= length + len {
                let t = curve.inv_arclen(offset - length, accuracy).clamp(0.0, 1.0);
                let pos = curve.eval(t);
                let d = curve.deriv().eval(t);
                let angle = kurbo::Vec2::new(-d.y, d.x).atan2().to_degrees() - 90.0;
                normals.push(Some((pos.x as f32, pos.y as f32, angle as f32)));
                if normals.len() == offsets.len() {
                    break;
                }
            }
        }
        length += len;
        normals.len() < offsets.len()
    });
    normals.resize(offsets.len(), None);
    normals
}

/// The stretches of a span a decoration is drawn under: start and width.
fn decoration_lines(span: &TextSpan, clusters: &[Cluster]) -> Vec<(Transform, f32)> {
    let mut lines: Vec<(Transform, f32)> = Vec::new();
    let mut open = false;
    for c in clusters {
        if !contains(span, c.byte) {
            open = false;
            continue;
        }
        if c.has_relative_shift {
            open = false;
        }
        match lines.last_mut() {
            Some(line) if open => line.1 += c.advance,
            _ => lines.push((c.transform, c.advance)),
        }
        open = true;
    }
    lines
}

fn decoration_path(
    dy: f32,
    span: &TextSpan,
    font: &Resolved,
    lines: &[(Transform, f32)],
    ts: Transform,
) -> Option<rusty_raster::Path> {
    let thickness = f32::from(font.m.underline_thickness) * font.scale(span.font_size);
    let mut b = PathBuilder::new();
    for (line_ts, width) in lines {
        let Some(r) = NonZeroRect::from_xywh(0.0, -thickness / 2.0, *width, thickness) else {
            continue;
        };
        let path = PathBuilder::from_rect(r.to_rect());
        if let Some(p) = path.transform(line_ts.pre_translate(0.0, dy)) {
            b.push_path(&p);
        }
    }
    b.finish()?.transform(ts)
}

/// The span's glyphs, placed, and the box of the line it is on (from the font
/// metrics, not the glyph shapes).
fn collect_span(
    span: &TextSpan,
    clusters: &[Cluster],
    ts: Transform,
) -> Option<(Vec<Positioned>, NonZeroRect)> {
    let mut glyphs = Vec::new();
    let mut boxes = PathBuilder::new();
    for c in clusters {
        if !c.visible {
            continue;
        }
        if contains(span, c.byte) {
            let cluster_ts = c.transform();
            glyphs.extend(c.glyphs.iter().map(|g| Positioned {
                cluster_ts,
                span_ts: ts,
                ..g.clone()
            }));
        }
        let advance = if c.advance <= 0.0 { 1.0 } else { c.advance };
        let r = NonZeroRect::from_xywh(0.0, -c.ascent, advance, c.ascent - c.descent)
            .and_then(|r| r.transform(c.transform()));
        if let Some(r) = r {
            boxes.push_rect(r.to_rect());
        }
    }
    let bbox = boxes
        .finish()?
        .transform(ts)?
        .compute_tight_bounds()?
        .to_non_zero_rect()?;
    Some((glyphs, bbox))
}

// ------------------------------------------------------------ flattening --

/// A glyph's drawing, kept for the document's other uses of it.
#[derive(Clone)]
pub(crate) enum Drawing {
    Outline(Arc<rusty_raster::Path>),
    Svg(Arc<Group>, bool, usize),
    Bitmap(Arc<GlyphBitmap>, ImageKind),
}

/// What a glyph's drawing depends on.
pub(crate) type DrawingKey = (FontId, u32, Vec<FontVariation>, Option<u32>);

fn drawing(
    g: &Positioned,
    span: &TextSpan,
    fonts: &dyn FontProvider,
    state: &State,
    cache: &mut Cache,
) -> Option<Drawing> {
    let optical = span.optical_sizing.then_some(g.font_size);
    let key = (
        g.font,
        g.id,
        span.font.variations.clone(),
        optical.map(f32::to_bits),
    );
    if let Some(d) = cache.glyphs.get(&key) {
        return d.clone();
    }
    let d = match fonts.glyph(g.font, g.id, &span.font.variations, optical) {
        None => None,
        Some(GlyphData::Outline(p)) => Some(Drawing::Outline(Arc::new(p))),
        Some(GlyphData::Svg { data, id, y_up }) => cache
            .glyph_document(&data, id.as_deref(), state.opt)
            .map(|(group, nodes)| Drawing::Svg(Arc::new(group), y_up, nodes)),
        Some(GlyphData::Png(mut b)) => {
            let data = std::mem::take(&mut b.data);
            Some(Drawing::Bitmap(Arc::new(b), ImageKind::Png(Arc::new(data))))
        }
    };
    cache.glyphs.insert(key, d.clone());
    d
}

fn flatten(
    text: &Text,
    spans: &[LaidSpan],
    fonts: &dyn FontProvider,
    state: &State,
    cache: &mut Cache,
) -> Option<Group> {
    let mut group = Group {
        abs_transform: text.abs_transform,
        ..Group::default()
    };
    for laid in spans {
        let span = &laid.span;
        let line = |path: &Option<rusty_raster::Path>,
                    style: &Option<TextDecorationStyle>,
                    group: &mut Group,
                    cache: &mut Cache| {
            if let (Some(p), Some(s)) = (path, style) {
                push_path(
                    p.clone(),
                    text,
                    span,
                    s,
                    PaintOrder::FillAndStroke,
                    group,
                    cache,
                );
            }
        };
        line(&laid.overline, &span.decoration.overline, &mut group, cache);
        line(
            &laid.underline,
            &span.decoration.underline,
            &mut group,
            cache,
        );
        let own = TextDecorationStyle {
            fill: span.fill.clone(),
            stroke: span.stroke.clone(),
        };
        // Outlines are gathered into one path until something else comes.
        let mut outlines = PathBuilder::new();
        for g in &laid.glyphs {
            match drawing(g, span, fonts, state, cache) {
                None => {}
                Some(Drawing::Outline(p)) => {
                    if let Some(p) = p.as_ref().clone().transform(g.outline_transform()) {
                        outlines.push_path(&p);
                    }
                }
                Some(Drawing::Svg(content, y_up, nodes)) => {
                    if !cache.spend_many(nodes) {
                        continue;
                    }
                    if !y_up {
                        flush(&mut outlines, text, span, &own, &mut group, cache);
                    }
                    let ts = if y_up {
                        g.outline_transform()
                    } else {
                        g.transform()
                    };
                    push_group(ts, Node::Group(Box::new((*content).clone())), &mut group);
                }
                Some(Drawing::Bitmap(b, kind)) => {
                    flush(&mut outlines, text, span, &own, &mut group, cache);
                    let Some(size) = Size::from_wh(b.width as f32, b.height as f32) else {
                        continue;
                    };
                    let image = Node::Image(Box::new(Image {
                        id: String::new(),
                        visible: true,
                        size,
                        quality: FilterQuality::Bicubic,
                        kind,
                    }));
                    push_group(g.bitmap_transform(&b), image, &mut group);
                }
            }
        }
        flush(&mut outlines, text, span, &own, &mut group, cache);
        line(
            &laid.line_through,
            &span.decoration.line_through,
            &mut group,
            cache,
        );
    }
    Some(group)
}

fn push_group(transform: Transform, content: Node, parent: &mut Group) {
    let mut g = Group {
        transform,
        abs_transform: parent.abs_transform.pre_concat(transform),
        ..Group::default()
    };
    g.children.push(content);
    g.calculate_bounding_boxes();
    parent.children.push(Node::Group(Box::new(g)));
}

fn flush(
    outlines: &mut PathBuilder,
    text: &Text,
    span: &TextSpan,
    style: &TextDecorationStyle,
    group: &mut Group,
    cache: &mut Cache,
) {
    let path = std::mem::replace(outlines, PathBuilder::new()).finish();
    if let Some(p) = path {
        push_path(p, text, span, style, span.paint_order, group, cache);
    }
}

fn push_path(
    data: rusty_raster::Path,
    text: &Text,
    span: &TextSpan,
    style: &TextDecorationStyle,
    paint_order: PaintOrder,
    group: &mut Group,
    cache: &mut Cache,
) {
    let Some(bounding_box) = data.compute_tight_bounds() else {
        return;
    };
    let stroke_bounding_box = cache
        .stroke_box(&data, style.stroke.as_ref())
        .unwrap_or(bounding_box);
    group.children.push(Node::Path(Box::new(Path {
        id: String::new(),
        abs_transform: text.abs_transform,
        visible: span.visible,
        fill: style.fill.clone(),
        stroke: style.stroke.clone(),
        paint_order,
        anti_alias: text.anti_alias,
        data: Arc::new(data),
        bounding_box,
        stroke_bounding_box,
    })));
}
