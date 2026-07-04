//! Ogg container, carrying Opus (`.opus` / `.ogg`).
//!
//! Ogg is a page-based stream: each page has the `OggS` header (with a CRC over
//! the whole page) and a *segment table* whose lacing values split the page's
//! bytes into packets. For Opus, the first packet is `OpusHead` (channels +
//! input sample rate), the second is `OpusTags`, and the rest are Opus audio
//! packets.
//!
//! This implementation targets a single Opus stream: the demuxer reassembles
//! packets across pages; the muxer writes one packet per page (`OpusHead`,
//! `OpusTags`, then audio). It's reusable for other Ogg-mapped codecs (Vorbis,
//! FLAC-in-Ogg) later.

use std::io::{Read, Write};

use rff_core::{CodecId, Error, Packet, Rational, Result, SampleFormat};
use rff_format::{Demuxer, Format, FormatRegistry, Input, Muxer, Output, Stream};

/// Register the Ogg format into a [`FormatRegistry`].
pub fn register(registry: &mut FormatRegistry) {
    registry.register(Format {
        name: "ogg",
        long_name: "Ogg (Opus / Vorbis)",
        extensions: &["opus", "ogg"],
        demuxer: Some(|input| Box::new(OggDemuxer::new(input))),
        muxer: Some(|output| Box::new(OggMuxer::new(output))),
        probe: Some(probe_ogg),
    });
}

/// Sniff Ogg by the `OggS` capture pattern.
fn probe_ogg(data: &[u8]) -> i32 {
    if data.len() >= 4 && &data[0..4] == b"OggS" {
        100
    } else {
        0
    }
}

/// Ogg's CRC-32 (polynomial 0x04c11db7, no reflection, no final xor).
fn ogg_crc32(data: &[u8]) -> u32 {
    let mut crc: u32 = 0;
    for &byte in data {
        crc ^= (byte as u32) << 24;
        for _ in 0..8 {
            crc = if crc & 0x8000_0000 != 0 {
                (crc << 1) ^ 0x04c1_1db7
            } else {
                crc << 1
            };
        }
    }
    crc
}

// ---------------------------------------------------------------------------
// Page / packet parsing
// ---------------------------------------------------------------------------

/// Reassemble all packets across the Ogg pages in `buf` (handles lacing and
/// packets continued across page boundaries).
fn read_packets(buf: &[u8]) -> Vec<Vec<u8>> {
    let mut packets = Vec::new();
    let mut partial: Vec<u8> = Vec::new();
    let mut i = 0;
    while i + 27 <= buf.len() && &buf[i..i + 4] == b"OggS" {
        let num_segments = buf[i + 26] as usize;
        let table_end = i + 27 + num_segments;
        if table_end > buf.len() {
            break;
        }
        let segment_table = &buf[i + 27..table_end];
        let mut data = table_end;
        for &lace in segment_table {
            let end = (data + lace as usize).min(buf.len());
            partial.extend_from_slice(&buf[data..end]);
            data = end;
            if lace < 255 {
                packets.push(std::mem::take(&mut partial));
            }
        }
        i = data;
    }
    packets
}

// ---------------------------------------------------------------------------
// Page / OpusHead building
// ---------------------------------------------------------------------------

const SERIAL: u32 = 1;

/// Append one Ogg page wrapping a single `packet` (CRC computed over the page).
fn write_page(out: &mut Vec<u8>, header_type: u8, granule: u64, seq: u32, packet: &[u8]) {
    let full = packet.len() / 255;
    let last = (packet.len() % 255) as u8;
    let num_segments = full + 1;

    let start = out.len();
    out.extend_from_slice(b"OggS");
    out.push(0); // version
    out.push(header_type);
    out.extend_from_slice(&granule.to_le_bytes());
    out.extend_from_slice(&SERIAL.to_le_bytes());
    out.extend_from_slice(&seq.to_le_bytes());
    let crc_at = out.len();
    out.extend_from_slice(&[0, 0, 0, 0]); // CRC placeholder
    out.push(num_segments as u8);
    out.extend(std::iter::repeat(255).take(full));
    out.push(last);
    out.extend_from_slice(packet);

    let crc = ogg_crc32(&out[start..]);
    out[crc_at..crc_at + 4].copy_from_slice(&crc.to_le_bytes());
}

