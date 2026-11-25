//! Damaged and hostile input: errors, no panics, bounded work.

mod common;

use std::collections::BTreeSet;
use std::time::{Duration, Instant};

use common::*;
use rusty_jbig2::{decode_jbig2, decode_jbig2_with, DecodeOptions, Error, Jbig2File, Limits};

fn small(lenient: bool) -> DecodeOptions {
    let limits = Limits::default().with_max_work(1 << 18);
    DecodeOptions::new()
        .with_limits(limits)
        .with_lenient(lenient)
}

fn try_all(data: &[u8], opts: &DecodeOptions) {
    if let Ok(f) = Jbig2File::parse_with(data, opts.clone()) {
        for i in 0..f.page_count().min(4) {
            let _ = f.decode_page(i);
        }
    }
    let _ = decode_jbig2_with(data.get(13..).unwrap_or(data), None, opts);
}

fn streams() -> Vec<(String, Vec<u8>)> {
    let names: BTreeSet<_> = manifest().into_iter().map(|e| e.name).collect();
    names
        .into_iter()
        .map(|n| (n.clone(), read(&format!("{n}.jbig2"))))
        .filter(|(n, d)| d.len() < 9000 && !n.contains("malformed"))
        .collect()
}

/// Cut anywhere, a stream is an error, unless all that is missing comes after
/// the page is complete (then the image is the whole one).
#[test]
fn truncation() {
    for e in manifest() {
        if e.strict == "ERR" || e.name.contains("malformed") || e.name.contains("memory") {
            continue;
        }
        let data = e.data();
        if data.len() > 9000 {
            continue;
        }
        let step = (data.len() / 24).max(1);
        // Every cut near the end, a sample before it.
        let cuts = (0..data.len()).filter(|&c| c + 24 > data.len() || c % step == 0);
        for cut in cuts {
            let d = &data[..cut];
            if let Ok(b) =
                Jbig2File::parse_with(d, small(false)).and_then(|f| f.decode_page(e.page))
            {
                assert_eq!(digest(&b), e.strict, "{} cut at {cut}", e.name);
            }
            try_all(d, &small(true));
            // As an embedded stream a cut at a segment boundary is not
            // detectable; anywhere else is.
            let _ = decode_jbig2_with(d.get(13..).unwrap_or(&[]), None, &small(false));
        }
    }
}

/// An embedded stream cut inside a segment is an error at every position.
#[test]
fn embedded_truncation_is_an_error() {
    let globals = read("pdf/globals.jb2");
    let page = read("pdf/page0.jb2");
    let whole = decode_jbig2(&page, Some(&globals)).unwrap();
    for cut in (0..globals.len()).step_by(97) {
        assert!(decode_jbig2(&page, Some(&globals[..cut])).is_err(), "{cut}");
    }
    let mut boundaries = 0;
    for cut in 0..page.len() {
        if let Ok(b) = decode_jbig2(&page[..cut], Some(&globals)) {
            boundaries += 1;
            assert!(b != whole || page.len() - cut <= 11, "{cut}");
        }
    }
    assert!(boundaries <= 2, "{boundaries}");
}

#[test]
fn byte_flips() {
    let mut r = Rng(0xFEED_FACE_CAFE_BEEF);
    for (_, data) in streams() {
        for k in 0..24 {
            let mut d = data.clone();
            for _ in 0..=r.below(3) {
                // Headers are where the structure is: half the hits go there.
                let at = if k % 2 == 0 {
                    r.below(data.len() as u32)
                } else {
                    13 + r.below(120).min(data.len() as u32 - 14)
                };
                d[at as usize] = match r.below(4) {
                    0 => 0,
                    1 => 0xFF,
                    2 => d[at as usize] ^ (1 << r.below(8)),
                    _ => r.next() as u8,
                };
            }
            try_all(&d, &small(k % 4 < 2));
        }
    }
}

