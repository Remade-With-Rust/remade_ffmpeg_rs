//! rusty_tiff vs libtiff.
//!
//! ```text
//! tiffvs corpus <dir>                    compare every page, byte for byte
//! tiffvs gen <src.ppm> <outdir>          write libtiff-produced variants
//! tiffvs dec <file.tif> <pairs> [null]   decode speed, ABBA, one process
//! tiffvs enc <file.tif> <lzw|deflate> <pairs> [null]   encode speed
//! ```
//!
//! Method (enforced here, see codec-measurement): both arms in one binary,
//! leading arm alternated every pair, `Instant` (CLOCK_MONOTONIC, ns) around
//! the work only, thread CPU time per sample as a validity check, a work
//! hash compared between arms before timing, min-of-N plus paired win rate
//! and z. Pin the process with `taskset -c N`.

#[global_allocator]
static GLOBAL_ALLOC: rusty_alloc_api::RustyAlloc = rusty_alloc_api::RustyAlloc;

mod ffi;

use std::collections::BTreeMap;
use std::ffi::{c_int, c_uint};
use std::io::Cursor;
use std::time::Instant;

use rusty_tiff::encoder::{colortype, Compression, TiffEncoder};
use rusty_tiff::{Decoder, DecodingResult};

fn fnv(b: &[u8]) -> u64 {
    let mut h = 0xcbf2_9ce4_8422_2325u64;
    for &x in b {
        h = (h ^ u64::from(x)).wrapping_mul(0x100_0000_01b3);
    }
    h
}

// ---------------------------------------------------------------------------
// libtiff decode of one page into rusty_tiff's raw layout (interleaved,
// rows packed to whole bytes, native-endian samples).

struct Page {
    desc: String,
    jpeg_ycbcr: bool,
    bytes: Vec<u8>,
}

fn copy_chunk(
    out: &mut [u8],
    chunk: &[u8],
    (w, spp, bits): (usize, usize, usize),
    (x0, y0, cols, rows): (usize, usize, usize, usize),
    chunk_row: usize,
    plane: Option<usize>,
) {
    let out_row = (w * spp * bits).div_ceil(8);
    for r in 0..rows {
        let src = &chunk[r * chunk_row..];
        let dst = &mut out[(y0 + r) * out_row..][..out_row];
        if bits >= 8 {
            let sb = bits / 8;
            match plane {
                None => {
                    let n = cols * spp * sb;
                    dst[x0 * spp * sb..][..n].copy_from_slice(&src[..n]);
                }
                Some(p) => {
                    for i in 0..cols {
                        dst[((x0 + i) * spp + p) * sb..][..sb]
                            .copy_from_slice(&src[i * sb..][..sb]);
                    }
                }
            }
        } else if plane.is_none() && x0 == 0 && chunk_row == out_row {
            dst.copy_from_slice(&src[..out_row]);
        } else {
            let n = if plane.is_some() { cols } else { cols * spp };
            for i in 0..n {
                for k in 0..bits {
                    let sb = i * bits + k;
                    let bit = (src[sb >> 3] >> (7 - (sb & 7))) & 1;
                    let di = match plane {
                        None => (x0 * spp + i) * bits + k,
                        Some(p) => ((x0 + i) * spp + p) * bits + k,
                    };
                    let m = 0x80u8 >> (di & 7);
                    if bit == 1 {
                        dst[di >> 3] |= m;
                    } else {
                        dst[di >> 3] &= !m;
                    }
                }
            }
        }
    }
}

