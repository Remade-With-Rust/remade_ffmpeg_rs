//! The `rtp` "container": timed frames from `rff-io`'s `rtp://` reader.
//!
//! An RTP stream has no container. What it does have — and what a raw
//! elementary stream would throw away — is a 90 kHz timestamp per frame, so
//! [`rff_io::rtp::RtpReader`] hands its reassembled frames over as records
//! ([`rff_io::rtp::write_frame`]: magic, codec, flags, timestamp, length,
//! bytes) and this demuxer turns them into packets with `pts` on a
//! `1/90000` time base. The engine opens `rtp://` inputs as this format by
//! name; the probe exists so a record stream dumped to a file reads back too.
//!
//! Two codecs, as the reader produces them: H.264 access units in Annex-B
//! form (`CodecId::H264`, dimensions from the first SPS) and complete baseline
//! JPEGs (`CodecId::Jpeg`, dimensions from `SOF0`).

use std::io::Read;

use rff_core::{CodecId, Error, Packet, Rational, Result};
use rff_format::avc::{find_sps_annexb, sps_dimensions};
use rff_format::{Demuxer, Format, FormatRegistry, Input, MuxCaps, Stream};
use rff_io::rtp::{
    parse_frame_header, FrameHeader, CODEC_H264, CODEC_JPEG, FLAG_KEYFRAME, FRAME_HEADER_LEN,
    FRAME_MAGIC,
};

/// The RTP video clock every record's timestamp is on.
pub const CLOCK_HZ: i32 = 90_000;

/// Register the `rtp` format into a [`FormatRegistry`].
pub fn register(registry: &mut FormatRegistry) {
    registry.register(Format {
        name: "rtp",
        long_name: "RTP frames (H.264 / JPEG, as received by rtp://)",
        extensions: &[],
        demuxer: Some(|input| Box::new(RtpDemuxer::new(input))),
        muxer: None,
        muxer_path: None,
        probe: Some(probe_rtp),
        mux_caps: MuxCaps::NONE,
    });
}

fn probe_rtp(data: &[u8]) -> i32 {
    if data.len() >= FRAME_HEADER_LEN
        && data[..4] == FRAME_MAGIC
        && matches!(data[4], CODEC_H264 | CODEC_JPEG)
    {
        100
    } else {
        0
    }
}

/// One record: header plus payload.
struct Record {
    header: FrameHeader,
    data: Vec<u8>,
}

/// Demuxes the record stream into timed packets.
pub struct RtpDemuxer {
    input: Input,
    /// The record `read_header` consumed to learn the codec and geometry.
    pending: Option<Record>,
    /// First timestamp, so `pts` starts at 0; and the 32-bit clock unwrapped.
    first_ts: Option<u32>,
    last_ts: u32,
    wraps: i64,
}

impl RtpDemuxer {
    /// Wrap a record stream.
    pub fn new(input: Input) -> RtpDemuxer {
        RtpDemuxer {
            input,
            pending: None,
            first_ts: None,
            last_ts: 0,
            wraps: 0,
        }
    }

    fn next_record(&mut self) -> Result<Option<Record>> {
        let mut head = [0u8; FRAME_HEADER_LEN];
        // A clean end of stream falls between records.
        let mut filled = 0;
        while filled < head.len() {
            match self.input.read(&mut head[filled..])? {
                0 if filled == 0 => return Ok(None),
                0 => return Err(Error::invalid("rtp: truncated frame record")),
                n => filled += n,
            }
        }
        let header = parse_frame_header(&head)?;
        let mut data = vec![0u8; header.len];
        self.input.read_exact(&mut data)?;
        Ok(Some(Record { header, data }))
    }

    /// Unwrap the 32-bit 90 kHz clock and rebase on the first frame.
    fn pts_of(&mut self, ts: u32) -> i64 {
        let first = *self.first_ts.get_or_insert(ts);
        if self.first_ts == Some(ts) && self.wraps == 0 {
            self.last_ts = ts;
        }
        // A backwards jump of more than half the range is a wrap forwards.
        if ts < self.last_ts && self.last_ts - ts > u32::MAX / 2 {
            self.wraps += 1;
        } else if ts > self.last_ts && ts - self.last_ts > u32::MAX / 2 && self.wraps > 0 {
            self.wraps -= 1;
        }
        self.last_ts = ts;
        (i64::from(ts) + (self.wraps << 32)) - i64::from(first)
    }

    fn packet_of(&mut self, rec: Record) -> Packet {
        let pts = self.pts_of(rec.header.timestamp);
        let keyframe = rec.header.flags & FLAG_KEYFRAME != 0;
        let mut packet = Packet::from_data(0, rec.data);
        packet.time_base = Rational::new(1, CLOCK_HZ);
        packet.pts = Some(pts);
        packet.dts = Some(pts);
        packet.flags.keyframe = keyframe;
        packet
    }
}

