//! Decode gates over the committed fixtures (see `fixtures/README.md`), and a
//! mutation sweep that must never panic.
//!
//! The expected hashes are FNV-1a 64 of the planar YCbCr bytes libheif 1.23.7
//! (with libde265) produces for each file with `nclx` passthrough; this crate
//! matches them exactly.

use std::panic::{catch_unwind, AssertUnwindSafe};
use std::sync::atomic::{AtomicUsize, Ordering};

use rff_format_heif::{Chroma, DecodeOptions, Heif};

fn fixture(name: &str) -> Vec<u8> {
    let p = std::path::Path::new(env!("CARGO_MANIFEST_DIR"))
        .join("tests/fixtures")
        .join(name);
    std::fs::read(&p).unwrap_or_else(|e| panic!("{}: {e}", p.display()))
}

fn fnv(b: &[u8]) -> u64 {
    b.iter().fold(0xcbf2_9ce4_8422_2325, |h, &x| {
        (h ^ x as u64).wrapping_mul(0x0000_0100_0000_01b3)
    })
}

/// (file, width, height, bit depth, libheif's planar hash)
const CASES: &[(&str, u32, u32, u8, u64)] = &[
    ("checker-1024.heic", 1024, 1024, 8, 0x8f84_08ea_284a_2325),
    ("gradient-512.heic", 512, 512, 8, 0xe659_f053_0adc_0825),
    (
        "full-range-no-colr.heic",
        128,
        128,
        10,
        0x8187_7a38_bf75_2ec5,
    ),
    ("with-exif.heic", 2048, 1536, 8, 0xd59f_eda7_046f_42cd),
];

#[test]
fn matches_libheif_exactly_single_and_multi_threaded() {
    for &(name, w, h, bits, want) in CASES {
        let data = fixture(name);
        for threads in [1, 0] {
            let opts = DecodeOptions {
                threads,
                ..DecodeOptions::default()
            };
            let heif = Heif::parse_with(&data, opts).unwrap();
            let info = heif.primary_info().unwrap();
            assert_eq!(
                (info.width, info.height, info.bit_depth),
                (w, h, bits),
                "{name}"
            );
            let d = heif.decode_primary().unwrap();
            assert_eq!((d.image.width, d.image.height), (w, h), "{name}");
            assert_eq!(d.image.chroma, Chroma::Yuv420, "{name}");
            assert_eq!(d.image.bit_depth, bits, "{name}");
            assert!(d.alpha.is_none(), "{name}");
            assert_eq!(
                fnv(&d.image.to_planar_bytes()),
                want,
                "{name}, threads {threads}"
            );
        }
    }
}

#[test]
fn exif_xmp_and_colour() {
    let data = fixture("with-exif.heic");
    let heif = Heif::parse(&data).unwrap();
    let tiff = heif.exif_tiff().expect("Exif item");
    assert!(tiff.starts_with(b"MM\0*") || tiff.starts_with(b"II*\0"));
    let raw = heif.exif().unwrap();
    assert_eq!(
        raw.len(),
        tiff.len() + 4 + u32::from_be_bytes(raw[..4].try_into().unwrap()) as usize
    );
    let xmp = heif.xmp().expect("XMP item");
    assert!(String::from_utf8_lossy(&xmp).contains("xmpmeta"));
    assert!(!heif.has_gain_map());

    // Full range said only in the HEVC VUI: no `colr`, so it comes from the
    // bitstream.
    let data = fixture("full-range-no-colr.heic");
    let d = Heif::parse(&data).unwrap().decode_primary().unwrap();
    let n = d.colour.nclx.expect("VUI colour");
    assert!(d.colour.nclx_from_bitstream);
    assert!(n.full_range);
    assert!(d.colour.icc.is_none());
    let rgba = d.to_rgba8();
    assert_eq!(rgba.len(), 128 * 128 * 4);
}

