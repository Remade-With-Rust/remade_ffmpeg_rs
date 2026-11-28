//! The read-only view a vector writer (a PDF exporter) draws from: glyph
//! runs, font data, `render_node` and decoded images.

use std::sync::Arc;

use rusty_svgtree::rusty_raster::{PathBuilder, PathSegment, Pixmap, Rect, Transform};
use rusty_svgtree::text::*;
use rusty_svgtree::{Group, ImageKind, Node, Options, Tree};

/// Boxes half an em wide; `#` is missing from font 0 and comes from font 1;
/// `ﬁ` (U+FB01) is shaped as two glyphs of one cluster.
#[derive(Debug)]
struct Boxes;

impl FontProvider for Boxes {
    fn select(&self, _: &Font) -> Option<FontId> {
        Some(FontId(0))
    }

    fn fallback(&self, c: char, used: &[FontId]) -> Option<FontId> {
        (c == '#' && used == [FontId(0)]).then_some(FontId(1))
    }

    fn metrics(&self, font: FontId, _: &[FontVariation]) -> Option<FontMetrics> {
        Some(FontMetrics {
            units_per_em: if font == FontId(0) { 1000 } else { 2048 },
            ascent: 800,
            descent: -200,
            x_height: 500,
            underline_position: -100,
            underline_thickness: 50,
            line_through_position: 250,
            subscript_offset: 200,
            superscript_offset: 400,
        })
    }

    fn shape(&self, font: FontId, r: &ShapeRequest) -> Vec<ShapedGlyph> {
        let em = if font == FontId(0) { 1000 } else { 2048 };
        let mut out = Vec::new();
        for (i, c) in r.text.char_indices() {
            let glyph = |g| ShapedGlyph {
                glyph: g,
                cluster: i,
                dx: 0,
                dy: 0,
                advance: em / 2,
            };
            match c {
                '#' if font == FontId(0) => out.push(glyph(0)),
                '\u{fb01}' => {
                    out.push(glyph('f' as u32));
                    out.push(glyph('i' as u32));
                }
                _ => out.push(glyph(c as u32)),
            }
        }
        if r.rtl {
            out.reverse();
        }
        out
    }

    fn glyph(
        &self,
        font: FontId,
        glyph: u32,
        _: &[FontVariation],
        _: Option<f32>,
    ) -> Option<GlyphData> {
        if glyph == 32 {
            return None;
        }
        let em = if font == FontId(0) { 1000.0 } else { 2048.0 };
        let r = Rect::from_xywh(0.05 * em, 0.0, 0.4 * em, 0.7 * em)?;
        Some(GlyphData::Outline(PathBuilder::from_rect(r)))
    }
}

fn opt() -> Options {
    Options {
        fonts: Some(Arc::new(Boxes)),
        ..Options::default()
    }
}

