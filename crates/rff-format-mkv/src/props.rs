//! Track and file properties beyond the codec (#114): `Name`, `Language`,
//! the track flags, interlacing, stereo mode, crop, display size,
//! `Projection` (rotation), `Info` dates and apps, and global `Tags`, read
//! into [`Stream`] / the metadata dictionary and written back from them.
//!
//! Mappings onto rff's container-neutral fields:
//! * `PixelCrop*` ↔ [`Stream::clean_aperture`] (MP4 `clap`): the crop is a
//!   clean aperture whose centre is offset by half the crop difference.
//! * `DisplayWidth`/`DisplayHeight` (unit pixels or aspect ratio) ↔
//!   [`Stream::pixel_aspect`] (MP4 `pasp`), as matroskadec/matroskaenc map
//!   them to the sample aspect ratio.
//! * `ProjectionType` 0 (rectangular) with only `ProjectionPoseRoll` ↔ a pure
//!   rotation in [`Stream::display_matrix`] (MP4 `tkhd`). The roll is
//!   counter-clockwise (RFC 9559), so a picture shown a quarter turn
//!   clockwise has roll -90.
//! * `FlagInterlaced` + `FieldOrder` ↔ [`Stream::field_order`] (which uses
//!   the FieldOrder codes, shared with QuickTime `fiel`).
//! * `Language` ("und" = unknown; absent means "eng", the element's default)
//!   and `LanguageBCP47` (preferred when present) ↔ [`Stream::language`].
//! * `Info` `Title` / `DateUTC` / `WritingApp` and global `Tags`
//!   (`SimpleTag` names, lower-cased) ↔ metadata `title` / `creation_time` /
//!   `encoder` / the other keys (upper-cased when written, as matroskaenc).

use rff_core::{CleanAperture, Dictionary};
use rff_format::Stream;

use crate::{children, float_of, uint_of};

pub(crate) const ID_NAME: u32 = 0x536E;
pub(crate) const ID_LANGUAGE: u32 = 0x22_B59C;
pub(crate) const ID_LANGUAGE_BCP47: u32 = 0x22_B59D;
pub(crate) const ID_FLAG_ENABLED: u32 = 0xB9;
pub(crate) const ID_FLAG_DEFAULT: u32 = 0x88;
pub(crate) const ID_FLAG_FORCED: u32 = 0x55AA;
pub(crate) const ID_FLAG_HEARING_IMPAIRED: u32 = 0x55AB;
pub(crate) const ID_FLAG_VISUAL_IMPAIRED: u32 = 0x55AC;
pub(crate) const ID_FLAG_ORIGINAL: u32 = 0x55AE;
pub(crate) const ID_FLAG_COMMENTARY: u32 = 0x55AF;
pub(crate) const ID_FLAG_INTERLACED: u32 = 0x9A;
pub(crate) const ID_FIELD_ORDER: u32 = 0x9D;
pub(crate) const ID_STEREO_MODE: u32 = 0x53B8;
pub(crate) const ID_PIXEL_CROP_BOTTOM: u32 = 0x54AA;
pub(crate) const ID_PIXEL_CROP_TOP: u32 = 0x54BB;
pub(crate) const ID_PIXEL_CROP_LEFT: u32 = 0x54CC;
pub(crate) const ID_PIXEL_CROP_RIGHT: u32 = 0x54DD;
pub(crate) const ID_DISPLAY_WIDTH: u32 = 0x54B0;
pub(crate) const ID_DISPLAY_HEIGHT: u32 = 0x54BA;
pub(crate) const ID_DISPLAY_UNIT: u32 = 0x54B2;
pub(crate) const ID_PROJECTION: u32 = 0x7670;
pub(crate) const ID_PROJECTION_TYPE: u32 = 0x7671;
pub(crate) const ID_PROJECTION_POSE_YAW: u32 = 0x7673;
pub(crate) const ID_PROJECTION_POSE_PITCH: u32 = 0x7674;
pub(crate) const ID_PROJECTION_POSE_ROLL: u32 = 0x7675;
pub(crate) const ID_TITLE: u32 = 0x7BA9;
pub(crate) const ID_MUXING_APP: u32 = 0x4D80;
pub(crate) const ID_WRITING_APP: u32 = 0x5741;
pub(crate) const ID_DATE_UTC: u32 = 0x4461;
pub(crate) const ID_TAG: u32 = 0x7373;
pub(crate) const ID_TARGETS: u32 = 0x63C0;
pub(crate) const ID_TARGET_TYPE_VALUE: u32 = 0x68CA;
pub(crate) const ID_TAG_TRACK_UID: u32 = 0x63C5;
pub(crate) const ID_TAG_EDITION_UID: u32 = 0x63C9;
pub(crate) const ID_TAG_CHAPTER_UID: u32 = 0x63C4;
pub(crate) const ID_TAG_ATTACHMENT_UID: u32 = 0x63C6;
pub(crate) const ID_SIMPLE_TAG: u32 = 0x67C8;
pub(crate) const ID_TAG_NAME: u32 = 0x45A3;
pub(crate) const ID_TAG_STRING: u32 = 0x4487;

