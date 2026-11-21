//! A damaged size field must not buy an allocation the file cannot fill.
//!
//! Each case is a tiny file whose header names hundreds of megabytes of
//! pixels (all under the 1 GiB `MAX_DECODED_BYTES` cap, so the cap is not
//! what stops them). Before the input budget these decoded as far as
//! "truncated image data" only after allocating the whole image (ArtCraft's
//! damaged corpus: a 26-byte QOI allocated 402,653,187 bytes, a 112-byte DIB
//! 805,306,371). They must now be refused, with the same error, before the
//! buffer exists. The boundary cases check the bounds are exact: a file
//! holding just enough bytes at the format's largest expansion still decodes.

use std::alloc::{GlobalAlloc, Layout, System};
use std::cell::Cell;

use rusty_image::{Error, Format, Image, PixelFormat};

/// Live and peak heap of the current thread (tests run on several threads).
struct PerThread;

thread_local! {
    static CUR: Cell<usize> = const { Cell::new(0) };
    static PEAK: Cell<usize> = const { Cell::new(0) };
}

fn grow(n: usize) {
    let _ = CUR.try_with(|c| {
        let v = c.get() + n;
        c.set(v);
        let _ = PEAK.try_with(|p| p.set(p.get().max(v)));
    });
}

fn shrink(n: usize) {
    let _ = CUR.try_with(|c| c.set(c.get().saturating_sub(n)));
}

unsafe impl GlobalAlloc for PerThread {
    unsafe fn alloc(&self, l: Layout) -> *mut u8 {
        grow(l.size());
        System.alloc(l)
    }
    unsafe fn alloc_zeroed(&self, l: Layout) -> *mut u8 {
        grow(l.size());
        System.alloc_zeroed(l)
    }
    unsafe fn dealloc(&self, p: *mut u8, l: Layout) {
        shrink(l.size());
        System.dealloc(p, l);
    }
    unsafe fn realloc(&self, p: *mut u8, l: Layout, n: usize) -> *mut u8 {
        if n > l.size() {
            grow(n - l.size());
        } else {
            shrink(l.size() - n);
        }
        System.realloc(p, l, n)
    }
}

#[global_allocator]
static ALLOC: PerThread = PerThread;

/// `decode` under a peak-heap probe: the result and the bytes it peaked at.
fn peak_of<T>(f: impl FnOnce() -> T) -> (T, usize) {
    let base = CUR.with(Cell::get);
    PEAK.with(|p| p.set(base));
    let out = f();
    (out, PEAK.with(Cell::get) - base)
}

/// Refused as truncated, having allocated less than 1 MiB.
fn refused_cheaply(name: &str, r: impl FnOnce() -> rusty_image::Result<Image>) {
    let (res, peak) = peak_of(r);
    assert_eq!(res.err(), Some(Error::Truncated), "{name}");
    assert!(peak < 1 << 20, "{name}: peaked at {peak} bytes");
}

fn rgb1x1() -> Image {
    Image::new(1, 1, PixelFormat::Rgb8, vec![10, 20, 30]).unwrap()
}

#[test]
fn bmp_and_dib_with_huge_dimensions() {
    let mut bmp = rusty_image::encode(&rgb1x1(), Format::Bmp).unwrap();
    bmp[18..22].copy_from_slice(&60_000i32.to_le_bytes());
    bmp[22..26].copy_from_slice(&4_000i32.to_le_bytes());
    refused_cheaply("bmp 60000x4000", || rusty_image::decode(&bmp));
    // Top-down, like most of the reported files.
    bmp[22..26].copy_from_slice(&(-4_000i32).to_le_bytes());
    refused_cheaply("bmp top-down", || rusty_image::decode(&bmp));

    let mut dib = rusty_image::bmp::encode_dib(&rgb1x1(), rusty_image::bmp::DibKind::Dib).unwrap();
    dib[4..8].copy_from_slice(&60_000i32.to_le_bytes());
    dib[8..12].copy_from_slice(&4_000i32.to_le_bytes());
    refused_cheaply("dib 60000x4000", || rusty_image::bmp::decode_dib(&dib));
}

