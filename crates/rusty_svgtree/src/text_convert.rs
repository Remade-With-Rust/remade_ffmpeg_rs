//! `<text>` elements to [`Text`] nodes.
//!
//! Derived from usvg 0.48.1 `parser/text.rs` (Copyright 2019 the Resvg
//! Authors, Apache-2.0 OR MIT; see `NOTICE.md`), reworked for this crate's
//! document.

use std::sync::Arc;

use rusty_raster::PathSegment;

use crate::convert::{
    convert_length, convert_shape, length_list_attr, paint_order, resolve_fill, resolve_font_size,
    resolve_length, resolve_stroke, Cache, State, Units,
};
use crate::svgtree::{AId, EId, SvgNode};
use crate::text::*;
use crate::tree::{Group, Node};
use crate::values::{self, Length, Unit};

/// A character's position attributes.
#[derive(Clone, Copy, Default)]
struct Position {
    x: Option<f32>,
    y: Option<f32>,
    dx: Option<f32>,
    dy: Option<f32>,
}

pub(crate) fn convert(node: SvgNode, state: &State, cache: &mut Cache, parent: &mut Group) {
    let total = count_chars(node);
    // Every character is positioned, styled and shaped on its own.
    if !cache.spend_many(total / 4 + 1) {
        cache.skipped.text += 1;
        return;
    }
    let positions = resolve_positions(node, total, state);
    let rotate = resolve_rotate(node, total);
    let mut chunks = Chunks {
        chars: 0,
        chunk_bytes: 0,
        split: false,
        flow: TextFlow::Linear,
        list: Vec::new(),
    };
    collect_chunks(node, &positions, state, cache, &mut chunks);
    let mut text = Text {
        id: state.element_id(node),
        chunks: chunks.list,
        dx: positions.iter().map(|p| p.dx.unwrap_or(0.0)).collect(),
        dy: positions.iter().map(|p| p.dy.unwrap_or(0.0)).collect(),
        rotate,
        writing_mode: match node.find_attribute(AId::WritingMode).map(values::trim) {
            Some("tb" | "tb-rl" | "vertical-rl" | "vertical-lr") => WritingMode::TopToBottom,
            _ => WritingMode::LeftToRight,
        },
        anti_alias: node.find_attribute(AId::TextRendering).map(values::trim)
            != Some("optimizeSpeed"),
        abs_transform: parent.abs_transform,
        bounding_box: None,
        stroke_bounding_box: None,
        flattened: None,
    };
    if text.chunks.is_empty() {
        return;
    }
    #[cfg(feature = "text")]
    if let Some(fonts) = &state.opt.fonts {
        match crate::text_layout::layout(&mut text, fonts.as_ref(), state, cache) {
            crate::text_layout::Outcome::Drawn => {
                parent.children.push(Node::Text(Box::new(text)));
            }
            // Nothing to draw, or no box: not in the tree, as in usvg.
            crate::text_layout::Outcome::Empty => {}
            crate::text_layout::Outcome::OverLimit => {
                cache.skipped.text += 1;
                parent.children.push(Node::Text(Box::new(text)));
            }
        }
        return;
    }
    cache.skipped.text += 1;
    // Kept for callers that read the tree; nothing to draw, and no box.
    resolve_paints(&mut text);
    parent.children.push(Node::Text(Box::new(text)));
}

/// Drops the paints that need the text's box, which it does not have.
fn resolve_paints(text: &mut Text) {
    for span in text.chunks.iter_mut().flat_map(|c| &mut c.spans) {
        if span.fill.as_ref().is_some_and(|f| f.paint.needs_box()) {
            span.fill = None;
        }
        if span.stroke.as_ref().is_some_and(|s| s.paint.needs_box()) {
            span.stroke = None;
        }
    }
}

struct Chunks {
    chars: usize,
    chunk_bytes: usize,
    split: bool,
    flow: TextFlow,
    list: Vec<TextChunk>,
}

