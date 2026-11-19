//! rusty_image against the C references: stb_image(+write), qoi.h,
//! ImageMagick 7 and gdk-pixbuf (and the `image` crate as the Rust peer).
//!
//!   imgbench corpus FILE...      decode with every engine, compare pixels
//!   imgbench decode N FILE...    timed decode, ABBA-rotated arms, N rounds
//!   imgbench encode N FILE...    timed encode of each file's pixels
#[global_allocator]
static GLOBAL_ALLOC: rusty_alloc_api::RustyAlloc = rusty_alloc_api::RustyAlloc;

use std::ffi::{c_char, c_int, c_long, c_uchar, c_ushort, c_void, CString};
use std::time::Instant;

use rusty_image::{Format, Image, PixelFormat};

extern "C" {
    fn c_stb_load(d: *const c_uchar, n: c_int, want: c_int, w: *mut c_int, h: *mut c_int, ch: *mut c_int, out: *mut *mut c_uchar) -> c_int;
    fn c_stb_load16(d: *const c_uchar, n: c_int, want: c_int, w: *mut c_int, h: *mut c_int, ch: *mut c_int, out: *mut *mut c_ushort) -> c_int;
    fn c_stb_loadf(d: *const c_uchar, n: c_int, w: *mut c_int, h: *mut c_int, ch: *mut c_int, out: *mut *mut f32) -> c_int;
    fn c_stb_free(p: *mut c_void);
    fn c_stb_write(kind: c_int, w: c_int, h: c_int, ch: c_int, px: *const c_void, out: *mut *mut c_uchar) -> c_long;
    fn c_free(p: *mut c_void);
    fn c_qoi_decode(d: *const c_uchar, n: c_int, w: *mut c_int, h: *mut c_int, ch: *mut c_int, out: *mut *mut c_uchar) -> c_int;
    fn c_qoi_encode(w: c_int, h: c_int, ch: c_int, px: *const c_uchar, out: *mut *mut c_uchar) -> c_long;
    fn c_magick_init();
    fn c_magick_decode(d: *const c_uchar, n: usize, hint: *const c_char, map: *const c_char, storage: c_int, w: *mut c_int, h: *mut c_int, out: *mut *mut c_void) -> c_int;
    fn c_magick_encode(fmt: *const c_char, w: c_int, h: c_int, map: *const c_char, storage: c_int, px: *const c_void, out: *mut *mut c_uchar) -> c_long;
    fn c_pixbuf_decode(d: *const c_uchar, n: usize, w: *mut c_int, h: *mut c_int, ch: *mut c_int, stride: *mut c_int, out: *mut *mut c_uchar) -> c_int;
}

/// Decoded pixels normalised for comparison.
#[derive(Debug, PartialEq)]
enum Px {
    Rgba8(u32, u32, Vec<u8>),
    Rgba16(u32, u32, Vec<u16>),
    Rgbf(u32, u32, Vec<f32>),
}

fn format_of(path: &str) -> Format {
    let e = path.rsplit('.').next().unwrap_or("").to_ascii_lowercase();
    match e.as_str() {
        "bmp" | "dib" => Format::Bmp,
        "tga" => Format::Tga,
        "hdr" => Format::Hdr,
        "ico" => Format::Ico,
        "qoi" => Format::Qoi,
        _ => Format::Pnm,
    }
}

fn hint_of(path: &str) -> CString {
    let e = path.rsplit('.').next().unwrap_or("").to_ascii_lowercase();
    CString::new(format!("{e}:x")).unwrap()
}

fn to_rgba8(img: &Image) -> Option<Vec<u8>> {
    let p = &img.pixels;
    Some(match img.format {
        PixelFormat::L8 => p.iter().flat_map(|&v| [v, v, v, 255]).collect(),
        PixelFormat::La8 => p.chunks_exact(2).flat_map(|c| [c[0], c[0], c[0], c[1]]).collect(),
        PixelFormat::Rgb8 => p.chunks_exact(3).flat_map(|c| [c[0], c[1], c[2], 255]).collect(),
        PixelFormat::Rgba8 => p.clone(),
        _ => return None,
    })
}