#[test]
fn tga_raw_and_rle_with_huge_dimensions() {
    // Encoded TGA is RLE.
    let mut rle = rusty_image::encode(&rgb1x1(), Format::Tga).unwrap();
    rle[12..14].copy_from_slice(&65_535u16.to_le_bytes());
    rle[14..16].copy_from_slice(&4_000u16.to_le_bytes());
    refused_cheaply("tga rle", || rusty_image::decode_as(&rle, Format::Tga));

    let mut raw = vec![0u8; 18];
    raw[2] = 2; // uncompressed true-colour
    raw[12..14].copy_from_slice(&65_535u16.to_le_bytes());
    raw[14..16].copy_from_slice(&4_000u16.to_le_bytes());
    raw[16] = 24;
    raw[17] = 0x20;
    raw.extend_from_slice(&[1, 2, 3]);
    refused_cheaply("tga raw", || rusty_image::decode_as(&raw, Format::Tga));
}

#[test]
fn qoi_with_huge_dimensions() {
    let mut qoi = rusty_image::encode(&rgb1x1(), Format::Qoi).unwrap();
    qoi[4..8].copy_from_slice(&16_384u32.to_be_bytes());
    qoi[8..12].copy_from_slice(&16_384u32.to_be_bytes());
    refused_cheaply("qoi 16384^2", || rusty_image::decode(&qoi));
}

#[test]
fn pnm_binary_and_ascii_with_huge_dimensions() {
    for (name, head) in [
        ("p6", "P6\n16000 16000\n255\n"),
        ("p5", "P5\n30000 30000\n255\n"),
        ("p4", "P4\n30000 30000\n"),
        ("p3", "P3\n16000 16000\n255\n"),
        ("p2", "P2\n30000 30000\n255\n"),
        ("p1", "P1\n30000 30000\n"),
        (
            "p7",
            "P7\nWIDTH 16000\nHEIGHT 16000\nDEPTH 3\nMAXVAL 255\nTUPLTYPE RGB\nENDHDR\n",
        ),
    ] {
        let mut f = head.as_bytes().to_vec();
        f.extend_from_slice(b"1 0 1 0\n");
        refused_cheaply(name, || rusty_image::decode(&f));
    }
}

#[test]
fn hdr_with_huge_dimensions() {
    let mut f = b"#?RADIANCE\nFORMAT=32-bit_rle_rgbe\n\n-Y 20000 +X 4000\n".to_vec();
    f.extend_from_slice(&[128, 128, 128, 128, 1, 1, 1, 255]);
    refused_cheaply("hdr 4000x20000", || rusty_image::decode(&f));
}

#[test]
fn the_bounds_are_exact() {
    // QOI: one QOI_OP_RUN byte covers 62 pixels; 63 need a second op.
    let qoi = |w: u32| {
        let mut f = b"qoif".to_vec();
        f.extend_from_slice(&w.to_be_bytes());
        f.extend_from_slice(&1u32.to_be_bytes());
        f.extend_from_slice(&[3, 0, 0xc0 | 61]);
        f
    };
    let img = rusty_image::decode(&qoi(62)).unwrap();
    assert_eq!((img.width, img.pixels.len()), (62, 62 * 3));
    assert_eq!(rusty_image::decode(&qoi(63)).err(), Some(Error::Truncated));

    // TGA RLE: one run packet (1 + 3 bytes) covers 128 pixels.
    let tga = |w: u16| {
        let mut f = vec![0u8; 18];
        f[2] = 10;
        f[12..14].copy_from_slice(&w.to_le_bytes());
        f[14..16].copy_from_slice(&1u16.to_le_bytes());
        f[16] = 24;
        f[17] = 0x20;
        f.extend_from_slice(&[0xff, 3, 2, 1]);
        f
    };
    let img = rusty_image::decode_as(&tga(128), Format::Tga).unwrap();
    assert_eq!(&img.pixels[381..], &[1, 2, 3]);
    assert_eq!(
        rusty_image::decode_as(&tga(129), Format::Tga).err(),
        Some(Error::Truncated)
    );

    // ASCII PNM: one digit and a separator per sample, the last at the end.
    let img = rusty_image::decode(b"P2\n3 1\n255\n0 7 9").unwrap();
    assert_eq!(img.pixels, [0, 7, 9]);
    let img = rusty_image::decode(b"P1\n4 1\n0110").unwrap();
    assert_eq!(img.pixels, [255, 0, 0, 255]);
}
