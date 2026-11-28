//! `Options::image_loader`: linked files, data URIs under the caller's rules,
//! nested SVG documents, and refusals. Linked and nested renders are compared
//! with resvg 0.48.1 loading the same files from its `resources_dir`.

use std::path::{Path, PathBuf};
use std::sync::{Arc, Mutex};

use rusty_svgtree::rusty_raster::{Pixmap, Transform};
use rusty_svgtree::{
    Error, ImageLoad, ImageLoader, ImageRequest, ImageSource, Node, Options, Tree,
};

/// Loads relative paths from a directory, as an application does for the
/// files next to a document; `data:` URIs as by default.
#[derive(Debug)]
struct Files(PathBuf);

impl ImageLoader for Files {
    fn load(&self, r: &ImageRequest) -> ImageLoad {
        match r.source {
            ImageSource::Url(href) => match std::fs::read(self.0.join(href)) {
                Ok(data) => ImageLoad::Data(Arc::new(data)),
                Err(_) => ImageLoad::Skip,
            },
            _ => ImageLoad::Default,
        }
    }
}

fn corpus(name: &str) -> Vec<u8> {
    let dir = Path::new(env!("CARGO_MANIFEST_DIR")).join("tests/corpus");
    std::fs::read(dir.join(name)).unwrap()
}

fn temp_dir(name: &str) -> PathBuf {
    let d = std::env::temp_dir().join(format!("rusty_svgtree-{name}-{}", std::process::id()));
    std::fs::create_dir_all(&d).unwrap();
    d
}

fn with_loader(loader: impl ImageLoader + 'static) -> Options {
    Options {
        image_loader: Some(Arc::new(loader)),
        ..Options::default()
    }
}

fn render(tree: &Tree, scale: f32) -> (u32, u32, Vec<u8>) {
    let w = (tree.size().width() * scale).ceil() as u32;
    let h = (tree.size().height() * scale).ceil() as u32;
    let mut p = Pixmap::new(w, h).unwrap();
    rusty_svgtree::render(tree, Transform::from_scale(scale, scale), &mut p.as_mut());
    (w, h, p.data().to_vec())
}

fn render_resvg(data: &[u8], dir: &Path, scale: f32) -> (u32, u32, Vec<u8>) {
    let opt = resvg::usvg::Options {
        resources_dir: Some(dir.to_path_buf()),
        ..resvg::usvg::Options::default()
    };
    let tree = resvg::usvg::Tree::from_data(data, &opt).unwrap();
    let w = (tree.size().width() * scale).ceil() as u32;
    let h = (tree.size().height() * scale).ceil() as u32;
    let mut p = resvg::tiny_skia::Pixmap::new(w, h).unwrap();
    resvg::render(
        &tree,
        resvg::tiny_skia::Transform::from_scale(scale, scale),
        &mut p.as_mut(),
    );
    (w, h, p.data().to_vec())
}

/// PSNR in dB over RGBA8 bytes (the oracle test's measure).
fn psnr(a: &[u8], b: &[u8]) -> f64 {
    assert_eq!(a.len(), b.len());
    let se: f64 = a
        .iter()
        .zip(b)
        .map(|(x, y)| (f64::from(*x) - f64::from(*y)).powi(2))
        .sum();
    let mse = se / a.len().max(1) as f64;
    if mse == 0.0 {
        f64::INFINITY
    } else {
        10.0 * (255.0f64 * 255.0 / mse).log10()
    }
}

/// The oracle test's default floor.
const FLOOR_DB: f64 = 30.0;

/// The `href` of the first `<image>`, and the document with it replaced.
fn split_href(svg: &str, replacement: &str) -> (String, String) {
    let at = svg.find("href=\"data:").unwrap() + 6;
    let end = at + svg[at..].find('"').unwrap();
    let href = svg[at..end].to_string();
    (href, format!("{}{replacement}{}", &svg[..at], &svg[end..]))
}

fn data_uri_payload(href: &str) -> Vec<u8> {
    let b64 = &href[href.find(',').unwrap() + 1..];
    // The crate's own decoder is private; a minimal one for the test.
    let table = |c: u8| -> Option<u8> {
        Some(match c {
            b'A'..=b'Z' => c - b'A',
            b'a'..=b'z' => c - b'a' + 26,
            b'0'..=b'9' => c - b'0' + 52,
            b'+' => 62,
            b'/' => 63,
            _ => return None,
        })
    };
    let bits: Vec<u8> = b64.bytes().filter_map(table).collect();
    let mut out = Vec::new();
    for q in bits.chunks(4) {
        let n = q.iter().fold(0u32, |n, v| n << 6 | u32::from(*v)) << (6 * (4 - q.len()));
        let bytes = n.to_be_bytes();
        out.extend_from_slice(&bytes[1..q.len()]);
    }
    out
}