/// Alpha planes coded as monochrome (4:0:0) HEVC, as libheif and Apple
/// write them. `three-images.heic` has two colour items with an `auxl`
/// alpha each (an image and its thumbnail), both `clap`-cropped. The hashes
/// are FFmpeg 9.1's decode of the same items (`-pix_fmt yuv420p` / `gray`,
/// cropped), which this crate matches exactly.
#[test]
fn monochrome_alpha_matches_ffmpeg() {
    // (item, width, height, colour planes hash, alpha hash)
    const ITEMS: &[(u32, u32, u32, u64, u64)] = &[
        (5, 32, 80, 0x7228_674a_6e93_9925, 0x24ad_a752_2fd1_eb25),
        (7, 6, 16, 0x25a9_d68a_9e3a_f345, 0xf047_22a7_b619_4aa5),
    ];
    let data = fixture("three-images.heic");
    for threads in [1, 0] {
        let opts = DecodeOptions {
            threads,
            ..DecodeOptions::default()
        };
        let heif = Heif::parse_with(&data, opts).unwrap();
        for &(id, w, h, want_yuv, want_alpha) in ITEMS {
            let d = heif.decode_with_alpha(id).unwrap();
            assert_eq!((d.image.width, d.image.height), (w, h), "item {id}");
            assert_eq!(d.image.chroma, Chroma::Yuv420, "item {id}");
            assert_eq!(fnv(&d.image.to_planar_bytes()), want_yuv, "item {id}");
            let a = d.alpha.as_ref().expect("alpha plane");
            assert_eq!((a.plane.width, a.plane.height, a.bit_depth), (w, h, 8));
            assert!(!a.premultiplied);
            let bytes: Vec<u8> = a.plane.data.iter().map(|&v| v as u8).collect();
            assert_eq!(fnv(&bytes), want_alpha, "item {id} alpha");
            // Straight alpha: the RGBA alpha channel is the plane itself.
            let rgba = d.to_rgba8();
            assert!(rgba.chunks_exact(4).zip(&bytes).all(|(p, &a)| p[3] == a));
            // The alpha item decodes on its own, as a monochrome image.
            let alpha_id = id + 1;
            let ai = heif.decode_item(alpha_id).unwrap();
            assert_eq!(ai.chroma, Chroma::Monochrome, "item {alpha_id}");
        }
    }
}

/// HEVC range-extension chroma formats: a 4:2:2 10-bit grid (the layout
/// of Canon and Sony `.HIF` files) and `clap`-cropped single items in 4:4:4
/// 8-bit and 4:2:2 12-bit. The hashes are FFmpeg 9.1's decode of the same
/// files (`-pix_fmt yuv422p10le` / `yuv444p` / `yuv422p12le`, grid composed
/// and cropped), which this crate matches exactly; `to_rgba8` converts them.
#[test]
fn range_extension_chroma_matches_ffmpeg() {
    // (file, width, height, chroma, bit depth, planar hash)
    const CASES: &[(&str, u32, u32, Chroma, u8, u64)] = &[
        (
            "grid-422-10bit.heic",
            192,
            128,
            Chroma::Yuv422,
            10,
            0xa517_381c_aab2_071c,
        ),
        (
            "yuv444-8bit-clap.heic",
            58,
            44,
            Chroma::Yuv444,
            8,
            0x7820_d335_e5ef_bba3,
        ),
        (
            "yuv422-12bit-clap.heic",
            90,
            60,
            Chroma::Yuv422,
            12,
            0x61fc_8b9a_b899_14cf,
        ),
    ];
    for &(name, w, h, chroma, bits, want) in CASES {
        let data = fixture(name);
        for threads in [1, 0] {
            let opts = DecodeOptions {
                threads,
                ..DecodeOptions::default()
            };
            let heif = Heif::parse_with(&data, opts).unwrap();
            let d = heif.decode_primary().unwrap();
            assert_eq!((d.image.width, d.image.height), (w, h), "{name}");
            assert_eq!(
                (d.image.chroma, d.image.bit_depth),
                (chroma, bits),
                "{name}"
            );
            assert_eq!(
                fnv(&d.image.to_planar_bytes()),
                want,
                "{name}, threads {threads}"
            );
            assert_eq!(d.to_rgba8().len(), (w * h * 4) as usize, "{name}");
        }
    }
}