fn text(body: &[u8]) -> String {
    // EBML strings may be NUL-padded.
    let end = body.iter().position(|&b| b == 0).unwrap_or(body.len());
    String::from_utf8_lossy(&body[..end]).into_owned()
}

/// A TrackEntry child this module owns; returns whether it was one.
pub(crate) fn track_element(s: &mut Stream, lang: &mut Lang, id: u32, body: &[u8]) -> bool {
    let flag = || uint_of(body) != 0;
    match id {
        ID_NAME => s.name = Some(text(body)),
        ID_LANGUAGE => lang.iso = Some(text(body)),
        ID_LANGUAGE_BCP47 => lang.bcp47 = Some(text(body)),
        ID_FLAG_DEFAULT => s.disposition.default = Some(flag()),
        ID_FLAG_ENABLED => s.disposition.enabled = Some(flag()),
        ID_FLAG_FORCED => s.disposition.forced = flag(),
        ID_FLAG_HEARING_IMPAIRED => s.disposition.hearing_impaired = flag(),
        ID_FLAG_VISUAL_IMPAIRED => s.disposition.visual_impaired = flag(),
        ID_FLAG_ORIGINAL => s.disposition.original = flag(),
        ID_FLAG_COMMENTARY => s.disposition.commentary = flag(),
        _ => return false,
    }
    true
}

/// `Language` and `LanguageBCP47` as read, resolved once the entry ends.
#[derive(Default)]
pub(crate) struct Lang {
    iso: Option<String>,
    bcp47: Option<String>,
}

impl Lang {
    pub(crate) fn resolve(self) -> Option<String> {
        let lang = self
            .bcp47
            .filter(|l| !l.is_empty())
            .or(self.iso)
            .unwrap_or_else(|| "eng".to_string());
        (lang != "und" && !lang.is_empty()).then_some(lang)
    }
}

