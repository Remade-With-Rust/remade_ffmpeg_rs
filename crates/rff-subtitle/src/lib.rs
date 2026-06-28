//! Shared helpers for text subtitle formats (SubRip, WebVTT).
//!
//! Both express cues as `start --> end` plus text; they differ only in the
//! millisecond separator (`,` vs `.`) and a few framing details. Carrying a cue
//! as a [`rff_core`]-free `(start_ms, end_ms, text)` lets one format's demuxer
//! feed another's muxer — so `.srt → .vtt` is a packet copy, the muxer just
//! reformats the timing.

/// Format `ms` as `HH:MM:SS{sep}mmm` (`sep` = `,` for SubRip, `.` for WebVTT).
pub fn format_timestamp(ms: i64, sep: char) -> String {
    let ms = ms.max(0);
    let h = ms / 3_600_000;
    let m = (ms / 60_000) % 60;
    let s = (ms / 1000) % 60;
    let milli = ms % 1000;
    format!("{h:02}:{m:02}:{s:02}{sep}{milli:03}")
}

/// Parse `HH:MM:SS,mmm` / `HH:MM:SS.mmm` (hours optional, as WebVTT allows) into
/// milliseconds. Returns `None` on a malformed stamp.
pub fn parse_timestamp(s: &str) -> Option<i64> {
    let s = s.trim();
    let (hms, milli) = s.split_once([',', '.'])?;
    let milli: i64 = milli.trim().parse().ok()?;
    let mut parts = hms.split(':').rev();
    let secs: i64 = parts.next()?.trim().parse().ok()?;
    let mins: i64 = parts.next()?.trim().parse().ok()?;
    let hours: i64 = match parts.next() {
        Some(h) => h.trim().parse().ok()?,
        None => 0,
    };
    Some(((hours * 60 + mins) * 60 + secs) * 1000 + milli)
}

/// Split a `start --> end` line into `(start_ms, end_ms)`. Ignores any trailing
/// WebVTT cue settings after the end stamp.
pub fn parse_cue_timing(line: &str) -> Option<(i64, i64)> {
    let (a, b) = line.split_once("-->")?;
    let end = b.split_whitespace().next()?;
    Some((parse_timestamp(a)?, parse_timestamp(end)?))
}

/// One subtitle cue, format-agnostic.
#[derive(Debug, Clone, PartialEq, Eq)]
pub struct Cue {
    pub start_ms: i64,
    pub end_ms: i64,
    pub text: String,
}

/// Parse text-subtitle cues from either SubRip or WebVTT. Normalizes line
/// endings + BOM, splits on blank lines, and takes each block that has a
/// `-->` timing line (so `WEBVTT`/`NOTE`/`STYLE` blocks and SubRip index lines
/// are skipped). The cue text is everything after the timing line.
pub fn parse_cues(text: &str) -> Vec<Cue> {
    let norm = text.replace("\r\n", "\n").replace('\r', "\n");
    let norm = norm.strip_prefix('\u{feff}').unwrap_or(&norm);
    let mut cues = Vec::new();
    for block in norm.split("\n\n") {
        let block = block.trim_matches('\n');
        if block.is_empty() {
            continue;
        }
        let mut timing = None;
        let mut rest = Vec::new();
        for line in block.lines() {
            if timing.is_none() && line.contains("-->") {
                timing = parse_cue_timing(line);
            } else if timing.is_some() {
                rest.push(line);
            }
        }
        if let Some((start, end)) = timing {
            cues.push(Cue {
                start_ms: start,
                end_ms: end,
                text: rest.join("\n"),
            });
        }
    }
    cues
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn format_and_parse_roundtrip() {
        assert_eq!(format_timestamp(3_661_500, ','), "01:01:01,500");
        assert_eq!(format_timestamp(3_661_500, '.'), "01:01:01.500");
        assert_eq!(parse_timestamp("01:01:01,500"), Some(3_661_500));
        assert_eq!(parse_timestamp("01:01:01.500"), Some(3_661_500));
        // WebVTT may omit the hour field.
        assert_eq!(parse_timestamp("01:01.500"), Some(61_500));
    }

    #[test]
    fn cue_timing_ignores_vtt_settings() {
        let (a, b) = parse_cue_timing("00:00:01.000 --> 00:00:02.000 line:0 position:50%").unwrap();
        assert_eq!((a, b), (1000, 2000));
    }
}