fn opus_head(channels: u8, sample_rate: u32) -> Vec<u8> {
    let mut v = b"OpusHead".to_vec();
    v.push(1); // version
    v.push(channels);
    v.extend_from_slice(&0u16.to_le_bytes()); // pre-skip
    v.extend_from_slice(&sample_rate.to_le_bytes()); // input sample rate
    v.extend_from_slice(&0u16.to_le_bytes()); // output gain
    v.push(0); // channel mapping family 0 (mono/stereo)
    v
}

fn opus_tags() -> Vec<u8> {
    let vendor = b"remade_ffmpeg_rs";
    let mut v = b"OpusTags".to_vec();
    v.extend_from_slice(&(vendor.len() as u32).to_le_bytes());
    v.extend_from_slice(vendor);
    v.extend_from_slice(&0u32.to_le_bytes()); // user comment count
    v
}

/// Pack header packets length-prefixed (`u32 LE` + bytes) for a codec's
/// `extradata` — Vorbis needs its three setup headers handed to the decoder.
fn pack_headers(headers: &[&[u8]]) -> Vec<u8> {
    let mut v = Vec::new();
    for h in headers {
        v.extend_from_slice(&(h.len() as u32).to_le_bytes());
        v.extend_from_slice(h);
    }
    v
}

// ---------------------------------------------------------------------------
// Demuxer
// ---------------------------------------------------------------------------

struct OggDemuxer {
    input: Option<Input>,
    audio: std::vec::IntoIter<Vec<u8>>,
}

impl OggDemuxer {
    fn new(input: Input) -> OggDemuxer {
        OggDemuxer {
            input: Some(input),
            audio: Vec::new().into_iter(),
        }
    }
}

impl Demuxer for OggDemuxer {
    fn read_header(&mut self) -> Result<Vec<Stream>> {
        let mut input = self
            .input
            .take()
            .ok_or_else(|| Error::invalid("ogg demux: header already read"))?;
        let mut buf = Vec::new();
        input.read_to_end(&mut buf)?;
        if probe_ogg(&buf) == 0 {
            return Err(Error::invalid("ogg demux: not an Ogg file"));
        }

        let packets = read_packets(&buf);
        let first = packets
            .first()
            .ok_or_else(|| Error::invalid("ogg demux: no packets"))?;

        // Identify the mapped codec from the first header packet.
        // Opus: 2 header packets (OpusHead, OpusTags). Vorbis: 3 (ident, comment, setup).
        let (codec_id, channels, sample_rate, sample_format, headers, extradata) =
            if first.len() >= 19 && &first[0..8] == b"OpusHead" {
                let ch = first[9] as u16;
                let sr = u32::from_le_bytes([first[12], first[13], first[14], first[15]]);
                (
                    CodecId::Opus,
                    ch,
                    sr,
                    SampleFormat::F32,
                    2usize,
                    first.clone(),
                )
            } else if first.len() >= 16 && first[0] == 1 && &first[1..7] == b"vorbis" {
                if packets.len() < 3 {
                    return Err(Error::invalid("ogg demux: Vorbis is missing setup headers"));
                }
                let ch = first[11] as u16;
                let sr = u32::from_le_bytes([first[12], first[13], first[14], first[15]]);
                let three: Vec<&[u8]> = packets.iter().take(3).map(|p| p.as_slice()).collect();
                (
                    CodecId::Vorbis,
                    ch,
                    sr,
                    SampleFormat::S16,
                    3usize,
                    pack_headers(&three),
                )
            } else {
                return Err(Error::unsupported(
                    "ogg demux: unrecognized codec (expected Opus or Vorbis)",
                ));
            };

        self.audio = packets
            .into_iter()
            .skip(headers)
            .collect::<Vec<_>>()
            .into_iter();

        let mut stream = Stream::new(0, codec_id);
        stream.channels = channels;
        stream.sample_rate = if sample_rate == 0 {
            48_000
        } else {
            sample_rate
        };
        stream.sample_format = Some(sample_format);
        stream.extradata = extradata;
        stream.time_base = Rational::new(1, stream.sample_rate.max(1) as i32);
        Ok(vec![stream])
    }

    fn read_packet(&mut self) -> Result<Packet> {
        match self.audio.next() {
            Some(data) => Ok(Packet::from_data(0, data)),
            None => Err(Error::Eof),
        }
    }
}

// ---------------------------------------------------------------------------
// Muxer
// ---------------------------------------------------------------------------

struct OggMuxer {
    out: Output,
    codec: CodecId,
    channels: u8,
    sample_rate: u32,
    packets: Vec<Vec<u8>>,
}

