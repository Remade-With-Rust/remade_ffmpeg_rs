//! Movie metadata: `set_metadata` written as `udta`, and read back for
//! [`Demuxer::metadata`](rff_format::Demuxer::metadata) (#114).
//!
//! The `mp4` muxer writes iTunes-style `udta/meta/ilst` (handler `mdir`,
//! each item a `data` box holding UTF-8), which is what FFmpeg's movenc
//! writes for MP4 and what Apple, VLC and FFmpeg read; the `mov` muxer writes
//! QuickTime text atoms (`udta/©nam`, a 16-bit length, a language code and
//! the text), as movenc does in MOV mode. The demuxer reads both, plus
//! `meta/keys` (`mdta`) items and QuickTime `moov/meta`, and gives
//! `creation_time` from `mvhd`. Keys are FFmpeg's names; keys with no atom
//! of their own are not written (as FFmpeg without `use_metadata_tags`).

use rff_core::Dictionary;

use super::{be16, be32, be64, bx, child_boxes, fbx, find, pu16, pu32};

/// FFmpeg metadata key ↔ (`ilst` atom, QuickTime `udta` atom).
const KEYS: &[(&str, &[u8; 4], &[u8; 4])] = &[
    ("title", b"\xA9nam", b"\xA9nam"),
    ("artist", b"\xA9ART", b"\xA9ART"),
    ("album_artist", b"aART", b"aART"),
    ("album", b"\xA9alb", b"\xA9alb"),
    ("comment", b"\xA9cmt", b"\xA9cmt"),
    ("composer", b"\xA9wrt", b"\xA9wrt"),
    ("date", b"\xA9day", b"\xA9day"),
    ("genre", b"\xA9gen", b"\xA9gen"),
    ("encoder", b"\xA9too", b"\xA9too"),
    ("description", b"desc", b"\xA9des"),
    ("copyright", b"cprt", b"\xA9cpy"),
    ("grouping", b"\xA9grp", b"\xA9grp"),
    ("lyrics", b"\xA9lyr", b"\xA9lyr"),
    ("synopsis", b"ldes", b"ldes"),
];

/// The `udta` box for `metadata`, with the box `extra` (empty when no key has
/// an atom and `extra` is empty).
pub(crate) fn build_udta(metadata: &Dictionary, qt: bool, extra: &[u8]) -> Vec<u8> {
    let mut items = Vec::new();
    for &(key, ilst_atom, qt_atom) in KEYS {
        let Some(value) = metadata.get(key) else {
            continue;
        };
        if qt {
            // QuickTime text atom: length, language (0 = English), text.
            let text = &value.as_bytes()[..value.len().min(0xFFFF)];
            let mut b = Vec::new();
            pu16(&mut b, text.len() as u16);
            pu16(&mut b, 0);
            b.extend_from_slice(text);
            items.extend_from_slice(&bx(qt_atom, &b));
        } else {
            let mut data = Vec::new();
            pu32(&mut data, 1); // well-known type 1: UTF-8
            pu32(&mut data, 0); // locale
            data.extend_from_slice(value.as_bytes());
            items.extend_from_slice(&bx(ilst_atom, &bx(b"data", &data)));
        }
    }
    if qt || items.is_empty() {
        items.extend_from_slice(extra);
        return if items.is_empty() {
            Vec::new()
        } else {
            bx(b"udta", &items)
        };
    }
    // meta (FullBox) / hdlr 'mdir' 'appl' / ilst
    let mut hdlr = Vec::new();
    pu32(&mut hdlr, 0); // pre_defined
    hdlr.extend_from_slice(b"mdir");
    hdlr.extend_from_slice(b"appl");
    pu32(&mut hdlr, 0);
    pu32(&mut hdlr, 0);
    hdlr.push(0); // empty name
    let meta = fbx(
        b"meta",
        0,
        0,
        &[fbx(b"hdlr", 0, 0, &hdlr), bx(b"ilst", &items)].concat(),
    );
    bx(b"udta", &[&meta[..], extra].concat())
}

