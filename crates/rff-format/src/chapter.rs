//! Chapters (FFmpeg's `AVChapter`): a titled span of the presentation.
//!
//! A demuxer gives them as [`Demuxer::chapters`](crate::Demuxer::chapters) (a
//! QuickTime chapter text track or Nero `chpl` in MP4/MOV, `Chapters` in
//! Matroska/WebM); a muxer takes them with
//! [`Muxer::set_chapters`](crate::Muxer::set_chapters).

use rff_core::{Dictionary, Rational};

/// The most chapters a demuxer reads from one file: far above any real one,
/// it bounds what a hostile index can make a reader build.
pub const MAX_CHAPTERS: usize = 1 << 16;

/// One chapter: `[start, end)` in [`time_base`](Chapter::time_base) units on
/// the presentation timeline, and its metadata (`title`, ...).
#[derive(Debug, Clone, PartialEq, Eq)]
#[non_exhaustive]
pub struct Chapter {
    /// The container's identifier: Matroska `ChapterUID`, the position in the
    /// list for MP4.
    pub id: u64,
    pub time_base: Rational,
    pub start: i64,
    /// Where the chapter ends; equal to `start` for a zero-length chapter.
    /// Where the container records only starts (Nero `chpl`), the demuxer
    /// fills in the next chapter's start, or the end of the file.
    pub end: i64,
    /// `title`, and any other tag the container attaches to the chapter.
    pub metadata: Dictionary,
}

impl Chapter {
    pub fn new(id: u64, time_base: Rational, start: i64, end: i64) -> Chapter {
        Chapter {
            id,
            time_base,
            start,
            end,
            metadata: Dictionary::new(),
        }
    }

    /// This chapter with the title `title`.
    pub fn with_title(mut self, title: impl Into<String>) -> Chapter {
        self.metadata.set("title", title);
        self
    }

    pub fn title(&self) -> Option<&str> {
        self.metadata.get("title")
    }

    /// `start` in units of `1 / scale` s, rounded to nearest.
    pub fn start_in(&self, scale: u32) -> i64 {
        rescale(self.start, self.time_base, scale)
    }

    /// `end` in units of `1 / scale` s, rounded to nearest, never before the
    /// start.
    pub fn end_in(&self, scale: u32) -> i64 {
        rescale(self.end.max(self.start), self.time_base, scale)
    }
}

/// `n / d` rounded to nearest (halves away from zero), saturating; 0 for an
/// invalid divisor.
fn div_round(n: i128, d: i128) -> i64 {
    if d <= 0 {
        return 0;
    }
    let q = if n >= 0 {
        (n + d / 2) / d
    } else {
        -((-n + d / 2) / d)
    };
    q.clamp(i128::from(i64::MIN), i128::from(i64::MAX)) as i64
}

/// `v` in `tb` units as `1 / scale` s units; 0 for an invalid time base.
fn rescale(v: i64, tb: Rational, scale: u32) -> i64 {
    if tb.num <= 0 {
        return 0;
    }
    div_round(
        i128::from(v) * i128::from(tb.num) * i128::from(scale),
        i128::from(tb.den),
    )
}

/// Give every chapter whose position in `open` is set (its end is not
/// recorded) an end, as FFmpeg's `compute_chapters_end` does: the next start
/// in time order when that is later, else the end of the file (`duration_us`
/// microseconds, when known), and never before its own start.
pub fn fill_ends(chapters: &mut [Chapter], open: &[bool], duration_us: Option<i64>) {
    let mut order: Vec<usize> = (0..chapters.len()).collect();
    order.sort_by_key(|&i| chapters[i].start_in(1_000_000_000));
    for (k, &i) in order.iter().enumerate() {
        if !open.get(i).copied().unwrap_or(false) {
            continue;
        }
        let (tb, start) = (chapters[i].time_base, chapters[i].start);
        let to_tb = |v: i64, from: Rational| -> i64 {
            div_round(
                i128::from(v) * i128::from(from.num.max(0)) * i128::from(tb.den.max(0)),
                i128::from(from.den) * i128::from(tb.num.max(1)),
            )
        };
        let mut end = duration_us
            .filter(|&d| d > 0)
            .map(|d| to_tb(d, Rational::new(1, 1_000_000)));
        if let Some(&n) = order.get(k + 1) {
            let next = to_tb(chapters[n].start, chapters[n].time_base);
            if next > start && end.is_none_or(|e| next < e) {
                end = Some(next);
            }
        }
        chapters[i].end = end.filter(|&e| e >= start).unwrap_or(start);
    }
}

