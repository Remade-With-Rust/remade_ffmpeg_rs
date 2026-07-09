//! IVF container — the minimal native wrapper for VP8/VP9 bitstreams.
//!
//! IVF is what `vpxenc` writes and `vpxdec`/ffmpeg read for raw VP9 testing: a
//! 32-byte file header, then `[size:u32][pts:u64]` + frame data per packet. This
//! is the output side of the VP9 RD campaign — rff encodes `y4m → ivf`, and
//! ffmpeg/libvpx decode the `.ivf` back for PSNR/VMAF.

use std::io::{Read, Write};

use rff_core::{CodecId, Error, Packet, Rational, Result};
use rff_format::{Demuxer, Format, FormatRegistry, Input, Muxer, Output, Stream};

/// Register the IVF format into a [`FormatRegistry`].
pub fn register(registry: &mut FormatRegistry) {
    registry.register(Format {
        name: "ivf",
        long_name: "IVF (raw VP8/VP9)",
        extensions: &["ivf"],
        demuxer: Some(|input| Box::new(IvfDemuxer::new(input))),
        muxer: Some(|output| Box::new(IvfMuxer::new(output))),
        probe: Some(probe_ivf),
    });
}

/// Sniff IVF: the file begins with the `DKIF` signature.
fn probe_ivf(data: &[u8]) -> i32 {
    if data.len() >= 4 && &data[..4] == b"DKIF" {
        100
    } else {
        0
    }
}

fn rd_u16(b: &[u8], at: usize) -> u16 {
    u16::from_le_bytes([b[at], b[at + 1]])
}
fn rd_u32(b: &[u8], at: usize) -> u32 {
    u32::from_le_bytes([b[at], b[at + 1], b[at + 2], b[at + 3]])
}

// ---------------------------------------------------------------------------
// Muxer
// ---------------------------------------------------------------------------

struct IvfMuxer {
    out: Output,
    ready: bool,
    /// Frame index used as the IVF timestamp. IVF pts is a plain frame counter;
    /// we assign it here rather than trust `packet.pts` (encoders often leave it
    /// unset, which would collapse every frame onto pts 0 and make decoders drop
    /// them as duplicates).
    frame: u64,
}

impl IvfMuxer {
    fn new(out: Output) -> IvfMuxer {
        IvfMuxer {
            out,
            ready: false,
            frame: 0,
        }
    }
}

impl Muxer for IvfMuxer {
    fn write_header(&mut self, streams: &[Stream]) -> Result<()> {
        let s = streams
            .first()
            .filter(|s| s.codec_id == CodecId::Vp9)
            .ok_or_else(|| Error::unsupported("ivf mux: needs a single `vp9` stream"))?;
        // fps num:den = 1/time_base = time_base.den : time_base.num.
        let (fnum, fden) = (s.time_base.den.max(1) as u32, s.time_base.num.max(1) as u32);
        let mut h = Vec::with_capacity(32);
        h.extend_from_slice(b"DKIF");
        h.extend_from_slice(&0u16.to_le_bytes()); // version
        h.extend_from_slice(&32u16.to_le_bytes()); // header length
        h.extend_from_slice(b"VP90"); // fourcc
        h.extend_from_slice(&(s.width as u16).to_le_bytes());
        h.extend_from_slice(&(s.height as u16).to_le_bytes());
        h.extend_from_slice(&fnum.to_le_bytes()); // framerate numerator
        h.extend_from_slice(&fden.to_le_bytes()); // framerate denominator
        // Frame count is not authoritative (decoders read to EOF); leave 0.
        h.extend_from_slice(&0u32.to_le_bytes());
        h.extend_from_slice(&0u32.to_le_bytes()); // unused
        self.out.write_all(&h)?;
        self.ready = true;
        Ok(())
    }

    fn write_packet(&mut self, packet: &Packet) -> Result<()> {
        if !self.ready {
            return Err(Error::invalid("ivf mux: write_header not called"));
        }
        let pts = self.frame;
        self.frame += 1;
        self.out
            .write_all(&(packet.data.len() as u32).to_le_bytes())?;
        self.out.write_all(&pts.to_le_bytes())?;
        self.out.write_all(&packet.data)?;
        Ok(())
    }