/// Decode the current directory of `t`.
fn libtiff_page(t: *mut ffi::TIFF) -> Result<Page, String> {
    let w = ffi::get_u32(t, 256, false).ok_or("no width")? as usize;
    let h = ffi::get_u32(t, 257, false).ok_or("no height")? as usize;
    let bits = usize::from(ffi::get_u16(t, 258, true).unwrap_or(1));
    let spp = usize::from(ffi::get_u16(t, 277, true).unwrap_or(1));
    let planar = ffi::get_u16(t, 284, true).unwrap_or(1) == 2 && spp > 1;
    let comp = ffi::get_u16(t, 259, true).unwrap_or(1);
    let photo = ffi::get_u16(t, 262, false).unwrap_or(9999);
    let fmt = ffi::get_u16(t, 339, true).unwrap_or(1);
    let tiled = unsafe { ffi::TIFFIsTiled(t) } != 0;
    let desc = format!(
        "comp={comp:<5} photo={photo:<5} bits={bits:<2} fmt={fmt} spp={spp} {}{}",
        if tiled { "tiles" } else { "strips" },
        if planar { " planar" } else { "" }
    );
    let jpeg_ycbcr = comp == 7 && photo == 6;
    if jpeg_ycbcr {
        // JPEGCOLORMODE = RGB: libjpeg converts, like rusty_tiff's read_image.
        unsafe { ffi::TIFFSetField(t, 65538, 1 as c_int) };
    }
    if w == 0 || h == 0 || bits == 0 || w * h * spp * bits > 1 << 34 {
        return Err("bad geometry".into());
    }
    let out_row = (w * spp * bits).div_ceil(8);
    let mut out = vec![0u8; out_row * h];
    let planes = if planar { spp } else { 1 };
    let spp_chunk = if planar { 1 } else { spp };
    let geom = (w, spp, bits);
    if tiled {
        let tw = ffi::get_u32(t, 322, false).ok_or("no tile width")? as usize;
        let th = ffi::get_u32(t, 323, false).ok_or("no tile length")? as usize;
        let size = unsafe { ffi::TIFFTileSize(t) };
        if size <= 0 {
            return Err("tile size".into());
        }
        let mut buf = vec![0u8; size as usize];
        let (across, down) = (w.div_ceil(tw), h.div_ceil(th));
        let n = unsafe { ffi::TIFFNumberOfTiles(t) } as usize;
        if n < across * down * planes {
            return Err("tile count".into());
        }
        let chunk_row = (tw * spp_chunk * bits).div_ceil(8);
        for i in 0..across * down * planes {
            let r = unsafe { ffi::TIFFReadEncodedTile(t, i as u32, buf.as_mut_ptr().cast(), size) };
            if r < 0 {
                return Err(format!("TIFFReadEncodedTile {i} failed"));
            }
            let plane = i / (across * down);
            let k = i % (across * down);
            let (x0, y0) = ((k % across) * tw, (k / across) * th);
            copy_chunk(
                &mut out,
                &buf,
                geom,
                (x0, y0, tw.min(w - x0), th.min(h - y0)),
                chunk_row,
                planar.then_some(plane),
            );
        }
    } else {
        let rps = (ffi::get_u32(t, 278, true).unwrap_or(u32::MAX) as usize).clamp(1, h);
        let size = unsafe { ffi::TIFFStripSize(t) };
        if size <= 0 {
            return Err("strip size".into());
        }
        let mut buf = vec![0u8; size as usize];
        let per_plane = h.div_ceil(rps);
        let n = unsafe { ffi::TIFFNumberOfStrips(t) } as usize;
        if n < per_plane * planes {
            return Err("strip count".into());
        }
        let chunk_row = (w * spp_chunk * bits).div_ceil(8);
        for s in 0..per_plane * planes {
            let r =
                unsafe { ffi::TIFFReadEncodedStrip(t, s as u32, buf.as_mut_ptr().cast(), size) };
            if r < 0 {
                return Err(format!("TIFFReadEncodedStrip {s} failed"));
            }
            let plane = s / per_plane;
            let y0 = (s % per_plane) * rps;
            copy_chunk(
                &mut out,
                &buf,
                geom,
                (0, y0, w, rps.min(h - y0)),
                chunk_row,
                planar.then_some(plane),
            );
        }
    }
    Ok(Page {
        desc,
        jpeg_ycbcr,
        bytes: out,
    })
}