/// A grid whose tiles do not fill their cells -- here 512x512 tiles in
/// 520x520 cells, made by editing the tiles' `ispe` -- is composed on the
/// neutral canvas: luma 0 and chroma at the midpoint wherever no tile
/// writes, with one thread or several. (Exactly fitting tiles take the
/// zeroed-canvas path; this is the fallback.)
#[test]
fn undersized_grid_tiles_leave_the_neutral_canvas() {
    let mut data = fixture("with-exif.heic");
    let at = data
        .windows(20)
        .position(|b| &b[4..8] == b"ispe" && b[12..20] == [0, 0, 2, 0, 0, 0, 2, 0])
        .expect("the tiles' 512x512 ispe");
    // size, type, version/flags, then width and height: 520 x 520.
    data[at + 12..at + 20].copy_from_slice(&[0, 0, 2, 8, 0, 0, 2, 8]);
    let decode = |threads| {
        let opts = DecodeOptions {
            threads,
            ..DecodeOptions::default()
        };
        Heif::parse_with(&data, opts)
            .unwrap()
            .decode_primary()
            .unwrap()
            .image
    };
    let img = decode(1);
    assert_eq!(img, decode(0));
    assert_eq!((img.width, img.height, img.bit_depth), (2048, 1536, 8));
    // Columns 512..520 (chroma 256..260) of every row lie between the first
    // two tiles.
    for y in [0u32, 700, 1535] {
        assert!(img.planes[0].row(y)[512..520].iter().all(|&v| v == 0));
        for c in &img.planes[1..] {
            assert!(c.row(y / 2)[256..260].iter().all(|&v| v == 128));
        }
    }
}

/// Tiles larger than their cells overlap their neighbours, so the result
/// depends on the order the tiles are placed in. It must not depend on how
/// many threads decode them: the same file decodes to the same picture with
/// one thread or several, every time. (Made from with-exif.heic: 512x512
/// tiles in 504x504 cells, the grid's output cut to 2016x1512 so the cells
/// still cover it.)
#[test]
fn oversized_grid_tiles_compose_the_same_on_any_thread_count() {
    let mut data = fixture("with-exif.heic");
    let at = data
        .windows(20)
        .position(|b| &b[4..8] == b"ispe" && b[12..20] == [0, 0, 2, 0, 0, 0, 2, 0])
        .expect("the tiles' 512x512 ispe");
    data[at + 12..at + 20].copy_from_slice(&[0, 0, 1, 0xf8, 0, 0, 1, 0xf8]);
    // The grid descriptor: version, flags, rows - 1, columns - 1, output
    // width and height (16-bit): 2048x1536 -> 2016x1512.
    let g = data
        .windows(8)
        .position(|b| b == [0, 0, 2, 3, 8, 0, 6, 0])
        .expect("the grid descriptor");
    data[g + 4..g + 8].copy_from_slice(&[0x07, 0xe0, 0x05, 0xe8]);
    let decode = |threads| {
        let opts = DecodeOptions {
            threads,
            ..DecodeOptions::default()
        };
        Heif::parse_with(&data, opts)
            .unwrap()
            .decode_primary()
            .unwrap()
            .image
    };
    let one = decode(1);
    assert_eq!((one.width, one.height), (2016, 1512));
    for round in 0..6 {
        assert!(decode(0) == one, "multi-threaded decode {round} differs");
    }
}

#[test]
fn demuxer_hands_on_one_raw_frame() {
    use rff_format::FormatRegistry;
    let mut reg = FormatRegistry::new();
    rff_format_heif::register(&mut reg);
    let data = fixture("gradient-512.heic");
    assert_eq!(reg.probe(&data).map(|f| f.name), Some("heif"));
    let mut dem = reg
        .open_demuxer("heif", Box::new(std::io::Cursor::new(data)))
        .unwrap();
    let s = dem.read_header().unwrap();
    assert_eq!((s[0].width, s[0].height), (512, 512));
    assert_eq!(s[0].pixel_format, Some(rff_core::PixelFormat::Yuv420p));
    let p = dem.read_packet().unwrap();
    assert_eq!(fnv(&p.data), 0xe659_f053_0adc_0825);
    assert!(matches!(dem.read_packet(), Err(rff_core::Error::Eof)));
}