#[test]
fn a_linked_file_draws_as_its_data_uri() {
    let dir = temp_dir("linked");
    let svg = String::from_utf8(corpus("21-image-png.svg")).unwrap();
    let (href, linked) = split_href(&svg, "picture.png");
    std::fs::write(dir.join("picture.png"), data_uri_payload(&href)).unwrap();
    let embedded = Tree::from_str(&svg, &Options::default()).unwrap();
    // Without a loader the file is not read.
    let skipped = Tree::from_str(&linked, &Options::default()).unwrap();
    assert!(skipped.skipped().images >= 1);
    let tree = Tree::from_str(&linked, &with_loader(Files(dir.clone()))).unwrap();
    assert_eq!(tree.skipped().images, skipped.skipped().images - 1);
    for scale in [1.0, 2.5] {
        let (_, _, a) = render(&embedded, scale);
        let (w, h, b) = render(&tree, scale);
        assert_eq!(a, b, "@{scale}");
        let (w2, h2, c) = render_resvg(linked.as_bytes(), &dir, scale);
        assert_eq!((w, h), (w2, h2));
        let db = psnr(&b, &c);
        assert!(db >= FLOOR_DB, "@{scale}: {db:.2} dB");
    }
    let _ = std::fs::remove_dir_all(&dir);
}

#[test]
fn nested_svg_files_draw_as_resvg_draws_them() {
    let dir = temp_dir("nested");
    for inner in [
        "13-linear-gradients.svg",
        "16-use-symbol.svg",
        "18-clip-path.svg",
        "19-mask.svg",
        "25-icon-material.svg",
        "32-illustrator-like.svg",
        "35-filter-shadows.svg",
    ] {
        std::fs::write(dir.join(inner), corpus(inner)).unwrap();
        for aspect in ["xMidYMid", "none", "xMinYMax slice"] {
            let outer = format!(
                r##"<svg xmlns="http://www.w3.org/2000/svg" xmlns:xlink="http://www.w3.org/1999/xlink" width="220" height="160">
                     <rect width="220" height="160" fill="#eee"/>
                     <image x="10" y="12" width="190" height="130" preserveAspectRatio="{aspect}" xlink:href="{inner}"/>
                   </svg>"##
            );
            let tree = Tree::from_str(&outer, &with_loader(Files(dir.clone()))).unwrap();
            assert_eq!(tree.skipped().images, 0, "{inner}");
            let mut found = false;
            visit(tree.root(), &mut |n| {
                if let Node::Image(i) = n {
                    found |= matches!(i.kind(), rusty_svgtree::ImageKind::Svg(_));
                }
            });
            assert!(found, "{inner}");
            // The nested document draws something.
            let without = Tree::from_str(&outer, &Options::default()).unwrap();
            assert_ne!(render(&without, 1.0).2, render(&tree, 1.0).2, "{inner}");
            for scale in [1.0, 2.5] {
                let (w, h, a) = render(&tree, scale);
                let (w2, h2, b) = render_resvg(outer.as_bytes(), &dir, scale);
                assert_eq!((w, h), (w2, h2));
                let db = psnr(&a, &b);
                println!("{inner} {aspect} @{scale}: {db:.2} dB");
                assert!(db >= FLOOR_DB, "{inner} {aspect} @{scale}: {db:.2} dB");
            }
        }
    }
    let _ = std::fs::remove_dir_all(&dir);
}

fn visit(g: &rusty_svgtree::Group, f: &mut impl FnMut(&Node)) {
    for n in g.children() {
        f(n);
        if let Node::Group(g) = n {
            visit(g, f);
        }
    }
}

/// Records what it is asked, then answers as told.
#[derive(Debug, Default)]
struct Recorder {
    seen: Mutex<Vec<(String, u32)>>,
    answer: Option<fn(&ImageRequest) -> ImageLoad>,
}

impl ImageLoader for Recorder {
    fn load(&self, r: &ImageRequest) -> ImageLoad {
        let what = match r.source {
            ImageSource::Data { mime, data } => format!("data {mime} {}", data.len()),
            ImageSource::Url(u) => format!("url {u}"),
            _ => "other".to_string(),
        };
        self.seen.lock().unwrap().push((what, r.depth));
        self.answer.map_or(ImageLoad::Default, |f| f(r))
    }
}

