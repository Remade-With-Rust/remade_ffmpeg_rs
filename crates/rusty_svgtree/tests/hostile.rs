//! Hostile inputs: every case must return an error or render a clamped
//! result, never panic, overflow the stack or allocate without bound.

use rusty_svgtree::rusty_raster::{Pixmap, Transform};
use rusty_svgtree::{Error, Options, Tree};

/// Parses and, if that succeeds, renders at a bounded size.
fn run(svg: &[u8]) -> Result<Tree, Error> {
    run_with(svg, &Options::default())
}

fn run_with(svg: &[u8], opt: &Options) -> Result<Tree, Error> {
    let tree = Tree::from_data(svg, opt)?;
    let w = (tree.size().width().ceil() as u32).clamp(1, 256);
    let h = (tree.size().height().ceil() as u32).clamp(1, 256);
    let mut pixmap = Pixmap::new(w, h).unwrap();
    rusty_svgtree::render(&tree, Transform::identity(), &mut pixmap.as_mut());
    Ok(tree)
}

const H: &str = r#"<svg xmlns="http://www.w3.org/2000/svg" xmlns:xlink="http://www.w3.org/1999/xlink" width="100" height="100">"#;

fn doc(body: &str) -> Vec<u8> {
    format!("{H}{body}</svg>").into_bytes()
}

#[test]
fn use_self_reference() {
    let r = run(&doc(
        r##"<use id="u" href="#u"/><rect width="10" height="10"/>"##,
    ));
    assert!(r.is_ok());
}

#[test]
fn use_mutual_cycle() {
    let r = run(&doc(
        r##"<g id="a"><use href="#b"/></g><g id="b"><use href="#a"/></g><rect width="5" height="5"/>"##,
    ));
    assert!(r.is_ok());
}

#[test]
fn use_three_cycle() {
    let r = run(&doc(
        r##"<g id="a"><use href="#b"/></g><g id="b"><use href="#c"/></g><g id="c"><use href="#a"/><rect width="5" height="5"/></g>"##,
    ));
    // Bounded by the depth / node limits: either outcome is fine, no panic.
    let _ = r;
}

