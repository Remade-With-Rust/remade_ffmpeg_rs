//! Chapters in MP4/MOV, both ways FFmpeg reads and writes them: a QuickTime
//! chapter track (a disabled `text` track the media tracks refer to with
//! `tref/chap`, one sample per chapter: a 16-bit length, the title, and an
//! `encd` atom naming UTF-8) and Nero's `udta/chpl` (starts in 100 ns, titles
//! up to 255 bytes, at most 255 chapters).
//!
//! When a file has both, the chapter track is what is read: it alone records
//! where the last chapter ends and holds any number of chapters.

use rff_core::Rational;
use rff_format::chapter::MAX_CHAPTERS;
use rff_format::Chapter;

use super::{be16, be64, build_hdlr, bx, child_boxes, elst_box, fbx, find, pu16, pu32};

/// `chpl`'s unit: 100 ns.
const CHPL_SCALE: u32 = 10_000_000;

/// The most title bytes read from one chapter track: chapter samples may all
/// point at the same bytes of the file, so their total is bounded here.
pub(crate) const MAX_TITLE_BYTES: u64 = 16 << 20;

/// Text up to the first NUL, as UTF-8.
fn utf8(b: &[u8]) -> String {
    let end = b.iter().position(|&c| c == 0).unwrap_or(b.len());
    String::from_utf8_lossy(&b[..end]).into_owned()
}

fn utf16(b: &[u8], unit: fn([u8; 2]) -> u16) -> String {
    let units: Vec<u16> = b
        .chunks_exact(2)
        .map(|c| unit([c[0], c[1]]))
        .take_while(|&u| u != 0)
        .collect();
    String::from_utf16_lossy(&units)
}

/// The title in a chapter track's text sample: a 16-bit byte count and the
/// text, UTF-8 unless a byte order mark says UTF-16. `None` when the sample
/// is shorter than it says, which is not a chapter.
pub(crate) fn sample_title(sample: &[u8]) -> Option<String> {
    let len = be16(sample.get(..2)?, 0) as usize;
    let text = sample.get(2..2 + len)?;
    Some(match text.get(..2) {
        Some([0xFE, 0xFF]) => utf16(&text[2..], u16::from_be_bytes),
        Some([0xFF, 0xFE]) => utf16(&text[2..], u16::from_le_bytes),
        _ => utf8(text),
    })
}

/// The chapters of `udta/chpl`, their ends still open.
pub(crate) fn read_chpl(moov: &[([u8; 4], &[u8])]) -> Vec<Chapter> {
    let mut out = Vec::new();
    let Some(chpl) = find(moov, b"udta").and_then(|u| find(&child_boxes(u), b"chpl")) else {
        return out;
    };
    // FullBox; a 32-bit field in version 1; an 8-bit count.
    let mut at = if chpl.first() == Some(&0) { 4 } else { 8 };
    let Some(&count) = chpl.get(at) else {
        return out;
    };
    at += 1;
    for id in 0..u64::from(count) {
        let Some(&len) = chpl.get(at + 8) else { break };
        let Some(title) = chpl.get(at + 9..at + 9 + usize::from(len)) else {
            break;
        };
        if let Ok(start) = i64::try_from(be64(chpl, at)) {
            let tb = Rational::new(1, CHPL_SCALE as i32);
            out.push(Chapter::new(id, tb, start, start).with_title(utf8(title)));
        }
        at += 9 + usize::from(len);
    }
    out
}

/// One chapter sample to read: where its text is and when it is shown.
pub(crate) struct TextSample {
    pub offset: u64,
    pub size: u64,
    pub start: i64,
    /// The next sample's time, or where this one's duration ends.
    pub end: i64,
}