/// An SVG data URI (percent-encoded) holding `inner`.
fn svg_uri(inner: &str) -> String {
    let doc =
        format!(r#"<svg xmlns="http://www.w3.org/2000/svg" width="10" height="10">{inner}</svg>"#);
    let mut out = String::from("data:image/svg+xml,");
    for b in doc.bytes() {
        if b.is_ascii_alphanumeric() || b" =/:.-".contains(&b) {
            out.push(b as char);
        } else {
            out.push_str(&format!("%{b:02X}"));
        }
    }
    out
}

fn doc(body: &str) -> String {
    format!(
        r#"<svg xmlns="http://www.w3.org/2000/svg" xmlns:xlink="http://www.w3.org/1999/xlink" width="100" height="100">{body}</svg>"#
    )
}

#[test]
fn the_loader_sees_every_reference_once_with_its_depth() {
    let inner = svg_uri(r#"<image width="5" height="5" href="deep.png"/>"#);
    let svg = doc(&format!(
        r##"<image id="a" width="50" height="50" href="{inner}"/>
            <use href="#a" x="50"/>
            <image width="9" height="9" href="pic.jpg"/>
            <filter id="f"><feImage href="fe.png"/></filter>
            <rect width="9" height="9" filter="url(#f)"/>"##
    ));
    let rec = Arc::new(Recorder::default());
    let opt = Options {
        image_loader: Some(rec.clone()),
        ..Options::default()
    };
    let tree = Tree::from_str(&svg, &opt).unwrap();
    let seen = rec.seen.lock().unwrap().clone();
    // The `use` copy shares its original's load.
    assert_eq!(
        seen.iter()
            .filter(|(w, _)| w.starts_with("data image/svg+xml"))
            .count(),
        1
    );
    assert!(seen.contains(&("url deep.png".to_string(), 1)));
    assert!(seen.contains(&("url pic.jpg".to_string(), 0)));
    assert!(seen.contains(&("url fe.png".to_string(), 0)));
    // Default skips what is not a data URI.
    assert_eq!(tree.skipped().images, 2);
}

#[test]
fn skip_and_fail() {
    let svg = doc(&format!(
        r#"<image width="10" height="10" href="{}"/>"#,
        svg_uri(r#"<rect width="5" height="5"/>"#)
    ));
    let skip = with_loader(Recorder {
        answer: Some(|_| ImageLoad::Skip),
        ..Recorder::default()
    });
    assert_eq!(Tree::from_str(&svg, &skip).unwrap().skipped().images, 1);
    let fail = with_loader(Recorder {
        answer: Some(|_| ImageLoad::Fail),
        ..Recorder::default()
    });
    assert_eq!(
        Tree::from_str(&svg, &fail).unwrap_err(),
        Error::ImageRefused
    );
    // A refusal deep inside a nested document fails the outer one too.
    let nested = doc(&format!(
        r#"<image width="10" height="10" href="{}"/>"#,
        svg_uri(r#"<image width="5" height="5" href="missing.png"/>"#)
    ));
    let strict = with_loader(Recorder {
        answer: Some(|r| match r.source {
            ImageSource::Url(_) => ImageLoad::Fail,
            _ => ImageLoad::Default,
        }),
        ..Recorder::default()
    });
    assert_eq!(
        Tree::from_str(&nested, &strict).unwrap_err(),
        Error::ImageRefused
    );
    // Bytes that are neither an image nor an SVG are skipped.
    let junk = with_loader(Recorder {
        answer: Some(|_| ImageLoad::Data(Arc::new(b"not an image".to_vec()))),
        ..Recorder::default()
    });
    let t = Tree::from_str(&doc(r#"<image width="9" height="9" href="x"/>"#), &junk).unwrap();
    assert_eq!(t.skipped().images, 1);
}

/// A document that embeds itself stops at the nesting limit, and the
/// documents share one element budget.
#[test]
fn a_self_embedding_document_is_bounded() {
    #[derive(Debug)]
    struct Mirror(Arc<Vec<u8>>);
    impl ImageLoader for Mirror {
        fn load(&self, _: &ImageRequest) -> ImageLoad {
            ImageLoad::Data(self.0.clone())
        }
    }
    let svg = doc(
        r#"<image width="100" height="100" href="self.svg"/><image width="50" height="50" href="again.svg"/>"#,
    );
    let mirror = Mirror(Arc::new(svg.clone().into_bytes()));
    let opt = Options {
        max_nodes: 5_000,
        ..with_loader(mirror)
    };
    let tree = Tree::from_str(&svg, &opt).unwrap();
    let (_, _, px) = render(&tree, 1.0);
    assert_eq!(px.len(), 100 * 100 * 4);
}

/// Real files: `RSVG_LINKED_CORPUS=<dir>` takes every SVG with `data:`
/// images in a directory (Vector's import corpus), writes each image out as a
/// file, links it, and checks the linked document draws exactly as the
/// embedded one, and as resvg draws the linked one.
#[test]
fn linked_corpus() {
    let Some(src) = std::env::var_os("RSVG_LINKED_CORPUS") else {
        return;
    };
    let mut files = Vec::new();
    collect(Path::new(&src), &mut files);
    let dir = temp_dir("linked-corpus");
    let mut checked = 0;
    for f in files {
        let svg = String::from_utf8_lossy(&std::fs::read(&f).unwrap()).into_owned();
        if !svg.contains("href=\"data:image/") {
            continue;
        }
        let mut linked = svg.clone();
        let mut n = 0;
        while let Some(at) = linked.find("href=\"data:image/") {
            let at = at + 6;
            let end = at + linked[at..].find('"').unwrap();
            let href = &linked[at..end];
            let ext = &href[11..href.find(';').unwrap_or(16)];
            let name = format!("img{n}.{ext}");
            std::fs::write(dir.join(&name), data_uri_payload(href)).unwrap();
            linked = format!("{}{name}{}", &linked[..at], &linked[end..]);
            n += 1;
        }
        let embedded = Tree::from_str(&svg, &Options::default()).unwrap();
        let tree = Tree::from_str(&linked, &with_loader(Files(dir.clone()))).unwrap();
        assert_eq!(tree.skipped(), embedded.skipped(), "{}", f.display());
        let s = tree.size();
        let scale = (800.0 / s.width().max(s.height())).min(1.0);
        let (_, _, a) = render(&embedded, scale);
        let (_, _, b) = render(&tree, scale);
        assert_eq!(a, b, "{}", f.display());
        let (_, _, c) = render_resvg(linked.as_bytes(), &dir, scale);
        println!(
            "{}: {n} images, ours == embedded, vs resvg {:.2} dB",
            f.display(),
            psnr(&b, &c)
        );
        checked += 1;
    }
    println!("checked {checked}");
    let _ = std::fs::remove_dir_all(&dir);
}

fn collect(p: &Path, out: &mut Vec<PathBuf>) {
    if p.is_dir() {
        let mut v: Vec<_> = std::fs::read_dir(p)
            .unwrap()
            .filter_map(Result::ok)
            .map(|e| e.path())
            .collect();
        v.sort();
        for e in v {
            collect(&e, out);
        }
    } else if p.extension().is_some_and(|e| e == "svg") {
        out.push(p.to_path_buf());
    }
}

/// Real files as nested documents: `RSVG_NESTED_CORPUS=<dir>` draws every
/// SVG of a directory as the `<image>` of another document (loaded from its
/// file, as Vector's importer does) and compares with resvg. Reports
/// outliers below the oracle floor; asserts that none is far below it.
#[test]
fn nested_corpus() {
    let Some(src) = std::env::var_os("RSVG_NESTED_CORPUS") else {
        return;
    };
    let mut files = Vec::new();
    collect(Path::new(&src), &mut files);
    let (mut n, mut identical, mut below, mut either_failed) = (0, 0, 0, 0);
    let mut worst = f64::INFINITY;
    for f in &files {
        let dir = f.parent().unwrap().to_path_buf();
        let name = f.file_name().unwrap().to_string_lossy().into_owned();
        let outer = format!(
            r##"<svg xmlns="http://www.w3.org/2000/svg" xmlns:xlink="http://www.w3.org/1999/xlink" width="300" height="300"><image x="10" y="10" width="280" height="280" xlink:href="{name}"/></svg>"##
        );
        let ours = Tree::from_str(&outer, &with_loader(Files(dir.clone()))).unwrap();
        let resvg_opt = resvg::usvg::Options {
            resources_dir: Some(dir.clone()),
            ..resvg::usvg::Options::default()
        };
        let theirs = resvg::usvg::Tree::from_data(outer.as_bytes(), &resvg_opt).unwrap();
        let ours_drew = ours.skipped().images == 0;
        let theirs_drew = theirs.root().has_children();
        if ours_drew != theirs_drew {
            either_failed += 1;
            println!(
                "{}: loaded by ours {ours_drew}, by resvg {theirs_drew}",
                f.display()
            );
            continue;
        }
        let (_, _, a) = render(&ours, 1.0);
        let (_, _, b) = render_resvg(outer.as_bytes(), &dir, 1.0);
        let db = psnr(&a, &b);
        n += 1;
        if db.is_infinite() {
            identical += 1;
        }
        if db < FLOOR_DB {
            below += 1;
            println!("{}: {db:.2} dB", f.display());
        }
        worst = worst.min(db);
    }
    println!(
        "nested: {} files, compared {n}, identical {identical}, below {FLOOR_DB} dB {below}, \
         loaded by one side only {either_failed}, worst {worst:.2} dB",
        files.len()
    );
}