#[test]
fn use_deep_chain() {
    // 2000 chained uses: deeper than `max_depth`.
    let mut body = String::from(r#"<rect id="u0" width="1" height="1"/>"#);
    for i in 1..2000 {
        body.push_str(&format!(r##"<use id="u{i}" href="#u{}"/>"##, i - 1));
    }
    let r = run(&doc(&body));
    assert!(matches!(r, Ok(_) | Err(Error::ElementsLimitReached)));
}

#[test]
fn use_exponential_amplification() {
    // "Billion laughs" through references: 10 uses per level, 9 levels.
    let mut body = String::from(r#"<defs><rect id="l0" width="1" height="1"/>"#);
    for i in 1..10 {
        body.push_str(&format!(r#"<g id="l{i}">"#));
        for _ in 0..10 {
            body.push_str(&format!(r##"<use href="#l{}"/>"##, i - 1));
        }
        body.push_str("</g>");
    }
    body.push_str(r##"</defs><use href="#l9"/>"##);
    let t = std::time::Instant::now();
    let r = run(&doc(&body));
    assert_eq!(r.err(), Some(Error::ElementsLimitReached));
    assert!(t.elapsed().as_secs() < 30);
}

#[test]
fn gradient_href_cycles() {
    let r = run(&doc(
        r##"<linearGradient id="a" href="#b"/><linearGradient id="b" href="#a"/>
        <linearGradient id="s" href="#s"/>
        <rect width="50" height="50" fill="url(#a)"/><rect x="50" width="50" height="50" fill="url(#s)"/>"##,
    ));
    assert!(r.is_ok());
}

#[test]
fn clip_path_cycles() {
    let r = run(&doc(
        r##"<clipPath id="c1"><rect width="10" height="10" clip-path="url(#c1)"/></clipPath>
        <clipPath id="c2" clip-path="url(#c3)"><rect width="10" height="10"/></clipPath>
        <clipPath id="c3" clip-path="url(#c2)"><rect width="10" height="10"/></clipPath>
        <clipPath id="d1" clip-path="url(#d2)"><rect width="10" height="10"/></clipPath>
        <clipPath id="d2" clip-path="url(#d3)"><rect width="10" height="10"/></clipPath>
        <clipPath id="d3" clip-path="url(#d1)"><rect width="10" height="10"/></clipPath>
        <rect width="50" height="50" clip-path="url(#c1)"/>
        <rect width="50" height="50" clip-path="url(#c2)"/>
        <rect width="50" height="50" clip-path="url(#d1)"/>"##,
    ));
    assert!(r.is_ok());
}

#[test]
fn mask_cycles() {
    let r = run(&doc(
        r##"<mask id="m1" mask="url(#m2)"><rect width="10" height="10" fill="white"/></mask>
        <mask id="m2" mask="url(#m1)"><rect width="10" height="10" fill="white" mask="url(#m2)"/></mask>
        <rect width="50" height="50" mask="url(#m1)"/>"##,
    ));
    assert!(r.is_ok());
}

#[test]
fn pattern_self_reference() {
    let r = run(&doc(
        r##"<pattern id="p" patternUnits="userSpaceOnUse" width="10" height="10"><rect width="5" height="5" fill="url(#p)"/></pattern>
        <pattern id="q" patternUnits="userSpaceOnUse" width="10" height="10"><rect width="5" height="5" fill="url(#r)"/></pattern>
        <pattern id="r" patternUnits="userSpaceOnUse" width="10" height="10"><rect width="5" height="5" fill="url(#q)"/></pattern>
        <rect width="50" height="50" fill="url(#p)"/><rect width="50" height="50" fill="url(#q)"/>"##,
    ));
    assert!(r.is_ok());
}

#[test]
fn huge_dimensions_error() {
    let svg = br#"<svg xmlns="http://www.w3.org/2000/svg" width="1000000000" height="1000000000"><rect width="10" height="10"/></svg>"#;
    // Nothing is allocated for a document's size: it is an error only past a
    // limit the caller sets.
    assert!(run(svg).is_ok());
    let opt = Options {
        max_canvas_pixels: 1 << 28,
        ..Options::default()
    };
    assert_eq!(run_with(svg, &opt).err(), Some(Error::InvalidSize));
    let svg = br#"<svg xmlns="http://www.w3.org/2000/svg" width="1e40" height="10"/>"#;
    assert!(run(svg).is_err());
    let svg = br#"<svg xmlns="http://www.w3.org/2000/svg" width="-5" height="10"/>"#;
    assert_eq!(run(svg).err(), Some(Error::InvalidSize));
    let svg = br#"<svg xmlns="http://www.w3.org/2000/svg" width="0" height="10"/>"#;
    assert_eq!(run(svg).err(), Some(Error::InvalidSize));
}

/// The tiny-viewBox stroke case panics resvg 0.48.1 / tiny-skia 0.12
/// (`blit_anti_h` slice index out of range); it must not panic here.
#[test]
fn giant_and_tiny_viewbox() {
    let svg = br#"<svg xmlns="http://www.w3.org/2000/svg" width="100" height="100" viewBox="0 0 1e30 1e30"><rect width="1e30" height="5e29" fill="red"/><circle cx="1e29" cy="1e29" r="1e28" stroke="black" stroke-width="1e27"/></svg>"#;
    assert!(run(svg).is_ok());
    let svg = br#"<svg xmlns="http://www.w3.org/2000/svg" width="100" height="100" viewBox="0 0 1e-30 1e-30"><rect width="1" height="1" fill="red"/><circle cx="1e-31" cy="1e-31" r="1e-31" stroke="black"/></svg>"#;
    assert!(run(svg).is_ok());
}

#[test]
fn deep_xml_nesting() {
    // 100k nested groups: rejected (by rusty_xml's depth limit or ours),
    // without exhausting the stack.
    let n = 100_000;
    let mut s = String::from(H);
    for _ in 0..n {
        s.push_str("<g>");
    }
    for _ in 0..n {
        s.push_str("</g>");
    }
    s.push_str("</svg>");
    assert!(run(s.as_bytes()).is_err());
}

#[test]
fn nested_isolated_groups_respect_layer_budget() {
    // 200 nested opacity groups would want 200 full-canvas layers.
    let mut s =
        String::from(r#"<svg xmlns="http://www.w3.org/2000/svg" width="256" height="256">"#);
    for _ in 0..200 {
        s.push_str(r#"<g opacity="0.99">"#);
    }
    s.push_str(r#"<rect width="256" height="256" fill="blue"/>"#);
    for _ in 0..200 {
        s.push_str("</g>");
    }
    s.push_str("</svg>");
    let opt = Options {
        max_layer_bytes: 4 * 1024 * 1024,
        ..Options::default()
    };
    assert!(run_with(s.as_bytes(), &opt).is_ok());
}

#[test]
fn malformed_xml() {
    for bad in [
        &b""[..],
        b"   ",
        b"<svg",
        b"<svg xmlns=\"http://www.w3.org/2000/svg\"><rect></svg>",
        b"\xff\xfe\x00garbage",
        b"<html/>",
        b"<?xml version=\"1.0\"?>",
        b"<svg:svg xmlns:svg=\"urn:not-svg\"/>",
        b"<svg xmlns=\"http://www.w3.org/2000/svg\" width=\"10\" height=\"10\">&undefined;</svg>",
    ] {
        assert!(run(bad).is_err(), "{:?}", String::from_utf8_lossy(bad));
    }
}

#[test]
fn malformed_values() {
    let r = run(&doc(r##"
        <rect width="NaN" height="10"/><rect width="inf" height="10"/>
        <rect width="1e999" height="1e999"/><rect x="--5" width="10" height="10"/>
        <circle r="-5"/><ellipse rx="1e38" ry="1e38" fill="red"/>
        <path d="M 1e38 1e38 L -1e38 -1e38 L 1e38 -1e38 Z" stroke="black" stroke-width="1e38"/>
        <path d="M0 0 A1e300 1e300 0 1 1 50 50 a 1e-300 1e-300 0 0 0 1 1"/>
        <path d="M0 0 L 10 10 L"/><path d="garbage"/><path d="M 0,0 z z z m 5 5 l 1e1000 0"/>
        <rect width="10" height="10" transform="matrix(0 0 0 0 0 0)"/>
        <rect width="10" height="10" transform="scale(1e30) rotate(1e30)"/>
        <rect width="10" height="10" transform="translate(nan)"/>
        <rect width="10" height="10" fill="rgb(1e30, -1e30, 50%)" stroke="url(#missing) red"/>
        <rect width="10" height="10" stroke-dasharray="1e-30" stroke="black"/>
        <rect width="10" height="10" stroke-dasharray="-1 2" stroke="black"/>
        <rect width="10" height="10" opacity="-3" fill-opacity="9"/>
        <rect width="10" height="10" style=";;:;fill:;stroke:url(;)"/>
        <polygon points="1 2 3"/><polyline points="nan nan 1 1 2 2"/>
    "##));
    assert!(r.is_ok());
}

#[test]
fn entity_bomb_rejected() {
    let svg = br#"<?xml version="1.0"?>
<!DOCTYPE svg [
<!ENTITY a "aaaaaaaaaaaaaaaaaaaaaaaaaaaaaaaaaaaaaaaaaaaaaaaaaaaaaaaaaaaaaaaaaaaaaaaaaaaaaaaaaaaaaaaaaaaaaaaaaaaaa">
<!ENTITY b "&a;&a;&a;&a;&a;&a;&a;&a;&a;&a;">
<!ENTITY c "&b;&b;&b;&b;&b;&b;&b;&b;&b;&b;">
<!ENTITY d "&c;&c;&c;&c;&c;&c;&c;&c;&c;&c;">
<!ENTITY e "&d;&d;&d;&d;&d;&d;&d;&d;&d;&d;">
<!ENTITY f "&e;&e;&e;&e;&e;&e;&e;&e;&e;&e;">
<!ENTITY g "&f;&f;&f;&f;&f;&f;&f;&f;&f;&f;">
<!ENTITY h "&g;&g;&g;&g;&g;&g;&g;&g;&g;&g;">
]>
<svg xmlns="http://www.w3.org/2000/svg" width="10" height="10"><desc>&h;</desc></svg>"#;
    let t = std::time::Instant::now();
    let _ = run(svg);
    assert!(t.elapsed().as_secs() < 30);
}

#[test]
fn external_entity_not_loaded() {
    let svg = br#"<?xml version="1.0"?>
<!DOCTYPE svg [<!ENTITY x SYSTEM "file:///etc/passwd">]>
<svg xmlns="http://www.w3.org/2000/svg" width="10" height="10"><rect width="10" height="10" fill="&x;"/></svg>"#;
    // Either rejected or parsed without the external content; never loaded.
    let _ = run(svg);
}

#[test]
fn huge_image_header_skipped() {
    // A PNG whose IHDR claims 100000 x 100000 pixels (no image data).
    let mut png = b"\x89PNG\r\n\x1a\n\x00\x00\x00\x0dIHDR".to_vec();
    png.extend_from_slice(&100_000u32.to_be_bytes());
    png.extend_from_slice(&100_000u32.to_be_bytes());
    png.extend_from_slice(&[8, 6, 0, 0, 0]);
    let crc = crc32(&png[12..]);
    png.extend_from_slice(&crc.to_be_bytes());
    let b64 = base64(&png);
    let r = run(&doc(&format!(
        r#"<image width="50" height="50" href="data:image/png;base64,{b64}"/>"#
    )));
    assert!(r.is_ok());
}

#[test]
fn huge_pattern_tile_refused() {
    let r = run(&doc(
        r##"<pattern id="p" patternUnits="userSpaceOnUse" width="100000000" height="100000000"><rect width="5" height="5"/></pattern>
        <rect width="100" height="100" fill="url(#p)"/>"##,
    ));
    assert!(r.is_ok());
}

#[test]
fn dense_dashes_bounded() {
    let t = std::time::Instant::now();
    let r = run(&doc(
        r#"<path d="M0 0 L100000 100000" stroke="black" stroke-dasharray="0.001 0.001"/>"#,
    ));
    assert!(r.is_ok());
    assert!(t.elapsed().as_secs() < 60);
}

#[test]
fn css_hostile() {
    let mut sheet = String::new();
    for i in 0..2000 {
        sheet.push_str(&format!(
            ".c{i} g g g g g g g g > rect + rect {{ fill: red }} "
        ));
    }
    sheet.push_str("@media { { { { } ] ) /* unterminated");
    let r = run(&doc(&format!(
        "<style>{sheet}</style><g class=\"c1\"><rect width=\"1\" height=\"1\"/></g>"
    )));
    assert!(r.is_ok());
}

/// Deterministic byte-mutation fuzzing over the feature corpus: parse and
/// render every mutant; any panic fails the test.
#[test]
fn mutation_fuzz_corpus() {
    // `RSVG_FUZZ_DIR` / `RSVG_FUZZ_ITERS` widen the run (seed files, mutants
    // per file); the defaults keep it to a couple of seconds.
    let dir = std::env::var_os("RSVG_FUZZ_DIR").map_or_else(
        || std::path::Path::new(env!("CARGO_MANIFEST_DIR")).join("tests/corpus"),
        std::path::PathBuf::from,
    );
    let iters: usize = std::env::var("RSVG_FUZZ_ITERS")
        .ok()
        .and_then(|v| v.parse().ok())
        .unwrap_or(60);
    let mut files: Vec<_> = std::fs::read_dir(&dir)
        .unwrap()
        .filter_map(Result::ok)
        .map(|e| e.path())
        .collect();
    files.sort();
    let mut state: u64 = 0x9e37_79b9_7f4a_7c15;
    let mut next = move || {
        state ^= state << 13;
        state ^= state >> 7;
        state ^= state << 17;
        state
    };
    let tokens: &[&[u8]] = &[
        b"<",
        b">",
        b"\"",
        b"#",
        b"url(#",
        b"-1e38",
        b"NaN",
        b"0",
        b" ",
        b"</g>",
        b"<g>",
        b"<use href=\"#a\"/>",
        b"9999999",
        b"z",
        b"A",
        b"%",
    ];
    let mut runs = 0usize;
    for path in &files {
        let base = std::fs::read(path).unwrap();
        for _ in 0..iters {
            let mut m = base.clone();
            let edits = 1 + (next() % 8) as usize;
            for _ in 0..edits {
                if m.is_empty() {
                    break;
                }
                let pos = (next() % m.len() as u64) as usize;
                match next() % 3 {
                    0 => m[pos] = (next() & 0xff) as u8,
                    1 => {
                        let t = tokens[(next() % tokens.len() as u64) as usize];
                        m.splice(pos..pos, t.iter().copied());
                    }
                    _ => {
                        let end = (pos + 1 + (next() % 16) as usize).min(m.len());
                        m.drain(pos..end);
                    }
                }
            }
            let _ = run(&m);
            runs += 1;
        }
    }
    println!("mutants run: {runs}");
    assert!(runs >= 1000, "{runs}");
}

fn crc32(data: &[u8]) -> u32 {
    let mut c = 0xffff_ffffu32;
    for &b in data {
        c ^= u32::from(b);
        for _ in 0..8 {
            c = if c & 1 != 0 {
                0xedb8_8320 ^ (c >> 1)
            } else {
                c >> 1
            };
        }
    }
    !c
}

fn base64(data: &[u8]) -> String {
    const T: &[u8; 64] = b"ABCDEFGHIJKLMNOPQRSTUVWXYZabcdefghijklmnopqrstuvwxyz0123456789+/";
    let mut out = String::new();
    for ch in data.chunks(3) {
        let b = [ch[0], *ch.get(1).unwrap_or(&0), *ch.get(2).unwrap_or(&0)];
        let n = (u32::from(b[0]) << 16) | (u32::from(b[1]) << 8) | u32::from(b[2]);
        out.push(T[(n >> 18) as usize & 63] as char);
        out.push(T[(n >> 12) as usize & 63] as char);
        out.push(if ch.len() > 1 {
            T[(n >> 6) as usize & 63] as char
        } else {
            '='
        });
        out.push(if ch.len() > 2 {
            T[n as usize & 63] as char
        } else {
            '='
        });
    }
    out
}

/// One big `objectBoundingBox` clip path referenced by many elements is
/// re-converted per reference; the conversion budget bounds the total.
#[test]
fn shared_obb_clip_amplification_bounded() {
    let mut body = String::from(r#"<clipPath id="c" clipPathUnits="objectBoundingBox">"#);
    for _ in 0..20_000 {
        body.push_str(r#"<rect width="0.5" height="0.5"/>"#);
    }
    body.push_str("</clipPath>");
    for _ in 0..20_000 {
        body.push_str(r##"<rect width="10" height="10" clip-path="url(#c)"/>"##);
    }
    let t = std::time::Instant::now();
    let opt = Options {
        max_layer_bytes: 64 * 1024 * 1024,
        ..Options::default()
    };
    let _ = Tree::from_data(&doc(&body), &opt);
    assert!(t.elapsed().as_secs() < 60);
}

/// A descendant-combinator chain over a deep tree backtracks exponentially
/// without the matching budget.
#[test]
fn css_backtracking_bounded() {
    // `foo` never matches, so every ancestor combination is tried.
    let sel = "foo ".to_string() + &"g ".repeat(40) + "rect";
    let mut body = format!("<style>{sel} {{ fill: red }}</style>");
    for _ in 0..200 {
        body.push_str("<g>");
    }
    for _ in 0..200 {
        body.push_str(r#"<rect width="1" height="1"/>"#);
    }
    for _ in 0..200 {
        body.push_str("</g>");
    }
    let t = std::time::Instant::now();
    let _ = Tree::from_data(&doc(&body), &Options::default());
    assert!(t.elapsed().as_secs() < 60);
}

/// Values that overflow inside rect/size helpers (tiny-skia-path's
/// `bbox_transform`, `scale_to` and `to_non_zero_rect` unwrap internally).
#[test]
fn overflowing_geometry_helpers() {
    let mut png = b"\x89PNG\r\n\x1a\n\x00\x00\x00\x0dIHDR".to_vec();
    png.extend_from_slice(&2u32.to_be_bytes());
    png.extend_from_slice(&2u32.to_be_bytes());
    png.extend_from_slice(&[8, 6, 0, 0, 0]);
    let crc = crc32(&png[12..]);
    png.extend_from_slice(&crc.to_be_bytes());
    let b64 = base64(&png);
    let r = run(&doc(&format!(
        r##"<image x="3e38" y="3e38" width="3e38" height="1e-30" href="data:image/png;base64,{b64}"/>
        <image x="-3e38" width="3e38" height="3e38" preserveAspectRatio="xMaxYMax slice" href="data:image/png;base64,{b64}"/>
        <mask id="m" x="3e38" y="3e38" width="3e38" height="3e38"><rect width="10" height="10" fill="white"/></mask>
        <pattern id="p" x="3e38" width="3e38" height="0.5"><rect width="1" height="1"/></pattern>
        <rect x="1e38" width="3e38" height="3e38" mask="url(#m)" fill="url(#p)"/>"##
    )));
    assert!(r.is_ok());
}

// -------------------------------------------------------------- filters --

/// Renders a filtered document under a deadline.
fn run_filter(body: &str) {
    let start = std::time::Instant::now();
    let _ = run(&doc(body));
    let t = start.elapsed();
    assert!(
        t.as_secs() < 60,
        "{t:?} for {}",
        &body[..body.len().min(200)]
    );
}

#[test]
fn filter_chain_of_2000_primitives() {
    let mut f = String::from(r#"<filter id="f">"#);
    // Unoptimised kernels are some 30 times as slow.
    let n = if cfg!(debug_assertions) { 100 } else { 2000 };
    for _ in 0..n {
        f.push_str(r#"<feGaussianBlur stdDeviation="3"/><feOffset dx="1"/>"#);
    }
    run_filter(&format!(
        r##"{f}</filter><rect width="90" height="90" filter="url(#f)"/>"##
    ));
}

#[test]
fn filter_results_all_kept_alive() {
    // Every result is read again by the last primitive.
    let mut f = String::from(r#"<filter id="f">"#);
    for i in 0..3000 {
        f.push_str(&format!(r#"<feFlood result="r{i}"/>"#));
    }
    f.push_str("<feMerge>");
    for i in 0..3000 {
        f.push_str(&format!(r#"<feMergeNode in="r{i}"/>"#));
    }
    let svg = doc(&format!(
        r##"{f}</feMerge></filter><rect width="90" height="90" filter="url(#f)"/>"##
    ));
    let opt = Options {
        max_layer_bytes: 4 << 20,
        ..Options::default()
    };
    let _ = run_with(&svg, &opt);
}

#[test]
fn filter_huge_numbers() {
    let huge = [
        "1e9", "1e38", "1e300", "-1e38", "NaN", "inf", "0", "-0", "1e-38",
    ];
    for v in huge {
        for fe in [
            format!(r#"<feGaussianBlur stdDeviation="{v}"/>"#),
            format!(r#"<feGaussianBlur stdDeviation="{v} 1"/>"#),
            format!(r#"<feDropShadow dx="{v}" dy="{v}" stdDeviation="{v}"/>"#),
            format!(r#"<feOffset dx="{v}" dy="{v}"/>"#),
            format!(r#"<feMorphology radius="{v}"/>"#),
            format!(r#"<feMorphology operator="dilate" radius="{v} 2"/>"#),
            format!(r#"<feTurbulence baseFrequency="{v}" numOctaves="{v}" seed="{v}"/>"#),
            format!(
                r#"<feTurbulence baseFrequency="{v}" numOctaves="9" seed="{v}" stitchTiles="stitch" type="fractalNoise"/>"#
            ),
            format!(r#"<feConvolveMatrix order="{v}" kernelMatrix="1 1 1" bias="{v}"/>"#),
            format!(
                r#"<feConvolveMatrix order="3 1" kernelMatrix="{v} 1 1" divisor="{v}" targetX="{v}"/>"#
            ),
            format!(r#"<feDisplacementMap in2="SourceGraphic" scale="{v}" xChannelSelector="R"/>"#),
            format!(r#"<feFlood x="{v}" y="{v}" width="{v}" height="{v}"/><feTile/>"#),
            format!(r#"<feComposite operator="arithmetic" k1="{v}" k2="{v}" k3="{v}" k4="{v}"/>"#),
            format!(r#"<feColorMatrix type="hueRotate" values="{v}"/>"#),
            format!(r#"<feColorMatrix type="saturate" values="{v}"/>"#),
            format!(
                r#"<feComponentTransfer><feFuncR type="gamma" amplitude="{v}" exponent="{v}" offset="{v}"/><feFuncG type="table" tableValues="{v} {v}"/></feComponentTransfer>"#
            ),
            format!(
                r#"<feDiffuseLighting surfaceScale="{v}" diffuseConstant="{v}"><feDistantLight azimuth="{v}" elevation="{v}"/></feDiffuseLighting>"#
            ),
            format!(
                r#"<feSpecularLighting surfaceScale="{v}" specularExponent="{v}"><feSpotLight x="{v}" y="{v}" z="{v}" pointsAtX="{v}" limitingConeAngle="{v}" specularExponent="{v}"/></feSpecularLighting>"#
            ),
            format!(r##"<feImage href="#t" x="{v}" width="{v}"/>"##),
        ] {
            for units in ["", r#"primitiveUnits="objectBoundingBox""#] {
                run_filter(&format!(
                    r##"<filter id="f" {units}>{fe}</filter><rect id="t" width="40" height="40" fill="red"/><rect x="9.5" y="9.25" width="50" height="60" filter="url(#f)"/>"##
                ));
            }
        }
        for f in [
            "blur",
            "hue-rotate",
            "brightness",
            "sepia",
            "invert",
            "opacity",
        ] {
            run_filter(&format!(
                r##"<rect width="50" height="50" filter="{f}({v})"/>"##
            ));
        }
        run_filter(&format!(
            r##"<rect width="50" height="50" filter="drop-shadow({v}px {v}px {v}px)"/>"##
        ));
    }
}

#[test]
fn filter_huge_regions() {
    for v in ["1e9", "1e30", "4294967296", "-1e9"] {
        for units in ["userSpaceOnUse", "objectBoundingBox"] {
            run_filter(&format!(
                r##"<filter id="f" filterUnits="{units}" x="-{v}" y="-{v}" width="{v}" height="{v}"><feFlood/><feGaussianBlur stdDeviation="50"/></filter><rect width="1e8" height="10" filter="url(#f)" transform="scale(1e5)"/>"##
            ));
            run_filter(&format!(
                r##"<filter id="f" filterUnits="{units}" x="-{v}" y="-5" width="2e30" height="{v}"><feTurbulence numOctaves="9" baseFrequency=".1"/></filter><rect width="90" height="90" filter="url(#f)"/>"##
            ));
        }
    }
}

#[test]
fn filter_work_budget() {
    // A 300x300 kernel on a full canvas, and a stack of filtered groups:
    // past the budget the element is not drawn, and the time is bounded.
    let kernel = "1 ".repeat(300 * 300);
    let mut body = format!(
        r##"<filter id="f" x="0" y="0" width="1" height="1"><feConvolveMatrix order="300" kernelMatrix="{kernel}"/></filter>"##
    );
    for _ in 0..200 {
        body.push_str(r##"<rect width="100" height="100" filter="url(#f)"/>"##);
    }
    let opt = Options {
        max_filter_work: 1 << 28,
        ..Options::default()
    };
    let start = std::time::Instant::now();
    let _ = run_with(&doc(&body), &opt);
    assert!(start.elapsed().as_secs() < 60);
}

#[test]
fn filter_image_cycles() {
    // Directly, through a second filter, and through itself in a chain that
    // would double at every step.
    run_filter(
        r##"<filter id="f"><feImage href="#r"/></filter><rect id="r" width="50" height="50" filter="url(#f)"/>"##,
    );
    run_filter(
        r##"<filter id="a"><feImage href="#y"/></filter><filter id="b"><feImage href="#x"/></filter><rect id="x" width="50" height="50" filter="url(#a)"/><rect id="y" width="50" height="50" filter="url(#b)"/>"##,
    );
    let mut body = String::from(r#"<rect id="e0" width="10" height="10"/>"#);
    for i in 1..60 {
        for units in ["objectBoundingBox", "userSpaceOnUse"] {
            body.push_str(&format!(
                r##"<filter id="{units}{i}" filterUnits="{units}" x="0" y="0" width="20" height="20"><feImage href="#e{p}"/><feImage href="#e{p}" result="q"/><feMerge><feMergeNode in="q"/><feMergeNode/></feMerge></filter>"##,
                p = i - 1
            ));
        }
        body.push_str(&format!(
            r##"<g id="e{i}"><rect width="10" height="10" filter="url(#userSpaceOnUse{i})"/><rect width="10" height="10" filter="url(#objectBoundingBox{i})"/></g>"##
        ));
    }
    let opt = Options {
        max_filter_work: 1 << 28,
        max_nodes: 20_000,
        ..Options::default()
    };
    let start = std::time::Instant::now();
    let _ = run_with(&doc(&body), &opt);
    assert!(start.elapsed().as_secs() < 60, "{:?}", start.elapsed());
}

#[test]
fn filter_many_users_of_one_large_filter() {
    let mut f = String::from(r#"<filter id="f">"#);
    for _ in 0..2000 {
        f.push_str("<feMerge><feMergeNode/><feMergeNode/></feMerge>");
    }
    f.push_str("</filter>");
    for _ in 0..3000 {
        f.push_str(r##"<rect width="1" height="1" filter="url(#f)"/>"##);
    }
    let opt = Options {
        max_filter_work: 1 << 28,
        ..Options::default()
    };
    let start = std::time::Instant::now();
    let _ = run_with(&doc(&f), &opt);
    assert!(start.elapsed().as_secs() < 60, "{:?}", start.elapsed());
}

#[test]
fn filter_external_references_are_not_followed() {
    let dir = std::env::temp_dir().join("rusty_svgtree_filter_canary.png");
    let mut red = Pixmap::new(8, 8).unwrap();
    red.fill(rusty_svgtree::rusty_raster::Color::from_rgba8(
        255, 0, 0, 255,
    ));
    if red.save_png(&dir).is_err() {
        return;
    }
    let path = dir.to_string_lossy().replace('\\', "/");
    for href in [
        path.clone(),
        format!("file:///{path}"),
        "http://127.0.0.1:9/x.png".to_string(),
        "//127.0.0.1/share/x.png".to_string(),
        "/dev/zero".to_string(),
    ] {
        let svg = doc(&format!(
            r##"<filter id="f"><feImage href="{href}"/></filter><rect width="90" height="90" fill="blue" filter="url(#f)"/><image href="{href}" width="90" height="90"/>"##
        ));
        let tree = Tree::from_data(&svg, &Options::default()).unwrap();
        let mut p = Pixmap::new(100, 100).unwrap();
        rusty_svgtree::render(&tree, Transform::identity(), &mut p.as_mut());
        assert!(p.data().chunks_exact(4).all(|c| c[0] == 0), "{href}");
    }
    let _ = std::fs::remove_file(&dir);
}

// ------------------------------------------------------------- nesting --

/// Runs `f` on a thread with a 1 MiB stack (a Windows main thread's) in
/// release; debug frames are several times as large.
fn on_small_stack(f: impl FnOnce() + Send + 'static) {
    let size = if cfg!(debug_assertions) { 8 } else { 1 } << 20;
    std::thread::Builder::new()
        .stack_size(size)
        .spawn(f)
        .unwrap()
        .join()
        .unwrap();
}

fn wrap(open: &str, close: &str, n: usize, inner: &str) -> String {
    format!("{}{inner}{}", open.repeat(n), close.repeat(n))
}

#[test]
fn deep_legal_nesting_renders() {
    on_small_stack(|| {
        for open in [
            "<g>",
            r#"<g opacity=".999">"#,
            r#"<g transform="translate(0 .01)">"#,
        ] {
            let svg = doc(&wrap(
                open,
                "</g>",
                1000,
                r#"<rect width="90" height="90"/>"#,
            ));
            let r = run(&svg);
            assert!(r.is_ok(), "{open}: {:?}", r.err());
        }
        for open in ["<svg>", r#"<a><switch>"#] {
            let close = if open == "<svg>" {
                "</svg>"
            } else {
                "</switch></a>"
            };
            for n in [200, 250, 1000, 100_000] {
                let _ = run(&doc(&wrap(
                    open,
                    close,
                    n,
                    r#"<rect width="9" height="9"/>"#,
                )));
            }
        }
    });
}

#[test]
fn deep_content_behind_every_reference() {
    // Each of 40 masks (clip paths, patterns, filters) holds 1000 nested
    // groups with the reference to the next at the bottom.
    on_small_stack(|| {
        for kind in ["mask", "clipPath", "pattern", "marker", "filter"] {
            let mut body = String::new();
            for i in 0..40 {
                let reference = match kind {
                    "mask" => format!(r#"mask="url(#m{})""#, i + 1),
                    "clipPath" => format!(r#"clip-path="url(#m{})""#, i + 1),
                    "pattern" => format!(r#"fill="url(#m{})""#, i + 1),
                    "marker" => format!(r#"marker-end="url(#m{})""#, i + 1),
                    _ => format!(r#"filter="url(#m{})""#, i + 1),
                };
                let content = wrap(
                    "<g>",
                    "</g>",
                    1000,
                    &format!(r#"<path d="M0 0L9 0 9 9z" {reference}/>"#),
                );
                body.push_str(&if kind == "filter" {
                    format!(
                        r##"<filter id="m{i}" filterUnits="userSpaceOnUse" x="0" y="0" width="9" height="9"><feImage href="#e{i}"/></filter><g id="e{i}">{content}</g>"##
                    )
                } else if kind == "pattern" {
                    format!(
                        r#"<pattern id="m{i}" patternUnits="userSpaceOnUse" width="9" height="9">{content}</pattern>"#
                    )
                } else {
                    format!(r#"<{kind} id="m{i}">{content}</{kind}>"#)
                });
            }
            body.push_str(r##"<path d="M0 0L9 0 9 9z" @/>"##);
            let first = match kind {
                "mask" => r#"mask="url(#m0)""#,
                "clipPath" => r#"clip-path="url(#m0)""#,
                "pattern" => r#"fill="url(#m0)""#,
                "marker" => r#"marker-end="url(#m0)""#,
                _ => r#"filter="url(#m0)""#,
            };
            let body = body.replace('@', first);
            let opt = Options {
                max_nodes: 100_000,
                max_filter_work: 1 << 28,
                ..Options::default()
            };
            let _ = run_with(&doc(&body), &opt);
        }
    });
}

// ------------------------------------------------------- nested images --

fn data_svg(svg: &str) -> String {
    // Percent-encoded, so that a level costs little more than its content.
    let mut out = String::from("data:image/svg+xml,");
    for c in svg.bytes() {
        match c {
            b'%' | b'"' | b'<' | b'&' | b'#' => out.push_str(&format!("%{c:02x}")),
            _ => out.push(c as char),
        }
    }
    out
}

const NS: &str = r#"xmlns="http://www.w3.org/2000/svg" width="20" height="20""#;

#[test]
fn nested_documents_stop_at_a_depth() {
    let mut svg = format!(r#"<svg {NS}><rect width="9" height="9"/></svg>"#);
    for _ in 0..11 {
        svg = format!(
            r#"<svg {NS}><image width="20" height="20" href="{}"/></svg>"#,
            data_svg(&svg)
        );
        assert!(svg.len() < 10 << 20);
    }
    let tree = run(svg.as_bytes()).unwrap();
    assert_eq!(tree.skipped().images, 0);
    // The innermost tree reports what it left out.
    fn missing(tree: &Tree) -> bool {
        fn group(g: &rusty_svgtree::Group) -> bool {
            g.children().iter().any(|n| match n {
                rusty_svgtree::Node::Group(g) => group(g),
                rusty_svgtree::Node::Image(i) => match i.kind() {
                    rusty_svgtree::ImageKind::Svg(t) => missing(t),
                    _ => false,
                },
                _ => false,
            })
        }
        tree.skipped().images > 0 || group(tree.root())
    }
    assert!(missing(&tree));
}

#[test]
fn nested_documents_share_the_node_limit() {
    // Each of 400 documents expands to some 40,000 elements.
    let mut bomb = format!(r#"<svg {NS}><g id="a0"><rect width="9" height="9"/></g>"#);
    for i in 1..13 {
        bomb.push_str(&format!(
            r##"<g id="a{i}"><use href="#a{p}"/><use href="#a{p}"/></g>"##,
            p = i - 1
        ));
    }
    bomb.push_str("</svg>");
    let mut body = String::new();
    for i in 0..400 {
        // Different text each time: nothing is shared.
        let svg = data_svg(&bomb.replace("<svg ", &format!(r#"<svg id="n{i}" "#)));
        body.push_str(&format!(r#"<image width="9" height="9" href="{svg}"/>"#));
    }
    let start = std::time::Instant::now();
    let tree = run(&doc(&body)).unwrap();
    assert!(tree.skipped().images > 300, "{:?}", tree.skipped());
    assert!(start.elapsed().as_secs() < 60, "{:?}", start.elapsed());
}

#[test]
fn nested_document_used_many_times() {
    let mut inner = format!(r#"<svg {NS}>"#);
    for _ in 0..5000 {
        inner.push_str(r#"<path d="M0 0h1"/>"#);
    }
    inner.push_str("</svg>");
    let mut body = format!(
        r#"<image id="i" width="9" height="9" href="{}"/>"#,
        data_svg(&inner)
    );
    for _ in 0..20_000 {
        body.push_str(r##"<use href="#i"/>"##);
    }
    let opt = Options {
        max_nodes: 100_000,
        ..Options::default()
    };
    let start = std::time::Instant::now();
    let tree = run_with(&doc(&body), &opt).unwrap();
    assert!(tree.skipped().images > 0);
    assert!(start.elapsed().as_secs() < 60, "{:?}", start.elapsed());
}

#[test]
fn nested_documents_have_no_access_either() {
    let inner = format!(
        r#"<svg {NS}><image width="20" height="20" href="/etc/hostname.png"/><image width="20" height="20" href="file:///C:/Windows/Web/Screen/img100.jpg"/></svg>"#
    );
    let svg = doc(&format!(
        r#"<image width="9" height="9" href="{}"/>"#,
        data_svg(&inner)
    ));
    let tree = run(&svg).unwrap();
    let mut p = Pixmap::new(100, 100).unwrap();
    rusty_svgtree::render(&tree, Transform::identity(), &mut p.as_mut());
    assert!(p.data().iter().all(|b| *b == 0));
}

#[cfg(feature = "svgz")]
#[test]
fn svgz_bomb_is_an_error() {
    let svg = format!(r#"<svg {NS}>{}</svg>"#, " ".repeat(8 << 20));
    let mut gz = vec![0x1f, 0x8b, 8, 0, 0, 0, 0, 0, 0, 255];
    gz.extend_from_slice(&miniz_oxide::deflate::compress_to_vec(svg.as_bytes(), 6));
    gz.extend_from_slice(&[0; 8]);
    let opt = Options {
        max_svgz_bytes: 1 << 20,
        ..Options::default()
    };
    assert_eq!(run_with(&gz, &opt).err(), Some(Error::MalformedGzip));
    // A wrong checksum is an error as well.
    assert_eq!(run(&gz).err(), Some(Error::MalformedGzip));
}

#[test]
fn broken_images_are_reported() {
    let svg = doc(
        r#"<image href="data:image/gif;base64,R0lGODlh//8=" width="9" height="9"/><image href="data:image/webp;base64,UklGRg==" width="9" height="9"/><image href="x.png" width="9" height="9"/><text>a</text>"#,
    );
    let tree = run(&svg).unwrap();
    assert_eq!(tree.skipped().images, 3);
    assert_eq!(tree.skipped().text, 1);
    assert!(run(&doc("<rect/>")).unwrap().skipped().is_empty());
}