/// `chapters` by start, as listed where starts are equal. A chapter track can
/// hold no other order, and FFmpeg's Matroska demuxer skips a chapter listed
/// after a later one, so muxers write them this way.
pub fn in_time_order(chapters: &[Chapter]) -> Vec<Chapter> {
    let mut out = chapters.to_vec();
    out.sort_by_key(|c| c.start_in(1_000_000_000));
    out
}

/// Chapters for an output cut to the window `[start_s, end_s)` (seconds),
/// moved so the window begins at zero: what FFmpeg does with `-ss` / `-t`.
/// Chapters outside the window are left out, the rest clipped.
pub fn trim(chapters: &[Chapter], start_s: f64, end_s: Option<f64>) -> Vec<Chapter> {
    chapters
        .iter()
        .filter_map(|c| {
            let tb = c.time_base;
            let ticks = |s: f64| (s * f64::from(tb.den) / f64::from(tb.num.max(1))).round() as i64;
            let off = ticks(start_s);
            if c.end < off {
                return None;
            }
            let mut out = c.clone();
            out.start = c.start.saturating_sub(off).max(0);
            out.end = c.end.saturating_sub(off).max(out.start);
            if let Some(end) = end_s {
                let stop = ticks(end);
                if c.start > stop {
                    return None;
                }
                out.end = out.end.min(stop.saturating_sub(off)).max(out.start);
            }
            Some(out)
        })
        .collect()
}

#[cfg(test)]
mod tests {
    use super::*;

    fn ms(id: u64, start: i64, end: i64) -> Chapter {
        Chapter::new(id, Rational::new(1, 1000), start, end)
    }

    #[test]
    fn open_ends_run_to_the_next_start_in_time_order() {
        let mut c = [
            ms(0, 5000, 0),
            ms(1, 0, 0),
            ms(2, 5000, 0),
            ms(3, 7000, 7000),
        ];
        fill_ends(&mut c, &[true, true, true, false], Some(10_000_000));
        let ends: Vec<i64> = c.iter().map(|c| c.end).collect();
        // Two chapters share a start: the first of them has no later start
        // right after it and runs to the end of the file, as in FFmpeg.
        assert_eq!(ends, [10_000, 5000, 7000, 7000]);
        let mut c = [ms(0, 9, 0)];
        fill_ends(&mut c, &[true], None);
        assert_eq!(c[0].end, 9);
        fill_ends(&mut c, &[true], Some(1_000)); // the file ends before it
        assert_eq!(c[0].end, 9);
    }

    #[test]
    fn scales_round_to_nearest_and_saturate() {
        let c = Chapter::new(0, Rational::new(1001, 30000), 100, 50);
        assert_eq!(c.start_in(1000), 3337);
        assert_eq!(c.end_in(1000), 3337); // never before the start
        let c = Chapter::new(0, Rational::new(i32::MAX, 1), i64::MAX, i64::MAX);
        assert_eq!(c.start_in(u32::MAX), i64::MAX);
        assert_eq!(Chapter::new(0, Rational::new(1, 0), 5, 9).start_in(10), 0);
        assert_eq!(ms(0, -1500, 0).start_in(1), -2);
    }

    #[test]
    fn a_trim_window_clips_and_moves_chapters() {
        let c = [
            ms(0, 0, 2000),
            ms(1, 2000, 6000),
            ms(2, 6000, 9000),
            ms(3, 9000, 10_000),
        ];
        let t = trim(&c, 3.0, Some(8.0));
        let spans: Vec<(u64, i64, i64)> = t.iter().map(|c| (c.id, c.start, c.end)).collect();
        assert_eq!(spans, [(1, 0, 3000), (2, 3000, 5000)]);
        assert_eq!(trim(&c, 0.0, None), c);
    }
}
