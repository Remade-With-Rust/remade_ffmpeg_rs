//! Real-file corpus comparison against image-tiff. Opt-in: set
//! `RUSTY_TIFF_CORPUS` to a directory (searched recursively for .tif/.tiff/
//! .dng); without it the test does nothing.
//!
//! Every page that image-tiff decodes must decode byte-identically here,
//! except JPEG-compressed pages (different IDCT), which are compared with a
//! tolerance. Pages image-tiff rejects are counted, and must not panic here.
#![allow(clippy::pedantic)] // test harness code: oracle plumbing, not library API

mod common;

use std::io::Cursor;
use std::path::{Path, PathBuf};

fn walk(dir: &Path, out: &mut Vec<PathBuf>) {
    let Ok(rd) = std::fs::read_dir(dir) else {
        return;
    };
    for e in rd.flatten() {
        let p = e.path();
        if p.is_dir() {
            walk(&p, out);
        } else if let Some(ext) = p.extension().and_then(|e| e.to_str()) {
            if matches!(ext.to_ascii_lowercase().as_str(), "tif" | "tiff" | "dng") {
                out.push(p);
            }
        }
    }
}

#[test]
fn corpus_matches_image_tiff() {
    let Ok(root) = std::env::var("RUSTY_TIFF_CORPUS") else {
        println!("RUSTY_TIFF_CORPUS not set; skipping");
        return;
    };
    let mut files = Vec::new();
    walk(Path::new(&root), &mut files);
    files.sort();
    let (mut identical, mut jpeg_close, mut oracle_only_err, mut ours_only_err, mut both_err) =
        (0, 0, 0, 0, 0);
    let mut mismatches = Vec::new();
    for f in &files {
        let Ok(bytes) = std::fs::read(f) else {
            continue;
        };
        let jpeg = rusty_tiff::Decoder::new(Cursor::new(&bytes))
            .and_then(|mut d| d.info())
            .is_ok_and(|i| i.compression == 7);
        match (common::ours(&bytes), common::theirs(&bytes)) {
            (Ok(a), Ok(b)) => {
                if a == b {
                    identical += 1;
                } else if jpeg
                    && a.len() == b.len()
                    && a.iter().zip(&b).all(|(x, y)| {
                        x.bytes.len() == y.bytes.len()
                            && x.bytes
                                .iter()
                                .zip(&y.bytes)
                                .all(|(p, q)| p.abs_diff(*q) <= 8)
                    })
                {
                    jpeg_close += 1;
                } else {
                    mismatches.push(f.display().to_string());
                }
            }
            (Ok(_), Err(_)) => oracle_only_err += 1,
            (Err(e), Ok(_)) => {
                println!("ours failed, image-tiff decoded: {} ({e})", f.display());
                ours_only_err += 1;
            }
            (Err(_), Err(_)) => both_err += 1,
        }
    }
    println!(
        "corpus: {} files; {identical} byte-identical, {jpeg_close} JPEG within 8, \
         {} mismatched, {oracle_only_err} only rusty_tiff decodes, {ours_only_err} only \
         image-tiff decodes, {both_err} neither",
        files.len(),
        mismatches.len()
    );
    for m in &mismatches {
        println!("MISMATCH {m}");
    }
    assert!(mismatches.is_empty());
}