fn collect_chunks(
    parent: SvgNode,
    positions: &[Position],
    state: &State,
    cache: &mut Cache,
    out: &mut Chunks,
) {
    // What the children's characters are styled with, made once. (Boxed,
    // like the rest that is out of line: this recurses.)
    let mut style: Option<Option<Box<(TextSpan, TextAnchor)>>> = None;
    for child in parent.children() {
        if let Some(text) = child.text() {
            let style = style
                .get_or_insert_with(|| make_span(parent, state, cache))
                .as_deref();
            append_chars(text, style, positions, cache, out);
            continue;
        }
        let is_path = child.tag_name() == Some(EId::TextPath);
        if is_path {
            // Only a direct child of `text`, with a path to follow.
            let flow = if parent.tag_name() == Some(EId::Text) {
                text_flow(child, state, cache)
            } else {
                None
            };
            let Some(flow) = flow else {
                out.chars += count_chars(child);
                continue;
            };
            out.flow = flow;
            out.split = true;
        }
        if cache.spend() {
            collect_chunks(child, positions, state, cache, out);
        } else {
            out.chars += count_chars(child);
        }
        out.flow = TextFlow::Linear;
        // The character after a text path starts a chunk too.
        out.split |= is_path;
    }
}

#[inline(never)]
fn append_chars(
    text: &str,
    style: Option<&(TextSpan, TextAnchor)>,
    positions: &[Position],
    cache: &mut Cache,
    out: &mut Chunks,
) {
    let Some((span, anchor)) = style else {
        out.chars += text.chars().count();
        return;
    };
    let mut new_span = true;
    for c in text.chars() {
        let len = c.len_utf8();
        let pos = positions.get(out.chars).copied().unwrap_or_default();
        let new_chunk = pos.x.is_some() || pos.y.is_some() || out.split || out.list.is_empty();
        out.split = false;
        // Every chunk and span carries a copy of its style.
        if (new_chunk || new_span) && !cache.spend_many(16) {
            out.chars += 1;
            continue;
        }
        if new_chunk {
            out.chunk_bytes = 0;
            out.list.push(TextChunk {
                x: pos.x,
                y: pos.y,
                anchor: *anchor,
                spans: vec![TextSpan {
                    start: 0,
                    end: len,
                    ..span.clone()
                }],
                text_flow: out.flow.clone(),
                text: c.to_string(),
            });
        } else if let Some(chunk) = out.list.last_mut() {
            chunk.text.push(c);
            match chunk.spans.last_mut() {
                Some(last) if !new_span => last.end += len,
                _ => chunk.spans.push(TextSpan {
                    start: out.chunk_bytes,
                    end: out.chunk_bytes + len,
                    ..span.clone()
                }),
            }
        }
        new_span = false;
        out.chars += 1;
        out.chunk_bytes += len;
    }
}

/// The style of an element's own characters; `None` when they are not shown.
#[inline(never)]
fn make_span(
    node: SvgNode,
    state: &State,
    cache: &mut Cache,
) -> Option<Box<(TextSpan, TextAnchor)>> {
    if !node.is_visible_element(state.opt) {
        return None;
    }
    let font_size = resolve_font_size(node, state);
    if !(font_size.is_finite() && font_size > 0.0) {
        return None;
    }
    let find = |name: AId| node.find_attribute(name).map(values::trim);
    let anchor = match find(AId::TextAnchor) {
        Some("middle") => TextAnchor::Middle,
        Some("end") => TextAnchor::End,
        _ => TextAnchor::Start,
    };
    let dominant = match find(AId::DominantBaseline) {
        // The parent's.
        Some("no-change") => node
            .parent_element()
            .and_then(|p| p.find_attribute(AId::DominantBaseline))
            .map(values::trim),
        v => v,
    };
    let text_length = node
        .attribute(AId::TextLength)
        .and_then(values::length)
        .map(|l| convert_length(l, node, AId::TextLength, Units::UserSpaceOnUse, state))
        .filter(|n| *n >= 0.0);
    let span = TextSpan {
        start: 0,
        end: 0,
        fill: resolve_fill(node, true, state, cache),
        stroke: resolve_stroke(node, true, state, cache),
        paint_order: node
            .find_attribute(AId::PaintOrder)
            .map(paint_order)
            .unwrap_or_default(),
        font: convert_font(node, state),
        font_size,
        small_caps: find(AId::FontVariant) == Some("small-caps"),
        apply_kerning: resolve_length(node, AId::Kerning, state, -1.0) != 0.0
            && find(AId::FontKerning) != Some("none"),
        optical_sizing: find(AId::FontOpticalSizing) != Some("none"),
        decoration: resolve_decoration(node, state, cache),
        dominant_baseline: baseline(dominant),
        alignment_baseline: baseline(find(AId::AlignmentBaseline)),
        baseline_shift: baseline_shift(node, state),
        visible: matches!(find(AId::Visibility), None | Some("visible")),
        letter_spacing: resolve_length(node, AId::LetterSpacing, state, 0.0),
        word_spacing: resolve_length(node, AId::WordSpacing, state, 0.0),
        text_length,
        adjust_glyphs: find(AId::LengthAdjust) == Some("spacingAndGlyphs"),
    };
    Some(Box::new((span, anchor)))
}

