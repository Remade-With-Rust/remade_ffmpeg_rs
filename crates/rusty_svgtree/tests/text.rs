//! Text: the nodes in the tree, layout through a font callback, the SVG
//! writer, and hostile text.

use std::sync::Arc;

use rusty_svgtree::rusty_raster::{PathBuilder, Pixmap, Rect, Transform};
use rusty_svgtree::text::*;
use rusty_svgtree::{Error, Group, Node, Options, Tree, WriteOptions};

/// A font whose every character is a box half an em wide, 0.7 em high, on
/// the baseline; spaces have no outline, and `#` is missing from font 0.
#[derive(Debug)]
struct Boxes;

impl FontProvider for Boxes {
    fn select(&self, font: &Font) -> Option<FontId> {
        (font.families.first() != Some(&FontFamily::Named("none".into()))).then_some(FontId(0))
    }

    fn fallback(&self, c: char, used: &[FontId]) -> Option<FontId> {
        (c == '#' && used == [FontId(0)]).then_some(FontId(1))
    }

    fn metrics(&self, _: FontId, _: &[FontVariation]) -> Option<FontMetrics> {
        Some(FontMetrics {
            units_per_em: 1000,
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
        let mut out: Vec<ShapedGlyph> = r
            .text
            .char_indices()
            .map(|(i, c)| ShapedGlyph {
                glyph: if c == '#' && font == FontId(0) {
                    0
                } else {
                    c as u32
                },
                cluster: i,
                dx: 0,
                dy: 0,
                advance: 500,
            })
            .collect();
        if r.rtl {
            out.reverse();
        }
        out
    }

    fn glyph(
        &self,
        _: FontId,
        glyph: u32,
        _: &[FontVariation],
        _: Option<f32>,
    ) -> Option<GlyphData> {
        if glyph == 32 {
            return None;
        }
        let r = Rect::from_xywh(50.0, 0.0, 400.0, 700.0)?;
        Some(GlyphData::Outline(PathBuilder::from_rect(r)))
    }
}

fn opt() -> Options {
    Options {
        fonts: Some(Arc::new(Boxes)),
        ..Options::default()
    }
}

fn doc(body: &str) -> String {
    format!(
        r#"<svg xmlns="http://www.w3.org/2000/svg" xmlns:xlink="http://www.w3.org/1999/xlink" width="100" height="100">{body}</svg>"#
    )
}

fn parse(body: &str) -> Tree {
    Tree::from_str(&doc(body), &opt()).unwrap()
}

fn texts(g: &Group) -> Vec<&Text> {
    let mut out = Vec::new();
    for n in g.children() {
        match n {
            Node::Text(t) => out.push(&**t),
            Node::Group(g) => out.extend(texts(g)),
            _ => {}
        }
    }
    out
}

/// The left edges and baselines of the boxes drawn, in user units.
fn boxes(tree: &Tree) -> Vec<(f32, f32)> {
    let mut out = Vec::new();
    for t in texts(tree.root()) {
        for n in t.flattened().unwrap().children() {
            if let Node::Path(p) = n {
                for seg in p.data().segments() {
                    if let rusty_svgtree::rusty_raster::PathSegment::MoveTo(p) = seg {
                        out.push((p.x, p.y));
                    }
                }
            }
        }
    }
    out
}

fn render(tree: &Tree) -> Vec<u8> {
    let mut p = Pixmap::new(100, 100).unwrap();
    rusty_svgtree::render(tree, Transform::identity(), &mut p.as_mut());
    p.data().to_vec()
}

fn close(a: &[(f32, f32)], b: &[(f32, f32)]) {
    let ok = a.len() == b.len()
        && a.iter()
            .zip(b)
            .all(|(a, b)| (a.0 - b.0).abs() < 1e-3 && (a.1 - b.1).abs() < 1e-3);
    assert!(ok, "{a:?} is not {b:?}");
}

#[test]
fn text_is_in_the_tree_without_fonts() {
    let svg = doc(
        r#"<text id="t" x="10" y="20" font-family="'My Font', serif" font-size="8" fill="red">Hello <tspan font-weight="bold" dy="3">big</tspan> world</text>"#,
    );
    let tree = Tree::from_str(&svg, &Options::default()).unwrap();
    assert_eq!(tree.skipped().text, 1);
    let t = texts(tree.root())[0];
    assert!(t.flattened().is_none() && t.bounding_box().is_none());
    assert_eq!(t.id, "t");
    let chunk = &t.chunks[0];
    assert_eq!((chunk.x, chunk.y), (Some(10.0), Some(20.0)));
    assert_eq!(chunk.text, "Hello big world");
    let parts: Vec<_> = chunk
        .spans
        .iter()
        .map(|s| (&chunk.text[s.start..s.end], s.font.weight))
        .collect();
    assert_eq!(parts, [("Hello ", 400), ("big", 700), (" world", 400)]);
    assert_eq!(
        chunk.spans[0].font.families,
        [FontFamily::Named("My Font".into()), FontFamily::Serif]
    );
    assert_eq!(t.dy[6], 3.0);
    assert!(render(&tree).iter().all(|b| *b == 0));
    assert!(matches!(tree.node_by_id("t"), Some(Node::Text(_))));
}

#[test]
fn glyphs_are_placed() {
    close(
        &boxes(&parse(r#"<text x="10" y="50" font-size="10">ab c</text>"#)),
        &[(10.5, 50.0), (15.5, 50.0), (25.5, 50.0)],
    );
    // 3 characters of 5: the middle at 50, the end at 50.
    close(
        &boxes(&parse(
            r#"<text x="50" y="9" font-size="10" text-anchor="middle">abc</text>"#,
        )),
        &[(43.0, 9.0), (48.0, 9.0), (53.0, 9.0)],
    );
    close(
        &boxes(&parse(
            r#"<text x="50" y="9" font-size="10" text-anchor="end">ab</text>"#,
        )),
        &[(40.5, 9.0), (45.5, 9.0)],
    );
    // Positions and shifts by character; spacing.
    close(
        &boxes(&parse(
            r#"<text x="10 30" y="50" dx="0 1" font-size="10" letter-spacing="2">abc</text>"#,
        )),
        &[(10.5, 50.0), (31.5, 50.0), (38.5, 50.0)],
    );
    close(
        &boxes(&parse(
            r#"<text x="0" y="50" font-size="10" textLength="30">abc</text>"#,
        )),
        &[(0.5, 50.0), (13.0, 50.0), (25.5, 50.0)],
    );
    // Right to left: the first character is the rightmost.
    close(
        &boxes(&parse(
            "<text x=\"0\" y=\"9\" font-size=\"10\">\u{5d0}\u{5d1}</text>",
        )),
        &[(0.5, 9.0), (5.5, 9.0)],
    );
    // Along a path, downwards: each box turned by a quarter.
    let down = boxes(&parse(
        r##"<path id="p" d="M10 0V90" fill="none"/><text font-size="10"><textPath href="#p">a</textPath></text>"##,
    ));
    // (Positions along a path are found to half a unit.)
    assert!(down.len() == 1 && (down[0].0 - 10.0).abs() < 1e-3 && (down[0].1 - 0.5).abs() < 0.6);
    // Superscript and baselines.
    close(
        &boxes(&parse(
            r#"<text y="50" font-size="10">a<tspan baseline-shift="super">b</tspan><tspan dominant-baseline="hanging">c</tspan></text>"#,
        )),
        &[(0.5, 50.0), (5.5, 46.0), (10.5, 56.4)],
    );
}

#[test]
fn fallback_fonts_and_missing_ones() {
    close(
        &boxes(&parse(r#"<text y="9" font-size="10">a#b</text>"#)),
        &[(0.5, 9.0), (5.5, 9.0), (10.5, 9.0)],
    );
    // No font at all: nothing is drawn, and nothing is in the tree.
    let tree = parse(r#"<text y="9" font-family="none">a</text>"#);
    assert!(texts(tree.root()).is_empty());
}

#[test]
fn white_space() {
    let text = |body: &str| {
        let tree = parse(body);
        let out: String = texts(tree.root())
            .iter()
            .flat_map(|t| &t.chunks)
            .map(|c| c.text.as_str())
            .collect();
        out
    };
    assert_eq!(text("<text>  a \n\t b  </text>"), "a b");
    assert_eq!(
        text("<text xml:space=\"preserve\"> a\n b </text>"),
        " a  b "
    );
    assert_eq!(text("<text>a <tspan> b </tspan> c</text>"), "a b c");
    assert_eq!(text("<text>a<!-- x -->b<![CDATA[ c]]></text>"), "ab c");
    assert_eq!(
        text(
            r##"<defs><g id="r"> one<!-- -->two </g></defs><text>a<tref xlink:href="#r"/></text>"##
        ),
        "aonetwo"
    );
    assert_eq!(text("<text>a<a>b</a><title>no</title></text>"), "ab");
}

#[test]
fn paint_servers_take_the_text_box() {
    // The box is 20 x 10 em-units: ascent 8, descent 2.
    let tree = parse(
        r##"<linearGradient id="g"><stop stop-color="red"/><stop offset="1" stop-color="blue"/></linearGradient><text x="10" y="50" font-size="10" fill="url(#g)" text-decoration="underline">ab<tspan fill="lime">cd</tspan></text>"##,
    );
    let t = texts(tree.root())[0];
    let b = t.bounding_box().unwrap();
    assert_eq!(
        (b.left(), b.top(), b.right(), b.bottom()),
        (10.0, 42.0, 30.0, 52.0)
    );
    let p = render(&tree);
    let at = |x: usize, y: usize| &p[(y * 100 + x) * 4..][..4];
    // Red at the left, half way in the second box, lime in the rest.
    assert!(at(11, 47)[0] > 230 && at(11, 47)[2] < 30);
    assert!(at(17, 47)[2] > at(11, 47)[2] + 50);
    assert_eq!(at(22, 47), [0, 255, 0, 255]);
    // The underline goes on below them.
    assert!(at(28, 51)[3] > 0);
}

#[test]
fn text_clips_masks_and_filters() {
    let tree = parse(
        r##"<clipPath id="c"><text x="10" y="50" font-size="40">a</text></clipPath><rect width="100" height="100" fill="red" clip-path="url(#c)"/>"##,
    );
    let p = render(&tree);
    let at = |x: usize, y: usize| p[(y * 100 + x) * 4 + 3];
    assert_eq!((at(20, 40), at(40, 40)), (255, 0));
    let tree = parse(r#"<text x="10" y="50" font-size="40" filter="blur(3px)">a</text>"#);
    let p = render(&tree);
    let at = |x: usize, y: usize| p[(y * 100 + x) * 4 + 3];
    assert!(at(20, 40) > 200 && at(11, 40) > 0 && at(11, 40) < 200);
}

/// Every corpus file written as SVG draws the same.
#[test]
fn writer_round_trip() {
    let dir = std::path::Path::new(env!("CARGO_MANIFEST_DIR")).join("tests/corpus");
    let mut n = 0;
    let mut extra = vec![doc(
        r##"<linearGradient id="g"><stop stop-color="red"/><stop offset="1" stop-color="blue"/></linearGradient><path id="p" d="M10 80Q50 10 90 80"/><text id="t&amp;1" x="10 20 35" y="30" rotate="10 20" font-size="12" fill="url(#g)" stroke="red" text-decoration="underline overline" font-family="'A B', serif">a &lt;<tspan baseline-shift="sub" font-weight="bold">b&amp;c</tspan> d</text><text font-size="9"><textPath href="#p" startOffset="10">on a path</textPath></text><g filter="saturate(1.7) blur(1px)"><text y="90">x</text></g>"##,
    )];
    for e in std::fs::read_dir(dir).unwrap() {
        extra.push(std::fs::read_to_string(e.unwrap().path()).unwrap());
    }
    for svg in extra {
        let tree = Tree::from_str(&svg, &opt()).unwrap();
        for preserve_text in [false, true] {
            let out = tree.to_svg_string(&WriteOptions { preserve_text });
            let again = Tree::from_str(&out, &opt()).unwrap_or_else(|e| panic!("{e}: {out}"));
            assert_eq!(tree.size(), again.size());
            assert!(render(&tree) == render(&again), "{svg}\n---\n{out}");
            n += 1;
        }
    }
    assert!(n > 80);
}

// -------------------------------------------------------------- hostile --

/// Answers nonsense.
#[derive(Debug)]
struct Liar;

impl FontProvider for Liar {
    fn select(&self, _: &Font) -> Option<FontId> {
        Some(FontId(u64::MAX))
    }

    fn fallback(&self, _: char, used: &[FontId]) -> Option<FontId> {
        Some(FontId(used.len() as u64))
    }

    fn metrics(&self, font: FontId, _: &[FontVariation]) -> Option<FontMetrics> {
        Some(FontMetrics {
            units_per_em: if font.0 == 3 { 0 } else { 1 },
            ascent: i16::MIN,
            descent: i16::MAX,
            x_height: u16::MAX,
            underline_position: i16::MIN,
            underline_thickness: u16::MAX,
            line_through_position: i16::MIN,
            subscript_offset: i16::MIN,
            superscript_offset: i16::MAX,
        })
    }

    fn shape(&self, font: FontId, r: &ShapeRequest) -> Vec<ShapedGlyph> {
        (0..r.text.len() % 7 + font.0 as usize % 3)
            .map(|i| ShapedGlyph {
                glyph: (i % 2) as u32,
                cluster: i.wrapping_mul(usize::MAX / 3),
                dx: i32::MIN,
                dy: i32::MAX,
                advance: if i % 2 == 0 { i32::MIN } else { i32::MAX },
            })
            .collect()
    }

    fn glyph(
        &self,
        _: FontId,
        glyph: u32,
        _: &[FontVariation],
        _: Option<f32>,
    ) -> Option<GlyphData> {
        match glyph {
            0 => Some(GlyphData::Svg {
                data: br#"<svg xmlns="http://www.w3.org/2000/svg"><text>a</text><g id="glyph0"><rect width="1e30" height="5"/></g></svg>"#.to_vec(),
                id: None,
                y_up: true,
            }),
            _ => Some(GlyphData::Png(GlyphBitmap {
                data: vec![1, 2, 3],
                width: 0,
                height: u32::MAX,
                x: f32::NAN,
                y: f32::INFINITY,
                pixels_per_em: 0.0,
                sbix_origin: Some((f32::NAN, 0.0)),
            })),
        }
    }
}

#[test]
fn a_provider_that_lies() {
    let opt = Options {
        fonts: Some(Arc::new(Liar)),
        ..Options::default()
    };
    for body in [
        "a",
        "a\u{5d0}b",
        "ab<tspan>cde\u{301}f</tspan>g",
        "<tspan x='1 2 3'>\u{1f600}x</tspan>",
        "\u{2029}a\u{2029}",
        "\u{202e}abc\u{202c}d",
    ] {
        for attrs in [
            "",
            "textLength='5' lengthAdjust='spacingAndGlyphs'",
            "letter-spacing='-1e30' word-spacing='1e30'",
            "writing-mode='tb' text-decoration='underline overline line-through'",
            "font-size='1e30' rotate='1e30 NaN'",
        ] {
            let svg = doc(&format!("<text y='5' {attrs}>{body}</text>"));
            if let Ok(tree) = Tree::from_str(&svg, &opt) {
                render(&tree);
                tree.to_svg_string(&WriteOptions::default());
            }
        }
    }
}

#[test]
fn hostile_text_is_bounded() {
    let start = std::time::Instant::now();
    let long = "a b ".repeat(50_000);
    let deep = format!(
        "<text>{}x{}</text>",
        "<tspan rotate='1 2' x='1'>".repeat(900),
        "</tspan>".repeat(900)
    );
    let too_deep = format!(
        "<text>{}x{}</text>",
        "<tspan>".repeat(5000),
        "</tspan>".repeat(5000)
    );
    let many_spans = format!("<text>{}</text>", "<tspan>ab</tspan>".repeat(20_000));
    let many_positions = format!("<text x='{}'>{long}</text>", "1 ".repeat(200_000));
    let mut trefs = format!("<defs><g id='r'>{long}</g></defs><text>");
    trefs.push_str(&"<tref xlink:href='#r'/>".repeat(2000));
    trefs.push_str("</text>");
    let mut uses = format!("<text id='r'>{long}</text>");
    uses.push_str(&"<use href='#r'/>".repeat(1000));
    let on_path = format!(
        "<path id='p' d='M0 0{}'/><text><textPath href='#p'>{}</textPath></text>",
        "Q9 1 2 3T5 5".repeat(3000),
        "ab".repeat(2000)
    );
    let cycle = "<text id='a'><textPath href='#a'>x</textPath><tref xlink:href='#a'/></text>";
    for (name, body) in [
        ("deep", deep.as_str()),
        ("too deep", &too_deep),
        ("many spans", &many_spans),
        ("many positions", &many_positions),
        ("trefs", &trefs),
        ("uses", &uses),
        ("on path", &on_path),
        ("cycle", cycle),
    ] {
        for opt in [opt(), Options::default()] {
            let t = std::time::Instant::now();
            match Tree::from_str(&doc(body), &opt) {
                Ok(tree) => {
                    render(&tree);
                }
                Err(Error::ElementsLimitReached) => {}
                // The XML parser has its own limit.
                Err(e) => assert_eq!(name, "too deep", "{e}"),
            }
            println!("{name}: {:?}", t.elapsed());
        }
    }
    assert!(start.elapsed().as_secs() < 120, "{:?}", start.elapsed());
}

#[test]
fn extreme_numbers_in_text() {
    for v in ["1e9", "1e38", "-1e38", "0", "-5", "NaN", "1e-38"] {
        for name in [
            "x",
            "y",
            "dx",
            "dy",
            "rotate",
            "font-size",
            "letter-spacing",
            "word-spacing",
            "textLength",
            "baseline-shift",
            "stroke-width",
            "font-weight",
            "kerning",
        ] {
            let tree = Tree::from_str(
                &doc(&format!(
                    "<path id='p' d='M0 0h{v}'/><text stroke='red' {name}='{v}'>ab <tspan {name}='{v} 1 {v}' textLength='{v}'>c</tspan></text><text {name}='{v}'><textPath href='#p' startOffset='{v}'>xy</textPath></text>"
                )),
                &opt(),
            );
            if let Ok(tree) = tree {
                render(&tree);
                let out = tree.to_svg_string(&WriteOptions {
                    preserve_text: true,
                });
                let _ = Tree::from_str(&out, &opt());
            }
        }
    }
}