/// All pages of a file with libtiff.
fn libtiff_pages(file: &[u8]) -> Result<Vec<Page>, String> {
    let mut m = ffi::Mem {
        data: file.to_vec(),
        pos: 0,
    };
    let t = ffi::open(&mut m, "r");
    if t.is_null() {
        return Err("TIFFClientOpen failed".into());
    }
    let mut pages = Vec::new();
    let res = loop {
        match libtiff_page(t) {
            Ok(p) => pages.push(p),
            Err(e) => break Err(e),
        }
        if pages.len() > 1000 || unsafe { ffi::TIFFReadDirectory(t) } == 0 {
            break Ok(());
        }
    };
    unsafe { ffi::TIFFClose(t) };
    res.map(|()| pages)
}

/// All pages with rusty_tiff: raw samples, except JPEG YCbCr (RGB).
fn rusty_pages(file: &[u8]) -> Result<Vec<Vec<u8>>, String> {
    let mut d = Decoder::new(Cursor::new(file)).map_err(|e| e.to_string())?;
    let mut out = Vec::new();
    loop {
        let i = d.info().map_err(|e| e.to_string())?;
        let r = if i.compression == 7 && i.photometric == 6 {
            d.read_image()
        } else {
            d.read_image_raw()
        }
        .map_err(|e| e.to_string())?;
        out.push(r.to_ne_bytes());
        if !d.more_images() || out.len() > 1000 {
            break;
        }
        d.next_image().map_err(|e| e.to_string())?;
    }
    Ok(out)
}

fn corpus(dir: &str) {
    ffi::quiet();
    println!("libtiff: {}", ffi::version());
    let mut files: Vec<_> = std::fs::read_dir(dir)
        .expect("corpus dir")
        .flatten()
        .map(|e| e.path())
        .filter(|p| {
            p.extension()
                .and_then(|e| e.to_str())
                .is_some_and(|e| matches!(e.to_ascii_lowercase().as_str(), "tif" | "tiff"))
        })
        .collect();
    files.sort();
    #[derive(Default)]
    struct Row {
        pages: usize,
        identical: usize,
        jpeg_close: usize,
        jpeg_max: u8,
        mismatch: usize,
        ours_err: usize,
        libtiff_err: usize,
    }
    let mut table: BTreeMap<String, Row> = BTreeMap::new();
    let mut nfiles = 0;
    for f in &files {
        let bytes = std::fs::read(f).unwrap();
        nfiles += 1;
        let lt = libtiff_pages(&bytes);
        let ours = rusty_pages(&bytes);
        match (lt, ours) {
            (Ok(lp), Ok(op)) => {
                for (k, l) in lp.iter().enumerate() {
                    let row = table.entry(l.desc.clone()).or_default();
                    row.pages += 1;
                    match op.get(k) {
                        Some(o) if *o == l.bytes => row.identical += 1,
                        Some(o) if l.jpeg_ycbcr || l.desc.starts_with("comp=7 ") => {
                            let max = o
                                .iter()
                                .zip(&l.bytes)
                                .map(|(a, b)| a.abs_diff(*b))
                                .max()
                                .unwrap_or(255);
                            if o.len() == l.bytes.len() && max <= 16 {
                                row.jpeg_close += 1;
                                row.jpeg_max = row.jpeg_max.max(max);
                            } else {
                                row.mismatch += 1;
                                println!("MISMATCH {} page {k} ({})", f.display(), l.desc);
                            }
                        }
                        _ => {
                            row.mismatch += 1;
                            println!("MISMATCH {} page {k} ({})", f.display(), l.desc);
                        }
                    }
                }
            }
            (Ok(lp), Err(e)) => {
                for l in &lp {
                    table.entry(l.desc.clone()).or_default().ours_err += 1;
                }
                println!("RUSTY_TIFF_ERR {} ({e})", f.display());
            }
            (Err(e), Ok(_)) => {
                table
                    .entry("(libtiff failed)".into())
                    .or_default()
                    .libtiff_err += 1;
                println!("LIBTIFF_ERR {} ({e})", f.display());
            }
            (Err(_), Err(e)) => {
                table.entry("(both failed)".into()).or_default().libtiff_err += 1;
                println!("BOTH_ERR {} ({e})", f.display());
            }
        }
    }
    println!("\n| layout | pages | identical | JPEG within tol (max) | mismatch | rusty_tiff err | libtiff err |");
    println!("|---|---:|---:|---:|---:|---:|---:|");
    let mut tot = Row::default();
    for (k, r) in &table {
        println!(
            "| {k} | {} | {} | {} | {} | {} | {} |",
            r.pages,
            r.identical,
            if r.jpeg_close > 0 {
                format!("{} ({})", r.jpeg_close, r.jpeg_max)
            } else {
                "0".into()
            },
            r.mismatch,
            r.ours_err,
            r.libtiff_err
        );
        tot.pages += r.pages;
        tot.identical += r.identical;
        tot.jpeg_close += r.jpeg_close;
        tot.mismatch += r.mismatch;
        tot.ours_err += r.ours_err;
        tot.libtiff_err += r.libtiff_err;
    }
    println!(
        "TOTAL files={nfiles} pages={} identical={} jpeg_close={} mismatch={} rusty_tiff_err={} libtiff_err={}",
        tot.pages, tot.identical, tot.jpeg_close, tot.mismatch, tot.ours_err, tot.libtiff_err
    );
}

