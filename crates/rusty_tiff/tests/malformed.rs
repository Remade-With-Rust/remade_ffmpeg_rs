//! Malformed input: every case must return `Err` (or a valid image), never
//! panic, never spin, never allocate past the limits.
#![allow(clippy::pedantic)] // test harness code: oracle plumbing, not library API

mod common;

use std::io::Cursor;
use std::time::{Duration, Instant};

use common::Rng;
use rusty_tiff::encoder::{colortype, Compression, Predictor, TiffEncoder};
use rusty_tiff::{ccitt, Decoder, Limits, TiffError};

fn r16(b: &[u8], o: usize) -> usize {
    usize::from(u16::from_le_bytes([b[o], b[o + 1]]))
}
fn r32(b: &[u8], o: usize) -> usize {
    u32::from_le_bytes([b[o], b[o + 1], b[o + 2], b[o + 3]]) as usize
}
fn w32(b: &mut [u8], o: usize, v: u32) {
    b[o..o + 4].copy_from_slice(&v.to_le_bytes());
}

/// Offsets of each IFD in a little-endian classic file.
fn ifds(b: &[u8]) -> Vec<usize> {
    let mut v = Vec::new();
    let mut o = r32(b, 4);
    while o != 0 && v.len() < 100 {
        v.push(o);
        o = r32(b, o + 2 + r16(b, o) * 12);
    }
    v
}

/// Position of the 4-byte value/offset field of `tag` in the IFD at `ifd`.
fn field_pos(b: &[u8], ifd: usize, tag: u16) -> usize {
    let n = r16(b, ifd);
    (0..n)
        .map(|k| ifd + 2 + k * 12)
        .find(|&e| r16(b, e) == usize::from(tag))
        .map(|e| e + 8)
        .expect("tag present")
}

fn two_page() -> Vec<u8> {
    let mut f = Cursor::new(Vec::new());
    let mut e = TiffEncoder::new(&mut f).unwrap();
    e.write_image::<colortype::Gray8>(8, 8, &[7u8; 64]).unwrap();
    e.write_image::<colortype::Gray8>(4, 4, &[9u8; 16]).unwrap();
    f.into_inner()
}

/// Decode everything reachable: all pages, all tags, EXIF/SubIFDs.
fn decode_all(file: &[u8], limits: Limits) -> Result<usize, TiffError> {
    let mut d = Decoder::new(Cursor::new(file))?.with_limits(limits);
    let mut pages = 0;
    for _ in 0..64 {
        let _ = d.tags();
        let _ = d.icc_profile();
        let _ = d.exif_ifd();
        if let Ok(subs) = d.sub_ifds() {
            let _ = subs.len();
        }
        let _ = d.colortype();
        d.read_image()?;
        pages += 1;
        if !d.more_images() {
            break;
        }
        d.next_image()?;
    }
    Ok(pages)
}

/// TIFF 6.0 section 2: readers skip a field whose type code is undefined.
/// A damaged type on an optional tag must not cost the image; on a required
/// tag the image still fails (as missing).
#[test]
fn unknown_field_type_is_skipped() {
    let clean = two_page();
    let first = ifds(&clean)[0];
    let pixels = |b: &[u8]| -> Result<Vec<u8>, TiffError> {
        let mut d = Decoder::new(Cursor::new(b))?;
        match d.read_image()? {
            rusty_tiff::DecodingResult::U8(v) => Ok(v),
            other => panic!("unexpected {other:?}"),
        }
    };
    let want = pixels(&clean).unwrap();
    let n = r16(&clean, first);
    let tags: Vec<u16> = (0..n)
        .map(|k| r16(&clean, first + 2 + k * 12) as u16)
        .collect();
    let optional = [282u16, 283, 284, 296, 305, 277]
        .into_iter()
        .find(|t| tags.contains(t))
        .expect("an optional tag is written");
    for (tag, should_decode) in [(optional, true), (256, false)] {
        let mut b = clean.clone();
        let ty = field_pos(&b, first, tag) - 6;
        b[ty..ty + 2].copy_from_slice(&1028u16.to_le_bytes());
        match (pixels(&b), should_decode) {
            (Ok(v), true) => assert_eq!(v, want, "tag {tag}"),
            (Err(_), false) => {}
            (r, _) => panic!("tag {tag}: {:?}", r.map(|v| v.len())),
        }
    }
}