fn to_rgba16(img: &Image) -> Option<Vec<u16>> {
    let s = img.as_u16()?;
    Some(match img.format {
        PixelFormat::L16 => s.iter().flat_map(|&v| [v, v, v, 65535]).collect(),
        PixelFormat::La16 => s.chunks_exact(2).flat_map(|c| [c[0], c[0], c[0], c[1]]).collect(),
        PixelFormat::Rgb16 => s.chunks_exact(3).flat_map(|c| [c[0], c[1], c[2], 65535]).collect(),
        PixelFormat::Rgba16 => s,
        _ => return None,
    })
}

fn ours(d: &[u8], path: &str) -> Option<Px> {
    let img = rusty_image::decode_as(d, format_of(path)).ok()?;
    let (w, h) = (img.width, img.height);
    if img.format == PixelFormat::Rgb32F {
        return Some(Px::Rgbf(w, h, img.as_f32()?));
    }
    if img.format.bytes_per_sample() == 2 {
        return Some(Px::Rgba16(w, h, to_rgba16(&img)?));
    }
    Some(Px::Rgba8(w, h, to_rgba8(&img)?))
}

fn stb(d: &[u8], hdr: bool, sixteen: bool) -> Option<Px> {
    let (mut w, mut h, mut ch) = (0, 0, 0);
    unsafe {
        if hdr {
            let mut out = std::ptr::null_mut();
            if c_stb_loadf(d.as_ptr(), d.len() as c_int, &mut w, &mut h, &mut ch, &mut out) != 0 {
                return None;
            }
            let v = std::slice::from_raw_parts(out, (w * h * ch) as usize).to_vec();
            c_stb_free(out.cast());
            return Some(Px::Rgbf(w as u32, h as u32, v));
        }
        if sixteen {
            let mut out = std::ptr::null_mut();
            if c_stb_load16(d.as_ptr(), d.len() as c_int, 4, &mut w, &mut h, &mut ch, &mut out) != 0 {
                return None;
            }
            let v = std::slice::from_raw_parts(out, (w * h * 4) as usize).to_vec();
            c_stb_free(out.cast());
            return Some(Px::Rgba16(w as u32, h as u32, v));
        }
        let mut out = std::ptr::null_mut();
        if c_stb_load(d.as_ptr(), d.len() as c_int, 4, &mut w, &mut h, &mut ch, &mut out) != 0 {
            return None;
        }
        let v = std::slice::from_raw_parts(out, (w * h * 4) as usize).to_vec();
        c_stb_free(out.cast());
        Some(Px::Rgba8(w as u32, h as u32, v))
    }
}

fn magick(d: &[u8], path: &str, kind: &Px) -> Option<Px> {
    let hint = hint_of(path);
    let (map, storage) = match kind {
        Px::Rgbf(..) => ("RGB", 3),
        Px::Rgba16(..) => ("RGBA", 7),
        Px::Rgba8(..) => ("RGBA", 1),
    };
    let map = CString::new(map).unwrap();
    let (mut w, mut h) = (0, 0);
    let mut out = std::ptr::null_mut();
    unsafe {
        if c_magick_decode(d.as_ptr(), d.len(), hint.as_ptr(), map.as_ptr(), storage, &mut w, &mut h, &mut out) != 0 {
            return None;
        }
        let n = (w * h) as usize;
        let px = match kind {
            Px::Rgbf(..) => Px::Rgbf(w as u32, h as u32, std::slice::from_raw_parts(out as *const f32, n * 3).to_vec()),
            Px::Rgba16(..) => Px::Rgba16(w as u32, h as u32, std::slice::from_raw_parts(out as *const u16, n * 4).to_vec()),
            Px::Rgba8(..) => Px::Rgba8(w as u32, h as u32, std::slice::from_raw_parts(out as *const u8, n * 4).to_vec()),
        };
        c_free(out);
        Some(px)
    }
}