fn parse(body: &str) -> Tree {
    let svg =
        format!(r#"<svg xmlns="http://www.w3.org/2000/svg" width="200" height="100">{body}</svg>"#);
    Tree::from_str(&svg, &opt()).unwrap()
}

fn first_text(g: &Group) -> Option<&Text> {
    for n in g.children() {
        match n {
            Node::Text(t) => return Some(t),
            Node::Group(g) => {
                if let Some(t) = first_text(g) {
                    return Some(t);
                }
            }
            _ => {}
        }
    }
    None
}

/// The corners of a path's points, rounded to 1/100.
fn corners(p: &rusty_svgtree::rusty_raster::Path) -> Vec<(i32, i32)> {
    let mut v: Vec<(i32, i32)> = p
        .segments()
        .filter_map(|s| match s {
            PathSegment::MoveTo(p) | PathSegment::LineTo(p) => {
                Some(((p.x * 100.0).round() as i32, (p.y * 100.0).round() as i32))
            }
            _ => None,
        })
        .collect();
    v.sort();
    v
}

#[test]
fn glyph_runs_draw_what_flattened_draws() {
    let tree = parse(
        r#"<text x="10" y="50" font-size="20" fill="red" text-decoration="underline"
              transform="rotate(10)">ab<tspan fill="blue">#c</tspan> d</text>"#,
    );
    let text = first_text(tree.root()).unwrap();
    let runs = text.glyph_runs().unwrap();
    assert_eq!(runs.len(), 3);
    assert_eq!((runs[0].chunk, runs[0].span), (0, 0));
    assert_eq!((runs[1].chunk, runs[1].span), (0, 1));
    assert_eq!((runs[2].chunk, runs[2].span), (0, 2));
    // The fallback font draws `#`, in the second span.
    let fonts: Vec<FontId> = runs[1].glyphs.iter().map(|g| g.font).collect();
    assert_eq!(fonts[0], FontId(1));
    assert_eq!(runs[1].glyphs[0].units_per_em, 2048);
    assert!(runs
        .iter()
        .all(|r| r.underline.is_some() && r.overline.is_none()));
    // Each glyph's text is its character.
    let chunk = &text.chunks[0];
    let chars: String = runs
        .iter()
        .flat_map(|r| &r.glyphs)
        .map(|g| &chunk.text[g.text.clone()])
        .collect();
    assert_eq!(chars, "ab#c d");
    // Outlines placed by the runs' transforms are the flattened paths.
    let mut ours = Vec::new();
    for run in runs {
        for g in &run.glyphs {
            if let Some(GlyphData::Outline(p)) = Boxes.glyph(g.font, g.id, &[], None) {
                ours.extend(corners(&p.transform(g.transform).unwrap()));
            }
        }
        ours.extend(corners(run.underline.as_ref().unwrap().data()));
    }
    let mut theirs = Vec::new();
    for n in text.flattened().unwrap().children() {
        if let Node::Path(p) = n {
            theirs.extend(corners(p.data()));
        }
    }
    ours.sort();
    theirs.sort();
    assert_eq!(ours, theirs);
}

#[test]
fn a_cluster_of_two_glyphs_shares_its_text() {
    let tree = parse(r#"<text x="10" y="50" font-size="20">x&#xFB01;y</text>"#);
    let text = first_text(tree.root()).unwrap();
    let run = &text.glyph_runs().unwrap()[0];
    let ranges: Vec<_> = run.glyphs.iter().map(|g| g.text.clone()).collect();
    assert_eq!(ranges, vec![0..1, 1..4, 1..4, 4..5]);
}

#[test]
fn decorations_are_resolved_paths() {
    let tree = parse(
        r#"<linearGradient id="g"><stop offset="0" stop-color="red"/><stop offset="1" stop-color="blue"/></linearGradient>
           <text x="10" y="50" font-size="20" fill="url(#g)"
                 text-decoration="overline line-through">ab</text>"#,
    );
    let run = &first_text(tree.root()).unwrap().glyph_runs().unwrap()[0];
    let over = run.overline.as_ref().unwrap();
    assert!(run.line_through.is_some() && run.underline.is_none());
    // The text's box, not the line's, and in user space.
    match over.fill().unwrap().paint() {
        rusty_svgtree::Paint::LinearGradient(g) => {
            assert!(!g.base().transform().is_identity());
        }
        p => panic!("{p:?}"),
    }
}

#[test]
fn variations_and_optical_size() {
    let tree = parse(
        r#"<text x="10" y="50" font-size="20" font-variation-settings="'wght' 600">a</text>"#,
    );
    let run = &first_text(tree.root()).unwrap().glyph_runs().unwrap()[0];
    assert_eq!(run.variations.len(), 1);
    assert_eq!(&run.variations[0].tag, b"wght");
    assert_eq!(run.optical_size, Some(20.0));
    let tree = parse(r#"<text x="10" y="50" font-optical-sizing="none">a</text>"#);
    let run = &first_text(tree.root()).unwrap().glyph_runs().unwrap()[0];
    assert_eq!(run.optical_size, None);
}

#[test]
fn no_runs_without_fonts() {
    let svg =
        r#"<svg xmlns="http://www.w3.org/2000/svg" width="10" height="10"><text>a</text></svg>"#;
    let tree = Tree::from_str(svg, &Options::default()).unwrap();
    assert!(tree.fonts().is_none());
    assert!(first_text(tree.root()).unwrap().glyph_runs().is_none());
}

#[test]
fn the_tree_keeps_its_provider() {
    let fonts: Arc<dyn FontProvider> = Arc::new(Boxes);
    let opt = Options {
        fonts: Some(fonts.clone()),
        ..Options::default()
    };
    let tree = Tree::from_str(
        r#"<svg xmlns="http://www.w3.org/2000/svg" width="10" height="10"/>"#,
        &opt,
    )
    .unwrap();
    assert!(Arc::ptr_eq(tree.fonts().unwrap(), &fonts));
    // A provider that does not give font files.
    assert!(tree.fonts().unwrap().font_data(FontId(0)).is_none());
}

fn render(tree: &Tree) -> Pixmap {
    let s = tree.size().to_int_size();
    let mut p = Pixmap::new(s.width(), s.height()).unwrap();
    rusty_svgtree::render(tree, Transform::identity(), &mut p.as_mut());
    p
}

#[test]
fn render_node_draws_a_node_as_render_does() {
    let tree = parse(
        r#"<filter id="f"><feGaussianBlur stdDeviation="3"/></filter>
           <clipPath id="c"><circle cx="60" cy="50" r="35"/></clipPath>
           <g transform="translate(5 3)" opacity="0.7" filter="url(#f)" clip-path="url(#c)">
             <rect x="20" y="20" width="80" height="60" fill="teal"/>
           </g>"#,
    );
    let whole = render(&tree);
    let Node::Group(g) = &tree.root().children()[0] else {
        panic!()
    };
    assert!(!g.filters().is_empty());
    let mut alone = Pixmap::new(200, 100).unwrap();
    rusty_svgtree::render_node(
        &tree,
        &tree.root().children()[0],
        Transform::identity(),
        &mut alone.as_mut(),
    );
    assert_eq!(whole.data(), alone.data());
    // At a scale, cropped to the layer box: the recipe in the docs.
    let b = g.layer_bounding_box().transform(g.transform()).unwrap();
    let s = 2.0;
    let mut crop = Pixmap::new(
        (b.width() * s).ceil() as u32,
        (b.height() * s).ceil() as u32,
    )
    .unwrap();
    rusty_svgtree::render_node(
        &tree,
        &tree.root().children()[0],
        Transform::from_scale(s, s).pre_translate(-b.x(), -b.y()),
        &mut crop.as_mut(),
    );
    assert!(crop.data().chunks_exact(4).any(|p| p[3] > 0));
}

#[test]
fn image_decode() {
    let dir = std::path::Path::new(env!("CARGO_MANIFEST_DIR")).join("tests/corpus");
    let data = std::fs::read(dir.join("21-image-png.svg")).unwrap();
    let tree = Tree::from_data(&data, &Options::default()).unwrap();
    let mut images = Vec::new();
    fn walk<'a>(g: &'a Group, out: &mut Vec<&'a rusty_svgtree::Image>) {
        for n in g.children() {
            match n {
                Node::Image(i) => out.push(i),
                Node::Group(g) => walk(g, out),
                _ => {}
            }
        }
    }
    walk(tree.root(), &mut images);
    let image = images.first().unwrap();
    assert!(matches!(image.kind(), ImageKind::Png(_)));
    let size = image.size().to_int_size();
    if cfg!(feature = "raster-images") {
        let px = image.decode(u64::MAX).unwrap();
        assert_eq!((px.width(), px.height()), (size.width(), size.height()));
        assert!(image.decode(1).is_none());
    }
}

/// `Fonts` gives the bytes it was loaded with, shared, under one id.
#[cfg(feature = "fonts")]
#[test]
fn fonts_font_data_is_shared() {
    let mut db = rusty_svgtree::Fonts::new();
    db.load_system_fonts();
    if db.is_empty() {
        return;
    }
    let a = db.font_data(FontId(0)).unwrap();
    let b = db.font_data(FontId(0)).unwrap();
    assert!(Arc::ptr_eq(&a.data, &b.data));
    assert!(a.bytes().len() > 12);
}
