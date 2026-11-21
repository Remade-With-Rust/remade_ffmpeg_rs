//! Oracle: every SVG in `tests/corpus/` is rendered by rusty_svgtree (on
//! rusty_raster) and by resvg 0.48.1 (on tiny-skia 0.12) at the same size,
//! and the RGBA8 premultiplied buffers are compared.
//!
//! Run `cargo test -p rusty_svgtree --release --test oracle -- --nocapture`
//! for the table. Set `RSVG_DIFF_DIR=<dir>` to also write both renders as
//! PNG for inspection.

use std::path::Path;

struct Stats {
    /// Share of pixels the reference painted (alpha > 0): a blank pair would
    /// otherwise read as "identical".
    coverage_pct: f64,
    max_diff: u8,
    mismatch_pct: f64,
    psnr: f64,
}

fn compare(a: &[u8], b: &[u8]) -> Stats {
    assert_eq!(a.len(), b.len());
    let mut max_diff = 0u8;
    let mut mismatched = 0usize;
    let mut se = 0f64;
    let mut painted = 0usize;
    for (pa, pb) in a.chunks_exact(4).zip(b.chunks_exact(4)) {
        let mut any = false;
        if pb[3] > 0 {
            painted += 1;
        }
        for c in 0..4 {
            let d = pa[c].abs_diff(pb[c]);
            if d > 0 {
                any = true;
            }
            max_diff = max_diff.max(d);
            se += f64::from(d) * f64::from(d);
        }
        if any {
            mismatched += 1;
        }
    }
    let n = (a.len() / 4).max(1);
    let mse = se / a.len().max(1) as f64;
    let psnr = if mse == 0.0 {
        f64::INFINITY
    } else {
        10.0 * (255.0f64 * 255.0 / mse).log10()
    };
    Stats {
        coverage_pct: painted as f64 * 100.0 / n as f64,
        max_diff,
        mismatch_pct: mismatched as f64 * 100.0 / n as f64,
        psnr,
    }
}

fn render_ours(data: &[u8], scale: f32) -> Option<(u32, u32, Vec<u8>)> {
    let tree = rusty_svgtree::Tree::from_data(data, &rusty_svgtree::Options::default()).ok()?;
    let w = (tree.size().width() * scale).ceil() as u32;
    let h = (tree.size().height() * scale).ceil() as u32;
    let mut pixmap = rusty_svgtree::rusty_raster::Pixmap::new(w, h)?;
    rusty_svgtree::render(
        &tree,
        rusty_svgtree::rusty_raster::Transform::from_scale(scale, scale),
        &mut pixmap.as_mut(),
    );
    Some((w, h, pixmap.data().to_vec()))
}

fn render_resvg(data: &[u8], scale: f32) -> Option<(u32, u32, Vec<u8>)> {
    let tree = resvg::usvg::Tree::from_data(data, &resvg::usvg::Options::default()).ok()?;
    let w = (tree.size().width() * scale).ceil() as u32;
    let h = (tree.size().height() * scale).ceil() as u32;
    let mut pixmap = resvg::tiny_skia::Pixmap::new(w, h)?;
    resvg::render(
        &tree,
        resvg::tiny_skia::Transform::from_scale(scale, scale),
        &mut pixmap.as_mut(),
    );
    Some((w, h, pixmap.data().to_vec()))
}

fn write_png(dir: &Path, name: &str, w: u32, h: u32, data: Vec<u8>) {
    if let Some(size) = resvg::tiny_skia::IntSize::from_wh(w, h) {
        if let Some(p) = resvg::tiny_skia::Pixmap::from_vec(data, size) {
            let _ = p.save_png(dir.join(name));
        }
    }
}

/// Per-file PSNR floors (dB) below which the test fails; the default floor
/// applies to every file not listed. Each exception is explained.
fn floor_for(name: &str) -> f64 {
    match name {
        // JPEG: rusty_jpeg (a jpeg-decoder fork) and resvg's zune-jpeg round
        // the IDCT/upsampling differently; geometry is identical.
        "22-image-jpeg.svg" => 25.0,
        _ => 30.0,
    }
}