fn baseline(v: Option<&str>) -> Baseline {
    match v {
        Some("baseline") => Baseline::Baseline,
        Some("before-edge" | "text-before-edge") => Baseline::BeforeEdge,
        Some("middle") => Baseline::Middle,
        Some("central") => Baseline::Central,
        Some("after-edge" | "text-after-edge") => Baseline::AfterEdge,
        Some("ideographic") => Baseline::Ideographic,
        Some("alphabetic") => Baseline::Alphabetic,
        Some("hanging") => Baseline::Hanging,
        Some("mathematical") => Baseline::Mathematical,
        _ => Baseline::Auto,
    }
}

#[inline(never)]
fn text_flow(node: SvgNode, state: &State, cache: &mut Cache) -> Option<TextFlow> {
    let link = node.href()?;
    let mut path = convert_shape(link, state, cache)?;
    let ts = link.resolve_transform(AId::Transform, state);
    if !ts.is_identity() {
        path = Arc::new(path.as_ref().clone().transform(ts)?);
    }
    let offset = node
        .attribute(AId::StartOffset)
        .and_then(values::length)
        .unwrap_or(Length::zero());
    let start_offset = if offset.unit == Unit::Percent {
        (path_length(&path) * (offset.number / 100.0)) as f32
    } else {
        resolve_length(node, AId::StartOffset, state, 0.0)
    };
    Some(TextFlow::Path(Arc::new(TextPath {
        id: link.element_id().to_string(),
        start_offset,
        path,
    })))
}

/// The length of a path.
fn path_length(path: &rusty_raster::Path) -> f64 {
    let mut length = 0.0;
    for_each_curve(path, |c| {
        #[cfg(feature = "text")]
        {
            use kurbo::ParamCurveArclen;
            length += c.arclen(0.5);
        }
        #[cfg(not(feature = "text"))]
        {
            // Close enough for a position nothing is drawn at.
            let mut prev = c[0];
            for i in 1..=16 {
                let t = f64::from(i) / 16.0;
                let u = 1.0 - t;
                let k = [u * u * u, 3.0 * u * u * t, 3.0 * u * t * t, t * t * t];
                let at = |a: usize| (0..4).map(|j| k[j] * [c[j].0, c[j].1][a]).sum::<f64>();
                let p = (at(0), at(1));
                length += (p.0 - prev.0).hypot(p.1 - prev.1);
                prev = p;
            }
        }
        true
    });
    length
}

#[cfg(feature = "text")]
pub(crate) type Curve = kurbo::CubicBez;
#[cfg(not(feature = "text"))]
pub(crate) type Curve = [(f64, f64); 4];