impl Demuxer for RtpDemuxer {
    fn read_header(&mut self) -> Result<Vec<Stream>> {
        let rec = self
            .next_record()?
            .ok_or_else(|| Error::invalid("rtp: no frames received"))?;
        let (codec, dims) = match rec.header.codec {
            CODEC_H264 => (
                CodecId::H264,
                find_sps_annexb(&rec.data).and_then(sps_dimensions),
            ),
            CODEC_JPEG => (CodecId::Jpeg, rff_format_mjpeg::jpeg_dimensions(&rec.data)),
            other => return Err(Error::unsupported(format!("rtp: record codec {other}"))),
        };
        let mut stream = Stream::new(0, codec);
        stream.time_base = Rational::new(1, CLOCK_HZ);
        if let Some((w, h)) = dims {
            stream.width = w;
            stream.height = h;
        }
        self.pending = Some(rec);
        Ok(vec![stream])
    }

    fn read_packet(&mut self) -> Result<Packet> {
        let rec = match self.pending.take() {
            Some(rec) => rec,
            None => self.next_record()?.ok_or(Error::Eof)?,
        };
        Ok(self.packet_of(rec))
    }
}

#[cfg(test)]
mod tests {
    use super::*;
    use rff_io::rtp::write_frame;
    use std::io::Cursor;

    fn jpeg(w: u16, h: u16) -> Vec<u8> {
        let rgb: Vec<u8> = (0..usize::from(w) * usize::from(h) * 3)
            .map(|i| (i % 253) as u8)
            .collect();
        let mut out = Vec::new();
        rusty_jpeg::encode::Encoder::new(&mut out, 75)
            .encode(&rgb, w, h, rusty_jpeg::encode::ColorType::Rgb)
            .unwrap();
        out
    }

    #[test]
    fn jpeg_records_become_timed_packets_from_zero() {
        let frame = jpeg(64, 48);
        let mut stream = Vec::new();
        write_frame(&mut stream, CODEC_JPEG, true, 5_000_000, &frame);
        write_frame(&mut stream, CODEC_JPEG, true, 5_009_000, &frame);
        assert_eq!(probe_rtp(&stream), 100);
        assert_eq!(probe_rtp(b"RFF1\x07"), 0);

        let mut dem = RtpDemuxer::new(Box::new(Cursor::new(stream)));
        let streams = dem.read_header().unwrap();
        assert_eq!(streams[0].codec_id, CodecId::Jpeg);
        assert_eq!((streams[0].width, streams[0].height), (64, 48));
        assert_eq!(streams[0].time_base, Rational::new(1, 90_000));
        let a = dem.read_packet().unwrap();
        let b = dem.read_packet().unwrap();
        assert_eq!(a.pts, Some(0));
        assert_eq!(b.pts, Some(9_000), "100 ms at 90 kHz");
        assert!(a.flags.keyframe && a.data == frame);
        assert!(matches!(dem.read_packet(), Err(Error::Eof)));
    }

    #[test]
    fn h264_records_take_dimensions_from_the_sps_and_unwrap_the_clock() {
        // One frame through the house encoder, so the SPS is real.
        let (w, h) = (64usize, 48usize);
        let cfg = rusty_h264::EncoderConfig::new(w, h);
        let mut enc = rusty_h264::Encoder::new(cfg).unwrap();
        let frame = rusty_h264::YuvFrame {
            width: w,
            height: h,
            y: vec![100; w * h],
            u: vec![128; w * h / 4],
            v: vec![128; w * h / 4],
        };
        let mut au = enc.encode(&frame);
        au.extend(enc.flush());
        assert!(!au.is_empty());

        let mut stream = Vec::new();
        // Two frames straddling the 32-bit wrap of the RTP clock.
        write_frame(&mut stream, CODEC_H264, true, u32::MAX - 1000, &au);
        write_frame(&mut stream, CODEC_H264, false, 2000, &[0, 0, 0, 1, 0x41, 1]);
        let mut dem = RtpDemuxer::new(Box::new(Cursor::new(stream)));
        let streams = dem.read_header().unwrap();
        assert_eq!(streams[0].codec_id, CodecId::H264);
        assert_eq!((streams[0].width, streams[0].height), (64, 48));
        let a = dem.read_packet().unwrap();
        let b = dem.read_packet().unwrap();
        assert_eq!(a.pts, Some(0));
        assert_eq!(b.pts, Some(3001), "unwrapped across 2^32");
        assert!(a.flags.keyframe && !b.flags.keyframe);
    }

    #[test]
    fn truncated_record_is_an_error_not_a_panic() {
        let mut stream = Vec::new();
        write_frame(&mut stream, CODEC_JPEG, true, 1, &jpeg(16, 16));
        stream.truncate(stream.len() - 5);
        let mut dem = RtpDemuxer::new(Box::new(Cursor::new(stream)));
        assert!(dem.read_header().is_err());
    }
}
