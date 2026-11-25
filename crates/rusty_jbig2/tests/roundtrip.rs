//! Random images through the specification-derived encoder in `common`:
//! every template, moved AT pixels, odd sizes, stripes and page rules that no
//! third-party stream exercises.

mod common;

use common::*;
use rusty_jbig2::{decode_jbig2, Error};

fn random_at(r: &mut Rng, t: u8, ext: bool) -> At {
    let mut at = nominal_at(t, ext);
    for p in &mut at {
        match r.below(6) {
            0 => {
                // Anywhere in Figure 7's field.
                let y = r.range(-128, 0);
                let x = if y == 0 {
                    r.range(-128, -1)
                } else {
                    r.range(-128, 127)
                };
                *p = (x, y);
            }
            1 => *p = (r.range(-20, -1), 0),
            2 => *p = (r.range(-9, 9), r.range(-3, -1)),
            _ => {}
        }
    }
    at
}

#[test]
fn generic_regions() {
    let mut r = Rng(0x1234_5678_9ABC_DEF1);
    for i in 0..1200 {
        let (w, h) = (r.range(1, 90), r.range(1, 40));
        let m = Img::random(&mut r, w, h);
        let t = (i % 4) as u8;
        let ext = t == 0 && r.below(3) == 0;
        let at = if r.below(3) == 0 {
            nominal_at(t, ext)
        } else {
            random_at(&mut r, t, ext)
        };
        let tpgd = r.below(2) == 0;
        let mut s = page_info(w as u32, h as u32, 0, 0);
        let d = generic_data(&m, 0, 0, 0, t, ext, &at, tpgd);
        s.extend(segment(1, 38, &[], &d));
        let b = decode_jbig2(&s, None).unwrap();
        assert_eq!(
            b.data,
            m.packed(),
            "{w}x{h} template {t} ext {ext} tpgd {tpgd} {at:?}"
        );
    }
}

#[test]
fn refinement_regions() {
    let mut r = Rng(0xDEAD_BEEF_0BAD_F00D);
    for i in 0..1200 {
        let (w, h) = (r.range(1, 70), r.range(1, 30));
        let reference = Img::random(&mut r, w, h);
        let mut m = reference.clone();
        for _ in 0..r.below(12) {
            let (x, y) = (r.range(0, w - 1), r.range(0, h - 1));
            m.set(x, y, 1 ^ m.get(x, y));
        }
        let one = i % 2 == 1;
        let mut at = vec![(-1, -1), (-1, -1)];
        if r.below(3) != 0 {
            let y = -(r.below(6) as i32);
            at[0] = (
                if y == 0 {
                    r.range(-128, -1)
                } else {
                    r.range(-128, 127)
                },
                y,
            );
            at[1] = if r.below(2) == 0 {
                (r.range(-128, 127), r.range(-128, 127))
            } else {
                (r.range(-9, 9), r.range(-9, 9))
            };
        }
        let tpgr = r.below(2) == 0;
        let nominal = nominal_at(2, false);
        let mut s = page_info(w as u32, h as u32, 0, 0);
        // The reference as an intermediate region, or as page contents.
        let intermediate = r.below(2) == 0;
        let g = generic_data(&reference, 0, 0, 0, 2, false, &nominal, false);
        s.extend(segment(1, if intermediate { 36 } else { 38 }, &[], &g));
        let op = if intermediate { 0 } else { 4 };
        let mut d = region(w as u32, h as u32, 0, 0, op);
        d.push(u8::from(one) | u8::from(tpgr) << 1);
        if !one {
            for &(x, y) in &at {
                d.extend([x as i8 as u8, y as i8 as u8]);
            }
        }
        d.extend(encode_refinement(&m, &reference, one, &at, tpgr));
        let refs: &[u8] = if intermediate { &[1] } else { &[] };
        s.extend(segment(2, 42, refs, &d));
        let b = decode_jbig2(&s, None).unwrap();
        assert_eq!(b.data, m.packed(), "{w}x{h} one {one} tpgr {tpgr} {at:?}");
    }
}