/// The text of an `ilst` item: its first `data` box of a text type.
fn item_text(item: &[u8]) -> Option<String> {
    let data = find(&child_boxes(item), b"data")?;
    let kind = be32(data, 0) & 0x00FF_FFFF;
    // 1 UTF-8, 2 UTF-16 BE (rare), 0 implicit (some writers): take UTF-8.
    match kind {
        0 | 1 => Some(String::from_utf8_lossy(data.get(8..)?).into_owned()),
        2 => {
            let units: Vec<u16> = data
                .get(8..)?
                .chunks_exact(2)
                .map(|c| u16::from_be_bytes([c[0], c[1]]))
                .collect();
            Some(String::from_utf16_lossy(&units))
        }
        _ => None,
    }
}

/// The key FFmpeg uses for an atom (`ilst` or QuickTime text atom).
fn key_of(atom: &[u8]) -> Option<&'static str> {
    KEYS.iter()
        .find(|(_, a, q)| &a[..] == atom || &q[..] == atom)
        .map(|(k, _, _)| *k)
}

/// Read a `meta` box (ISO FullBox or QuickTime plain container).
fn read_meta(meta: &[u8], out: &mut Dictionary) {
    let body = if meta.get(4..8) == Some(b"hdlr") {
        meta
    } else {
        meta.get(4..).unwrap_or(&[])
    };
    let kids = child_boxes(body);
    let handler = find(&kids, b"hdlr").and_then(|h| h.get(8..12));
    let keys: Vec<String> = match (handler, find(&kids, b"keys")) {
        (Some(b"mdta"), Some(k)) => {
            let n = be32(k, 4) as usize;
            let mut at = 8;
            let mut names = Vec::new();
            for _ in 0..n.min(k.len() / 8) {
                let size = be32(k, at) as usize;
                if size < 8 || at + size > k.len() {
                    break;
                }
                names.push(String::from_utf8_lossy(&k[at + 8..at + size]).into_owned());
                at += size;
            }
            names
        }
        _ => Vec::new(),
    };
    let Some(ilst) = find(&kids, b"ilst") else {
        return;
    };
    for (atom, item) in child_boxes(ilst) {
        let Some(text) = item_text(item) else {
            continue;
        };
        if handler == Some(b"mdta") {
            let index = u32::from_be_bytes(atom) as usize;
            if let Some(name) = index.checked_sub(1).and_then(|i| keys.get(i)) {
                out.set(name.clone(), text);
            }
        } else if let Some(key) = key_of(&atom) {
            out.set(key, text);
        }
    }
}

/// The movie metadata of a `moov`'s children.
pub(crate) fn read_movie_metadata(moov: &[([u8; 4], &[u8])]) -> Dictionary {
    let mut out = Dictionary::new();
    if let Some(mvhd) = find(moov, b"mvhd") {
        let created = if mvhd.first() == Some(&1) {
            be64(mvhd, 4)
        } else {
            u64::from(be32(mvhd, 4))
        };
        if created > 0 {
            if let Ok(secs) = i64::try_from(created) {
                let unix = secs - rff_format::time::MP4_EPOCH_OFFSET;
                out.set(
                    "creation_time",
                    rff_format::time::format_iso8601(unix.saturating_mul(1_000_000)),
                );
            }
        }
    }
    if let Some(meta) = find(moov, b"meta") {
        read_meta(meta, &mut out);
    }
    if let Some(udta) = find(moov, b"udta") {
        for (atom, p) in child_boxes(udta) {
            if &atom == b"meta" {
                read_meta(p, &mut out);
                continue;
            }
            let Some(key) = key_of(&atom) else { continue };
            if p.get(4..8) == Some(b"data") {
                if let Some(text) = item_text(p) {
                    out.set(key, text);
                }
                continue;
            }
            let len = be16(p, 0) as usize;
            if let Some(text) = p.get(4..4 + len) {
                out.set(key, String::from_utf8_lossy(text).into_owned());
            }
        }
    }
    out
}