/// Apply the Video children this module owns (needs `s.width`/`s.height`).
pub(crate) fn apply_video(s: &mut Stream, video: &[u8]) {
    let (mut interlaced, mut field_order) = (0u64, None);
    let mut crop = [0u64; 4]; // top, bottom, left, right
    let (mut dw, mut dh, mut unit) = (None, None, 0u64);
    for (id, b) in children(video) {
        match id {
            ID_FLAG_INTERLACED => interlaced = uint_of(b),
            ID_FIELD_ORDER => field_order = Some(uint_of(b)),
            ID_STEREO_MODE => s.stereo_mode = u8::try_from(uint_of(b)).ok(),
            ID_PIXEL_CROP_TOP => crop[0] = uint_of(b),
            ID_PIXEL_CROP_BOTTOM => crop[1] = uint_of(b),
            ID_PIXEL_CROP_LEFT => crop[2] = uint_of(b),
            ID_PIXEL_CROP_RIGHT => crop[3] = uint_of(b),
            ID_DISPLAY_WIDTH => dw = Some(uint_of(b)),
            ID_DISPLAY_HEIGHT => dh = Some(uint_of(b)),
            ID_DISPLAY_UNIT => unit = uint_of(b),
            ID_PROJECTION => apply_projection(s, b),
            _ => {}
        }
    }
    s.field_order = match (interlaced, field_order) {
        (2, _) => Some(0),
        (1, Some(o)) => u8::try_from(o).ok(),
        (1, None) => Some(2),
        _ => None,
    };
    let (w, h) = (u64::from(s.width), u64::from(s.height));
    let [t, bo, l, r] = crop;
    let (cw, ch) = (
        w.saturating_sub(l.saturating_add(r)),
        h.saturating_sub(t.saturating_add(bo)),
    );
    if crop != [0; 4] && cw > 0 && ch > 0 {
        let fits = |v: u64| u32::try_from(v).ok();
        let signed = |a: u64, b: u64| i32::try_from(a as i128 - b as i128).ok();
        if let (Some(cw), Some(ch), Some(ho), Some(vo)) =
            (fits(cw), fits(ch), signed(l, r), signed(t, bo))
        {
            s.clean_aperture = Some(CleanAperture {
                width: (cw, 1),
                height: (ch, 1),
                horiz_offset: (ho, 2),
                vert_offset: (vo, 2),
            });
        }
    }
    // Display size → sample aspect ratio, of the cropped picture.
    if let (Some(dw), Some(dh)) = (dw, dh) {
        if matches!(unit, 0 | 3) && dw > 0 && dh > 0 && cw > 0 && ch > 0 {
            let (n, d) = (
                u128::from(dw) * u128::from(ch),
                u128::from(dh) * u128::from(cw),
            );
            let g = gcd(n, d);
            let (n, d) = (n / g, d / g);
            if n != d {
                if let (Ok(n), Ok(d)) = (u32::try_from(n), u32::try_from(d)) {
                    s.pixel_aspect = Some((n, d));
                }
            }
        }
    }
}

fn gcd(mut a: u128, mut b: u128) -> u128 {
    while b != 0 {
        (a, b) = (b, a % b);
    }
    a.max(1)
}

fn apply_projection(s: &mut Stream, body: &[u8]) {
    let (mut kind, mut yaw, mut pitch, mut roll) = (0u64, 0.0f64, 0.0f64, 0.0f64);
    for (id, b) in children(body) {
        match id {
            ID_PROJECTION_TYPE => kind = uint_of(b),
            ID_PROJECTION_POSE_YAW => yaw = float_of(b),
            ID_PROJECTION_POSE_PITCH => pitch = float_of(b),
            ID_PROJECTION_POSE_ROLL => roll = float_of(b),
            _ => {}
        }
    }
    if kind == 0 && yaw == 0.0 && pitch == 0.0 && roll.is_finite() && roll != 0.0 {
        s.display_matrix = Some(rff_format::rotation_matrix(-roll, s.width, s.height));
    }
}

// ---- writing ---------------------------------------------------------------

/// What a muxer appends to a TrackEntry for these properties.
pub(crate) fn track_elements(s: &Stream, out: &mut Vec<u8>) {
    use crate::mux::{put_string, put_uint};
    if let Some(name) = &s.name {
        put_string(out, ID_NAME, name);
    }
    match s.language.as_deref() {
        Some(l) if l.len() == 3 && l.bytes().all(|c| c.is_ascii_lowercase()) => {
            put_string(out, ID_LANGUAGE, l)
        }
        Some(l) => {
            put_string(out, ID_LANGUAGE, "und");
            put_string(out, ID_LANGUAGE_BCP47, l);
        }
        // Unknown: "und", not the element's default "eng".
        None => put_string(out, ID_LANGUAGE, "und"),
    }
    let d = &s.disposition;
    if let Some(v) = d.enabled {
        put_uint(out, ID_FLAG_ENABLED, u64::from(v));
    }
    if let Some(v) = d.default {
        put_uint(out, ID_FLAG_DEFAULT, u64::from(v));
    }
    for (on, id) in [
        (d.forced, ID_FLAG_FORCED),
        (d.hearing_impaired, ID_FLAG_HEARING_IMPAIRED),
        (d.visual_impaired, ID_FLAG_VISUAL_IMPAIRED),
        (d.original, ID_FLAG_ORIGINAL),
        (d.commentary, ID_FLAG_COMMENTARY),
    ] {
        if on {
            put_uint(out, id, 1);
        }
    }
}