fn pixbuf(d: &[u8]) -> Option<Px> {
    let (mut w, mut h, mut ch, mut stride) = (0, 0, 0, 0);
    let mut out = std::ptr::null_mut();
    unsafe {
        if c_pixbuf_decode(d.as_ptr(), d.len(), &mut w, &mut h, &mut ch, &mut stride, &mut out) != 0 {
            return None;
        }
        let v = std::slice::from_raw_parts(out, (w * h * ch) as usize);
        let rgba = if ch == 4 { v.to_vec() } else { v.chunks_exact(3).flat_map(|c| [c[0], c[1], c[2], 255]).collect() };
        c_free(out.cast());
        Some(Px::Rgba8(w as u32, h as u32, rgba))
    }
}

/// Max absolute difference between two decodes (None: shapes differ).
fn diff(a: &Px, b: &Px) -> Option<f64> {
    match (a, b) {
        (Px::Rgba8(w1, h1, x), Px::Rgba8(w2, h2, y)) if (w1, h1) == (w2, h2) => {
            Some(x.iter().zip(y).map(|(p, q)| (i32::from(*p) - i32::from(*q)).abs()).max().unwrap_or(0) as f64)
        }
        (Px::Rgba16(w1, h1, x), Px::Rgba16(w2, h2, y)) if (w1, h1) == (w2, h2) => {
            Some(x.iter().zip(y).map(|(p, q)| (i32::from(*p) - i32::from(*q)).abs()).max().unwrap_or(0) as f64)
        }
        (Px::Rgbf(w1, h1, x), Px::Rgbf(w2, h2, y)) if (w1, h1) == (w2, h2) => Some(
            x.iter().zip(y).map(|(p, q)| ((p - q).abs() / p.abs().max(q.abs()).max(1e-30)) as f64).fold(0.0, f64::max),
        ),
        _ => None,
    }
}

fn corpus(files: &[String]) {
    println!("file\tours\tstb\tmagick\tpixbuf\t|ours-stb|\t|ours-magick|\t|ours-pixbuf|");
    let mut tally = std::collections::BTreeMap::<String, [usize; 3]>::new();
    for f in files {
        let d = std::fs::read(f).unwrap();
        let o = ours(&d, f);
        let kind = o.as_ref().map(|p| match p {
            Px::Rgbf(..) => 2,
            Px::Rgba16(..) => 1,
            Px::Rgba8(..) => 0,
        });
        let s = stb(&d, kind == Some(2), kind == Some(1));
        let m = o.as_ref().and_then(|k| magick(&d, f, k));
        let p = pixbuf(&d);
        let show = |x: &Option<Px>| if x.is_some() { "ok" } else { "-" };
        let dd = |x: &Option<Px>| match (&o, x) {
            (Some(a), Some(b)) => diff(a, b).map_or("shape".into(), |v| format!("{v}")),
            _ => "".into(),
        };
        println!("{f}\t{}\t{}\t{}\t{}\t{}\t{}\t{}", show(&o), show(&s), show(&m), show(&p), dd(&s), dd(&m), dd(&p));
        let fmt = format!("{:?}", format_of(f));
        let t = tally.entry(fmt).or_default();
        for (i, x) in [&s, &m, &p].iter().enumerate() {
            if let (Some(a), Some(b)) = (&o, x) {
                if diff(a, b) == Some(0.0) {
                    t[i] += 1;
                }
            }
        }
    }
    println!("\nidentical to ours (per format): stb / magick / pixbuf");
    for (k, v) in tally {
        println!("{k}\t{}\t{}\t{}", v[0], v[1], v[2]);
    }
}

type Arm<'a> = (&'static str, Box<dyn FnMut() -> usize + 'a>);