/// Calls `f` with every segment of a path as a cubic, until it returns
/// `false`.
pub(crate) fn for_each_curve(path: &rusty_raster::Path, mut f: impl FnMut(Curve) -> bool) {
    type P = rusty_raster::Point;
    let Some(first) = path.points().first().copied() else {
        return;
    };
    let wide = |p: P| (f64::from(p.x), f64::from(p.y));
    let cubic = |p: [(f64, f64); 4]| {
        #[cfg(feature = "text")]
        {
            kurbo::CubicBez::new(p[0], p[1], p[2], p[3])
        }
        #[cfg(not(feature = "text"))]
        {
            p
        }
    };
    // The inner points are where usvg puts them.
    let line = |a: P, b: P| {
        let (a, b) = (wide(a), wide(b));
        let at = |t: f64| (a.0 + (b.0 - a.0) * t, a.1 + (b.1 - a.1) * t);
        cubic([a, at(0.33), at(0.66), b])
    };
    let (mut start, mut prev) = (first, first);
    for seg in path.segments() {
        let (curve, end) = match seg {
            PathSegment::MoveTo(p) => {
                start = p;
                prev = p;
                continue;
            }
            PathSegment::LineTo(p) => (line(prev, p), p),
            PathSegment::QuadTo(p1, p) => {
                let (a, b, c) = (wide(prev), wide(p1), wide(p));
                let third = |x: (f64, f64)| {
                    (
                        x.0 + (2.0 / 3.0) * (b.0 - x.0),
                        x.1 + (2.0 / 3.0) * (b.1 - x.1),
                    )
                };
                (cubic([a, third(a), third(c), c]), p)
            }
            PathSegment::CubicTo(p1, p2, p) => {
                (cubic([wide(prev), wide(p1), wide(p2), wide(p)]), p)
            }
            PathSegment::Close => (line(prev, start), start),
        };
        if !f(curve) {
            return;
        }
        prev = end;
    }
}

fn convert_font(node: SvgNode, state: &State) -> Font {
    let style = match node.find_attribute(AId::FontStyle).map(values::trim) {
        Some("italic") => FontStyle::Italic,
        Some("oblique") => FontStyle::Oblique,
        _ => FontStyle::Normal,
    };
    let (stretch, width) = match node.find_attribute(AId::FontStretch).map(values::trim) {
        Some("ultra-condensed") => (FontStretch::UltraCondensed, 50.0),
        Some("extra-condensed") => (FontStretch::ExtraCondensed, 62.5),
        Some("narrower" | "condensed") => (FontStretch::Condensed, 75.0),
        Some("semi-condensed") => (FontStretch::SemiCondensed, 87.5),
        Some("semi-expanded") => (FontStretch::SemiExpanded, 112.5),
        Some("wider" | "expanded") => (FontStretch::Expanded, 125.0),
        Some("extra-expanded") => (FontStretch::ExtraExpanded, 150.0),
        Some("ultra-expanded") => (FontStretch::UltraExpanded, 200.0),
        _ => (FontStretch::Normal, 100.0),
    };
    let weight = font_weight(node);
    let mut variations = node
        .find_attribute(AId::FontVariationSettings)
        .map(values::font_variations)
        .unwrap_or_default();
    // The other properties set the axes that mean the same.
    let mut imply = |tag: &[u8; 4], value: f32, when: bool| {
        if when && !variations.iter().any(|v| &v.tag == tag) {
            variations.push(FontVariation { tag: *tag, value });
        }
    };
    imply(b"wght", f32::from(weight), true);
    imply(b"wdth", width, width != 100.0);
    imply(b"ital", 1.0, style == FontStyle::Italic);
    imply(b"slnt", -12.0, style == FontStyle::Oblique);
    let mut families = node
        .find_attribute(AId::FontFamily)
        .and_then(values::font_families)
        .unwrap_or_default();
    if families.is_empty() {
        families.push(FontFamily::Named(state.opt.font_family.clone()));
    }
    Font {
        families,
        style,
        stretch,
        weight,
        variations,
    }
}

fn font_weight(node: SvgNode) -> u16 {
    let nodes: Vec<SvgNode> = node.ancestors().collect();
    let mut weight: u16 = 400;
    for n in nodes.iter().rev() {
        weight = match n.attribute(AId::FontWeight).map(values::trim) {
            Some("normal") => 400,
            Some("bold") => 700,
            // The steps Chrome and Inkscape take.
            Some("bolder") => (weight + if weight == 400 { 300 } else { 100 }).min(900),
            Some("lighter") => (weight - if weight == 400 { 200 } else { 100 }).max(100),
            Some(v) => match v.parse::<u16>() {
                Ok(w) if v.len() == 3 && w % 100 == 0 && w > 0 => w,
                _ => weight,
            },
            None => weight,
        };
        weight = weight.clamp(100, 900);
    }
    weight
}