/// A duplicate tag keeps its last entry (as the `tiff` crate does): a bit
/// flip turning BitsPerSample (258) into 259 leaves Compression = 8 ahead of
/// the real Compression = 1, and the strip is uncompressed.
#[test]
fn duplicate_tag_keeps_the_last_entry() {
    let (w, h) = (37u32, 21u32);
    let px: Vec<u8> = (0..w * h).map(|i| (i * 13) as u8).collect();
    let mut f = Cursor::new(Vec::new());
    TiffEncoder::new(&mut f)
        .unwrap()
        .write_image::<colortype::Gray8>(w, h, &px)
        .unwrap();
    let mut b = f.into_inner();
    let first = ifds(&b)[0];
    let tag_pos = field_pos(&b, first, 258) - 8;
    b[tag_pos..tag_pos + 2].copy_from_slice(&259u16.to_le_bytes());
    let mut d = Decoder::new(Cursor::new(&b)).unwrap();
    assert!(d.read_image().is_ok(), "duplicate Compression tag refused");
}

#[test]
fn ifd_self_loop_errors() {
    let mut b = two_page();
    let first = ifds(&b)[0];
    let next_pos = first + 2 + r16(&b, first) * 12;
    w32(&mut b, next_pos, first as u32);
    let mut d = Decoder::new(Cursor::new(&b)).unwrap();
    assert!(d.more_images());
    match d.next_image() {
        Err(TiffError::Format(m)) => assert!(m.contains("loop"), "{m}"),
        r => panic!("self-loop not detected: {r:?}"),
    }
}

#[test]
fn ifd_two_cycle_errors() {
    let mut b = two_page();
    let v = ifds(&b);
    let second = v[1];
    let next_pos = second + 2 + r16(&b, second) * 12;
    w32(&mut b, next_pos, v[0] as u32);
    let mut d = Decoder::new(Cursor::new(&b)).unwrap();
    d.next_image().unwrap();
    assert!(matches!(d.next_image(), Err(TiffError::Format(_))));
    // seek_to_image restarts the walk: the same chain is walkable again.
    d.seek_to_image(1).unwrap();
    assert_eq!(d.dimensions().unwrap(), (4, 4));
}

#[test]
fn offsets_past_eof_error() {
    let mut b = two_page();
    let first = ifds(&b)[0];
    let p = field_pos(&b, first, 273);
    w32(&mut b, p, 0x7FFF_FFF0);
    let mut d = Decoder::new(Cursor::new(&b)).unwrap();
    assert!(matches!(d.read_image(), Err(TiffError::Format(_))));
    // First IFD offset past EOF.
    let mut b = two_page();
    w32(&mut b, 4, 0xFFFF_FF00);
    assert!(Decoder::new(Cursor::new(&b)).is_err());
}

#[test]
fn absurd_dimensions_error_before_allocating() {
    let mut b = two_page();
    let first = ifds(&b)[0];
    // ImageWidth/ImageLength/RowsPerStrip are SHORT here; retype as LONG 2^31
    // (one strip, so the layout is consistent and only the size is absurd).
    for tag in [256u16, 257, 278] {
        let p = field_pos(&b, first, tag);
        b[p - 6..p - 4].copy_from_slice(&4u16.to_le_bytes());
        w32(&mut b, p, 1 << 31);
    }
    let t = Instant::now();
    let mut d = Decoder::new(Cursor::new(&b)).unwrap();
    assert_eq!(d.dimensions().unwrap(), (1 << 31, 1 << 31));
    assert!(matches!(d.read_image(), Err(TiffError::Limits(_))));
    // Even with no limits, a 2^62-byte buffer is a recoverable error.
    let mut d = Decoder::new(Cursor::new(&b))
        .unwrap()
        .with_limits(Limits::unlimited());
    assert!(d.read_image().is_err());
    assert!(t.elapsed() < Duration::from_secs(2));
}

#[test]
fn lying_strip_byte_counts() {
    // Too large: clamped to EOF, data still decodes.
    let mut b = two_page();
    let first = ifds(&b)[0];
    let p = field_pos(&b, first, 279);
    w32(&mut b, p, 0xFFFF_0000);
    let mut d = Decoder::new(Cursor::new(&b)).unwrap();
    assert_eq!(
        d.read_image().unwrap(),
        rusty_tiff::DecodingResult::U8(vec![7; 64])
    );
    // Too small: the strip keeps its bytes and the rest reads as zero (as
    // the `tiff` crate does), rather than losing the image.
    let mut b = two_page();
    let p = field_pos(&b, first, 279);
    w32(&mut b, p, 10);
    let mut d = Decoder::new(Cursor::new(&b)).unwrap();
    let mut want = vec![7u8; 10];
    want.resize(64, 0);
    assert_eq!(
        d.read_image().unwrap(),
        rusty_tiff::DecodingResult::U8(want)
    );
}