/// Time arms over `rounds` rounds, rotating the order each round (ABBA-style),
/// one decode per sample; report min, median and paired wins of arm 0.
fn race(label: &str, mpx: f64, rounds: usize, mut arms: Vec<Arm>) {
    let k = arms.len();
    let mut t = vec![Vec::new(); k];
    let mut work = vec![0usize; k];
    // warm-up
    for (i, a) in arms.iter_mut().enumerate() {
        work[i] = (a.1)();
    }
    for r in 0..rounds {
        for j in 0..k {
            let i = if r % 2 == 0 { j } else { k - 1 - j };
            let s = Instant::now();
            let w = (arms[i].1)();
            t[i].push(s.elapsed().as_secs_f64() * 1e3);
            assert_eq!(w, work[i], "work changed");
        }
    }
    for (i, (name, _)) in arms.iter().enumerate() {
        let mut v = t[i].clone();
        v.sort_by(f64::total_cmp);
        let (min, med) = (v[0], v[v.len() / 2]);
        let wins = (0..rounds).filter(|&r| t[0][r] < t[i][r]).count();
        let z = (wins as f64 - rounds as f64 / 2.0) / (0.5 * (rounds as f64).sqrt());
        let vs = if i == 0 { String::new() } else { format!("\tours wins {wins}/{rounds} z={z:+.2}\tours {:.2}x", min / t[0].iter().cloned().fold(f64::MAX, f64::min)) };
        println!("{label}\t{name}\tmin {min:.3} ms\tmed {med:.3} ms\t{:.1} Mpx/s{vs}", mpx / (min / 1e3));
    }
}

fn decode_bench(rounds: usize, files: &[String]) {
    unsafe { c_magick_init() };
    for f in files {
        let d = std::fs::read(f).unwrap();
        let fmt = format_of(f);
        let probe = rusty_image::decode_as(&d, fmt).expect("ours decodes");
        let mpx = f64::from(probe.width) * f64::from(probe.height) / 1e6;
        let hdr = fmt == Format::Hdr;
        let hint = hint_of(f);
        let label = f.rsplit('/').next().unwrap().to_string();
        let mut arms: Vec<Arm> = vec![
            ("rusty_image", Box::new(|| rusty_image::decode_as(&d, fmt).unwrap().pixels.len())),
            ("rusty_image (null arm)", Box::new(|| rusty_image::decode_as(&d, fmt).unwrap().pixels.len())),
        ];
        if fmt != Format::Ico {
            arms.push(("stb_image", Box::new(|| unsafe {
                let (mut w, mut h, mut ch) = (0, 0, 0);
                if hdr {
                    let mut out = std::ptr::null_mut();
                    assert_eq!(c_stb_loadf(d.as_ptr(), d.len() as c_int, &mut w, &mut h, &mut ch, &mut out), 0);
                    c_stb_free(out.cast());
                } else {
                    let mut out = std::ptr::null_mut();
                    if c_stb_load(d.as_ptr(), d.len() as c_int, 0, &mut w, &mut h, &mut ch, &mut out) != 0 {
                        return 0;
                    }
                    c_stb_free(out.cast());
                }
                (w * h * ch) as usize
            })));
        }
        if fmt == Format::Qoi {
            arms.push(("qoi.h (reference)", Box::new(|| unsafe {
                let (mut w, mut h, mut ch) = (0, 0, 0);
                let mut out = std::ptr::null_mut();
                assert_eq!(c_qoi_decode(d.as_ptr(), d.len() as c_int, &mut w, &mut h, &mut ch, &mut out), 0);
                c_free(out.cast());
                (w * h * ch) as usize
            })));
        }
        arms.push(("ImageMagick 7", Box::new(|| unsafe {
            let (mut w, mut h) = (0, 0);
            let mut out = std::ptr::null_mut();
            if c_magick_decode(d.as_ptr(), d.len(), hint.as_ptr(), std::ptr::null(), 0, &mut w, &mut h, &mut out) != 0 {
                return 0;
            }
            (w * h) as usize
        })));
        if !hdr && fmt != Format::Qoi {
            arms.push(("gdk-pixbuf", Box::new(|| unsafe {
                let (mut w, mut h, mut ch, mut st) = (0, 0, 0, 0);
                let mut out = std::ptr::null_mut();
                if c_pixbuf_decode(d.as_ptr(), d.len(), &mut w, &mut h, &mut ch, &mut st, &mut out) != 0 {
                    return 0;
                }
                c_free(out.cast());
                (w * h * ch) as usize
            })));
        }
        let ifmt = match fmt {
            Format::Bmp => Some(image::ImageFormat::Bmp),
            Format::Tga => Some(image::ImageFormat::Tga),
            Format::Hdr => Some(image::ImageFormat::Hdr),
            Format::Qoi => Some(image::ImageFormat::Qoi),
            Format::Pnm => Some(image::ImageFormat::Pnm),
            Format::Ico => Some(image::ImageFormat::Ico),
            _ => None,
        };
        if let Some(ifmt) = ifmt {
            let dref = &d;
            arms.push(("image (Rust)", Box::new(move || image::load_from_memory_with_format(dref, ifmt).map(|i| i.as_bytes().len()).unwrap_or(0))));
        }
        race(&label, mpx, rounds, arms);
        println!();
    }
}

