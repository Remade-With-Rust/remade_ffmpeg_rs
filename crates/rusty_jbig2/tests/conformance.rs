//! Every third-party stream in `tests/data` decodes to its reference image
//! (see `tests/data/README.md` for where streams and references come from).

mod common;

use common::{digest, manifest, read};
use rusty_jbig2::{decode_jbig2, decode_jbig2_with, DecodeOptions, Jbig2File};

#[test]
fn streams_match_their_references() {
    let mut checked = 0;
    for e in manifest() {
        let data = e.data();
        for (lenient, want) in [(false, &e.strict), (true, &e.lenient)] {
            // Damaged streams that only end at `max_work` are timed elsewhere.
            if want == "-" || (e.name.contains("malformed") && want == "ERR") {
                continue;
            }
            let opts = DecodeOptions::new().with_lenient(lenient);
            let got = Jbig2File::parse_with(&data, opts).and_then(|f| f.decode_page(e.page));
            match (got, want.as_str()) {
                (Err(_), "ERR") => {}
                (Ok(b), w) if digest(&b) == w => {
                    let last = b.width.div_ceil(8) as usize;
                    assert_eq!(b.stride, last);
                    assert_eq!(b.data.len(), b.stride * b.height as usize);
                }
                (got, w) => panic!(
                    "{} page {} lenient {lenient}: want {w}, got {:?}",
                    e.name,
                    e.page,
                    got.map(|b| digest(&b))
                ),
            }
            checked += 1;
        }
    }
    assert!(checked > 500, "{checked}");
}

/// A sequential file without its header is an embedded stream.
#[test]
fn embedded_organisation_gives_the_same_pages() {
    let mut n = 0;
    for e in manifest() {
        let data = e.data();
        // Sequential, page count known: a 13-byte header.
        if e.page != 0 || e.strict == "ERR" || data[8] & 3 != 1 || e.name.contains("malformed") {
            continue;
        }
        let b = decode_jbig2(&data[13..], None).unwrap_or_else(|x| panic!("{}: {x}", e.name));
        assert_eq!(digest(&b), e.strict, "{}", e.name);
        // The file itself is accepted too, and so is a trailing end of line.
        assert_eq!(decode_jbig2(&data, None).unwrap(), b);
        let mut eol = data[13..].to_vec();
        eol.extend_from_slice(b"\r\n");
        assert_eq!(decode_jbig2(&eol, None).unwrap(), b, "{}", e.name);
        n += 1;
    }
    assert!(n > 250, "{n}");
}

/// PDF-style streams written by jbig2enc: three pages sharing a globals
/// stream. The digests are of jbig2dec's output for the same streams.
#[test]
fn pdf_pages_with_shared_globals() {
    let globals = read("pdf/globals.jb2");
    let want = include_str!("data/pdf/digests.txt");
    for (i, want) in want.split_whitespace().take(3).enumerate() {
        let page = read(&format!("pdf/page{i}.jb2"));
        let b = decode_jbig2(&page, Some(&globals)).unwrap();
        assert_eq!((b.width, b.height), (2479, 3508));
        assert_eq!(digest(&b), want, "page {i}");
        // Without the globals the symbols are missing.
        assert!(decode_jbig2(&page, None).is_err());
        let lenient = DecodeOptions::new().with_lenient(true);
        let blank = decode_jbig2_with(&page, None, &lenient).unwrap();
        assert!(blank.data.iter().all(|&b| b == 0));
    }
    let b = decode_jbig2(&read("pdf/p1-generic.jbig2"), None).unwrap();
    assert_eq!(digest(&b), want.split_whitespace().nth(3).unwrap());
}