#[test]
fn bigtiff_lying_entry_count() {
    let mut f = Cursor::new(Vec::new());
    TiffEncoder::new_big(&mut f)
        .unwrap()
        .write_image::<colortype::Gray8>(4, 4, &[1u8; 16])
        .unwrap();
    let mut b = f.into_inner();
    let ifd = u64::from_le_bytes(b[8..16].try_into().unwrap()) as usize;
    b[ifd..ifd + 8].copy_from_slice(&(u64::MAX / 3).to_le_bytes());
    assert!(Decoder::new(Cursor::new(&b)).is_err());
}

fn lzw_file(w: u32, h: u32) -> Vec<u8> {
    let mut f = Cursor::new(Vec::new());
    let px: Vec<u8> = (0..w * h * 3).map(|i| (i % 251) as u8).collect();
    TiffEncoder::new(&mut f)
        .unwrap()
        .with_compression(Compression::Lzw)
        .write_image::<colortype::RGB8>(w, h, &px)
        .unwrap();
    f.into_inner()
}

#[test]
fn lzw_garbage_errors() {
    let b = lzw_file(64, 64);
    let first = ifds(&b)[0];
    let off = r32(&b, field_pos(&b, first, 273));
    let mut r = Rng(5);
    for trial in 0..200 {
        let mut g = b.clone();
        let n = 20 + trial;
        let end = (off + n).min(g.len());
        for x in &mut g[off..end] {
            *x = r.next() as u8;
        }
        let _ = decode_all(&g, Limits::default());
    }
}

/// Seed files covering every decode path: compressions, predictors, tiles,
/// planar, sub-byte, float, CCITT, BigTIFF.
fn seeds() -> Vec<Vec<u8>> {
    let mut out = Vec::new();
    let px8: Vec<u8> = (0..33 * 21 * 4).map(|i| (i * 13 % 256) as u8).collect();
    let px16: Vec<u16> = (0..33 * 21 * 3).map(|i| (i * 977) as u16).collect();
    let pxf: Vec<f32> = (0..33 * 21).map(|i| i as f32 * 0.5).collect();
    for comp in [
        Compression::None,
        Compression::Lzw,
        Compression::deflate(),
        Compression::PackBits,
    ] {
        let mut f = Cursor::new(Vec::new());
        {
            let mut e = TiffEncoder::new(&mut f).unwrap().with_compression(comp);
            let mut i = e
                .new_image::<colortype::RGBA8>(33, 21)
                .unwrap()
                .with_predictor(Predictor::Horizontal);
            i.tiles(16, 16);
            i.write_data(&px8).unwrap();
            let mut i = e.new_image::<colortype::RGB16>(33, 21).unwrap();
            i.planar(true);
            i.rows_per_strip(4);
            i.write_data(&px16).unwrap();
            e.new_image::<colortype::Gray32Float>(33, 21)
                .unwrap()
                .with_predictor(Predictor::FloatingPoint)
                .write_data(&pxf)
                .unwrap();
            let bits: Vec<u8> = (0..5 * 21).map(|i| (i * 37) as u8).collect();
            e.write_image::<colortype::Gray1>(33, 21, &bits).unwrap();
        }
        out.push(f.into_inner());
    }
    for comp in [Compression::Fax3, Compression::Fax4] {
        let mut f = Cursor::new(Vec::new());
        let bits: Vec<u8> = (0..5 * 21)
            .map(|i| if i % 3 == 0 { 0xF0 } else { 0 })
            .collect();
        TiffEncoder::new(&mut f)
            .unwrap()
            .with_compression(comp)
            .write_image::<colortype::Gray1>(33, 21, &bits)
            .unwrap();
        out.push(f.into_inner());
    }
    let mut f = Cursor::new(Vec::new());
    TiffEncoder::new_big(&mut f)
        .unwrap()
        .with_compression(Compression::Lzw)
        .write_image::<colortype::CMYK8>(33, 21, &px8[..33 * 21 * 4])
        .unwrap();
    out.push(f.into_inner());
    out
}