// ---------------------------------------------------------------------------
// libtiff writers for the corpus (CCITT, JPEG and other layouts IM lacks).

fn read_ppm(path: &str) -> (usize, usize, Vec<u8>) {
    let b = std::fs::read(path).expect("ppm");
    let mut fields = Vec::new();
    let mut i = 0;
    while fields.len() < 4 {
        while b[i].is_ascii_whitespace() {
            i += 1;
        }
        if b[i] == b'#' {
            while b[i] != b'\n' {
                i += 1;
            }
            continue;
        }
        let s = i;
        while !b[i].is_ascii_whitespace() {
            i += 1;
        }
        fields.push(String::from_utf8_lossy(&b[s..i]).to_string());
    }
    assert_eq!(fields[0], "P6");
    let (w, h): (usize, usize) = (fields[1].parse().unwrap(), fields[2].parse().unwrap());
    (w, h, b[i + 1..i + 1 + w * h * 3].to_vec())
}

struct WriteSpec<'a> {
    w: usize,
    h: usize,
    bits: u16,
    spp: u16,
    photo: u16,
    comp: u16,
    rps: u32,
    extra: &'a [(u32, i64)],
    data: &'a [u8],
}

/// Write one page with libtiff into memory. `extra` tags are passed as int
/// (u16-typed tags) except where the value needs u32 (negative marks u32).
fn libtiff_write(mode: &str, pages: &[WriteSpec<'_>]) -> Vec<u8> {
    let mut m = ffi::Mem {
        data: Vec::new(),
        pos: 0,
    };
    let t = ffi::open(&mut m, mode);
    assert!(!t.is_null());
    for p in pages {
        unsafe {
            ffi::TIFFSetField(t, 256, p.w as c_uint);
            ffi::TIFFSetField(t, 257, p.h as c_uint);
            ffi::TIFFSetField(t, 258, c_int::from(p.bits));
            ffi::TIFFSetField(t, 277, c_int::from(p.spp));
            ffi::TIFFSetField(t, 262, c_int::from(p.photo));
            ffi::TIFFSetField(t, 259, c_int::from(p.comp));
            ffi::TIFFSetField(t, 284, 1 as c_int);
            ffi::TIFFSetField(t, 278, p.rps as c_uint);
            for &(tag, v) in p.extra {
                if matches!(tag, 292 | 293) {
                    ffi::TIFFSetField(t, tag, v as c_uint);
                } else {
                    ffi::TIFFSetField(t, tag, v as c_int);
                }
            }
        }
        let row = (p.w * usize::from(p.spp) * usize::from(p.bits)).div_ceil(8);
        let rps = (p.rps as usize).min(p.h);
        for (s, y0) in (0..p.h).step_by(rps).enumerate() {
            let rows = rps.min(p.h - y0);
            let mut chunk = p.data[y0 * row..(y0 + rows) * row].to_vec();
            let r = unsafe {
                ffi::TIFFWriteEncodedStrip(
                    t,
                    s as u32,
                    chunk.as_mut_ptr().cast(),
                    chunk.len() as isize,
                )
            };
            assert!(r >= 0, "TIFFWriteEncodedStrip failed");
        }
        unsafe { ffi::TIFFWriteDirectory(t) };
    }
    unsafe { ffi::TIFFClose(t) };
    m.data
}

#[allow(clippy::too_many_arguments)]
fn one<'a>(
    w: usize,
    h: usize,
    bits: u16,
    spp: u16,
    photo: u16,
    comp: u16,
    rps: u32,
    extra: &'a [(u32, i64)],
    data: &'a [u8],
) -> Vec<WriteSpec<'a>> {
    vec![WriteSpec {
        w,
        h,
        bits,
        spp,
        photo,
        comp,
        rps,
        extra,
        data,
    }]
}

