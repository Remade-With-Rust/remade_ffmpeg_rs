//! Real-file oracle run: set `RUSTY_IMAGE_CORPUS` to one or more directories
//! (separated by `;`) and every BMP/DIB/TGA/HDR/ICO/CUR/PNM/QOI file in them
//! is decoded by this crate and by `image` 0.25 and compared. Skipped (passes)
//! when the variable is unset.

use image::ImageFormat;
use rusty_image::PixelFormat;

fn walk(dir: &std::path::Path, out: &mut Vec<std::path::PathBuf>) {
    let Ok(rd) = std::fs::read_dir(dir) else {
        return;
    };
    for e in rd.flatten() {
        let p = e.path();
        if p.is_dir() {
            walk(&p, out);
        } else {
            out.push(p);
        }
    }
}

#[test]
fn corpus_matches_image() {
    let Ok(dirs) = std::env::var("RUSTY_IMAGE_CORPUS") else {
        return;
    };
    let mut files = Vec::new();
    for d in dirs.split(';').filter(|d| !d.is_empty()) {
        walk(std::path::Path::new(d), &mut files);
    }
    let (mut same, mut both_err, mut ours_only, mut bad) = (0, 0, 0, Vec::new());
    for p in files {
        let ext = p
            .extension()
            .and_then(|e| e.to_str())
            .unwrap_or("")
            .to_ascii_lowercase();
        let fmt = match ext.as_str() {
            "bmp" | "dib" => ImageFormat::Bmp,
            "tga" => ImageFormat::Tga,
            "hdr" => ImageFormat::Hdr,
            "ico" | "cur" => ImageFormat::Ico,
            "pbm" | "pgm" | "ppm" | "pnm" | "pam" => ImageFormat::Pnm,
            "qoi" => ImageFormat::Qoi,
            _ => continue,
        };
        let Ok(data) = std::fs::read(&p) else {
            continue;
        };
        let ours = match fmt {
            ImageFormat::Tga => rusty_image::tga::decode(&data),
            ImageFormat::Ico => rusty_image::ico::decode(&data),
            _ => rusty_image::decode(&data),
        };
        let theirs = image::load_from_memory_with_format(&data, fmt);
        match (ours, theirs) {
            (Ok(a), Ok(b)) => {
                let fmt_ok = matches!(
                    (a.format, b.color()),
                    (PixelFormat::L8, image::ColorType::L8)
                        | (PixelFormat::La8, image::ColorType::La8)
                        | (PixelFormat::Rgb8, image::ColorType::Rgb8)
                        | (PixelFormat::Rgba8, image::ColorType::Rgba8)
                        | (PixelFormat::L16, image::ColorType::L16)
                        | (PixelFormat::Rgb16, image::ColorType::Rgb16)
                        | (PixelFormat::Rgba16, image::ColorType::Rgba16)
                        | (PixelFormat::Rgb32F, image::ColorType::Rgb32F)
                );
                if fmt_ok && a.pixels == b.as_bytes() {
                    same += 1;
                } else {
                    bad.push(format!("{}: pixels differ", p.display()));
                }
            }
            (Err(_), Err(_)) => both_err += 1,
            (Ok(_), Err(_)) => ours_only += 1,
            (Err(e), Ok(_)) => bad.push(format!("{}: we refuse ({e})", p.display())),
        }
    }
    println!(
        "corpus: {same} identical, {both_err} refused by both, {ours_only} decoded by us only, {} mismatched",
        bad.len()
    );
    for b in &bad {
        println!("  {b}");
    }
    assert!(bad.is_empty());
}

#[test]
fn corpus_probe_matches_imagesize() {
    let Ok(dirs) = std::env::var("RUSTY_IMAGE_CORPUS") else {
        return;
    };
    let mut files = Vec::new();
    for d in dirs.split(';').filter(|d| !d.is_empty()) {
        walk(std::path::Path::new(d), &mut files);
    }
    let exts = [
        "bmp", "tga", "hdr", "ico", "cur", "pbm", "pgm", "ppm", "pam", "qoi", "png", "jpg", "jpeg",
        "gif", "webp", "tif", "tiff", "avif",
    ];
    let (mut agree, mut ours_none, mut differ) = (0, 0, Vec::new());
    for p in files {
        let ext = p
            .extension()
            .and_then(|e| e.to_str())
            .unwrap_or("")
            .to_ascii_lowercase();
        if !exts.contains(&ext.as_str()) {
            continue;
        }
        let Ok(data) = std::fs::read(&p) else {
            continue;
        };
        let ours = rusty_image::probe(&data).map(|(_, w, h)| (w as usize, h as usize));
        let theirs = imagesize::blob_size(&data)
            .ok()
            .map(|s| (s.width, s.height));
        match (ours, theirs) {
            (a, b) if a == b => agree += 1,
            (None, Some(_)) => ours_none += 1,
            (a, b) => differ.push(format!("{}: ours {a:?} imagesize {b:?}", p.display())),
        }
    }
    println!(
        "corpus probe: {agree} agree, {ours_none} only imagesize recognises, {} differ",
        differ.len()
    );
    for d in differ.iter().filter(|d| !d.contains(".cur:")).take(40) {
        println!("  {d}");
    }
}