#[test]
fn truncations_never_panic() {
    let mut n = 0;
    let mut worst = Duration::ZERO;
    for s in seeds() {
        let step = (s.len() / 400).max(1);
        for len in (0..s.len()).step_by(step) {
            let t = Instant::now();
            let _ = decode_all(&s[..len], Limits::default());
            worst = worst.max(t.elapsed());
            n += 1;
        }
    }
    println!("truncation: {n} truncated files, slowest decode {worst:?}");
    assert!(worst < Duration::from_secs(2));
}

#[test]
fn mutation_fuzz_never_panics() {
    let seeds = seeds();
    let limits = {
        let mut l = Limits::default();
        l.decoding_buffer_size = 16 << 20;
        l.intermediate_buffer_size = 16 << 20;
        l.ifd_value_size = 1 << 20;
        l
    };
    let mut r = Rng(0xC0FFEE);
    let iterations = 20_000;
    let mut ok = 0;
    let mut worst = Duration::ZERO;
    let start = Instant::now();
    for it in 0..iterations {
        let s = &seeds[it % seeds.len()];
        let mut m = s.clone();
        let flips = 1 + (r.next() % 8) as usize;
        for _ in 0..flips {
            let pos = (r.next() as usize) % m.len();
            match r.next() % 4 {
                0 => m[pos] ^= 1 << (r.next() % 8),
                1 => m[pos] = r.next() as u8,
                2 => m[pos] = 0xFF,
                _ => m[pos] = 0,
            }
        }
        if r.next().is_multiple_of(8) {
            let cut = (r.next() as usize) % m.len();
            m.truncate(cut);
        }
        let t = Instant::now();
        if decode_all(&m, limits.clone()).is_ok() {
            ok += 1;
        }
        worst = worst.max(t.elapsed());
    }
    println!(
        "mutation fuzz: {iterations} mutated files ({ok} still decoded), no panic, \
         slowest {worst:?}, total {:?}",
        start.elapsed()
    );
    assert!(worst < Duration::from_secs(2));
}

#[test]
fn ccitt_garbage_with_huge_columns_is_bounded() {
    let mut r = Rng(77);
    for _ in 0..500 {
        let data: Vec<u8> = (0..64).map(|_| r.next() as u8).collect();
        let mut o = ccitt::Options::new(ccitt::Coding::Group4, 1 << 20);
        o.max_output_bytes = 1 << 20;
        let _ = ccitt::decode(&data, &o);
        let _ = ccitt::decode_g4(&data, 1 << 20, 1 << 20, &o);
    }
}

/// One 64x64 Gray8 strip, then ImageWidth damaged to 60,000: the file holds
/// about 4 KB (less when compressed) but declares 3.8 MB.
fn inflated(comp: Compression) -> Vec<u8> {
    let px: Vec<u8> = (0..64u32 * 64).map(|i| (i * 7) as u8).collect();
    let mut f = Cursor::new(Vec::new());
    {
        let mut e = TiffEncoder::new(&mut f).unwrap().with_compression(comp);
        let mut i = e.new_image::<colortype::Gray8>(64, 64).unwrap();
        i.rows_per_strip(64);
        i.write_data(&px).unwrap();
    }
    let mut b = f.into_inner();
    let first = ifds(&b)[0];
    let p = field_pos(&b, first, 256);
    // ImageWidth is SHORT or LONG, value inline either way (little-endian).
    w32(&mut b, p, 60_000);
    let ty = p - 6;
    b[ty..ty + 2].copy_from_slice(&4u16.to_le_bytes());
    b
}

/// ArtCraft's report on 0.1.1: zero-filling short strips let a damaged size
/// field turn a 53 KB file into 223 MB of zeros. The zero-fill is bounded by
/// what the file really holds, before allocating (uncompressed) and after
/// decoding (compressed, where the stored bytes alone cannot tell).
#[test]
fn damaged_size_field_cannot_inflate_the_image() {
    for comp in [
        Compression::None,
        Compression::Lzw,
        Compression::PackBits,
        Compression::Deflate(6),
    ] {
        let b = inflated(comp);
        assert!(b.len() < 8192, "{comp:?}: {} bytes", b.len());
        // Default limits: refused.
        let r = Decoder::new(Cursor::new(&b)).unwrap().read_image();
        assert!(matches!(r, Err(TiffError::Format(_))), "{comp:?}: {r:?}");
        // Explicitly unlimited: the old behaviour, zero-filled.
        let mut d = Decoder::new(Cursor::new(&b))
            .unwrap()
            .with_limits(Limits::unlimited());
        match d.read_image() {
            Ok(rusty_tiff::DecodingResult::U8(v)) => assert_eq!(v.len(), 60_000 * 64),
            r => panic!("{comp:?}: {:?}", r.map(|_| ())),
        }
    }
}