/// What a muxer appends to a Video element for these properties.
pub(crate) fn video_elements(s: &Stream, out: &mut Vec<u8>) {
    use crate::mux::{put_elem, put_float, put_uint};
    if let Some(order) = s.field_order {
        if order == 0 {
            put_uint(out, ID_FLAG_INTERLACED, 2);
        } else {
            put_uint(out, ID_FLAG_INTERLACED, 1);
            put_uint(out, ID_FIELD_ORDER, u64::from(order));
        }
    }
    if let Some(mode) = s.stereo_mode {
        put_uint(out, ID_STEREO_MODE, u64::from(mode));
    }
    let (w, h) = (i64::from(s.width), i64::from(s.height));
    let (mut cw, mut ch) = (w, h);
    if let Some(c) = s.clean_aperture {
        // Back to edge crops: centre offset o = (l - r) / 2, width w - l - r.
        let frac = |(n, d): (u32, u32)| {
            if d == 0 {
                0
            } else {
                i64::from(n) / i64::from(d)
            }
        };
        let sfrac = |(n, d): (i32, u32)| {
            if d == 0 {
                0
            } else {
                i64::from(n) * 2 / i64::from(d)
            }
        };
        let (aw, ah) = (frac(c.width), frac(c.height));
        let (dx, dy) = (sfrac(c.horiz_offset), sfrac(c.vert_offset)); // l - r, t - b
        let (lr, tb) = (w - aw, h - ah); // l + r, t + b
        let (l, r) = ((lr + dx) / 2, (lr - dx) / 2);
        let (t, b) = ((tb + dy) / 2, (tb - dy) / 2);
        if aw > 0 && ah > 0 && [l, r, t, b].iter().all(|&v| v >= 0) {
            for (v, id) in [
                (b, ID_PIXEL_CROP_BOTTOM),
                (t, ID_PIXEL_CROP_TOP),
                (l, ID_PIXEL_CROP_LEFT),
                (r, ID_PIXEL_CROP_RIGHT),
            ] {
                if v > 0 {
                    put_uint(out, id, v as u64);
                }
            }
            (cw, ch) = (aw, ah);
        }
    }
    if let Some((hs, vs)) = s.pixel_aspect.filter(|&(a, b)| a != b && a > 0 && b > 0) {
        // DisplayWidth/Height in pixels of the cropped picture: stretch the
        // width by the aspect, as matroskaenc does.
        let dw = (cw as i128 * i128::from(hs) + i128::from(vs) / 2) / i128::from(vs);
        if dw > 0 && ch > 0 {
            put_uint(out, ID_DISPLAY_WIDTH, dw as u64);
            put_uint(out, ID_DISPLAY_HEIGHT, ch as u64);
        }
    }
    if let Some(deg) = s.rotation().filter(|&d| d != 0.0) {
        // A clockwise rotation as a counter-clockwise roll in [-180, 180].
        let mut roll = -deg;
        if roll < -180.0 {
            roll += 360.0;
        }
        let mut p = Vec::new();
        put_uint(&mut p, ID_PROJECTION_TYPE, 0);
        put_float(&mut p, ID_PROJECTION_POSE_ROLL, roll);
        put_elem(out, ID_PROJECTION, &p);
    }
}

