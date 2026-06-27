//! `rff-format-srt` — SubRip (`.srt`) subtitle container.
//!
//! A `.srt` is a sequence of cues: an index line, a `start --> end` timing line
//! (`HH:MM:SS,mmm`), then one or more text lines, blank-line separated. Each cue
//! becomes a [`Packet`] whose `data` is the UTF-8 text and whose `pts`/`duration`
//! carry the timing (milliseconds). The muxer reverses it. Because the packet is
//! just text + timing, an `.srt` demux can feed a `.vtt` mux unchanged.

use std::io::{Read, Write};

use rff_core::{CodecId, Error, Packet, Rational, Result};
use rff_format::{Demuxer, Format, FormatRegistry, Input, Muxer, Output, Stream};

/// Register the SubRip format (demuxer + muxer).
pub fn register(registry: &mut FormatRegistry) {
    registry.register(Format {
        name: "srt",
        long_name: "SubRip subtitle",
        extensions: &["srt"],
        demuxer: Some(|input| Box::new(SrtDemuxer::new(input))),
        muxer: Some(|output| Box::new(SrtMuxer::new(output))),
        probe: Some(probe_srt),
    });
}

fn probe_srt(d: &[u8]) -> i32 {
    let head = &d[..d.len().min(512)];
    if head.windows(3).any(|w| w == b"-->") {
        40
    } else {
        0
    }
}

/// Read the whole input as lossy UTF-8 — subtitle files are small, so buffering
/// is simplest; [`rff_subtitle::parse_cues`] normalizes line endings + BOM.
fn read_text(input: &mut Input) -> String {
    let mut bytes = Vec::new();
    let _ = input.read_to_end(&mut bytes);
    String::from_utf8_lossy(&bytes).into_owned()
}

/// Parse SubRip text into cue packets (`data` = text, `pts`/`duration` = ms).
pub fn parse_srt(text: &str) -> Vec<Packet> {
    rff_subtitle::parse_cues(text)
        .into_iter()
        .map(|c| {
            let mut pkt = Packet::from_data(0, c.text.into_bytes());
            pkt.pts = Some(c.start_ms);
            pkt.dts = Some(c.start_ms);
            pkt.duration = (c.end_ms - c.start_ms).max(0);
            pkt
        })
        .collect()
}

pub struct SrtDemuxer {
    input: Input,
    cues: std::collections::VecDeque<Packet>,
    parsed: bool,
}

impl SrtDemuxer {
    pub fn new(input: Input) -> SrtDemuxer {
        SrtDemuxer { input, cues: Default::default(), parsed: false }
    }
}

impl Demuxer for SrtDemuxer {
    fn read_header(&mut self) -> Result<Vec<Stream>> {
        let text = read_text(&mut self.input);
        self.cues = parse_srt(&text).into();
        self.parsed = true;
        let mut s = Stream::new(0, CodecId::Subrip);
        s.time_base = Rational::new(1, 1000);
        Ok(vec![s])
    }

    fn read_packet(&mut self) -> Result<Packet> {
        if !self.parsed {
            self.read_header()?;
        }
        self.cues.pop_front().ok_or(Error::Eof)
    }
}

pub struct SrtMuxer {
    out: Output,
    counter: u32,
}

impl SrtMuxer {
    pub fn new(out: Output) -> SrtMuxer {
        SrtMuxer { out, counter: 0 }
    }
}

impl Muxer for SrtMuxer {
    fn write_header(&mut self, _streams: &[Stream]) -> Result<()> {
        Ok(())
    }

    fn write_packet(&mut self, packet: &Packet) -> Result<()> {
        self.counter += 1;
        let start = packet.pts.unwrap_or(0);
        let end = start + packet.duration.max(0);
        let text = String::from_utf8_lossy(&packet.data);
        write!(
            self.out,
            "{}\n{} --> {}\n{}\n\n",
            self.counter,
            rff_subtitle::format_timestamp(start, ','),
            rff_subtitle::format_timestamp(end, ','),
            text.trim_end()
        )?;
        Ok(())
    }

    fn write_trailer(&mut self) -> Result<()> {
        self.out.flush()?;
        Ok(())
    }
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn parses_cues_with_timing_and_text() {
        let srt = "1\n00:00:01,000 --> 00:00:02,500\nHello\nworld\n\n2\n00:00:03,000 --> 00:00:04,000\nBye\n";
        let cues = parse_srt(srt);
        assert_eq!(cues.len(), 2);
        assert_eq!(cues[0].pts, Some(1000));
        assert_eq!(cues[0].duration, 1500);
        assert_eq!(String::from_utf8_lossy(&cues[0].data), "Hello\nworld");
        assert_eq!(cues[1].pts, Some(3000));
    }
}