/// Read a chapter track's samples as chapters. `read` fetches at most the
/// given number of bytes at an offset. Returns the chapters and which of them
/// have no usable end.
pub(crate) fn read_track(
    samples: impl Iterator<Item = TextSample>,
    time_base: Rational,
    mut read: impl FnMut(u64, u64) -> Option<Vec<u8>>,
) -> (Vec<Chapter>, Vec<bool>) {
    let (mut chapters, mut open) = (Vec::new(), Vec::new());
    let mut budget = MAX_TITLE_BYTES;
    for (id, s) in samples.take(MAX_CHAPTERS).enumerate() {
        // The length field says how much of the sample is text (`encd` and
        // other atoms follow); it cannot say more than 64 KiB.
        let want = s.size.min(2 + 0xFFFF);
        if want < 2 || want > budget {
            continue;
        }
        budget -= want;
        let Some(title) = read(s.offset, want).and_then(|b| sample_title(&b)) else {
            continue;
        };
        // Shown before the presentation starts (an edit cut it away).
        if s.start < 0 && s.end <= 0 {
            continue;
        }
        let start = s.start.max(0);
        open.push(s.end < start);
        chapters
            .push(Chapter::new(id as u64, time_base, start, s.end.max(start)).with_title(title));
    }
    (chapters, open)
}

// ---------------------------------------------------------------------------
// Writing
// ---------------------------------------------------------------------------

/// `s` cut to at most `max` bytes, at a character boundary.
fn clip(s: &str, max: usize) -> &str {
    if s.len() <= max {
        return s;
    }
    let mut end = max;
    while !s.is_char_boundary(end) {
        end -= 1;
    }
    &s[..end]
}

/// Append the chapter track's samples to `blob`, back to back; their sizes.
pub(crate) fn text_samples(chapters: &[Chapter], blob: &mut Vec<u8>) -> Vec<u32> {
    let mut sizes = Vec::with_capacity(chapters.len());
    for c in chapters {
        let title = clip(c.title().unwrap_or(""), 0xFFFF);
        pu16(blob, title.len() as u16);
        blob.extend_from_slice(title.as_bytes());
        // `encd` 0x100: the text is UTF-8.
        blob.extend_from_slice(b"\0\0\0\x0Cencd\0\0\x01\0");
        sizes.push(title.len() as u32 + 14);
    }
    sizes
}

/// `udta/chpl` for `chapters`, whose times count from `origin` (100 ns units).
fn chpl_box(chapters: &[Chapter], origin: i64) -> Vec<u8> {
    let listed = &chapters[..chapters.len().min(255)];
    let mut b = Vec::new();
    pu32(&mut b, 0); // reserved
    b.push(listed.len() as u8);
    for c in listed {
        let start = c.start_in(CHPL_SCALE).saturating_sub(origin).max(0);
        b.extend_from_slice(&start.to_be_bytes());
        let title = clip(c.title().unwrap_or(""), 255);
        b.push(title.len() as u8);
        b.extend_from_slice(title.as_bytes());
    }
    fbx(b"chpl", 1, 0, &b)
}

/// The chapter box `udta` takes (empty for no chapters). `origin` is where
/// the movie starts on the chapters' timeline, `(ticks, scale)`.
pub(crate) fn nero(chapters: &[Chapter], (ticks, scale): (i64, u32)) -> Vec<u8> {
    if chapters.is_empty() {
        return Vec::new();
    }
    chpl_box(chapters, to_scale(ticks, scale, CHPL_SCALE))
}

fn to_scale(v: i64, from: u32, to: u32) -> i64 {
    super::rescale(v, i64::from(to), i64::from(from.max(1)))
}

/// Times of the chapter track in its own timescale.
struct Timing {
    timescale: u32,
    /// Where the first chapter starts.
    delay: i64,
    durations: Vec<u32>,
}