/// An Info child this module owns, read into `md`; returns whether it was.
pub(crate) fn info_element(md: &mut Dictionary, id: u32, body: &[u8]) -> bool {
    match id {
        ID_TITLE => md.set("title", text(body)),
        ID_WRITING_APP => md.set("encoder", text(body)),
        ID_DATE_UTC if body.len() == 8 => {
            let ns = i64::from_be_bytes(body.try_into().unwrap_or([0; 8]));
            let us = ns.div_euclid(1000) + rff_format::time::MATROSKA_EPOCH_OFFSET * 1_000_000;
            md.set("creation_time", rff_format::time::format_iso8601(us));
        }
        ID_MUXING_APP => {}
        _ => return false,
    }
    true
}

/// The Info children for `md` (Title, DateUTC, MuxingApp, WritingApp).
pub(crate) fn info_elements(md: &Dictionary, out: &mut Vec<u8>) {
    use crate::mux::put_string;
    if let Some(title) = md.get("title") {
        put_string(out, ID_TITLE, title);
    }
    if let Some(us) = md
        .get("creation_time")
        .and_then(rff_format::time::parse_iso8601)
    {
        let ns = (i128::from(us) - i128::from(rff_format::time::MATROSKA_EPOCH_OFFSET) * 1_000_000)
            * 1000;
        if let Ok(ns) = i64::try_from(ns) {
            let mut body = Vec::new();
            crate::mux::put_elem(&mut body, ID_DATE_UTC, &ns.to_be_bytes());
            out.extend_from_slice(&body);
        }
    }
    let app = concat!("rff ", env!("CARGO_PKG_VERSION"));
    put_string(out, ID_MUXING_APP, app);
    put_string(out, ID_WRITING_APP, md.get("encoder").unwrap_or(app));
}

/// Keys carried by Info rather than Tags.
fn in_info(key: &str) -> bool {
    matches!(key, "title" | "creation_time" | "encoder")
}

/// A `Tags` element holding `md`'s other keys as global SimpleTags (empty
/// when there are none).
pub(crate) fn tags_element(md: &Dictionary) -> Vec<u8> {
    use crate::mux::{put_elem, put_string, put_uint};
    let mut tag = Vec::new();
    let mut targets = Vec::new();
    put_uint(&mut targets, ID_TARGET_TYPE_VALUE, 50);
    put_elem(&mut tag, ID_TARGETS, &targets);
    let mut any = false;
    for (k, v) in md.iter() {
        if in_info(k) || k.is_empty() {
            continue;
        }
        let mut st = Vec::new();
        put_string(&mut st, ID_TAG_NAME, &k.to_ascii_uppercase());
        put_string(&mut st, ID_TAG_STRING, v);
        put_elem(&mut tag, ID_SIMPLE_TAG, &st);
        any = true;
    }
    if !any {
        return Vec::new();
    }
    let mut tags = Vec::new();
    put_elem(&mut tags, ID_TAG, &tag);
    let mut out = Vec::new();
    put_elem(&mut out, crate::ID_TAGS, &tags);
    out
}

/// Read a `Tags` body's global tags (no track/edition/chapter/attachment
/// target) into `md`, names lower-cased. Info keys already read win.
pub(crate) fn read_tags(md: &mut Dictionary, body: &[u8]) {
    for (id, tag) in children(body) {
        if id != ID_TAG {
            continue;
        }
        let targeted = children(tag).any(|(id, t)| {
            id == ID_TARGETS
                && children(t).any(|(id, _)| {
                    matches!(
                        id,
                        ID_TAG_TRACK_UID
                            | ID_TAG_EDITION_UID
                            | ID_TAG_CHAPTER_UID
                            | ID_TAG_ATTACHMENT_UID
                    )
                })
        });
        if targeted {
            continue;
        }
        for (id, st) in children(tag) {
            if id != ID_SIMPLE_TAG {
                continue;
            }
            let (mut name, mut value) = (None, None);
            for (id, b) in children(st) {
                match id {
                    ID_TAG_NAME => name = Some(text(b).to_ascii_lowercase()),
                    ID_TAG_STRING => value = Some(text(b)),
                    _ => {}
                }
            }
            if let (Some(n), Some(v)) = (name, value) {
                if !n.is_empty() && md.get(&n).is_none() {
                    md.set(n, v);
                }
            }
        }
    }
}
