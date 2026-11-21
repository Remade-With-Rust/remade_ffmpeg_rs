//! Uncompressed tiles, the two fast paths. Rows long enough are read straight
//! into the output (one vectored read per tile, no band); the oracle matrix
//! uses 16- and 32-pixel tiles, which stay on the band path, so this file
//! covers the direct path: against image-tiff in both byte orders, through
//! readers that do and do not support vectored reads, and on short
//! (truncated) tiles, whose missing bytes must read as zero exactly as on
//! the band path. Narrow tiles stored back to back are read a band at a
//! time: covered against image-tiff, with tiles stored out of order, and
//! with a short tile in a band.
#![allow(clippy::pedantic)] // test harness code: oracle plumbing, not library API

mod common;

use std::io::{BufReader, Cursor, IoSliceMut, Read, Seek, SeekFrom};

use common::Rng;
use rusty_tiff::encoder::{colortype, ColorType, Compression, Predictor, TiffEncoder};
use rusty_tiff::{Decoder, Limits};

/// An uncompressed tiled image of random samples.
fn make<C: ColorType<Inner = u16>>(w: u32, h: u32, tw: u32, th: u32, seed: u64) -> Vec<u8> {
    let mut r = Rng(seed);
    let px: Vec<u16> = (0..w as usize * h as usize * usize::from(C::SAMPLES))
        .map(|_| r.next() as u16)
        .collect();
    let mut f = Cursor::new(Vec::new());
    {
        let mut e = TiffEncoder::new(&mut f).unwrap();
        let mut i = e
            .new_image::<C>(w, h)
            .unwrap()
            .with_compression(Compression::None);
        i.tiles(tw, th);
        i.write_data(&px).unwrap();
    }
    f.into_inner()
}

fn decode<R: Read + Seek>(r: R) -> Vec<u8> {
    Decoder::new(r)
        .unwrap()
        .with_limits(Limits::unlimited())
        .read_image()
        .unwrap()
        .into_ne_bytes()
}

/// (width, height, tile width, tile height): exact fits, right and bottom
/// padding, one tile wider and taller than the image, a single tile column.
const SHAPES: [(u32, u32, u32, u32); 6] = [
    (512, 256, 256, 64),
    (1000, 700, 272, 160),
    (300, 100, 512, 128),
    (257, 33, 256, 16),
    (256, 300, 256, 32),
    (777, 95, 400, 48),
];

fn files() -> Vec<(String, Vec<u8>)> {
    let mut v = Vec::new();
    for (k, &(w, h, tw, th)) in SHAPES.iter().enumerate() {
        let s = k as u64 * 7919 + 1;
        v.push((
            format!("gray16 {w}x{h} t{tw}x{th}"),
            make::<colortype::Gray16>(w, h, tw, th, s),
        ));
        v.push((
            format!("graya16 {w}x{h} t{tw}x{th}"),
            make::<colortype::GrayA16>(w, h, tw, th, s + 1),
        ));
        v.push((
            format!("rgb16 {w}x{h} t{tw}x{th}"),
            make::<colortype::RGB16>(w, h, tw, th, s + 2),
        ));
        v.push((
            format!("rgba16 {w}x{h} t{tw}x{th}"),
            make::<colortype::RGBA16>(w, h, tw, th, s + 3),
        ));
        v.push((
            format!("cmyk16 {w}x{h} t{tw}x{th}"),
            make::<colortype::CMYK16>(w, h, tw, th, s + 4),
        ));
    }
    v
}

#[test]
fn wide_tiles_match_image_tiff_in_both_byte_orders() {
    let mut compared = 0;
    for (name, le) in files() {
        let be = common::to_big_endian(&le).expect("uncompressed");
        for (order, f) in [("LE", &le), ("BE", &be)] {
            let ours = common::ours(f).unwrap_or_else(|e| panic!("{name} {order}: {e}"));
            let theirs = common::theirs(f).unwrap_or_else(|e| panic!("{name} {order}: {e}"));
            assert_eq!(ours, theirs, "{name} {order}");
            compared += 1;
        }
    }
    assert_eq!(compared, SHAPES.len() * 5 * 2);
}

/// A reader with only `read`: the default `read_vectored` fills one buffer
/// per call, as a Windows `File` does.
struct OneBuffer<R>(R);
impl<R: Read> Read for OneBuffer<R> {
    fn read(&mut self, b: &mut [u8]) -> std::io::Result<usize> {
        self.0.read(b)
    }
}
impl<R: Seek> Seek for OneBuffer<R> {
    fn seek(&mut self, p: SeekFrom) -> std::io::Result<u64> {
        self.0.seek(p)
    }
}