fn baseline_shift(node: SvgNode, state: &State) -> Vec<BaselineShift> {
    let mut shift = Vec::new();
    for n in node
        .ancestors()
        .take_while(|n| n.tag_name() != Some(EId::Text))
    {
        let Some(v) = n.attribute(AId::BaselineShift) else {
            continue;
        };
        shift.push(match (values::length(v), v) {
            (Some(l), _) if l.unit == Unit::Percent => {
                BaselineShift::Number(resolve_font_size(n, state) * (l.number as f32 / 100.0))
            }
            (Some(l), _) => BaselineShift::Number(convert_length(
                l,
                n,
                AId::BaselineShift,
                Units::ObjectBoundingBox,
                state,
            )),
            (None, "sub") => BaselineShift::Subscript,
            (None, "super") => BaselineShift::Superscript,
            _ => BaselineShift::Baseline,
        });
    }
    if shift.iter().all(|s| *s == BaselineShift::Baseline) {
        shift.clear();
    }
    shift
}

/// A decoration set on any ancestor applies; it is painted as the nearest one
/// that sets it, or else as the `text` element.
fn resolve_decoration(node: SvgNode, state: &State, cache: &mut Cache) -> TextDecoration {
    let has = |n: &SvgNode, kind: &str| {
        n.attribute(AId::TextDecoration)
            .is_some_and(|v| v.split(' ').any(|w| w == kind))
    };
    let mut style = |kind: &str| {
        if !node.ancestors().any(|n| has(&n, kind)) {
            return None;
        }
        let from = node
            .ancestors()
            .find(|n| has(n, kind) || n.tag_name() == Some(EId::Text))?;
        Some(TextDecorationStyle {
            fill: resolve_fill(from, true, state, cache),
            stroke: resolve_stroke(from, true, state, cache),
        })
    };
    TextDecoration {
        underline: style("underline"),
        overline: style("overline"),
        line_through: style("line-through"),
    }
}

fn count_chars(node: SvgNode) -> usize {
    node.descendants()
        .filter_map(|n| n.text())
        .map(|t| t.chars().count())
        .sum()
}

/// The characters each element below `node` starts at and has, in document
/// order.
fn element_spans<'a>(node: SvgNode<'a>) -> Vec<(SvgNode<'a>, usize, usize)> {
    // (element, first character, characters): the counts are filled in on
    // the way back up, so the whole is linear.
    let mut out: Vec<(SvgNode, usize, usize)> = Vec::new();
    let mut open: Vec<(SvgNode, usize)> = Vec::new();
    let mut chars = 0usize;
    for n in node.descendants() {
        while let Some((e, at)) = open.last().copied() {
            if n.parent().is_some_and(|p| p == e) {
                break;
            }
            out[at].2 = chars - out[at].1;
            open.pop();
        }
        match n.text() {
            Some(t) => chars += t.chars().count(),
            None => {
                open.push((n, out.len()));
                out.push((n, chars, 0));
            }
        }
    }
    for (_, at) in open {
        out[at].2 = chars - out[at].1;
    }
    out
}

/// `x`, `y`, `dx` and `dy` of every character. An element's list applies to
/// its own characters, descendants included, and no further.
fn resolve_positions(node: SvgNode, total: usize, state: &State) -> Vec<Position> {
    let mut list = vec![Position::default(); total];
    for (e, start, len) in element_spans(node) {
        if !matches!(e.tag_name(), Some(EId::Text | EId::Tspan)) {
            continue;
        }
        let own = &mut list[start..start + len];
        for name in [AId::X, AId::Y, AId::Dx, AId::Dy] {
            for (p, v) in own.iter_mut().zip(length_list_attr(e, name, state)) {
                let slot = match name {
                    AId::X => &mut p.x,
                    AId::Y => &mut p.y,
                    AId::Dx => &mut p.dx,
                    _ => &mut p.dy,
                };
                *slot = Some(v);
            }
        }
    }
    list
}

/// The rotation of every character: a list's last value goes on to the rest
/// of the element.
fn resolve_rotate(node: SvgNode, total: usize) -> Vec<f32> {
    let mut list = vec![0.0; total];
    let mut last = 0.0;
    for (e, start, len) in element_spans(node) {
        let Some(v) = e.attribute(AId::Rotate) else {
            continue;
        };
        let (angles, true) = values::number_list(v) else {
            continue;
        };
        for (i, slot) in list[start..start + len].iter_mut().enumerate() {
            if let Some(a) = angles.get(i) {
                last = *a as f32;
            }
            *slot = last;
        }
    }
    list
}