/// `max_fill_ratio = 0` is strict: a strip even a little short is an error;
/// the default still recovers it.
#[test]
fn strict_fill_ratio_refuses_short_strips() {
    let mut b = two_page();
    let first = ifds(&b)[0];
    let p = field_pos(&b, first, 279);
    w32(&mut b, p, 60);
    assert!(Decoder::new(Cursor::new(&b)).unwrap().read_image().is_ok());
    let mut strict = Limits::default();
    strict.max_fill_ratio = 0;
    let mut d = Decoder::new(Cursor::new(&b)).unwrap().with_limits(strict);
    assert!(matches!(d.read_image(), Err(TiffError::Format(_))));
    // An intact file passes strict mode.
    let ok = two_page();
    let mut strict = Limits::default();
    strict.max_fill_ratio = 0;
    let mut d = Decoder::new(Cursor::new(&ok)).unwrap().with_limits(strict);
    assert!(d.read_image().is_ok());
}

/// ArtCraft's report on 0.1.4: on bit-flipped 16-bit Deflate tiles the error
/// text changed from "invalid distance too far back" (0.1.3) to "repeated
/// call with bad state". The inflater was called once, as before; zlib-rs
/// overwrites the message of an error its fast loop finds, and the 16-bit
/// band gives that loop the room to run. The message is the real one again.
#[test]
fn damaged_deflate_tile_keeps_the_inflate_message() {
    // A zlib stream: one fixed-Huffman block, literal 'A', then a match 33
    // bytes back (bits listed least significant first), then zero padding.
    let bad: Vec<u8> = {
        let mut bits: Vec<u8> = vec![1, 1, 0]; // BFINAL, BTYPE 01
        let mut code = |c: u32, n: u32| (0..n).rev().for_each(|i| bits.push((c >> i & 1) as u8));
        code(0x30 + 65, 8); // literal 'A'
        code(1, 7); // length code 257: length 3
        code(10, 5); // distance code 10: 33 + 4 extra bits (all 0)
        bits.extend([0; 4]);
        let mut b = vec![0x78, 0x9c];
        b.extend(
            bits.chunks(8)
                .map(|c| c.iter().enumerate().fold(0u8, |a, (i, &v)| a | v << i)),
        );
        b.extend([0u8; 32]);
        b
    };
    for (w, h, pred) in [(37u32, 29u32, false), (130, 67, true)] {
        let px: Vec<u16> = (0..w * h)
            .map(|i| (i.wrapping_mul(2654435761) >> 19) as u16)
            .collect();
        let mut f = Cursor::new(Vec::new());
        {
            let mut e = TiffEncoder::new(&mut f).unwrap();
            let mut i = e
                .new_image::<colortype::Gray16>(w, h)
                .unwrap()
                .with_compression(Compression::Deflate(6));
            if pred {
                i = i.with_predictor(Predictor::Horizontal);
            }
            i.tiles(16, 16);
            i.write_data(&px).unwrap();
        }
        let mut b = f.into_inner();
        let mut d = Decoder::new(Cursor::new(&b)).unwrap();
        let offs = d
            .get_tag(rusty_tiff::Tag::TileOffsets)
            .unwrap()
            .to_u64_vec()
            .unwrap();
        let counts = d
            .get_tag(rusty_tiff::Tag::TileByteCounts)
            .unwrap()
            .to_u64_vec()
            .unwrap();
        // The second tile of the first band: decoded with the band's room
        // after it, as in the reported files.
        let (o, n) = (offs[1] as usize, counts[1] as usize);
        assert!(n >= bad.len(), "{n}");
        b[o..o + bad.len()].copy_from_slice(&bad);
        let e = Decoder::new(Cursor::new(&b))
            .unwrap()
            .read_image()
            .unwrap_err();
        assert!(matches!(e, TiffError::Format(_)), "{e:?}");
        let msg = e.to_string();
        assert!(
            msg.contains("invalid distance too far back"),
            "{w}x{h}: {msg}"
        );
        assert!(!msg.contains("repeated call"), "{w}x{h}: {msg}");
    }
}