/// Deterministic xorshift.
struct Rng(u64);
impl Rng {
    fn next(&mut self) -> u64 {
        self.0 ^= self.0 << 13;
        self.0 ^= self.0 >> 7;
        self.0 ^= self.0 << 17;
        self.0
    }
    fn below(&mut self, n: usize) -> usize {
        (self.next() % n.max(1) as u64) as usize
    }
}

fn mutate(rng: &mut Rng, src: &[u8]) -> Vec<u8> {
    let mut d = src.to_vec();
    match rng.below(5) {
        0 => d.truncate(rng.below(d.len())),
        1 => {
            for _ in 0..1 + rng.below(8) {
                let i = rng.below(d.len());
                d[i] ^= 1 << rng.below(8);
            }
        }
        2 => {
            // A big value into a likely size/count field.
            let i = rng.below(d.len().saturating_sub(4));
            let v = [0xFFu8, 0xFF, 0xFF, 0xF0 | rng.below(16) as u8];
            d[i..i + 4].copy_from_slice(&v);
        }
        3 => {
            let i = rng.below(d.len());
            d[i] = rng.next() as u8;
            let j = rng.below(d.len());
            d[j] = 0;
        }
        _ => {
            let i = rng.below(d.len());
            let n = rng.below(64).min(d.len() - i);
            d.drain(i..i + n);
        }
    }
    d
}

/// Decode `data` every way the API offers. Any panic that reaches here is a
/// failure; the HEVC decoder's own contained panics are counted separately.
fn exercise(data: &[u8]) {
    let opts = DecodeOptions {
        threads: 1,
        ..DecodeOptions::default()
    };
    if let Ok(h) = Heif::parse_with(data, opts) {
        let _ = h.primary_info();
        let _ = h.decode_primary();
        let _ = h.exif_tiff();
        let _ = h.xmp();
        let _ = h.decode_depth();
        for t in h.thumbnails() {
            let _ = h.decode_item(t);
        }
    }
}

fn sweep(seeds: &[(&str, Vec<u8>)], rounds: usize) -> (usize, usize) {
    static CONTAINED: AtomicUsize = AtomicUsize::new(0);
    let prev = std::panic::take_hook();
    std::panic::set_hook(Box::new(|_| {
        CONTAINED.fetch_add(1, Ordering::Relaxed);
    }));
    let mut escaped = Vec::new();
    let mut rng = Rng(0x9E37_79B9_7F4A_7C15);
    for (name, src) in seeds {
        for round in 0..rounds {
            let m = mutate(&mut rng, src);
            if catch_unwind(AssertUnwindSafe(|| exercise(&m))).is_err() {
                escaped.push(format!("{name} round {round}"));
            }
        }
    }
    std::panic::set_hook(prev);
    assert!(escaped.is_empty(), "panics escaped the API: {escaped:?}");
    (seeds.len() * rounds, CONTAINED.load(Ordering::Relaxed))
}

#[test]
fn mutated_files_are_errors_not_panics() {
    let seeds: Vec<_> = [
        "checker-1024.heic",
        "gradient-512.heic",
        "full-range-no-colr.heic",
    ]
    .iter()
    .map(|n| (*n, fixture(n)))
    .collect();
    let (n, contained) = sweep(&seeds, 400);
    eprintln!("{n} mutated files, {contained} decoder panics contained");
}

/// Deep sweep over a directory of real files:
/// `HEIF_FUZZ_DIR=... HEIF_FUZZ_ROUNDS=2000 cargo test --release -- --ignored`.
#[test]
#[ignore]
fn deep_mutation_sweep() {
    let dir = std::env::var("HEIF_FUZZ_DIR").expect("HEIF_FUZZ_DIR");
    let rounds: usize = std::env::var("HEIF_FUZZ_ROUNDS")
        .ok()
        .and_then(|v| v.parse().ok())
        .unwrap_or(500);
    let seeds: Vec<_> = std::fs::read_dir(&dir)
        .unwrap()
        .filter_map(|e| e.ok())
        .filter(|e| e.path().is_file())
        .map(|e| {
            let p = e.path();
            (
                Box::leak(p.display().to_string().into_boxed_str()) as &str,
                std::fs::read(&p).unwrap(),
            )
        })
        .collect();
    let (n, contained) = sweep(&seeds, rounds);
    eprintln!("{n} mutated files, {contained} decoder panics contained");
}