fn gen(src: &str, outdir: &str) {
    ffi::quiet();
    std::fs::create_dir_all(outdir).unwrap();
    let (w, h, rgb) = read_ppm(src);
    let grey: Vec<u8> = rgb
        .chunks_exact(3)
        .map(|p| ((u32::from(p[0]) * 77 + u32::from(p[1]) * 150 + u32::from(p[2]) * 29) >> 8) as u8)
        .collect();
    // Bilevel, document-like: threshold, black = 1 (MINISWHITE).
    let row1 = w.div_ceil(8);
    let mut bilevel = vec![0u8; row1 * h];
    for y in 0..h {
        for x in 0..w {
            if grey[y * w + x] < 110 {
                bilevel[y * row1 + x / 8] |= 0x80 >> (x % 8);
            }
        }
    }
    let rgb16: Vec<u8> = rgb
        .iter()
        .flat_map(|&v| (u16::from(v) * 257 ^ u16::from(v >> 3)).to_ne_bytes())
        .collect();
    let f32s: Vec<u8> = grey
        .iter()
        .flat_map(|&v| (f32::from(v) / 255.0 - 0.25).to_ne_bytes())
        .collect();
    let mut n = 0;
    let mut put = |name: &str, bytes: Vec<u8>| {
        std::fs::write(format!("{outdir}/lt_{name}.tif"), bytes).unwrap();
        n += 1;
    };
    let ccitt: &[(&str, u16, &[(u32, i64)])] = &[
        ("mh", 2, &[]),
        ("g3_1d", 3, &[(292, 0)]),
        ("g3_2d", 3, &[(292, 1)]),
        ("g3_2d_fill", 3, &[(292, 5)]),
        ("g3_1d_fill", 3, &[(292, 4)]),
        ("g4", 4, &[]),
    ];
    for &(name, comp, extra) in ccitt {
        for (mode, tag) in [("w", "le"), ("wb", "be")] {
            for rps in [h as u32, 64] {
                put(
                    &format!("{name}_{tag}_rps{rps}"),
                    libtiff_write(mode, &one(w, h, 1, 1, 0, comp, rps, extra, &bilevel)),
                );
            }
        }
        // FillOrder 2 (LSB first), as many fax devices write.
        let mut e = extra.to_vec();
        e.push((266, 2));
        put(
            &format!("{name}_lsb"),
            libtiff_write("w", &one(w, h, 1, 1, 0, comp, h as u32, &e, &bilevel)),
        );
    }
    // Multipage fax.
    let pages: Vec<WriteSpec> = (0..3)
        .map(|_| WriteSpec {
            w,
            h,
            bits: 1,
            spp: 1,
            photo: 0,
            comp: 4,
            rps: h as u32,
            extra: &[],
            data: &bilevel,
        })
        .collect();
    put("g4_multipage", libtiff_write("w", &pages));
    // JPEG: YCbCr (libtiff converts RGB input, 2x2 subsampling), RGB, grey.
    for (name, photo, spp, data, extra) in [
        (
            "jpeg_ycbcr",
            6u16,
            3u16,
            &rgb,
            vec![(65537u32, 85i64), (65538, 1)],
        ),
        ("jpeg_rgb", 2, 3, &rgb, vec![(65537, 85)]),
        ("jpeg_grey", 1, 1, &grey, vec![(65537, 85)]),
    ] {
        for rps in [16u32, 64] {
            put(
                &format!("{name}_rps{rps}"),
                libtiff_write("w", &one(w, h, 8, spp, photo, 7, rps, &extra, data)),
            );
        }
    }
    // Predictors, byte orders, other compressions.
    for (mode, tag) in [("w", "le"), ("wb", "be")] {
        for (cname, comp) in [("none", 1u16), ("lzw", 5), ("zip", 8), ("packbits", 32773)] {
            put(
                &format!("rgb8_{cname}_{tag}"),
                libtiff_write(mode, &one(w, h, 8, 3, 2, comp, 32, &[], &rgb)),
            );
            put(
                &format!("rgb16_{cname}_{tag}"),
                libtiff_write(mode, &one(w, h, 16, 3, 2, comp, 32, &[], &rgb16)),
            );
            put(
                &format!("grey8_{cname}_{tag}"),
                libtiff_write(mode, &one(w, h, 8, 1, 1, comp, 32, &[], &grey)),
            );
            put(
                &format!("f32_{cname}_{tag}"),
                libtiff_write(mode, &one(w, h, 32, 1, 1, comp, 32, &[(339, 3)], &f32s)),
            );
            if comp == 5 || comp == 8 {
                put(
                    &format!("rgb8_{cname}_p2_{tag}"),
                    libtiff_write(mode, &one(w, h, 8, 3, 2, comp, 32, &[(317, 2)], &rgb)),
                );
                put(
                    &format!("rgb16_{cname}_p2_{tag}"),
                    libtiff_write(mode, &one(w, h, 16, 3, 2, comp, 32, &[(317, 2)], &rgb16)),
                );
                put(
                    &format!("f32_{cname}_p3_{tag}"),
                    libtiff_write(
                        mode,
                        &one(w, h, 32, 1, 1, comp, 32, &[(339, 3), (317, 3)], &f32s),
                    ),
                );
            }
            put(
                &format!("bilevel_{cname}_{tag}"),
                libtiff_write(mode, &one(w, h, 1, 1, 0, comp, 32, &[], &bilevel)),
            );
        }
    }
    println!("wrote {n} libtiff files to {outdir}");
}