#[test]
fn random_bytes() {
    let mut r = Rng(0x0123_4567_89AB_CDEF);
    for _ in 0..4000 {
        let n = r.below(300) as usize;
        let d: Vec<u8> = (0..n).map(|_| r.next() as u8).collect();
        let _ = decode_jbig2_with(&d, Some(&d), &small(false));
        // Plausible segments with random data.
        let mut s = page_info(1 + r.below(200), 1 + r.below(200), r.next() as u8, 0);
        let kinds = [0, 4, 6, 16, 20, 22, 36, 38, 39, 40, 42, 50, 53, 62];
        for i in 0..=r.below(4) {
            let kind = kinds[r.below(14) as usize];
            let mut d: Vec<u8> = (0..r.below(80)).map(|_| r.next() as u8).collect();
            if kind != 0 && kind != 16 && kind < 50 {
                let mut g = region(1 + r.below(99), 1 + r.below(99), 0, 0, 0);
                g.append(&mut d);
                d = g;
            }
            let refs: Vec<u8> = (0..r.below(4)).map(|_| r.below(6) as u8).collect();
            s.extend(segment(i + 1, kind, &refs, &d));
        }
        let _ = decode_jbig2_with(&s, None, &small(false));
        let _ = decode_jbig2_with(&s, None, &small(true));
    }
}

fn limit(data: &[u8]) {
    let t = Instant::now();
    let r = decode_jbig2(data, None);
    assert!(matches!(r, Err(Error::LimitExceeded(_))), "{r:?}");
    // Default limits, debug build: generous.
    assert!(t.elapsed() < Duration::from_secs(120), "{:?}", t.elapsed());
}

/// Like [`limit`], with a lower work bound to keep debug runs short.
fn work_limit(data: &[u8]) {
    let opts = DecodeOptions::new().with_limits(Limits::default().with_max_work(1 << 24));
    let r = decode_jbig2_with(data, None, &opts);
    assert_eq!(r, Err(Error::LimitExceeded("max_work")));
}

#[test]
fn huge_dimensions() {
    for (w, h) in [
        (u32::MAX, u32::MAX - 1),
        (1, u32::MAX - 1),
        (u32::MAX, 1),
        (65536, 65536),
    ] {
        limit(&page_info(w, h, 0, 0));
    }
    let at = nominal_at(0, false);
    let g = generic_data(&Img::new(8, 8), 0, 0, 0, 0, false, &at, false);
    for (w, h) in [(u32::MAX, u32::MAX), (1, u32::MAX), (1 << 20, 1 << 20)] {
        for flags in [g[17], 1] {
            let mut d = region(w, h, 0, 0, 0);
            d.push(flags);
            d.extend(&g[18..]);
            let mut s = page_info(8, 8, 0, 0);
            s.extend(segment(1, 38, &[], &d));
            limit(&s);
        }
    }
    // Open-ended page pushed down by a region or a stripe end.
    let mut d = g.clone();
    d[12..16].copy_from_slice(&0xFFFF_FFF0u32.to_be_bytes());
    let mut s = page_info(8, u32::MAX, 0, 0x8001);
    s.extend(segment(1, 38, &[], &d));
    limit(&s);
    let mut s = page_info(64, u32::MAX, 0, 0x8001);
    s.extend(segment(1, 50, &[], &0xFFFF_FFFEu32.to_be_bytes()));
    limit(&s);
    // A region far outside a page of known size is simply clipped.
    let mut d = g;
    d[8..16].fill(0xFF);
    let mut s = page_info(8, 8, 0, 0);
    s.extend(segment(1, 38, &[], &d));
    assert!(decode_jbig2(&s, None).is_ok());
}

