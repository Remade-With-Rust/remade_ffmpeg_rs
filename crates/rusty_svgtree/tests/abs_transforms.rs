//! Absolute transforms inside clip paths, masks and patterns (Fig's report):
//! each node's `abs_transform` is its parents' transforms, down from the root
//! of the tree or of the clip path, mask or pattern it is in, as usvg 0.48
//! has them; and a pattern that needs nothing from the painted element is
//! one `Arc` for all its uses.

use std::sync::Arc;

use rusty_svgtree::rusty_raster::Transform;
use rusty_svgtree::{Group, Node, Options, Paint, Tree};

const PATTERNS: &str = r##"<svg xmlns="http://www.w3.org/2000/svg" width="300" height="200">
  <pattern id="checks" patternUnits="userSpaceOnUse" width="20" height="20"><rect width="10" height="10" fill="blue"/></pattern>
  <pattern id="spot" patternContentUnits="objectBoundingBox" width="1" height="1"><circle cx="0.5" cy="0.5" r="0.25" fill="red"/></pattern>
  <pattern id="nested" patternContentUnits="objectBoundingBox" width="1" height="1"><g transform="translate(.1 .1)"><rect width=".3" height=".3" transform="rotate(10)"/></g></pattern>
  <pattern id="vb" patternUnits="userSpaceOnUse" width="30" height="20" viewBox="0 0 3 2"><circle cx="1" cy="1" r="1"/></pattern>
  <pattern id="vbobb" width=".5" height=".5" viewBox="0 0 10 10"><rect width="5" height="5"/></pattern>
  <mask id="m1" maskContentUnits="objectBoundingBox"><rect x=".1" y=".1" width=".5" height=".5" fill="white"/></mask>
  <mask id="m2" maskContentUnits="objectBoundingBox"><g transform="translate(.2 0)"><circle cx=".3" cy=".3" r=".2" fill="white"/></g></mask>
  <clipPath id="c1" clipPathUnits="objectBoundingBox"><rect width=".5" height="1"/></clipPath>
  <rect width="100" height="100" fill="url(#checks)"/>
  <rect x="100" width="100" height="100" fill="url(#spot)"/>
  <rect x="200" y="20" width="80" height="40" fill="url(#nested)"/>
  <rect y="100" width="90" height="90" fill="url(#vb)"/>
  <rect x="100" y="100" width="90" height="60" fill="url(#vbobb)"/>
  <g transform="translate(200 100)"><rect width="80" height="80" mask="url(#m1)"/>
    <rect x="10" width="60" height="60" mask="url(#m2)" clip-path="url(#c1)"/></g>
  <rect y="10" width="50" height="50" fill="url(#checks)" stroke="url(#checks)"/>
</svg>"##;

/// Checks the absolute transforms of a group's content against its own,
/// and of every root inside it against identity.
fn check(g: &Group, errors: &mut Vec<String>) {
    for child in g.children() {
        match child {
            Node::Group(c) => {
                let want = g.abs_transform().pre_concat(c.transform());
                if !close(c.abs_transform(), want) {
                    errors.push(format!(
                        "group {:?}: {:?} != {want:?}",
                        c.id(),
                        c.abs_transform()
                    ));
                }
                check(c, errors);
                roots(c, errors);
            }
            Node::Path(p) => {
                if !close(p.abs_transform(), g.abs_transform()) {
                    errors.push(format!(
                        "path {:?}: {:?} != {:?}",
                        p.id(),
                        p.abs_transform(),
                        g.abs_transform()
                    ));
                }
                for paint in [p.fill().map(|f| f.paint()), p.stroke().map(|s| s.paint())]
                    .into_iter()
                    .flatten()
                {
                    if let Paint::Pattern(pat) = paint {
                        check_root(pat.root(), errors);
                    }
                }
            }
            _ => {}
        }
    }
}

fn check_root(root: &Group, errors: &mut Vec<String>) {
    if !root.abs_transform().is_identity() {
        errors.push(format!("root: {:?}", root.abs_transform()));
    }
    check(root, errors);
}

fn roots(g: &Group, errors: &mut Vec<String>) {
    if let Some(c) = g.clip_path() {
        check_root(c.root(), errors);
    }
    if let Some(m) = g.mask() {
        check_root(m.root(), errors);
    }
}

fn close(a: Transform, b: Transform) -> bool {
    let (a, b) = (
        [a.sx, a.kx, a.ky, a.sy, a.tx, a.ty],
        [b.sx, b.kx, b.ky, b.sy, b.tx, b.ty],
    );
    a.iter()
        .zip(&b)
        .all(|(x, y)| (x - y).abs() <= 1e-4 * x.abs().max(y.abs()).max(1.0))
}

#[test]
fn abs_transforms_follow_the_transforms() {
    let tree = Tree::from_str(PATTERNS, &Options::default()).unwrap();
    let mut errors = Vec::new();
    check_root(tree.root(), &mut errors);
    roots(tree.root(), &mut errors);
    assert!(errors.is_empty(), "{}", errors.join("\n"));
}

