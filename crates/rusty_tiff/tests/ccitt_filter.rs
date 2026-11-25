//! `ccitt::decode_filter`: the PDF `CCITTFaxDecode` parameters.
//!
//! `tests/data/ccitt-pdf/*.fax` are one 123x40 image encoded by Ghostscript
//! 10's `CCITTFaxEncode` filter over every combination of `K`, `EndOfLine`,
//! `EncodedByteAlign` and `EndOfBlock` tried here; Ghostscript's
//! `CCITTFaxDecode` gives `img.raw` back for each of them. `K` > 0 without
//! `EndOfLine` is left out: Ghostscript writes such rows without the tag bit
//! T.4 has, which PDF readers (and this decoder) expect.

use std::path::PathBuf;

use rusty_tiff::ccitt::{self, decode_filter, Coding, FilterOptions, Options};

const W: u32 = 123;
const H: u32 = 40;
const STRIDE: usize = 16;

fn read(name: &str) -> Vec<u8> {
    let p = PathBuf::from(env!("CARGO_MANIFEST_DIR"))
        .join("tests/data/ccitt-pdf")
        .join(name);
    std::fs::read(&p).unwrap_or_else(|e| panic!("{}: {e}", p.display()))
}

#[test]
fn ghostscript_parameter_matrix() {
    let img = read("img.raw");
    let mut n = 0;
    for k in [-1, 0, 4] {
        for bits in 0..8 {
            let (eol, align, eob) = (bits & 1 != 0, bits & 2 != 0, bits & 4 != 0);
            if (k < 0 && eol) || (k > 0 && !eol) {
                continue;
            }
            let b = u8::from;
            let name = format!("k{k}_eol{}_al{}_eob{}.fax", b(eol), b(align), b(eob));
            let data = read(&name);
            // Rows only where nothing else ends the data, and also everywhere.
            for rows in [if eob { 0 } else { H }, H] {
                let f = FilterOptions::from_pdf(k, W, rows)
                    .with_end_of_line(eol)
                    .with_end_of_block(eob)
                    .with_encoded_byte_align(align);
                let out = decode_filter(&data, &f).unwrap_or_else(|e| panic!("{name}: {e}"));
                assert_eq!((out.rows, out.damaged_rows), (H, 0), "{name}");
                assert_eq!(out.data, img, "{name}");
                let black = decode_filter(&data, &f.with_black_is_1(true)).unwrap();
                for (i, (a, b)) in black.data.iter().zip(&img).enumerate() {
                    let m = if i % STRIDE == STRIDE - 1 { 0xE0 } else { 0xFF };
                    assert_eq!(*a, !b & m, "{name}");
                }
                n += 1;
            }
        }
    }
    assert_eq!(n, 32);
}

/// Where `decode` and `decode_filter` answer the same question they agree.
#[test]
fn agrees_with_decode() {
    let img = read("img.raw");
    for coding in [
        Coding::ModifiedHuffman,
        Coding::Group3_1D,
        Coding::Group3_2D,
        Coding::Group4,
    ] {
        let mut o = Options::new(coding, W).with_rows(H);
        o.black_is_1 = false;
        let coded = ccitt::encode(&img, &o).unwrap();
        assert_eq!(ccitt::decode(&coded, &o).unwrap(), img);
        let f = FilterOptions::new(o);
        assert_eq!(decode_filter(&coded, &f).unwrap().data, img, "{coding:?}");
        let f = f.with_end_of_block(false);
        assert_eq!(decode_filter(&coded, &f).unwrap().data, img, "{coding:?}");
    }
}