#[test]
fn oracle_corpus() {
    let dir = Path::new(env!("CARGO_MANIFEST_DIR")).join("tests/corpus");
    let diff_dir = std::env::var_os("RSVG_DIFF_DIR").map(std::path::PathBuf::from);
    let mut entries: Vec<_> = std::fs::read_dir(&dir)
        .unwrap()
        .filter_map(Result::ok)
        .map(|e| e.path())
        .filter(|p| p.extension().is_some_and(|e| e == "svg"))
        .collect();
    entries.sort();
    assert!(!entries.is_empty());
    println!(
        "{:<36} {:>5} {:>9} {:>6} {:>4} {:>8} {:>9}",
        "file", "scale", "size", "cover%", "max", "mism%", "PSNR dB"
    );
    let mut failures = Vec::new();
    let mut rows = 0;
    let mut identical = 0;
    let mut psnrs = Vec::new();
    for path in &entries {
        let name = path.file_name().unwrap().to_string_lossy().to_string();
        let data = std::fs::read(path).unwrap();
        for scale in [0.66f32, 1.0, 2.5] {
            let ours = render_ours(&data, scale);
            let theirs = render_resvg(&data, scale);
            let (Some((w, h, a)), Some((w2, h2, b))) = (ours, theirs) else {
                println!("{name:<36} {scale:>5} parse/render failed on one side");
                failures.push(format!("{name}@{scale}: one side failed"));
                continue;
            };
            if (w, h) != (w2, h2) {
                println!("{name:<36} {scale:>5} size mismatch {w}x{h} vs {w2}x{h2}");
                failures.push(format!("{name}@{scale}: size {w}x{h} vs {w2}x{h2}"));
                continue;
            }
            let s = compare(&a, &b);
            rows += 1;
            if s.max_diff == 0 {
                identical += 1;
            }
            psnrs.push(s.psnr);
            let psnr = if s.psnr.is_finite() {
                format!("{:.2}", s.psnr)
            } else {
                "inf".to_string()
            };
            println!(
                "{name:<36} {scale:>5} {:>9} {:>6.1} {:>4} {:>8.3} {:>9}",
                format!("{w}x{h}"),
                s.coverage_pct,
                s.max_diff,
                s.mismatch_pct,
                psnr
            );
            if let Some(d) = &diff_dir {
                let stem = name.trim_end_matches(".svg");
                write_png(d, &format!("{stem}@{scale}-ours.png"), w, h, a);
                write_png(d, &format!("{stem}@{scale}-resvg.png"), w, h, b);
            }
            if s.psnr < floor_for(&name) {
                failures.push(format!(
                    "{name}@{scale}: PSNR {psnr} < {}",
                    floor_for(&name)
                ));
            }
        }
    }
    let finite: Vec<f64> = psnrs.iter().copied().filter(|p| p.is_finite()).collect();
    let min = finite.iter().copied().fold(f64::INFINITY, f64::min);
    println!(
        "rows {rows}, pixel-identical {identical}, min finite PSNR {:.2} dB",
        min
    );
    assert!(
        failures.is_empty(),
        "oracle failures:\n{}",
        failures.join("\n")
    );
}

/// Opt-in: `RSVG_EXTRA_DIR=<dir>` compares every `*.svg` in a directory
/// (rendered to fit 800 px), printing outliers and a summary. Measurement
/// only: it asserts nothing beyond "no panic".
#[test]
fn oracle_external_dir() {
    let Some(dir) = std::env::var_os("RSVG_EXTRA_DIR") else {
        return;
    };
    let mut entries: Vec<_> = std::fs::read_dir(dir)
        .unwrap()
        .filter_map(Result::ok)
        .map(|e| e.path())
        .filter(|p| p.extension().is_some_and(|e| e == "svg"))
        .collect();
    entries.sort();
    let (mut both_fail, mut ours_only_fail, mut resvg_only_fail) = (0, 0, 0);
    let mut psnrs = Vec::new();
    let mut identical = 0;
    for path in &entries {
        let name = path.file_name().unwrap().to_string_lossy().to_string();
        let data = std::fs::read(path).unwrap();
        let ours = rusty_svgtree::Tree::from_data(&data, &rusty_svgtree::Options::default());
        let theirs = resvg::usvg::Tree::from_data(&data, &resvg::usvg::Options::default());
        let (ok_o, ok_t) = (ours.is_ok(), theirs.is_ok());
        match (ok_o, ok_t) {
            (false, false) => {
                both_fail += 1;
                continue;
            }
            (false, true) => {
                ours_only_fail += 1;
                println!("{name}: ours failed: {:?}", ours.err());
                continue;
            }
            (true, false) => {
                resvg_only_fail += 1;
                println!("{name}: resvg failed: {:?}", theirs.err());
                continue;
            }
            (true, true) => {}
        }
        let t = theirs.unwrap();
        let scale = (800.0 / t.size().width().max(t.size().height())).min(1.0);
        let (Some((w, h, a)), Some((w2, h2, b))) =
            (render_ours(&data, scale), render_resvg(&data, scale))
        else {
            println!("{name}: render failed on one side");
            continue;
        };
        if (w, h) != (w2, h2) {
            println!("{name}: size {w}x{h} vs {w2}x{h2}");
            continue;
        }
        let s = compare(&a, &b);
        if s.max_diff == 0 {
            identical += 1;
        }
        if s.max_diff > 0 {
            println!(
                "{name}: {w}x{h} cover {:.1}% max {} mism {:.3}% PSNR {:.2}",
                s.coverage_pct, s.max_diff, s.mismatch_pct, s.psnr
            );
            if let Some(d) = std::env::var_os("RSVG_DIFF_DIR") {
                let d = std::path::PathBuf::from(d);
                let stem = name.trim_end_matches(".svg");
                write_png(&d, &format!("{stem}-ours.png"), w, h, a);
                write_png(&d, &format!("{stem}-resvg.png"), w, h, b);
            }
        }
        psnrs.push(s.psnr);
    }
    let mut finite: Vec<f64> = psnrs.iter().copied().filter(|p| p.is_finite()).collect();
    finite.sort_by(f64::total_cmp);
    let n = psnrs.len();
    let at_least = |db: f64| psnrs.iter().filter(|p| **p >= db).count();
    println!(
        "files {}, compared {n}, pixel-identical {identical}, PSNR>=40 {}, PSNR>=30 {}, \
         min finite {:.2}, both-fail {both_fail}, ours-only-fail {ours_only_fail}, \
         resvg-only-fail {resvg_only_fail}",
        entries.len(),
        at_least(40.0),
        at_least(30.0),
        finite.first().copied().unwrap_or(f64::INFINITY),
    );
}
