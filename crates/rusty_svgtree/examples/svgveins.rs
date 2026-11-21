//! `svgveins`: the instruction-count instrument for rusty_svgtree.
//!
//! Two stages, kept apart so a callgrind census can tell them from each
//! other (`--toggle-collect='*hot_parse*'` / `'*hot_render*'`):
//!
//! - `svgveins parse  REPS FILE...`: XML text to render tree. Anchor: file
//!   count, node count and an FNV-1a hash of the tree's `Debug` text.
//! - `svgveins render REPS FIT FILE...`: render tree to pixmap, each file
//!   fitted to a `FIT` pixel box (`0` = natural size). Anchor: pixel count
//!   and an FNV-1a hash of the premultiplied RGBA8 bytes.
//! - `svgveins both REPS FIT FILE...`: the whole per-document cost, as an
//!   application pays it for an icon: XML text to tree, a fitted pixmap
//!   made, the tree rendered into it (`'*hot_both*'`). Anchor: node count,
//!   pixel count and the FNV-1a hash of the pixels.
//!
//! The anchors cannot move unless the work or its result changed; the
//! `min_ns` line is the in-process minimum over `REPS` of one pass over all
//! files, for pinned ABBA wall checks.

#[global_allocator]
static GLOBAL_ALLOC: rusty_alloc_api::RustyAlloc = rusty_alloc_api::RustyAlloc;

use std::time::Instant;

use rusty_svgtree::rusty_raster::{Pixmap, Transform};
use rusty_svgtree::{Group, Node, Options, Tree};

fn fnv(h: &mut u64, bytes: &[u8]) {
    for &b in bytes {
        *h ^= u64::from(b);
        *h = h.wrapping_mul(0x0000_0100_0000_01b3);
    }
}

fn count_nodes(g: &Group) -> u64 {
    let mut n = 0;
    for c in g.children() {
        n += 1;
        if let Node::Group(g) = c {
            n += count_nodes(g);
        }
    }
    n
}

#[inline(never)]
fn hot_parse(data: &[u8], opt: &Options) -> Option<Tree> {
    Tree::from_data(data, opt).ok()
}

#[inline(never)]
fn hot_render(tree: &Tree, ts: Transform, pixmap: &mut Pixmap) {
    rusty_svgtree::render(tree, ts, &mut pixmap.as_mut());
}

#[inline(never)]
fn hot_both(data: &[u8], opt: &Options, box_px: f32) -> Option<(Tree, Pixmap)> {
    let tree = Tree::from_data(data, opt).ok()?;
    let (scale, w, h) = fit(&tree, box_px);
    let mut pixmap = Pixmap::new(w, h)?;
    rusty_svgtree::render(
        &tree,
        Transform::from_scale(scale, scale),
        &mut pixmap.as_mut(),
    );
    Some((tree, pixmap))
}

fn fit(tree: &Tree, fit: f32) -> (f32, u32, u32) {
    let (w, h) = (tree.size().width(), tree.size().height());
    // The same fit as `rusty_raster/bench-c` (the resvg comparison).
    let scale = if fit > 0.0 {
        (fit / w.max(h)).min(16.0)
    } else {
        1.0
    };
    (
        scale,
        ((w * scale).ceil() as u32).max(1),
        ((h * scale).ceil() as u32).max(1),
    )
}

fn main() {
    let args: Vec<String> = std::env::args().skip(1).collect();
    let usage = "usage: svgveins parse REPS FILE... | svgveins render|both REPS FIT FILE...";
    let (Some(mode), Some(reps)) = (args.first(), args.get(1).and_then(|r| r.parse().ok())) else {
        eprintln!("{usage}");
        std::process::exit(2);
    };
    let reps: u32 = reps;
    let opt = Options::default();
    let mut hash = 0xcbf2_9ce4_8422_2325u64;
    let mut min_ns = u128::MAX;
    match mode.as_str() {
        "parse" => {
            let files: Vec<Vec<u8>> = args[2..]
                .iter()
                .map(|f| std::fs::read(f).unwrap_or_else(|e| panic!("{f}: {e}")))
                .collect();
            let (mut nodes, mut failed) = (0u64, 0u64);
            for rep in 0..reps.max(1) {
                let mut ns = 0;
                for data in &files {
                    let t = Instant::now();
                    let tree = hot_parse(data, &opt);
                    ns += t.elapsed().as_nanos();
                    if rep == 0 {
                        match &tree {
                            Some(tree) => {
                                nodes += count_nodes(tree.root());
                                fnv(&mut hash, format!("{tree:?}").as_bytes());
                            }
                            None => failed += 1,
                        }
                    }
                }
                min_ns = min_ns.min(ns);
            }
            println!(
                "parse files {} failed {failed} nodes {nodes} fnv {hash:016x} reps {reps} min_ns {min_ns}",
                files.len()
            );
        }
        "render" => {
            let Some(box_px) = args.get(2).and_then(|r| r.parse::<f32>().ok()) else {
                eprintln!("{usage}");
                std::process::exit(2);
            };
            let trees: Vec<Tree> = args[3..]
                .iter()
                .filter_map(|f| {
                    let data = std::fs::read(f).unwrap_or_else(|e| panic!("{f}: {e}"));
                    Tree::from_data(&data, &opt).ok()
                })
                .collect();
            let mut pixels = 0u64;
            for rep in 0..reps.max(1) {
                let mut ns = 0;
                for tree in &trees {
                    let (scale, w, h) = fit(tree, box_px);
                    let Some(mut pixmap) = Pixmap::new(w, h) else {
                        continue;
                    };
                    let ts = Transform::from_scale(scale, scale);
                    let t = Instant::now();
                    hot_render(tree, ts, &mut pixmap);
                    ns += t.elapsed().as_nanos();
                    if rep == 0 {
                        pixels += u64::from(w) * u64::from(h);
                        fnv(&mut hash, pixmap.data());
                    }
                }
                min_ns = min_ns.min(ns);
            }
            println!(
                "render files {} pixels {pixels} fnv {hash:016x} reps {reps} min_ns {min_ns}",
                trees.len()
            );
        }
        "both" => {
            let Some(box_px) = args.get(2).and_then(|r| r.parse::<f32>().ok()) else {
                eprintln!("{usage}");
                std::process::exit(2);
            };
            let files: Vec<Vec<u8>> = args[3..]
                .iter()
                .map(|f| std::fs::read(f).unwrap_or_else(|e| panic!("{f}: {e}")))
                .collect();
            let (mut nodes, mut pixels, mut failed) = (0u64, 0u64, 0u64);
            for rep in 0..reps.max(1) {
                let mut ns = 0;
                for data in &files {
                    let t = Instant::now();
                    let out = hot_both(data, &opt, box_px);
                    ns += t.elapsed().as_nanos();
                    if rep == 0 {
                        match &out {
                            Some((tree, pixmap)) => {
                                nodes += count_nodes(tree.root());
                                pixels += u64::from(pixmap.width()) * u64::from(pixmap.height());
                                fnv(&mut hash, pixmap.data());
                            }
                            None => failed += 1,
                        }
                    }
                }
                min_ns = min_ns.min(ns);
            }
            println!(
                "both files {} failed {failed} nodes {nodes} pixels {pixels} fnv {hash:016x} reps {reps} min_ns {min_ns}",
                files.len()
            );
        }
        _ => {
            eprintln!("{usage}");
            std::process::exit(2);
        }
    }
}