#[test]
fn end_of_block_and_rows() {
    let img = read("img.raw");
    let eob = read("k-1_eol0_al0_eob1.fax");
    let bare = read("k-1_eol0_al0_eob0.fax");
    let f = |rows| FilterOptions::from_pdf(-1, W, rows);
    // The end-of-block code overrides Rows, both ways.
    for rows in [0, 3, 40, 900] {
        let out = decode_filter(&eob, &f(rows)).unwrap();
        assert_eq!((out.rows, &out.data), (H, &img), "Rows {rows}");
    }
    // Without it Rows decides, or the end of the data.
    let out = decode_filter(&eob, &f(3).with_end_of_block(false)).unwrap();
    assert_eq!((out.rows, &out.data[..]), (3, &img[..3 * STRIDE]));
    let out = decode_filter(&bare, &f(900).with_end_of_block(false)).unwrap();
    assert_eq!((out.rows, &out.data), (H, &img));
    // Expected but missing: an error, unless every row is there.
    assert!(decode_filter(&bare, &f(900)).is_err());
    assert_eq!(decode_filter(&bare, &f(H)).unwrap().data, img);
    // Bytes after the rows that are not fax data.
    let mut junk = bare.clone();
    junk.extend_from_slice(&[0x00, 0x0A, 0x00, 0x00, 0x00, 0x55]);
    let out = decode_filter(&junk, &f(H)).unwrap();
    assert_eq!(out.data[..img.len()], img[..]);
    // Cut inside a row: an error, whatever EndOfBlock says.
    let mut errors = 0;
    for cut in 0..bare.len() {
        for end in [true, false] {
            match decode_filter(&bare[..cut], &f(H).with_end_of_block(end)) {
                Err(_) => errors += 1,
                Ok(o) => assert!(!end || o.rows == H),
            }
        }
    }
    assert!(errors > bare.len(), "{errors}");
}

#[test]
fn end_of_line_is_required_when_asked_for() {
    for k in [0, 4] {
        let with = read(&format!("k{k}_eol1_al0_eob1.fax"));
        let without = if k == 0 {
            read("k0_eol0_al0_eob1.fax")
        } else {
            strip_eols(&read("k4_eol1_al0_eob0.fax"))
        };
        let f = FilterOptions::from_pdf(k, W, H);
        assert_eq!(decode_filter(&without, &f).unwrap().data, read("img.raw"));
        assert!(decode_filter(&without, &f.clone().with_end_of_line(true)).is_err());
        // Always accepted.
        assert!(decode_filter(&with, &f).is_ok());
    }
}

/// The bits of `data` and where its EOLs (000000000001) start and end.
fn eols(data: &[u8]) -> (Vec<u8>, Vec<(usize, usize)>) {
    let bits: Vec<u8> = data
        .iter()
        .flat_map(|b| (0..8).map(move |i| (b >> (7 - i)) & 1))
        .collect();
    let mut eols = Vec::new();
    let mut zeros = 0;
    for (i, &b) in bits.iter().enumerate() {
        if b == 1 {
            if zeros >= 11 {
                eols.push((i - 11, i + 1));
            }
            zeros = 0;
        } else {
            zeros += 1;
        }
    }
    (bits, eols)
}

fn pack(bits: &[u8]) -> Vec<u8> {
    bits.chunks(8)
        .map(|c| c.iter().enumerate().fold(0, |b, (i, v)| b | v << (7 - i)))
        .collect()
}

/// Take the EOL codes out (tag bits stay).
fn strip_eols(data: &[u8]) -> Vec<u8> {
    let (bits, eols) = eols(data);
    let mut out = Vec::new();
    let mut at = 0;
    for (s, e) in eols {
        out.extend_from_slice(&bits[at..s]);
        at = e;
    }
    out.extend_from_slice(&bits[at..]);
    pack(&out)
}

/// Replace rows of an EOL-framed stream by `junk` bits.
fn damage(data: &[u8], rows: &[usize], junk: &[u8]) -> Vec<u8> {
    let (bits, eols) = eols(data);
    let mut out = Vec::new();
    let mut at = 0;
    for &r in rows {
        let start = eols[r].1;
        out.extend_from_slice(&bits[at..start]);
        out.extend_from_slice(junk);
        at = eols[r + 1].0;
    }
    out.extend_from_slice(&bits[at..]);
    pack(&out)
}