/// Arithmetic data may run out and still "decode": declared counts must not
/// buy unbounded work.
#[test]
fn bombs() {
    let dict = |new: u32, exported: u32| {
        // Arithmetic, template 2, one AT pixel.
        let mut d = vec![0x08, 0x00, 2, 0xFF];
        d.extend(exported.to_be_bytes());
        d.extend(new.to_be_bytes());
        d.extend([0x12, 0x34, 0x56]);
        d
    };
    // Symbol counts.
    for n in [u32::MAX, 1 << 21] {
        let mut s = page_info(8, 8, 0, 0);
        s.extend(segment(1, 0, &[], &dict(n, n)));
        limit(&s);
    }
    // Text: four billion instances of nothing much.
    let mut d = region(8, 8, 0, 0, 0);
    d.extend([0, 0]);
    d.extend(u32::MAX.to_be_bytes());
    d.extend([0x00; 4]);
    let mut s = page_info(8, 8, 0, 0);
    s.extend(segment(1, 6, &[], &d));
    let r = decode_jbig2(&s, None);
    assert!(r.is_err());
    // Halftone: an enormous grid over a tiny region.
    let mut p = vec![0, 1, 1];
    p.extend(1u32.to_be_bytes());
    p.extend([0; 8]);
    for (gw, gh) in [(u32::MAX, u32::MAX), (1 << 16, 1 << 16)] {
        let mut d = region(8, 8, 0, 0, 0);
        d.push(0x08);
        d.extend(gw.to_be_bytes());
        d.extend(gh.to_be_bytes());
        d.extend([0; 12]);
        let mut s = page_info(8, 8, 0, 0);
        s.extend(segment(1, 16, &[], &p));
        s.extend(segment(2, 22, &[1], &d));
        limit(&s);
    }
    // Pattern count.
    let mut p = vec![0, 1, 1];
    p.extend(u32::MAX.to_be_bytes());
    p.extend([0; 8]);
    let mut s = page_info(8, 8, 0, 0);
    s.extend(segment(1, 16, &[], &p));
    limit(&s);
    // Many large regions, each individually allowed.
    let at = nominal_at(3, false);
    let g = generic_data(&Img::new(8, 8), 0, 0, 0, 3, false, &at, false);
    let mut d = region(4096, 1024, 0, 0, 0);
    d.extend(&g[17..]);
    let mut s = page_info(8, 8, 0, 0);
    for n in 1..60 {
        s.extend(segment(n, 38, &[], &d));
    }
    work_limit(&s);
    // Dictionaries doubling by reference: each refers to the last one 200
    // times over.
    let mut s = page_info(8, 8, 0, 0);
    s.extend(segment(1, 0, &[], &dict(0, 0)));
    for n in 2..250u32 {
        s.extend(segment(n, 0, &[n as u8 - 1; 250], &dict(0, 0)));
    }
    assert!(decode_jbig2(&s, None).is_ok());
}

/// Segments can only refer to ones already decoded.
#[test]
fn forward_self_and_wrong_references() {
    let mut text = region(8, 8, 0, 0, 0);
    text.extend([0, 0, 0, 0, 0, 1, 0, 0]);
    for refs in [&[2u8][..], &[9], &[2, 9, 9, 9, 9, 9]] {
        let mut s = page_info(8, 8, 0, 0);
        s.extend(segment(2, 6, refs, &text));
        assert!(matches!(decode_jbig2(&s, None), Err(Error::Invalid(_))));
    }
    // A refinement of itself, and of the page information segment.
    let mut d = region(8, 8, 0, 0, 0);
    d.extend([1, 0, 0]);
    for r in [0u8, 1, 2] {
        let mut s = page_info(8, 8, 0, 0);
        s.extend(segment(1, 42, &[r], &d));
        assert!(decode_jbig2(&s, None).is_err());
    }
    // A halftone region whose dictionary is missing.
    let mut h = region(8, 8, 0, 0, 0);
    h.extend([0; 21]);
    let mut s = page_info(8, 8, 0, 0);
    s.extend(segment(1, 22, &[1], &h));
    assert!(decode_jbig2(&s, None).is_err());
}

#[test]
fn small_structural_errors() {
    assert!(decode_jbig2(&[], None).is_err());
    assert!(Jbig2File::parse(b"not a file").is_err());
    let ok = page_info(8, 8, 0, 0);
    assert_eq!(decode_jbig2(&ok, None).unwrap().data, [0; 8]);
    let mut twice = ok.clone();
    twice.extend(&ok);
    assert!(decode_jbig2(&twice, None).is_err());
    assert!(decode_jbig2(&page_info(0, 8, 0, 0), None).is_err());
    assert!(decode_jbig2(&page_info(8, 0, 0, 0), None).is_err());
    // An AT pixel that is not yet decoded, in every place AT pixels appear.
    let at = nominal_at(0, false);
    for (x, y) in [(0i8, 0i8), (1, 0), (0, 1), (-1, 127)] {
        let mut d = generic_data(&Img::new(8, 8), 0, 0, 0, 0, false, &at, false);
        (d[18], d[19]) = (x as u8, y as u8);
        let mut s = ok.clone();
        s.extend(segment(1, 38, &[], &d));
        assert!(matches!(decode_jbig2(&s, None), Err(Error::Invalid(_))));
    }
    // 4-GiB reference counts and lengths in an 11-byte header.
    let s = [
        0, 0, 0, 1, 0, 0xFF, 0xFF, 0xFF, 0xFF, 1, 0xFF, 0xFF, 0xFF, 0xFE,
    ];
    assert_eq!(decode_jbig2(&s, None), Err(Error::Truncated));
}