// ---------------------------------------------------------------------------
// Timing.

struct Stats {
    min_a: f64,
    min_b: f64,
    med_a: f64,
    med_b: f64,
    wins_a: usize,
    n: usize,
    cpu_wall: (f64, f64),
}

fn median(v: &mut [f64]) -> f64 {
    v.sort_by(f64::total_cmp);
    v[v.len() / 2]
}

/// Interleave two arms; arm functions return a work hash.
fn abba(pairs: usize, mut a: impl FnMut() -> u64, mut b: impl FnMut() -> u64) -> Stats {
    // Warm up and check work parity.
    let (ha, hb) = (a(), b());
    assert_eq!(ha, hb, "work parity: arms produced different output");
    let _ = (a(), b());
    let (mut ta, mut tb) = (Vec::new(), Vec::new());
    let mut ratios = Vec::new();
    let time = |f: &mut dyn FnMut() -> u64| {
        let c0 = ffi::thread_cpu();
        let t0 = Instant::now();
        let h = f();
        let wall = t0.elapsed().as_secs_f64();
        let cpu = ffi::thread_cpu() - c0;
        std::hint::black_box(h);
        (wall, cpu / wall)
    };
    for i in 0..pairs {
        let (x, y) = if i % 2 == 0 {
            let x = time(&mut a);
            (x, time(&mut b))
        } else {
            let y = time(&mut b);
            (time(&mut a), y)
        };
        ta.push(x.0);
        tb.push(y.0);
        ratios.push(x.1);
        ratios.push(y.1);
    }
    let wins_a = ta.iter().zip(&tb).filter(|(x, y)| x < y).count();
    let lo = ratios.iter().copied().fold(f64::MAX, f64::min);
    let hi = ratios.iter().copied().fold(0.0, f64::max);
    let min_a = ta.iter().copied().fold(f64::MAX, f64::min);
    let min_b = tb.iter().copied().fold(f64::MAX, f64::min);
    Stats {
        min_a,
        min_b,
        med_a: median(&mut ta),
        med_b: median(&mut tb),
        wins_a,
        n: pairs,
        cpu_wall: (lo, hi),
    }
}