#[test]
fn damaged_rows() {
    let img = read("img.raw");
    let row = |r: usize| img[r * STRIDE..(r + 1) * STRIDE].to_vec();
    let mut white = vec![0xFF; STRIDE];
    white[STRIDE - 1] = 0xE0;
    let data = read("k0_eol1_al0_eob1.fax");
    let f = FilterOptions::from_pdf(0, W, H).with_end_of_line(true);
    // An invalid white code, and a row cut short (white 20).
    let invalid = [0, 0, 0, 0, 0, 0, 0, 0, 1, 1];
    let short = [0, 0, 0, 1, 0, 0, 0];
    for junk in [&invalid[..], &short[..]] {
        let d = damage(&data, &[5, 12, 13, 30], junk);
        // Rows 5 and 12 repeat the row before; 13 follows a damaged row and
        // is white; 30 repeats 29.
        let mut want = img.clone();
        for (r, with) in [
            (5, row(4)),
            (12, row(11)),
            (13, white.clone()),
            (30, row(29)),
        ] {
            want[r * STRIDE..(r + 1) * STRIDE].copy_from_slice(&with);
        }
        let tolerate = |n| f.clone().with_damaged_rows_before_error(n);
        let out = decode_filter(&d, &tolerate(4)).unwrap();
        assert_eq!((out.rows, out.damaged_rows), (H, 4));
        assert_eq!(out.data, want);
        // One fewer is an error at the fourth.
        assert_eq!(decode_filter(&d, &tolerate(3)).unwrap_err().row, 30);
        assert_eq!(decode_filter(&d, &f).unwrap_err().row, 5);
    }
    // A damaged first row has nothing to repeat.
    let d = damage(&data, &[0], &invalid);
    let out = decode_filter(&d, &f.clone().with_damaged_rows_before_error(9)).unwrap();
    assert_eq!(out.data[..STRIDE], white[..]);
    assert_eq!(out.data[STRIDE..], img[STRIDE..]);
    // Not without EndOfLine, and not for Group 4.
    let lax = FilterOptions::from_pdf(0, W, H).with_damaged_rows_before_error(9);
    assert!(decode_filter(&damage(&data, &[5], &invalid), &lax).is_err());
    let mut g4 = read("k-1_eol0_al0_eob1.fax");
    g4[60..70].fill(0x01);
    let f4 = FilterOptions::from_pdf(-1, W, H)
        .with_end_of_line(true)
        .with_damaged_rows_before_error(99);
    assert!(decode_filter(&g4, &f4).is_err());
}

#[test]
fn hostile_input_is_bounded() {
    let mut x = 0x9E37_79B9_7F4A_7C15u64;
    let mut next = || {
        x ^= x << 13;
        x ^= x >> 7;
        x ^= x << 17;
        (x >> 32) as u32
    };
    let seeds: Vec<Vec<u8>> = ["k0_eol1_al0_eob1", "k4_eol1_al1_eob1", "k-1_eol0_al0_eob1"]
        .iter()
        .map(|n| read(&format!("{n}.fax")))
        .collect();
    for i in 0..6000u32 {
        let mut d = seeds[(i % 3) as usize].clone();
        for _ in 0..1 + next() % 6 {
            let at = next() as usize % d.len();
            d[at] = next() as u8;
        }
        d.truncate(next() as usize % (d.len() + 1) + 1);
        let k = [0, 4, -1][((i / 3) % 3) as usize];
        let mut f = FilterOptions::from_pdf(k, 1 + next() % 300, next() % 50)
            .with_end_of_line(next() % 2 == 0)
            .with_end_of_block(next() % 2 == 0)
            .with_encoded_byte_align(next() % 2 == 0)
            .with_damaged_rows_before_error(next() % 8);
        f.options.max_output_bytes = 4096;
        if let Ok(o) = decode_filter(&d, &f) {
            assert!(o.data.len() <= 4096);
            let stride = f.options.columns.div_ceil(8) as usize;
            assert_eq!(o.data.len(), o.rows as usize * stride);
            assert!(o.damaged_rows <= f.damaged_rows_before_error);
        }
    }
    // Long zero tails and EOL floods, with unlimited tolerance.
    for d in [
        vec![0u8; 1 << 20],
        [0x00, 0x10, 0x01].repeat(1 << 18),
        [0x00, 0x1F].repeat(1 << 19),
    ] {
        for k in [0, 4, -1] {
            let mut f = FilterOptions::from_pdf(k, 2000, 0)
                .with_end_of_line(true)
                .with_damaged_rows_before_error(u32::MAX);
            f.options.max_output_bytes = 1 << 20;
            let _ = decode_filter(&d, &f);
        }
    }
    assert!(decode_filter(&[0; 8], &FilterOptions::from_pdf(0, 0, 1)).is_err());
}
