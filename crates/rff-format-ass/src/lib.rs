//! ASS / SSA (Advanced SubStation Alpha) subtitle demuxer.
//!
//! Reads the `[Events]` section's `Dialogue:` lines, strips override tags
//! (`{\i1}` …) and resolves `\N` line breaks, and yields the cues as plain
//! text packets labelled [`CodecId::Subrip`] — the same packet contract as
//! the SubRip/WebVTT crates, so `.ass → .srt`/`.vtt` (and into Matroska as
//! `S_TEXT/UTF8`) is a stream copy. Styling is dropped by design: rff's
//! subtitle pipeline carries text, not renderer state.
//!
//! Demux only: writing styled ASS from plain cues would fabricate a style —
//! write `.srt`/`.vtt` instead.

use std::collections::VecDeque;
use std::io::Read;

use rff_core::{CodecId, Error, Packet, Rational, Result};
use rff_format::{Demuxer, Format, FormatRegistry, Input, MuxCaps, Stream};
use rff_subtitle::{ass_dialogue_text, parse_ass_timestamp};

/// Register the ASS/SSA demuxer.
pub fn register(registry: &mut FormatRegistry) {
    registry.register(Format {
        name: "ass",
        long_name: "ASS / SSA (Advanced SubStation Alpha)",
        extensions: &["ass", "ssa"],
        demuxer: Some(|input| Box::new(AssDemuxer::new(input))),
        muxer: None,
        muxer_path: None,
        probe: Some(probe_ass),
        mux_caps: MuxCaps::NONE,
    });
}

/// Sniff: a `[Script Info]` section heads every ASS/SSA file.
pub fn probe_ass(bytes: &[u8]) -> i32 {
    let head = String::from_utf8_lossy(&bytes[..bytes.len().min(256)]);
    let head = head.trim_start_matches('\u{feff}').trim_start();
    if head.starts_with("[Script Info]") {
        95
    } else {
        0
    }
}

/// Parse Dialogue lines into (start, end, text) cues, in start order.
fn parse_ass(text: &str) -> Vec<(i64, i64, String)> {
    let mut cues = Vec::new();
    for line in text.lines() {
        let line = line.trim();
        let Some(rest) = line.strip_prefix("Dialogue:") else {
            continue;
        };
        // Dialogue: Layer,Start,End,Style,Name,MarginL,MarginR,MarginV,Effect,Text
        let mut fields = rest.splitn(4, ',');
        let _layer = fields.next();
        let (Some(start), Some(end), Some(tail)) =
            (fields.next(), fields.next(), fields.next())
        else {
            continue;
        };
        let (Some(start), Some(end)) = (parse_ass_timestamp(start), parse_ass_timestamp(end))
        else {
            continue;
        };
        // `tail` still carries Style,Name,MarginL,MarginR,MarginV,Effect,Text.
        let text = ass_dialogue_text(tail, 6);
        if !text.is_empty() {
            cues.push((start, end, text));
        }
    }
    cues.sort_by_key(|c| c.0);
    cues
}

pub struct AssDemuxer {
    input: Input,
    cues: VecDeque<Packet>,
    parsed: bool,
}

impl AssDemuxer {
    fn new(input: Input) -> AssDemuxer {
        AssDemuxer {
            input,
            cues: VecDeque::new(),
            parsed: false,
        }
    }
}

impl Demuxer for AssDemuxer {
    fn read_header(&mut self) -> Result<Vec<Stream>> {
        if !self.parsed {
            let mut bytes = Vec::new();
            self.input.read_to_end(&mut bytes)?;
            let text = String::from_utf8_lossy(&bytes);
            let cues = parse_ass(&text);
            if cues.is_empty() {
                return Err(Error::invalid("ass: no Dialogue events found"));
            }
            for (start, end, text) in cues {
                let mut pkt = Packet::from_data(0, text.into_bytes());
                pkt.pts = Some(start);
                pkt.dts = Some(start);
                pkt.duration = (end - start).max(0);
                self.cues.push_back(pkt);
            }
            self.parsed = true;
        }
        let mut s = Stream::new(0, CodecId::Subrip);
        s.time_base = Rational::new(1, 1000);
        s.nb_frames = Some(self.cues.len() as u64);
        Ok(vec![s])
    }

    fn read_packet(&mut self) -> Result<Packet> {
        if !self.parsed {
            self.read_header()?;
        }
        self.cues.pop_front().ok_or(Error::Eof)
    }
}

#[cfg(test)]
mod tests {
    use super::*;
    use std::io::Cursor;

    const SAMPLE: &str = "\
[Script Info]\r\nTitle: t\r\n\r\n[Events]\r\nFormat: Layer, Start, End, Style, Name, MarginL, MarginR, MarginV, Effect, Text\r\nDialogue: 0,0:00:01.00,0:00:02.50,Default,,0,0,0,,{\\i1}Hello{\\i0} there\\Nfriend\r\nDialogue: 0,0:00:03.00,0:00:04.00,Default,,0,0,0,,Second, with comma\r\n";

    #[test]
    fn probe_needs_script_info() {
        assert_eq!(probe_ass(b"[Script Info]\nTitle: x"), 95);
        assert_eq!(probe_ass(b"1\n00:00:01,000 --> 2"), 0);
    }

    #[test]
    fn dialogue_lines_become_clean_cues() {
        let mut dem = AssDemuxer::new(Box::new(Cursor::new(SAMPLE.as_bytes().to_vec())));
        let streams = dem.read_header().unwrap();
        assert_eq!(streams[0].codec_id, CodecId::Subrip);

        let p = dem.read_packet().unwrap();
        assert_eq!(p.pts, Some(1000));
        assert_eq!(p.duration, 1500);
        assert_eq!(std::str::from_utf8(&p.data).unwrap(), "Hello there\nfriend");

        // Commas inside the text survive (only the 9 meta fields are split).
        let p = dem.read_packet().unwrap();
        assert_eq!(std::str::from_utf8(&p.data).unwrap(), "Second, with comma");
        assert!(matches!(dem.read_packet(), Err(Error::Eof)));
    }
}