/// The chapters' starts and last end at `scale`, from `origin`; `None` when
/// a gap between starts does not fit the 32 bits of an `stts` delta.
fn timing_at(chapters: &[Chapter], (ticks, from): (i64, u32), scale: u32) -> Option<Timing> {
    let origin = to_scale(ticks, from, scale);
    let at = |t: i64| t.saturating_sub(origin).max(0);
    let starts: Vec<i64> = chapters.iter().map(|c| at(c.start_in(scale))).collect();
    let end = chapters.last().map_or(0, |c| at(c.end_in(scale)));
    let nexts = starts.iter().skip(1).copied().chain([end]);
    let durations = starts
        .iter()
        .zip(nexts)
        .map(|(&s, n)| u32::try_from((n - s).max(0)).ok())
        .collect::<Option<Vec<u32>>>()?;
    Some(Timing {
        timescale: scale,
        delay: starts.first().copied().unwrap_or(0),
        durations,
    })
}

/// Whether `c` starts and ends on whole ticks of `1 / scale` s.
fn exact(c: &Chapter, scale: u32) -> bool {
    let whole = |v: i64| {
        let den = i128::from(c.time_base.den.max(1));
        i128::from(v) * i128::from(c.time_base.num) * i128::from(scale) % den == 0
    };
    whole(c.start) && whole(c.end)
}

/// The first timescale that holds every chapter exactly: the one they share,
/// milliseconds, or microseconds; else milliseconds, rounded.
fn timing(chapters: &[Chapter], origin: (i64, u32)) -> Timing {
    let shared = chapters
        .first()
        .map(|c| c.time_base)
        .filter(|tb| tb.num == 1 && (1..=10_000_000).contains(&tb.den))
        .map(|tb| tb.den as u32);
    for scale in shared.into_iter().chain([1000, 1_000_000]) {
        if chapters.iter().all(|c| exact(c, scale)) {
            if let Some(t) = timing_at(chapters, origin, scale) {
                return t;
            }
        }
    }
    timing_at(chapters, origin, 1000).unwrap_or_else(|| Timing {
        // A gap of over 49 days: keep the order, clamp the gap.
        timescale: 1000,
        delay: 0,
        durations: vec![u32::MAX; chapters.len()],
    })
}