fn report(label: &str, a: &str, b: &str, bytes: usize, px: usize, s: &Stats) {
    let z = (s.wins_a as f64 - s.n as f64 / 2.0) / (0.5 * (s.n as f64).sqrt());
    println!(
        "{label}: {a} min {:.2} ms ({:.0} MB/s, {:.1} Mpx/s) | {b} min {:.2} ms ({:.0} MB/s, {:.1} Mpx/s) | \
         {b}/{a} time {:.3}x | medians {:.2}/{:.2} ms | {a} faster in {}/{} pairs, z={:+.2} | cpu/wall {:.3}-{:.3}",
        s.min_a * 1e3,
        bytes as f64 / s.min_a / 1e6,
        px as f64 / s.min_a / 1e6,
        s.min_b * 1e3,
        bytes as f64 / s.min_b / 1e6,
        px as f64 / s.min_b / 1e6,
        s.min_b / s.min_a,
        s.med_a * 1e3,
        s.med_b * 1e3,
        s.wins_a,
        s.n,
        z,
        s.cpu_wall.0,
        s.cpu_wall.1
    );
}

fn rust_decode(file: &[u8]) -> u64 {
    let mut d = Decoder::new(Cursor::new(file)).unwrap();
    let r = d.read_image_raw().unwrap();
    hash_result(&r)
}

fn hash_result(r: &DecodingResult) -> u64 {
    // Cheap, order-sensitive digest of a sample of the buffer (full hashing
    // would be timed work); parity is checked in full before timing.
    let b = r.to_ne_bytes();
    fnv(&b[..b.len().min(4096)]) ^ b.len() as u64
}

fn libtiff_decode(m: &mut ffi::Mem) -> u64 {
    m.pos = 0;
    let t = ffi::open(m, "r");
    let p = libtiff_page(t).unwrap();
    unsafe { ffi::TIFFClose(t) };
    fnv(&p.bytes[..p.bytes.len().min(4096)]) ^ p.bytes.len() as u64
}

fn dec(path: &str, pairs: usize, null: bool) {
    ffi::quiet();
    let file = std::fs::read(path).unwrap();
    let (w, h, bytes) = {
        let mut d = Decoder::new(Cursor::new(&file[..])).unwrap();
        let (w, h) = d.dimensions().unwrap();
        let r = d.read_image_raw().unwrap();
        // Full work parity before timing.
        let lt = libtiff_pages(&file).unwrap();
        assert_eq!(r.to_ne_bytes(), lt[0].bytes, "decoded buffers differ");
        println!(
            "parity: {} bytes, FNV {:016x} both arms ({})",
            lt[0].bytes.len(),
            fnv(&lt[0].bytes),
            lt[0].desc
        );
        (w as usize, h as usize, lt[0].bytes.len())
    };
    let mut m = ffi::Mem {
        data: file.clone(),
        pos: 0,
    };
    let label = format!("decode {}", path.rsplit('/').next().unwrap_or(path));
    if null {
        let s = abba(pairs, || rust_decode(&file), || rust_decode(&file));
        report(&format!("{label} NULL"), "rust", "rust'", bytes, w * h, &s);
    } else {
        let s = abba(pairs, || rust_decode(&file), || libtiff_decode(&mut m));
        report(&label, "rusty_tiff", "libtiff", bytes, w * h, &s);
    }
}