fn stripe_end(number: u32, y: u32) -> Vec<u8> {
    segment(number, 50, &[], &y.to_be_bytes())
}

/// Unknown page height: rows appear with the stripes and the last end of
/// stripe fixes the height, whatever the stripe size declared.
#[test]
fn unknown_height_grows_with_stripes() {
    let mut r = Rng(77);
    let m = Img::random(&mut r, 33, 10);
    let at = nominal_at(0, false);
    let part = |y0: i32, y1: i32| {
        let mut p = Img::new(33, y1 - y0);
        for y in y0..y1 {
            for x in 0..33 {
                p.set(x, y - y0, m.get(x, y));
            }
        }
        p
    };
    for default_black in [false, true] {
        let mut s = page_info(33, u32::MAX, u8::from(default_black) << 2, 0x8004);
        let mut n = 1;
        for (y0, y1) in [(0, 4), (4, 8), (8, 10)] {
            let d = generic_data(&part(y0, y1), 0, y0 as u32, 4, 0, false, &at, true);
            s.extend(segment(n, 38, &[], &d));
            s.extend(stripe_end(n + 1, y1 as u32 - 1));
            n += 2;
        }
        // A last stripe nothing is drawn in.
        s.extend(stripe_end(n, 12));
        let b = decode_jbig2(&s, None).unwrap();
        assert_eq!((b.width, b.height), (33, 13));
        assert_eq!(b.data[..50], m.packed()[..]);
        let blank = if default_black {
            [0xFF, 0xFF, 0xFF, 0xFF, 0x80].repeat(3)
        } else {
            vec![0; 15]
        };
        assert_eq!(b.data[50..], blank[..]);
    }
    // No stripe at all: the height was never given.
    let s = page_info(33, u32::MAX, 0, 0x8004);
    assert!(matches!(decode_jbig2(&s, None), Err(Error::Invalid(_))));
}

/// A region of unknown length may hold fewer rows than it declared.
#[test]
fn unknown_length_with_fewer_rows() {
    let mut r = Rng(5);
    let m = Img::random(&mut r, 40, 9);
    let at = nominal_at(1, false);
    let mut d = generic_data(&m, 0, 0, 0, 1, false, &at, false);
    d[4..8].copy_from_slice(&20u32.to_be_bytes());
    d.extend(9u32.to_be_bytes());
    let mut s = page_info(40, 20, 0, 0);
    s.extend(segment_len(1, 38, &[], &d, u32::MAX));
    s.extend(segment(2, 49, &[], &[]));
    let b = decode_jbig2(&s, None).unwrap();
    let mut want = m.packed();
    want.resize(100, 0);
    assert_eq!(b.data, want);
}

/// Region placement: partly and wholly off the page, every operator.
#[test]
fn composition_matches_the_definition() {
    let mut r = Rng(99);
    let at = nominal_at(3, false);
    for i in 0..200 {
        let black = r.below(2) == 0;
        let mut page = Img::new(50, 20);
        page.px.fill(u8::from(black));
        let mut s = page_info(50, 20, u8::from(black) << 2 | 0x40, 0);
        for n in 0..3 {
            let (w, h) = (r.range(1, 70), r.range(1, 30));
            let m = Img::random(&mut r, w, h);
            let (x, y) = (r.range(0, 60), r.range(0, 25));
            let op = ((i + n) % 5) as u8;
            let d = generic_data(&m, x as u32, y as u32, op, 3, false, &at, false);
            s.extend(segment(n + 1, 39, &[], &d));
            for py in y..(y + h).min(20) {
                for px in x..(x + w).min(50) {
                    let (d, s) = (page.get(px, py), m.get(px - x, py - y));
                    let v = match op {
                        0 => d | s,
                        1 => d & s,
                        2 => d ^ s,
                        3 => 1 ^ d ^ s,
                        _ => s,
                    };
                    page.set(px, py, v);
                }
            }
        }
        assert_eq!(decode_jbig2(&s, None).unwrap().data, page.packed());
    }
}