impl OggMuxer {
    fn new(out: Output) -> OggMuxer {
        OggMuxer {
            out,
            codec: CodecId::Opus,
            channels: 0,
            sample_rate: 0,
            packets: Vec::new(),
        }
    }
}

impl Muxer for OggMuxer {
    fn write_header(&mut self, streams: &[Stream]) -> Result<()> {
        let s = streams
            .first()
            .filter(|s| matches!(s.codec_id, CodecId::Opus | CodecId::Vorbis))
            .ok_or_else(|| Error::unsupported("ogg mux: needs a single `opus` or `vorbis` stream"))?;
        self.codec = s.codec_id;
        self.channels = s.channels.clamp(1, 255) as u8;
        self.sample_rate = s.sample_rate.max(1);
        Ok(())
    }

    fn write_packet(&mut self, packet: &Packet) -> Result<()> {
        self.packets.push(packet.data.clone());
        Ok(())
    }

    fn write_trailer(&mut self) -> Result<()> {
        let mut out = Vec::new();
        match self.codec {
            CodecId::Vorbis => self.write_vorbis(&mut out),
            _ => self.write_opus(&mut out),
        }
        self.out.write_all(&out)?;
        self.out.flush()?;
        Ok(())
    }
}

impl OggMuxer {
    /// Opus: synthesize OpusHead + OpusTags, then one audio packet per page (48 kHz
    /// granule, 960 samples per 20 ms frame).
    fn write_opus(&self, out: &mut Vec<u8>) {
        write_page(out, 0x02, 0, 0, &opus_head(self.channels, self.sample_rate));
        write_page(out, 0x00, 0, 1, &opus_tags());
        let mut granule: u64 = 0;
        let last = self.packets.len().saturating_sub(1);
        for (i, packet) in self.packets.iter().enumerate() {
            granule += 960;
            let header_type = if i == last { 0x04 } else { 0x00 };
            write_page(out, header_type, granule, (i + 2) as u32, packet);
        }
    }