fn rust_encode(px: &DecodingResult, w: u32, h: u32, comp: Compression) -> Vec<u8> {
    let mut f = Cursor::new(Vec::new());
    {
        let mut e = TiffEncoder::new(&mut f).unwrap();
        match px {
            DecodingResult::U8(v) => {
                let mut i = e
                    .new_image::<colortype::RGB8>(w, h)
                    .unwrap()
                    .with_compression(comp);
                i.rows_per_strip(16);
                i.write_data(v).unwrap();
            }
            DecodingResult::U16(v) => {
                let mut i = e
                    .new_image::<colortype::RGB16>(w, h)
                    .unwrap()
                    .with_compression(comp);
                i.rows_per_strip(16);
                i.write_data(v).unwrap();
            }
            _ => panic!("RGB8/RGB16 only"),
        }
    }
    f.into_inner()
}

fn pixel_hash(file: &[u8]) -> u64 {
    let mut d = Decoder::new(Cursor::new(file)).unwrap();
    fnv(&d.read_image_raw().unwrap().to_ne_bytes())
}

fn enc(path: &str, which: &str, pairs: usize, null: bool) {
    ffi::quiet();
    let file = std::fs::read(path).unwrap();
    let mut d = Decoder::new(Cursor::new(&file[..])).unwrap();
    let (w, h) = d.dimensions().unwrap();
    let px = d.read_image_raw().unwrap();
    let raw = px.to_ne_bytes();
    let bits: u16 = if matches!(px, DecodingResult::U16(_)) {
        16
    } else {
        8
    };
    let (comp, lt_comp, extra): (Compression, u16, Vec<(u32, i64)>) = match which {
        "lzw" => (Compression::Lzw, 5, vec![]),
        _ => (Compression::Deflate(6), 8, vec![(65557, 6)]),
    };
    let spec = || {
        vec![WriteSpec {
            w: w as usize,
            h: h as usize,
            bits,
            spp: 3,
            photo: 2,
            comp: lt_comp,
            rps: 16,
            extra: &extra,
            data: &raw,
        }]
    };
    let ours = rust_encode(&px, w, h, comp);
    let theirs = libtiff_write("w", &spec());
    let (ho, ht) = (pixel_hash(&ours), pixel_hash(&theirs));
    assert_eq!(ho, ht, "encoded files decode to different pixels");
    assert_eq!(ho, fnv(&raw), "encode changed the pixels");
    println!(
        "parity: both outputs decode to the source ({} bytes, FNV {ho:016x}); sizes rusty_tiff {} B, libtiff {} B",
        raw.len(),
        ours.len(),
        theirs.len()
    );
    let label = format!("encode {which} {}", path.rsplit('/').next().unwrap_or(path));
    // Arms return the decoded-pixel parity class (constant), not the bytes:
    // the two encoders legitimately emit different streams.
    if null {
        let s = abba(
            pairs,
            || rust_encode(&px, w, h, comp).len() as u64 * 0,
            || rust_encode(&px, w, h, comp).len() as u64 * 0,
        );
        report(
            &format!("{label} NULL"),
            "rust",
            "rust'",
            raw.len(),
            (w * h) as usize,
            &s,
        );
    } else {
        let s = abba(
            pairs,
            || rust_encode(&px, w, h, comp).len() as u64 * 0,
            || libtiff_write("w", &spec()).len() as u64 * 0,
        );
        report(
            &label,
            "rusty_tiff",
            "libtiff",
            raw.len(),
            (w * h) as usize,
            &s,
        );
    }
}

fn main() {
    let a: Vec<String> = std::env::args().collect();
    match a.get(1).map(String::as_str) {
        Some("corpus") => corpus(&a[2]),
        Some("gen") => gen(&a[2], &a[3]),
        Some("dec") => dec(&a[2], a[3].parse().unwrap(), a.get(4).is_some_and(|x| x == "null")),
        Some("enc") => enc(&a[2], &a[3], a[4].parse().unwrap(), a.get(5).is_some_and(|x| x == "null")),
        _ => eprintln!("usage: tiffvs corpus <dir> | gen <src.ppm> <dir> | dec <f> <pairs> [null] | enc <f> <lzw|deflate> <pairs> [null]"),
    }
}
