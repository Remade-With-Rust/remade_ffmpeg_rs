//! `rff-format-webvtt` — WebVTT (`.vtt`) subtitle container.
//!
//! WebVTT opens with a `WEBVTT` line, then blank-line-separated cues:
//! `start --> end` (`HH:MM:SS.mmm`, hours optional, plus optional cue settings)
//! and text. Like [`rff_format_srt`](https://docs.rs/rff-format-srt), each cue is
//! a [`Packet`] of UTF-8 text with `pts`/`duration` in ms — so a `.vtt` demux can
//! feed a `.srt` mux and vice versa.

use std::io::{Read, Write};

use rff_core::{CodecId, Error, Packet, Rational, Result};
use rff_format::{Demuxer, Format, FormatRegistry, Input, Muxer, Output, Stream};

/// Register the WebVTT format (demuxer + muxer).
pub fn register(registry: &mut FormatRegistry) {
    registry.register(Format {
        name: "webvtt",
        long_name: "WebVTT subtitle",
        extensions: &["vtt"],
        demuxer: Some(|input| Box::new(VttDemuxer::new(input))),
        muxer: Some(|output| Box::new(VttMuxer::new(output))),
        probe: Some(probe_vtt),
    });
}

fn probe_vtt(d: &[u8]) -> i32 {
    if d.starts_with(b"WEBVTT") {
        95
    } else {
        0
    }
}

pub struct VttDemuxer {
    input: Input,
    cues: std::collections::VecDeque<Packet>,
    parsed: bool,
}

impl VttDemuxer {
    pub fn new(input: Input) -> VttDemuxer {
        VttDemuxer {
            input,
            cues: Default::default(),
            parsed: false,
        }
    }
}

impl Demuxer for VttDemuxer {
    fn read_header(&mut self) -> Result<Vec<Stream>> {
        let mut bytes = Vec::new();
        let _ = self.input.read_to_end(&mut bytes);
        let text = String::from_utf8_lossy(&bytes);
        self.cues = rff_subtitle::parse_cues(&text)
            .into_iter()
            .map(|c| {
                let mut pkt = Packet::from_data(0, c.text.into_bytes());
                pkt.pts = Some(c.start_ms);
                pkt.dts = Some(c.start_ms);
                pkt.duration = (c.end_ms - c.start_ms).max(0);
                pkt
            })
            .collect();
        self.parsed = true;
        let mut s = Stream::new(0, CodecId::WebVtt);
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

pub struct VttMuxer {
    out: Output,
    wrote_header: bool,
}

impl VttMuxer {
    pub fn new(out: Output) -> VttMuxer {
        VttMuxer {
            out,
            wrote_header: false,
        }
    }
}

impl Muxer for VttMuxer {
    fn write_header(&mut self, _streams: &[Stream]) -> Result<()> {
        self.out.write_all(b"WEBVTT\n\n")?;
        self.wrote_header = true;
        Ok(())
    }

    fn write_packet(&mut self, packet: &Packet) -> Result<()> {
        if !self.wrote_header {
            self.write_header(&[])?;
        }
        let start = packet.pts.unwrap_or(0);
        let end = start + packet.duration.max(0);
        let text = String::from_utf8_lossy(&packet.data);
        write!(
            self.out,
            "{} --> {}\n{}\n\n",
            rff_subtitle::format_timestamp(start, '.'),
            rff_subtitle::format_timestamp(end, '.'),
            text.trim_end()
        )?;
        Ok(())
    }

    fn write_trailer(&mut self) -> Result<()> {
        self.out.flush()?;
        Ok(())
    }
}
