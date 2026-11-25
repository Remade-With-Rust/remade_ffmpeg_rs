//! Matroska / WebM `Chapters`, read and written as FFmpeg's matroskadec and
//! matroskaenc do: one flat edition, times in nanoseconds, the title as the
//! chapter's `ChapString`, and its other metadata as `Tags` that target the
//! chapter's UID.
//!
//! Read: the default edition (the first when none is marked), every
//! `ChapterAtom` directly in it, in stored order. Nested chapters are not
//! listed, as in FFmpeg.

use rff_core::Rational;
use rff_format::chapter::{in_time_order, MAX_CHAPTERS};
use rff_format::Chapter;

use crate::mux::{put_elem, put_string, put_uint};
use crate::{children, uint_of, ID_CHAPTERS};

const ID_EDITION_ENTRY: u32 = 0x45B9;
const ID_EDITION_FLAG_DEFAULT: u32 = 0x45DB;
const ID_CHAPTER_ATOM: u32 = 0xB6;
const ID_CHAPTER_UID: u32 = 0x73C4;
const ID_CHAPTER_TIME_START: u32 = 0x91;
const ID_CHAPTER_TIME_END: u32 = 0x92;
const ID_CHAPTER_DISPLAY: u32 = 0x80;
const ID_CHAP_STRING: u32 = 0x85;
const ID_CHAP_LANGUAGE: u32 = 0x437C;

/// Nanoseconds, the unit of chapter times.
const NS: Rational = Rational {
    num: 1,
    den: 1_000_000_000,
};

/// The chapters of a `Chapters` body, and which have no recorded end.
pub(crate) fn read(body: &[u8]) -> (Vec<Chapter>, Vec<bool>) {
    let editions = || {
        children(body)
            .filter(|&(id, _)| id == ID_EDITION_ENTRY)
            .map(|(_, e)| e)
    };
    let is_default =
        |e: &&[u8]| children(e).any(|(id, b)| id == ID_EDITION_FLAG_DEFAULT && uint_of(b) == 1);
    let (mut list, mut open) = (Vec::new(), Vec::new());
    let Some(edition) = editions().find(is_default).or_else(|| editions().next()) else {
        return (list, open);
    };
    for (id, atom) in children(edition) {
        if id != ID_CHAPTER_ATOM || list.len() >= MAX_CHAPTERS {
            continue;
        }
        let (mut uid, mut start, mut end, mut title) = (0, None, None, None);
        for (id, b) in children(atom) {
            match id {
                ID_CHAPTER_UID => uid = uint_of(b),
                ID_CHAPTER_TIME_START => start = i64::try_from(uint_of(b)).ok(),
                ID_CHAPTER_TIME_END => end = i64::try_from(uint_of(b)).ok(),
                // With several displays (languages), the last one, as FFmpeg.
                ID_CHAPTER_DISPLAY => {
                    let text = children(b).find(|&(id, _)| id == ID_CHAP_STRING);
                    title = text.map(|(_, s)| crate::props::text(s)).or(title);
                }
                _ => {}
            }
        }
        let Some(start) = start else { continue };
        let mut c = Chapter::new(uid, NS, start, end.unwrap_or(start).max(start));
        if let Some(t) = title {
            c = c.with_title(t);
        }
        list.push(c);
        open.push(end.is_none());
    }
    (list, open)
}

/// `chapters` in the order and with the UIDs they are written with: by start
/// (see [`in_time_order`]), their ids where those are
/// valid (unique and not 0), moved up by one when one is 0, else 1, 2, ...
pub(crate) fn to_write(chapters: &[Chapter]) -> Vec<Chapter> {
    let mut out = in_time_order(chapters);
    let mut ids: Vec<u64> = out.iter().map(|c| c.id).collect();
    ids.sort_unstable();
    let unique = ids.windows(2).all(|w| w[0] != w[1]);
    let shift = u64::from(ids.first() == Some(&0));
    let fits = ids.last().is_none_or(|m| m.checked_add(shift).is_some());
    for (i, c) in out.iter_mut().enumerate() {
        c.id = if unique && fits {
            c.id + shift
        } else {
            i as u64 + 1
        };
    }
    out
}

/// The `Chapters` element (empty for no chapters). `chapters` come from
/// [`to_write`].
pub(crate) fn element(chapters: &[Chapter]) -> Vec<u8> {
    if chapters.is_empty() {
        return Vec::new();
    }
    const SCALE: u32 = 1_000_000_000;
    let mut edition = Vec::new();
    put_uint(&mut edition, ID_EDITION_FLAG_DEFAULT, 1);
    for c in chapters {
        let mut atom = Vec::new();
        put_uint(&mut atom, ID_CHAPTER_UID, c.id);
        let start = c.start_in(SCALE).max(0);
        put_uint(&mut atom, ID_CHAPTER_TIME_START, start as u64);
        put_uint(
            &mut atom,
            ID_CHAPTER_TIME_END,
            c.end_in(SCALE).max(start) as u64,
        );
        if let Some(title) = c.title() {
            let mut display = Vec::new();
            put_string(&mut display, ID_CHAP_STRING, title);
            put_string(&mut display, ID_CHAP_LANGUAGE, "und");
            put_elem(&mut atom, ID_CHAPTER_DISPLAY, &display);
        }
        put_elem(&mut edition, ID_CHAPTER_ATOM, &atom);
    }
    let mut body = Vec::new();
    put_elem(&mut body, ID_EDITION_ENTRY, &edition);
    let mut out = Vec::new();
    put_elem(&mut out, ID_CHAPTERS, &body);
    out
}