/// A reader that hands out at most 37 bytes a call, across buffer
/// boundaries when reading vectored.
struct Trickle<R>(R);
impl<R: Read> Read for Trickle<R> {
    fn read(&mut self, b: &mut [u8]) -> std::io::Result<usize> {
        let n = b.len().min(37);
        self.0.read(&mut b[..n])
    }
    fn read_vectored(&mut self, bufs: &mut [IoSliceMut<'_>]) -> std::io::Result<usize> {
        let mut left = 37;
        let mut done = 0;
        for b in bufs {
            let n = b.len().min(left);
            let k = self.0.read(&mut b[..n])?;
            done += k;
            left -= k;
            if k < b.len() || left == 0 {
                break;
            }
        }
        Ok(done)
    }
}
impl<R: Seek> Seek for Trickle<R> {
    fn seek(&mut self, p: SeekFrom) -> std::io::Result<u64> {
        self.0.seek(p)
    }
}

#[test]
fn every_kind_of_reader_gives_the_same_pixels() {
    let dir = std::env::temp_dir().join(format!("rusty_tiff_wide_{}", std::process::id()));
    std::fs::create_dir_all(&dir).unwrap();
    for (k, (name, le)) in files().into_iter().enumerate() {
        let want = decode(Cursor::new(&le));
        assert_eq!(
            want,
            decode(OneBuffer(Cursor::new(&le))),
            "{name} one buffer"
        );
        assert_eq!(want, decode(Trickle(Cursor::new(&le))), "{name} trickle");
        if k % 5 == 2 {
            let p = dir.join("t.tif");
            std::fs::write(&p, &le).unwrap();
            assert_eq!(
                want,
                decode(std::fs::File::open(&p).unwrap()),
                "{name} File"
            );
            let b = BufReader::new(std::fs::File::open(&p).unwrap());
            assert_eq!(want, decode(b), "{name} BufReader");
        }
    }
    let _ = std::fs::remove_dir_all(&dir);
}

fn r32(b: &[u8], o: usize) -> u64 {
    u64::from(u32::from_le_bytes(b[o..o + 4].try_into().unwrap()))
}

/// (offset, byte count) of every tile, from the first IFD.
fn tiles_of(f: &[u8]) -> Vec<(usize, usize)> {
    let ifd = r32(f, 4) as usize;
    let n = u16::from_le_bytes([f[ifd], f[ifd + 1]]) as usize;
    let field = |tag: u16| {
        (0..n)
            .map(|k| ifd + 2 + k * 12)
            .find(|&e| u16::from_le_bytes([f[e], f[e + 1]]) == tag)
            .unwrap()
    };
    let list = |e: usize| -> Vec<u64> {
        let count = r32(f, e + 4) as usize;
        let at = if count == 1 {
            e + 8
        } else {
            r32(f, e + 8) as usize
        };
        (0..count).map(|i| r32(f, at + 4 * i)).collect()
    };
    let (o, c) = (field(324), field(325));
    list(o)
        .into_iter()
        .zip(list(c))
        .map(|(a, b)| (a as usize, b as usize))
        .collect()
}

/// A tile whose byte count is cut short keeps the bytes it has; the rest
/// reads as zero. The oracle is the intact file with those same bytes
/// zeroed in place.
#[test]
fn short_tiles_read_as_zero_past_their_bytes() {
    for (name, le) in files().into_iter().step_by(3) {
        let tiles = tiles_of(&le);
        let ifd = r32(&le, 4) as usize;
        let n = u16::from_le_bytes([le[ifd], le[ifd + 1]]) as usize;
        let counts_at = (0..n)
            .map(|k| ifd + 2 + k * 12)
            .find(|&e| u16::from_le_bytes([le[e], le[e + 1]]) == 325)
            .unwrap();
        let list_at = if tiles.len() == 1 {
            counts_at + 8
        } else {
            r32(&le, counts_at + 8) as usize
        };
        for (t, cut) in [
            (0usize, 1usize),
            (tiles.len() / 2, 777),
            (tiles.len() - 1, 4096),
        ] {
            let (off, count) = tiles[t];
            let keep = count.saturating_sub(cut);
            let mut short = le.clone();
            short[list_at + 4 * t..list_at + 4 * t + 4]
                .copy_from_slice(&(keep as u32).to_le_bytes());
            let mut oracle = le.clone();
            oracle[off + keep..off + count].fill(0);
            assert_eq!(
                decode(Cursor::new(&short)),
                decode(Cursor::new(&oracle)),
                "{name} tile {t} cut {cut}"
            );
            assert_eq!(
                decode(OneBuffer(Cursor::new(&short))),
                decode(Cursor::new(&oracle)),
                "{name} tile {t} cut {cut} one buffer"
            );
        }
    }
}

/// Narrow tiles (band path, read a band at a time when stored back to back):
/// 16-bit and 8-bit, exact and padded, with and without the horizontal
/// predictor on uncompressed data.
fn narrow_files() -> Vec<(String, Vec<u8>)> {
    fn one<C: ColorType>(
        name: &str,
        w: u32,
        h: u32,
        tw: u32,
        th: u32,
        pred: bool,
        seed: u64,
    ) -> (String, Vec<u8>)
    where
        C::Inner: From<u8>,
    {
        let mut r = Rng(seed);
        let px: Vec<C::Inner> = (0..w as usize * h as usize * usize::from(C::SAMPLES))
            .map(|_| C::Inner::from(r.next() as u8))
            .collect();
        let mut f = Cursor::new(Vec::new());
        {
            let mut e = TiffEncoder::new(&mut f).unwrap();
            let mut i = e
                .new_image::<C>(w, h)
                .unwrap()
                .with_compression(Compression::None);
            if pred {
                i = i.with_predictor(Predictor::Horizontal);
            }
            i.tiles(tw, th);
            i.write_data(&px).unwrap();
        }
        (
            format!("{name} {w}x{h} t{tw}x{th} pred {pred}"),
            f.into_inner(),
        )
    }
    vec![
        one::<colortype::Gray16>("gray16", 200, 160, 48, 80, false, 1),
        one::<colortype::Gray16>("gray16", 190, 150, 48, 80, false, 2),
        one::<colortype::RGB16>("rgb16", 128, 96, 32, 32, false, 3),
        one::<colortype::RGB16>("rgb16", 100, 70, 32, 32, true, 4),
        one::<colortype::RGBA16>("rgba16", 64, 48, 16, 16, false, 5),
        one::<colortype::RGB8>("rgb8", 100, 64, 16, 16, false, 6),
        one::<colortype::Gray8>("gray8", 99, 33, 32, 16, true, 7),
    ]
}

#[test]
fn narrow_tile_bands_match_image_tiff_in_both_byte_orders() {
    for (name, le) in narrow_files() {
        let be = common::to_big_endian(&le).expect("uncompressed");
        for (order, f) in [("LE", &le), ("BE", &be)] {
            let ours = common::ours(f).unwrap_or_else(|e| panic!("{name} {order}: {e}"));
            let theirs = common::theirs(f).unwrap_or_else(|e| panic!("{name} {order}: {e}"));
            assert_eq!(ours, theirs, "{name} {order}");
        }
    }
}

/// The byte count list's position in the first IFD.
fn list_of(f: &[u8], tag: u16, n_tiles: usize) -> usize {
    let ifd = r32(f, 4) as usize;
    let n = u16::from_le_bytes([f[ifd], f[ifd + 1]]) as usize;
    let e = (0..n)
        .map(|k| ifd + 2 + k * 12)
        .find(|&e| u16::from_le_bytes([f[e], f[e + 1]]) == tag)
        .unwrap();
    if n_tiles == 1 {
        e + 8
    } else {
        r32(f, e + 8) as usize
    }
}

/// Tiles stored out of order (the first two of each band swapped in the
/// file, offsets swapped to match) decode to the same pixels; a short tile
/// in a band reads as zero past its bytes, as anywhere else.
#[test]
fn narrow_tile_bands_out_of_order_or_short() {
    for (name, le) in narrow_files() {
        let tiles = tiles_of(&le);
        let want = decode(Cursor::new(&le));
        let offs = list_of(&le, 324, tiles.len());
        let cnts = list_of(&le, 325, tiles.len());
        // Swap tiles 0 and 1 in storage (equal sizes: uncompressed).
        let (o0, c0) = tiles[0];
        let (o1, c1) = tiles[1];
        assert_eq!(c0, c1, "{name}");
        let mut swapped = le.clone();
        swapped[o0..o0 + c0].copy_from_slice(&le[o1..o1 + c1]);
        swapped[o1..o1 + c1].copy_from_slice(&le[o0..o0 + c0]);
        swapped[offs..offs + 4].copy_from_slice(&(o1 as u32).to_le_bytes());
        swapped[offs + 4..offs + 8].copy_from_slice(&(o0 as u32).to_le_bytes());
        assert_eq!(decode(Cursor::new(&swapped)), want, "{name} swapped");
        // Tile 1 cut short by 100 bytes.
        let mut short = le.clone();
        short[cnts + 4..cnts + 8].copy_from_slice(&((c1 - 100) as u32).to_le_bytes());
        let mut oracle = le.clone();
        oracle[o1 + c1 - 100..o1 + c1].fill(0);
        assert_eq!(
            decode(Cursor::new(&short)),
            decode(Cursor::new(&oracle)),
            "{name} short"
        );
    }
}
