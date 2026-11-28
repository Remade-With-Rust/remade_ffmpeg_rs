//! `Fonts::set_select` / `set_fallback`: an application's own font matching
//! is what lays the text out. Needs the `fonts` feature and one font file on
//! the system (the tests say so and pass when there is none).
#![cfg(feature = "fonts")]

use std::path::{Path, PathBuf};
use std::sync::{Arc, Mutex};

use rusty_svgtree::text::{FontFamily, FontId, FontProvider};
use rusty_svgtree::{Fonts, Group, Node, Options, Tree};

/// A TrueType file of the system's, if there is one.
fn a_font_file() -> Option<PathBuf> {
    fn find(dir: &Path, depth: u32) -> Option<PathBuf> {
        let mut entries: Vec<_> = std::fs::read_dir(dir)
            .ok()?
            .filter_map(Result::ok)
            .map(|e| e.path())
            .collect();
        entries.sort();
        for p in &entries {
            let name = p.file_name()?.to_string_lossy().to_ascii_lowercase();
            if name.ends_with(".ttf")
                && ["arial", "dejavusans", "liberationsans", "verdana", "roboto"]
                    .iter()
                    .any(|n| name.starts_with(n))
            {
                return Some(p.clone());
            }
        }
        if depth == 0 {
            return None;
        }
        entries
            .iter()
            .filter(|p| p.is_dir())
            .find_map(|p| find(p, depth - 1))
    }
    [
        "C:/Windows/Fonts",
        "/usr/share/fonts",
        "/System/Library/Fonts",
        "/Library/Fonts",
    ]
    .iter()
    .find_map(|d| find(Path::new(d), 3))
}

fn text_of(g: &Group) -> Option<&rusty_svgtree::text::Text> {
    g.children().iter().find_map(|n| match n {
        Node::Text(t) => Some(&**t),
        Node::Group(g) => text_of(g),
        _ => None,
    })
}

const DOC: &str = r#"<svg xmlns="http://www.w3.org/2000/svg" width="200" height="50">
    <text x="5" y="30" font-family="'MY BRAND Regular', serif" font-size="20">Hi</text></svg>"#;

fn parse(fonts: Arc<Fonts>) -> Tree {
    Tree::from_str(
        DOC,
        &Options {
            fonts: Some(fonts),
            ..Options::default()
        },
    )
    .unwrap()
}

#[test]
fn the_built_in_matching_finds_nothing_in_an_empty_database() {
    // Nothing to draw: not in the tree, as in usvg.
    let tree = parse(Arc::new(Fonts::new()));
    assert!(text_of(tree.root()).is_none());
}

/// Design's matcher: names in any case, and faces added from a file only
/// when a document first names them.
#[test]
fn an_application_matcher_selects_and_adds_faces_lazily() {
    let Some(file) = a_font_file() else {
        eprintln!("no system font file: skipped");
        return;
    };
    let asked = Arc::new(Mutex::new(Vec::new()));
    let mut fonts = Fonts::new();
    let seen = asked.clone();
    let path = file.clone();
    fonts.set_select(move |font, db| {
        let names: Vec<String> = font
            .families
            .iter()
            .filter_map(|f| match f {
                FontFamily::Named(n) => Some(n.clone()),
                _ => None,
            })
            .collect();
        seen.lock().unwrap().push(names);
        let wanted = font.families.iter().any(
            |f| matches!(f, FontFamily::Named(n) if n.eq_ignore_ascii_case("my brand regular")),
        );
        if !wanted {
            return db.query(font);
        }
        // Once: the face stays in the database for the next request.
        if let Some(face) = db.faces().find(|f| f.path == Some(path.as_path())) {
            return Some(face.id);
        }
        db.add_file(path.clone()).first().copied()
    });
    let fonts = Arc::new(fonts);
    assert!(fonts.is_empty());
    let tree = parse(fonts.clone());
    let text = text_of(tree.root()).unwrap();
    let runs = text.glyph_runs().expect("laid out with the matcher's face");
    assert!(runs[0]
        .glyphs
        .iter()
        .all(|g| g.font == FontId(0) && g.id != 0));
    assert_eq!(fonts.len(), 1);
    assert!(asked.lock().unwrap()[0].contains(&"MY BRAND Regular".to_string()));
    // A second document finds the face without adding it again.
    let _ = parse(fonts.clone());
    assert_eq!(fonts.len(), 1);
    // The file's bytes, for a PDF.
    let data = fonts.font_data(FontId(0)).unwrap();
    assert_eq!(data.bytes(), std::fs::read(&file).unwrap());
}

/// A face added from memory is shared, not copied.
#[test]
fn a_shared_face_is_not_copied() {
    let Some(file) = a_font_file() else {
        eprintln!("no system font file: skipped");
        return;
    };
    let bytes: Arc<Vec<u8>> = Arc::new(std::fs::read(&file).unwrap());
    let mut fonts = Fonts::new();
    let shared = bytes.clone();
    fonts.set_select(move |_, db| db.add_shared(shared.clone()).first().copied());
    let fonts = Arc::new(fonts);
    let tree = parse(fonts.clone());
    assert!(text_of(tree.root()).unwrap().glyph_runs().is_some());
    let data = fonts.font_data(FontId(0)).unwrap();
    assert_eq!(data.bytes().as_ptr(), bytes.as_ptr());
    // The database lists the face's names.
    let mut fonts = Fonts::new();
    let ids = fonts.load_shared_font_data(bytes.clone());
    let db = fonts.db();
    let face = db.face(ids[0]).unwrap();
    assert!(!face.families.is_empty() && !face.post_script_name.is_empty());
    assert!(db.has_char(ids[0], 'H'));
}

#[test]
fn an_application_fallback_is_asked() {
    let Some(file) = a_font_file() else {
        eprintln!("no system font file: skipped");
        return;
    };
    let asked = Arc::new(Mutex::new(Vec::new()));
    let mut fonts = Fonts::new();
    fonts.load_font_data(std::fs::read(&file).unwrap());
    fonts.set_select(|_, _| Some(FontId(0)));
    let seen = asked.clone();
    fonts.set_fallback(move |c, used, db| {
        seen.lock().unwrap().push((c, used.to_vec()));
        db.default_fallback(c, used)
    });
    let svg = r#"<svg xmlns="http://www.w3.org/2000/svg" width="200" height="50">
        <text x="5" y="30" font-size="20">A&#x10FFFD;</text></svg>"#;
    let opt = Options {
        fonts: Some(Arc::new(fonts)),
        ..Options::default()
    };
    let _ = Tree::from_str(svg, &opt).unwrap();
    let asked = asked.lock().unwrap();
    assert_eq!(asked.first().map(|(c, _)| *c), Some('\u{10FFFD}'));
    assert_eq!(asked[0].1, vec![FontId(0)]);
}
