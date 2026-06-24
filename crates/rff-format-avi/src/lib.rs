//! AVI (Audio Video Interleaved) container.
//!
//! AVI is a RIFF-based container: a tree of `LIST`/chunk records. The demuxer
//! walks `hdrl` (stream headers) then `movi` (interleaved data chunks); the
//! muxer writes those back and patches the `idx1` index in the trailer.
//!
//! Status: **scaffold** — the format is registered (so `-f avi` and `.avi`
//! resolve), but header/packet handling returns [`Error::Unimplemented`].

use rff_core::{Error, Packet, Result};
use rff_format::{Demuxer, Format, FormatRegistry, Input, Muxer, Output, Stream};

/// Register the AVI format into a [`FormatRegistry`].
pub fn register(registry: &mut FormatRegistry) {
    registry.register(Format {
        name: "avi",
        long_name: "AVI (Audio Video Interleaved)",
        extensions: &["avi"],
        demuxer: Some(|input| Box::new(AviDemuxer::new(input))),
        muxer: Some(|output| Box::new(AviMuxer::new(output))),
    });
}

struct AviDemuxer {
    _input: Input,
}

impl AviDemuxer {
    fn new(input: Input) -> AviDemuxer {
        AviDemuxer { _input: input }
    }
}

impl Demuxer for AviDemuxer {
    fn read_header(&mut self) -> Result<Vec<Stream>> {
        Err(Error::Unimplemented("avi demux: read_header"))
    }

    fn read_packet(&mut self) -> Result<Packet> {
        Err(Error::Unimplemented("avi demux: read_packet"))
    }
}

struct AviMuxer {
    _output: Output,
}

impl AviMuxer {
    fn new(output: Output) -> AviMuxer {
        AviMuxer { _output: output }
    }
}

impl Muxer for AviMuxer {
    fn write_header(&mut self, _streams: &[Stream]) -> Result<()> {
        Err(Error::Unimplemented("avi mux: write_header"))
    }

    fn write_packet(&mut self, _packet: &Packet) -> Result<()> {
        Err(Error::Unimplemented("avi mux: write_packet"))
    }

    fn write_trailer(&mut self) -> Result<()> {
        Err(Error::Unimplemented("avi mux: write_trailer"))
    }
}