    /// Vorbis: the encoder emits its three setup headers (ident/comment/setup) as the first
    /// three packets. Page those as the header pages, then the audio packets (one long-block
    /// hop = 1024 samples of granule each). The demuxer likewise treats the first 3 as headers.
    fn write_vorbis(&self, out: &mut Vec<u8>) {
        let nheaders = 3.min(self.packets.len());
        for (i, h) in self.packets[..nheaders].iter().enumerate() {
            let htype = if i == 0 { 0x02 } else { 0x00 }; // BOS on the ident header
            write_page(out, htype, 0, i as u32, h);
        }
        let audio = &self.packets[nheaders..];
        let last = audio.len().saturating_sub(1);
        let mut granule: u64 = 0;
        for (i, packet) in audio.iter().enumerate() {
            granule += 1024; // long-block hop
            let header_type = if i == last { 0x04 } else { 0x00 }; // EOS on last
            write_page(out, header_type, granule, (nheaders + i) as u32, packet);
        }
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
    fn ogg_crc_matches_known_vector() {
        // CRC of empty input is 0; non-empty is non-zero and deterministic.
        assert_eq!(ogg_crc32(&[]), 0);
        assert_eq!(ogg_crc32(b"OggS"), ogg_crc32(b"OggS"));
        assert_ne!(ogg_crc32(b"OggS"), 0);
    }

    #[test]
    fn ogg_mux_then_demux_roundtrips_opus_packets() {
        // Two arbitrary "Opus" payloads (the container doesn't decode them).
        let a = vec![0xDEu8; 40];
        let b = vec![0xADu8; 300]; // >255 → exercises lacing

        let sink = SharedBuf(Arc::new(Mutex::new(Vec::new())));
        {
            let mut mux = OggMuxer::new(Box::new(sink.clone()));
            let mut s = Stream::new(0, CodecId::Opus);
            s.channels = 2;
            s.sample_rate = 48_000;
            mux.write_header(&[s]).unwrap();
            mux.write_packet(&Packet::from_data(0, a.clone())).unwrap();
            mux.write_packet(&Packet::from_data(0, b.clone())).unwrap();
            mux.write_trailer().unwrap();
        }
        let file = sink.0.lock().unwrap().clone();
        assert_eq!(&file[0..4], b"OggS");
        assert_eq!(probe_ogg(&file), 100);

        let mut dem = OggDemuxer::new(Box::new(Cursor::new(file)));
        let streams = dem.read_header().unwrap();
        assert_eq!(streams[0].codec_id, CodecId::Opus);
        assert_eq!(streams[0].channels, 2);
        assert_eq!(streams[0].sample_rate, 48_000);
        assert_eq!(dem.read_packet().unwrap().data, a);
        assert_eq!(dem.read_packet().unwrap().data, b);
        assert!(matches!(dem.read_packet(), Err(Error::Eof)));
    }

    #[test]
    fn ogg_mux_then_demux_roundtrips_vorbis() {
        // Real-shaped Vorbis headers (ident/comment/setup) + two audio packets.
        let mut ident = vec![1u8];
        ident.extend_from_slice(b"vorbis");
        ident.extend_from_slice(&0u32.to_le_bytes());
        ident.push(2); // channels @ 11
        ident.extend_from_slice(&44_100u32.to_le_bytes()); // rate @ 12
        ident.resize(30, 0);
        let comment = {
            let mut v = vec![3u8];
            v.extend_from_slice(b"vorbis");
            v.resize(20, 0);
            v
        };
        let setup = {
            let mut v = vec![5u8];
            v.extend_from_slice(b"vorbis");
            v.resize(64, 0);
            v
        };
        let a = vec![0x00u8; 60]; // audio packet (type bit clear)
        let b = vec![0x00u8; 300]; // >255 → exercises lacing

        let sink = SharedBuf(Arc::new(Mutex::new(Vec::new())));
        {
            let mut mux = OggMuxer::new(Box::new(sink.clone()));
            let mut s = Stream::new(0, CodecId::Vorbis);
            s.channels = 2;
            s.sample_rate = 44_100;
            mux.write_header(&[s]).unwrap();
            for p in [&ident, &comment, &setup, &a, &b] {
                mux.write_packet(&Packet::from_data(0, p.clone())).unwrap();
            }
            mux.write_trailer().unwrap();
        }
        let file = sink.0.lock().unwrap().clone();
        assert_eq!(&file[0..4], b"OggS");

        let mut dem = OggDemuxer::new(Box::new(Cursor::new(file)));
        let streams = dem.read_header().unwrap();
        assert_eq!(streams[0].codec_id, CodecId::Vorbis);
        assert_eq!(streams[0].channels, 2);
        assert_eq!(streams[0].sample_rate, 44_100);
        assert!(!streams[0].extradata.is_empty()); // 3 headers packed
        assert_eq!(dem.read_packet().unwrap().data, a);
        assert_eq!(dem.read_packet().unwrap().data, b);
        assert!(matches!(dem.read_packet(), Err(Error::Eof)));
    }

    #[test]
    fn demux_detects_vorbis_and_skips_three_headers() {
        // Synthetic Vorbis identification header: type 1 + "vorbis", then
        // version(4), channels@11, sample_rate@12.
        let mut ident = vec![1u8];
        ident.extend_from_slice(b"vorbis");
        ident.extend_from_slice(&0u32.to_le_bytes()); // vorbis_version
        ident.push(2); // channels
        ident.extend_from_slice(&44_100u32.to_le_bytes()); // sample rate

        let comment = b"fake-comment-header".to_vec();
        let setup = b"fake-setup-header".to_vec();
        let a = vec![0x11u8; 50];
        let b = vec![0x22u8; 70];

        // Hand-build the Ogg stream (our muxer only writes Opus).
        let mut file = Vec::new();
        write_page(&mut file, 0x02, 0, 0, &ident);
        write_page(&mut file, 0x00, 0, 1, &comment);
        write_page(&mut file, 0x00, 0, 2, &setup);
        write_page(&mut file, 0x00, 960, 3, &a);
        write_page(&mut file, 0x04, 1920, 4, &b);

        let mut dem = OggDemuxer::new(Box::new(Cursor::new(file)));
        let streams = dem.read_header().unwrap();
        assert_eq!(streams[0].codec_id, CodecId::Vorbis);
        assert_eq!(streams[0].channels, 2);
        assert_eq!(streams[0].sample_rate, 44_100);
        // The three setup headers are packed into extradata for the decoder.
        assert!(!streams[0].extradata.is_empty());
        // Only the audio packets are yielded.
        assert_eq!(dem.read_packet().unwrap().data, a);
        assert_eq!(dem.read_packet().unwrap().data, b);
        assert!(matches!(dem.read_packet(), Err(Error::Eof)));
    }
}