fn encode_bench(rounds: usize, files: &[String]) {
    unsafe { c_magick_init() };
    for f in files {
        let d = std::fs::read(f).unwrap();
        let fmt = format_of(f);
        let img = rusty_image::decode_as(&d, fmt).unwrap();
        let (w, h) = (img.width as c_int, img.height as c_int);
        let ch = img.format.channels() as c_int;
        let mpx = f64::from(img.width) * f64::from(img.height) / 1e6;
        let label = format!("{} (encode {:?} {:?})", f.rsplit('/').next().unwrap(), fmt, img.format);
        let px = img.pixels.clone();
        let mut arms: Vec<Arm> = vec![
            ("rusty_image", Box::new(|| rusty_image::encode(&img, fmt).unwrap().len())),
            ("rusty_image (null arm)", Box::new(|| rusty_image::encode(&img, fmt).unwrap().len())),
        ];
        let stb_kind = match fmt {
            Format::Bmp => Some(0),
            Format::Tga => Some(1),
            Format::Hdr => Some(2),
            _ => None,
        };
        if let Some(kind) = stb_kind {
            let px = px.clone();
            arms.push(("stb_image_write", Box::new(move || unsafe {
                let mut out = std::ptr::null_mut();
                let n = c_stb_write(kind, w, h, ch, px.as_ptr().cast(), &mut out);
                c_free(out.cast());
                n as usize
            })));
        }
        if fmt == Format::Qoi {
            let px = px.clone();
            arms.push(("qoi.h (reference)", Box::new(move || unsafe {
                let mut out = std::ptr::null_mut();
                let n = c_qoi_encode(w, h, ch, px.as_ptr(), &mut out);
                c_free(out.cast());
                n as usize
            })));
        }
        let (map, storage) = match img.format {
            PixelFormat::Rgb32F => ("RGB", 3),
            PixelFormat::Rgba8 => ("RGBA", 1),
            PixelFormat::L8 => ("I", 1),
            _ => ("RGB", 1),
        };
        let mfmt = CString::new(match fmt {
            Format::Bmp => "BMP",
            Format::Tga => "TGA",
            Format::Hdr => "HDR",
            Format::Qoi => "QOI",
            _ => "PPM",
        })
        .unwrap();
        let map = CString::new(map).unwrap();
        arms.push(("ImageMagick 7", Box::new(move || unsafe {
            let mut out = std::ptr::null_mut();
            let n = c_magick_encode(mfmt.as_ptr(), w, h, map.as_ptr(), storage, px.as_ptr().cast(), &mut out);
            c_free(out.cast());
            n as usize
        })));
        race(&label, mpx, rounds, arms);
        println!();
    }
}

fn main() {
    let a: Vec<String> = std::env::args().collect();
    match a[1].as_str() {
        "corpus" => corpus(&a[2..]),
        "decode" => decode_bench(a[2].parse().unwrap(), &a[3..]),
        "encode" => encode_bench(a[2].parse().unwrap(), &a[3..]),
        _ => panic!("mode"),
    }
}