/// The chapter track: handler `text`, disabled (players show it as chapters,
/// not as captions), its samples one chunk at `offset`.
pub(crate) fn build_track(
    chapters: &[Chapter],
    sizes: &[u32],
    (track_id, offset): (u32, u64),
    origin: (i64, u32),
    movie_timescale: u32,
    qt: bool,
) -> Vec<u8> {
    let t = timing(chapters, origin);
    let media: u64 = t.durations.iter().map(|&d| u64::from(d)).sum();
    let in_movie = |v: i64| to_scale(v, t.timescale, movie_timescale).max(0);
    let media_i = i64::try_from(media).unwrap_or(i64::MAX);
    let delay = in_movie(t.delay);
    // A first chapter after the start of the movie sits behind an empty edit.
    // The edit on the media reaches past the last sample's start even when
    // that chapter has no length, so no reader takes it for cut away.
    let edits = if t.delay > 0 {
        elst_box(&[
            (delay, -1, 0x0001_0000),
            (in_movie(media_i).saturating_add(1), 0, 0x0001_0000),
        ])
    } else {
        Vec::new()
    };
    let presented = delay.saturating_add(in_movie(media_i)) as u64;

    let mut tk = Vec::new();
    let wide = presented > u64::from(u32::MAX);
    if wide {
        tk.extend_from_slice(&[0u8; 16]);
        pu32(&mut tk, track_id);
        pu32(&mut tk, 0);
        tk.extend_from_slice(&presented.to_be_bytes());
    } else {
        pu32(&mut tk, 0);
        pu32(&mut tk, 0);
        pu32(&mut tk, track_id);
        pu32(&mut tk, 0);
        pu32(&mut tk, presented as u32);
    }
    tk.extend_from_slice(&[0u8; 16]); // reserved, layer, group, volume, reserved
    super::identity_matrix(&mut tk);
    pu32(&mut tk, 0);
    pu32(&mut tk, 0);
    // In the movie, not enabled.
    let tkhd = fbx(b"tkhd", u8::from(wide), 2, &tk);

    let mut md = Vec::new();
    let wide = media > u64::from(u32::MAX);
    if wide {
        md.extend_from_slice(&[0u8; 16]);
        pu32(&mut md, t.timescale);
        md.extend_from_slice(&media.to_be_bytes());
    } else {
        pu32(&mut md, 0);
        pu32(&mut md, 0);
        pu32(&mut md, t.timescale);
        pu32(&mut md, media as u32);
    }
    pu16(&mut md, 0x55C4); // 'und'
    pu16(&mut md, 0);
    let mdhd = fbx(b"mdhd", u8::from(wide), 0, &md);
    let hdlr = build_hdlr(b"text", "SubtitleHandler", qt);

    let dinf = bx(
        b"dinf",
        &fbx(
            b"dref",
            0,
            0,
            &[&1u32.to_be_bytes()[..], &fbx(b"url ", 0, 1, &[])].concat(),
        ),
    );
    // A QuickTime `text` sample entry, all defaults, with the font table it
    // must have (one unnamed font), as movenc writes it.
    let mut entry = vec![0, 0, 0, 0, 0, 0, 0, 1];
    entry.extend_from_slice(&[0, 0, 0, 1]); // display flags
    entry.extend_from_slice(&[0; 2 + 4 + 8 + 4]); // justification, colour, box, characters
    entry.extend_from_slice(&[0, 1]); // font id
    entry.extend_from_slice(&[0; 2 + 4]); // style, size, colour
    entry.extend_from_slice(b"\0\0\0\x0Dftab\0\x01\0\x01\0");
    let words = |v: &[u32]| -> Vec<u8> { v.iter().flat_map(|x| x.to_be_bytes()).collect() };
    let stsd = fbx(b"stsd", 0, 0, &[words(&[1]), bx(b"text", &entry)].concat());
    let mut runs: Vec<(u32, u32)> = Vec::new();
    for &d in &t.durations {
        match runs.last_mut() {
            Some((n, v)) if *v == d => *n += 1,
            _ => runs.push((1, d)),
        }
    }
    let mut b = words(&[runs.len() as u32]);
    for (n, v) in runs {
        b.extend(words(&[n, v]));
    }
    let stts = fbx(b"stts", 0, 0, &b);
    let n = sizes.len() as u32;
    let stsc = fbx(b"stsc", 0, 0, &words(&[1, 1, n, 1]));
    let stsz = fbx(b"stsz", 0, 0, &[words(&[0, n]), words(sizes)].concat());
    let stco = match u32::try_from(offset) {
        Ok(o) => fbx(b"stco", 0, 0, &words(&[1, o])),
        Err(_) => fbx(
            b"co64",
            0,
            0,
            &[words(&[1]), offset.to_be_bytes().to_vec()].concat(),
        ),
    };
    let stbl = bx(b"stbl", &[stsd, stts, stsc, stsz, stco].concat());

    let mut gmin = Vec::new();
    pu16(&mut gmin, 0x40);
    for _ in 0..3 {
        pu16(&mut gmin, 0x8000);
    }
    pu32(&mut gmin, 0);
    // The text media header: an identity matrix in the odd shape QuickTime
    // writes it.
    let mut text = vec![0, 1];
    text.extend_from_slice(&[0; 14]);
    text.extend_from_slice(&[0, 1]);
    text.extend_from_slice(&[0; 14]);
    text.extend_from_slice(&[0x40, 0, 0, 0]);
    let gmhd = bx(
        b"gmhd",
        &[fbx(b"gmin", 0, 0, &gmin), bx(b"text", &text)].concat(),
    );
    let minf = bx(b"minf", &[gmhd, dinf, stbl].concat());
    let mdia = bx(b"mdia", &[mdhd, hdlr, minf].concat());
    bx(b"trak", &[tkhd, edits, mdia].concat())
}