/// The patterns and masks of `PATTERNS` whose content usvg 0.48.1 gives
/// absolute transforms down from the root. (In `objectBoundingBox` pattern
/// content, usvg leaves the box out of them; here they include it, as
/// `abs_transforms_follow_the_transforms` checks.)
const USVG_SAME: &str = r##"<svg xmlns="http://www.w3.org/2000/svg" width="300" height="200">
  <pattern id="checks" patternUnits="userSpaceOnUse" width="20" height="20"><rect width="10" height="10" fill="blue"/></pattern>
  <pattern id="vb" patternUnits="userSpaceOnUse" width="30" height="20" viewBox="0 0 3 2"><circle cx="1" cy="1" r="1"/></pattern>
  <mask id="m1" maskContentUnits="objectBoundingBox"><rect x=".1" y=".1" width=".5" height=".5" fill="white"/></mask>
  <mask id="m2" maskContentUnits="objectBoundingBox"><g transform="translate(.2 0)"><circle cx=".3" cy=".3" r=".2" fill="white"/></g></mask>
  <mask id="m3"><g transform="scale(2)"><rect width="9" height="9" fill="white"/></g></mask>
  <rect width="100" height="100" fill="url(#checks)"/>
  <rect y="100" width="90" height="90" fill="url(#vb)"/>
  <g transform="translate(200 100)"><rect width="80" height="80" mask="url(#m1)"/>
    <rect x="10" width="60" height="60" mask="url(#m2)"/>
    <rect x="10" y="10" width="60" height="60" mask="url(#m3)"/></g>
</svg>"##;

/// The abs transforms of the paths inside the patterns and masks, in
/// document order, as usvg 0.48.1 has them.
#[test]
fn abs_transforms_match_usvg() {
    let ours = Tree::from_str(USVG_SAME, &Options::default()).unwrap();
    let theirs = resvg::usvg::Tree::from_str(USVG_SAME, &resvg::usvg::Options::default()).unwrap();
    let mut a = Vec::new();
    ours_inner(ours.root(), false, &mut a);
    let mut b = Vec::new();
    theirs_inner(theirs.root(), false, &mut b);
    assert!(a.len() >= 5, "{a:?}");
    assert_eq!(a.len(), b.len(), "{a:?}\n{b:?}");
    for (x, y) in a.iter().zip(&b) {
        let y = Transform::from_row(y.sx, y.ky, y.kx, y.sy, y.tx, y.ty);
        assert!(close(*x, y), "{x:?} != {y:?}\nours {a:?}\nusvg {b:?}");
    }
}

fn ours_inner(g: &Group, inside: bool, out: &mut Vec<Transform>) {
    if let Some(m) = g.mask() {
        ours_inner(m.root(), true, out);
    }
    for c in g.children() {
        match c {
            Node::Group(c) => ours_inner(c, inside, out),
            Node::Path(p) => {
                if inside {
                    out.push(p.abs_transform());
                }
                if let Some(Paint::Pattern(pat)) = p.fill().map(|f| f.paint()) {
                    ours_inner(pat.root(), true, out);
                }
            }
            _ => {}
        }
    }
}

fn theirs_inner(g: &resvg::usvg::Group, inside: bool, out: &mut Vec<resvg::usvg::Transform>) {
    use resvg::usvg::{Node, Paint};
    if let Some(m) = g.mask() {
        theirs_inner(m.root(), true, out);
    }
    for c in g.children() {
        match c {
            Node::Group(c) => theirs_inner(c, inside, out),
            Node::Path(p) => {
                if inside {
                    out.push(p.abs_transform());
                }
                if let Some(Paint::Pattern(pat)) = p.fill().map(|f| f.paint()) {
                    theirs_inner(pat.root(), true, out);
                }
            }
            _ => {}
        }
    }
}

/// `RSVG_ABS_DIR=<dir>`: the consistency check over every SVG of a
/// directory (Fig's and Material's corpora).
#[test]
fn abs_transforms_corpus() {
    let Some(dir) = std::env::var_os("RSVG_ABS_DIR") else {
        return;
    };
    let mut files: Vec<_> = std::fs::read_dir(dir)
        .unwrap()
        .filter_map(Result::ok)
        .map(|e| e.path())
        .filter(|p| p.extension().is_some_and(|e| e == "svg"))
        .collect();
    files.sort();
    let mut bad = 0;
    for f in &files {
        let Ok(tree) = Tree::from_data(&std::fs::read(f).unwrap(), &Options::default()) else {
            continue;
        };
        let mut errors = Vec::new();
        check_root(tree.root(), &mut errors);
        roots(tree.root(), &mut errors);
        if !errors.is_empty() {
            bad += 1;
            println!("{}: {}", f.display(), errors[0]);
        }
    }
    println!("abs: {} files, {bad} inconsistent", files.len());
    assert_eq!(bad, 0);
}

#[test]
fn a_user_space_pattern_is_shared_by_its_uses() {
    let tree = Tree::from_str(PATTERNS, &Options::default()).unwrap();
    let mut checks = Vec::new();
    let mut spots = 0;
    fn walk(g: &Group, out: &mut Vec<Arc<rusty_svgtree::Pattern>>, obb: &mut usize) {
        for c in g.children() {
            match c {
                Node::Group(c) => walk(c, out, obb),
                Node::Path(p) => {
                    for paint in [p.fill().map(|f| f.paint()), p.stroke().map(|s| s.paint())]
                        .into_iter()
                        .flatten()
                    {
                        if let Paint::Pattern(pat) = paint {
                            if pat.rect().width() == 20.0 {
                                out.push(pat.clone());
                            } else {
                                *obb += 1;
                            }
                        }
                    }
                }
                _ => {}
            }
        }
    }
    walk(tree.root(), &mut checks, &mut spots);
    // Three uses of `checks` (a fill, a fill and a stroke): one pattern.
    assert_eq!(checks.len(), 3);
    assert!(checks.iter().all(|p| Arc::ptr_eq(p, &checks[0])));
    // Bounding-box patterns are resolved per element.
    assert!(spots >= 3);
}