    fn write_trailer(&mut self) -> Result<()> {
        self.out.flush()?;
        Ok(())
    }
}

// ---------------------------------------------------------------------------
// Demuxer
// ---------------------------------------------------------------------------

struct IvfDemuxer {
    input: Option<Input>,
    buf: Vec<u8>,
    pos: usize,
}

impl IvfDemuxer {
    fn new(input: Input) -> IvfDemuxer {
        IvfDemuxer {
            input: Some(input),
            buf: Vec::new(),
            pos: 0,
        }
    }
}

impl Demuxer for IvfDemuxer {
    fn read_header(&mut self) -> Result<Vec<Stream>> {
        let mut input = self
            .input
            .take()
            .ok_or_else(|| Error::invalid("ivf demux: header already read"))?;
        input.read_to_end(&mut self.buf)?;
        if self.buf.len() < 32 || probe_ivf(&self.buf) == 0 {
            return Err(Error::invalid("ivf demux: not an IVF file"));
        }
        let width = rd_u16(&self.buf, 12) as u32;
        let height = rd_u16(&self.buf, 14) as u32;
        let fnum = rd_u32(&self.buf, 16).max(1);
        let fden = rd_u32(&self.buf, 20).max(1);
        self.pos = rd_u16(&self.buf, 6).max(32) as usize; // header length

        let mut stream = Stream::new(0, CodecId::Vp9);
        stream.width = width;
        stream.height = height;
        stream.time_base = Rational::new(fden as i32, fnum as i32);
        Ok(vec![stream])
    }

    fn read_packet(&mut self) -> Result<Packet> {
        if self.pos + 12 > self.buf.len() {
            return Err(Error::Eof);
        }
        let size = rd_u32(&self.buf, self.pos) as usize;
        let pts = u64::from_le_bytes(self.buf[self.pos + 4..self.pos + 12].try_into().unwrap());
        let start = self.pos + 12;
        let end = start + size;
        if end > self.buf.len() {
            return Err(Error::invalid("ivf demux: truncated frame"));
        }
        let mut packet = Packet::from_data(0, self.buf[start..end].to_vec());
        packet.pts = Some(pts as i64);
        self.pos = end;
        Ok(packet)
    }
}

#[cfg(test)]
mod tests {
    use super::*;
    use std::io::Cursor;
    use std::sync::{Arc, Mutex};

    #[derive(Clone)]
    struct SharedBuf(Arc<Mutex<Vec<u8>>>);
    impl Write for SharedBuf {
        fn write(&mut self, buf: &[u8]) -> std::io::Result<usize> {
            self.0.lock().unwrap().extend_from_slice(buf);
            Ok(buf.len())
        }
        fn flush(&mut self) -> std::io::Result<()> {
            Ok(())
        }
    }

    #[test]
    fn mux_then_demux_roundtrips() {
        let sink = SharedBuf(Arc::new(Mutex::new(Vec::new())));
        {
            let mut mux = IvfMuxer::new(Box::new(sink.clone()));
            let mut s = Stream::new(0, CodecId::Vp9);
            s.width = 352;
            s.height = 288;
            s.time_base = Rational::new(1, 30);
            mux.write_header(&[s]).unwrap();
            let mut p = Packet::from_data(0, vec![1, 2, 3, 4, 5]);
            p.pts = Some(0);
            mux.write_packet(&p).unwrap();
            mux.write_trailer().unwrap();
        }
        let file = sink.0.lock().unwrap().clone();
        assert_eq!(&file[0..4], b"DKIF");
        assert_eq!(probe_ivf(&file), 100);

        let mut dem = IvfDemuxer::new(Box::new(Cursor::new(file)));
        let streams = dem.read_header().unwrap();
        assert_eq!(streams[0].codec_id, CodecId::Vp9);
        assert_eq!((streams[0].width, streams[0].height), (352, 288));
        assert_eq!(dem.read_packet().unwrap().data, vec![1, 2, 3, 4, 5]);
        assert!(matches!(dem.read_packet(), Err(Error::Eof)));
    }
}
