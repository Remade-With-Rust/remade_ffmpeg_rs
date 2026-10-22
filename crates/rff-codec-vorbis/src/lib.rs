//! Vorbis audio **decoder** and **encoder**, both the in-house pure-Rust
//! [`rusty_vorbis`] (no C/FFI).
//!
//! Vorbis carries three setup headers (identification, comment, setup) before
//! any audio. The Ogg demuxer hands those to us packed (length-prefixed) as the
//! stream's `extradata`; [`configure`](rff_codec::Decoder::configure) builds a
//! [`rusty_vorbis::decode::Decoder`] from them, and each audio packet then
//! decodes to an interleaved `f32` [`AudioFrame`] — float, like FFmpeg's native
//! Vorbis decoder (`fltp`), not quantised to 16 bits. Channels come out in
//! FFmpeg's (WAV) order, not Vorbis's: see [`ffmpeg_channel_map`]. The stream's
//! last packet is trimmed so the output ends at the final page's granule
//! position (see [`Packet::end_position`]), dropping the encoder's end padding
//! as FFmpeg, libvorbis and lewton's Ogg reader do.
//!
//! Encode is the in-house Vorbis **encoder** ([`rusty_vorbis`], see
//! `docs/codec-vorbis-encoder.md`) — masking-driven floor, rate-distortion
//! residue, stereo coupling, `-q` control; this crate is a thin adapter mapping
//! the rff [`Encoder`] trait onto `rusty_vorbis`'s native API. The encoder emits
//! its three setup headers as its first packets (also via
//! [`VorbisEncoder::headers`]); the Ogg muxer pages them ahead of the audio packets.

use std::collections::VecDeque;

use rff_codec::{Codec, CodecParams, CodecRegistry, Decoder, Encoder};
use rff_core::{AudioFrame, Dictionary, Error, Frame, MediaType, Packet, Result, SampleFormat};

pub use rusty_vorbis;

/// Register the Vorbis codec (decode + encode) into a [`CodecRegistry`].
pub fn register(registry: &mut CodecRegistry) {
    registry.register(Codec {
        id: rff_core::CodecId::Vorbis,
        name: "vorbis",
        long_name: "Vorbis (Ogg Vorbis)",
        media_type: MediaType::Audio,
        decoder: Some(|| Box::new(VorbisDecoder::default())),
        encoder: Some(|| Box::new(VorbisEncoder::new())),
    });
}

/// Map a `rusty_vorbis` error onto the equivalent rff [`Error`], preserving the
/// EAGAIN-style `Again`/`Eof` flow-control variants.
fn map_err(e: rusty_vorbis::Error) -> Error {
    match e {
        rusty_vorbis::Error::Again => Error::Again,
        rusty_vorbis::Error::Eof => Error::Eof,
        rusty_vorbis::Error::Unimplemented(what) => Error::Unimplemented(what),
        rusty_vorbis::Error::InvalidData(msg) => Error::InvalidData(msg),
        rusty_vorbis::Error::Unsupported(msg) => Error::Unsupported(msg),
        other => Error::InvalidData(format!("rusty_vorbis: {other}")),
    }
}

/// Unpack length-prefixed (`u32 LE` + bytes) header blobs from `extradata`.
fn unpack_headers(data: &[u8]) -> Vec<&[u8]> {
    let mut out = Vec::new();
    let mut i = 0;
    while i + 4 <= data.len() {
        let len = u32::from_le_bytes([data[i], data[i + 1], data[i + 2], data[i + 3]]) as usize;
        i += 4;
        if len > data.len() - i {
            break;
        }
        out.push(&data[i..i + len]);
        i += len;
    }
    out
}

/// Vorbis → FFmpeg channel order: output channel `i` takes Vorbis channel
/// `map[i]`. Vorbis orders 3–8 channels with the centre second and the LFE
/// last (spec §4.3.9); FFmpeg/WAV put the centre third and the LFE fourth.
/// This is libavcodec's `ff_vorbis_channel_layout_offsets`, applied the way
/// `vorbisdec.c` does (`channel_ptrs[offsets[i]] = plane i`). 1, 2 and 4
/// channels are identical in both orders; beyond 8 Vorbis leaves the order
/// application-defined and FFmpeg keeps it as is.
fn ffmpeg_channel_map(channels: usize) -> Vec<usize> {
    let map: &[usize] = match channels {
        3 => &[0, 2, 1],
        5 => &[0, 2, 1, 3, 4],
        6 => &[0, 2, 1, 5, 3, 4],
        7 => &[0, 2, 1, 6, 5, 3, 4],
        8 => &[0, 2, 1, 7, 5, 6, 3, 4],
        _ => return (0..channels).collect(),
    };
    map.to_vec()
}

/// Interleave planar f32 into little-endian f32 bytes: output channel `i`
/// takes plane `map[i]`, and `out` holds `out.len() / (4 * map.len())` frames
/// (each plane must have at least that many samples).
fn interleave_into<'a>(plane: impl Fn(usize) -> &'a [f32], map: &[usize], out: &mut [u8]) {
    match *map {
        [c] => {
            for (o, s) in out.chunks_exact_mut(4).zip(plane(c)) {
                o.copy_from_slice(&s.to_le_bytes());
            }
        }
        [l, r] => {
            for ((o, a), b) in out.chunks_exact_mut(8).zip(plane(l)).zip(plane(r)) {
                o[..4].copy_from_slice(&a.to_le_bytes());
                o[4..].copy_from_slice(&b.to_le_bytes());
            }
        }
        _ => {
            let stride = map.len() * 4;
            for (k, &c) in map.iter().enumerate() {
                for (frame, s) in out.chunks_exact_mut(stride).zip(plane(c)) {
                    frame[k * 4..k * 4 + 4].copy_from_slice(&s.to_le_bytes());
                }
            }
        }
    }
}

/// How many of the `decoded` samples FFmpeg keeps from a packet, given the
/// stream `position` before it (both in FFmpeg's per-packet terms, see
/// [`Timeline::position`]). Only the stream's last packet is cut, by however far
/// it would run past the final granule — FFmpeg's Ogg end trimming
/// (`oggparsevorbis.c` + `decode.c`), which for an ordinary multi-page file is
/// also lewton's `OggStreamReader` logic. As in FFmpeg, padding longer than the
/// packet itself is not applied at all.
fn end_trim(position: i64, packet: &Packet, decoded: usize) -> usize {
    match packet.end_position {
        Some(end) if packet.flags.end_of_stream => {
            let excess = position.saturating_add(decoded as i64).saturating_sub(end);
            if excess > 0 && excess <= decoded as i64 {
                decoded - excess as usize
            } else {
                decoded
            }
        }
        _ => decoded,
    }
}

/// Where the stream is, in FFmpeg's terms, and how much to keep of each
/// packet so the output ends where FFmpeg's does.
struct Timeline {
    /// `blocksize_0`.
    bs0: i64,
    /// The previous audio packet's block size.
    prev_blocksize: i64,
    /// Stream position (granule units = samples) after the last packet, the
    /// way FFmpeg's Ogg demuxer counts it: the last page granule seen plus the
    /// packet durations since (`prev_blocksize/4 + blocksize/4` each). Before
    /// any granule it is counted from 0, crediting the first packet with
    /// `(blocksize_0 + its blocksize) / 4` although it decodes to nothing — so
    /// when all the audio is on one page, FFmpeg's output ends that much short
    /// of the granule, and so does ours.
    position: i64,
    /// Samples we have output minus the samples FFmpeg's decoder would have by
    /// now. Our decoder hands a long block's `(long - short)/4` tail out one
    /// packet early when a short block follows (see
    /// [`rusty_vorbis::decode::Decoder::packet_frames`]); FFmpeg's splits each
    /// packet at the window centres. The end trim is computed in FFmpeg's
    /// terms and corrected by this lead.
    lead: i64,
    /// No audio packet has been decoded yet.
    first: bool,
}

impl Timeline {
    fn new(bs0: i64) -> Self {
        Timeline {
            bs0,
            prev_blocksize: bs0,
            position: 0,
            lead: 0,
            first: true,
        }
    }

    /// Account for an audio packet of `blocksize` that decoded to `decoded`
    /// samples; returns how many of them to output.
    fn packet(&mut self, packet: &Packet, blocksize: i64, decoded: usize) -> usize {
        // The packet as FFmpeg sees it: its parser's duration, and what its
        // decoder outputs (nothing for the first packet).
        let (duration, ffmpeg_out) = if self.first {
            self.first = false;
            ((self.bs0 + blocksize) / 4, 0)
        } else {
            let d = (self.prev_blocksize + blocksize) / 4;
            (d, d)
        };
        self.prev_blocksize = blocksize;
        let samples = if packet.flags.end_of_stream {
            // End FFmpeg's way: its output, trimmed to the granule, less what
            // we have already output ahead of it.
            let keep = end_trim(self.position, packet, ffmpeg_out as usize) as i64;
            (keep - self.lead).clamp(0, decoded as i64) as usize
        } else {
            decoded
        };
        self.lead += samples as i64 - ffmpeg_out;
        self.position = packet
            .end_position
            .unwrap_or(self.position.saturating_add(duration));
        samples
    }
}

struct State {
    dec: rusty_vorbis::decode::Decoder,
    /// Output channel order (see [`ffmpeg_channel_map`]).
    map: Vec<usize>,
    timeline: Timeline,
    channels: u16,
    sample_rate: u32,
}

#[derive(Default)]
struct VorbisDecoder {
    state: Option<State>,
    queue: VecDeque<Frame>,
    eof: bool,
}

impl Decoder for VorbisDecoder {
    fn configure(&mut self, params: &CodecParams) -> Result<()> {
        let headers = unpack_headers(&params.extradata);
        let [ident, comment, setup, ..] = headers[..] else {
            return Err(Error::invalid(
                "vorbis: expected 3 setup headers in extradata",
            ));
        };
        let dec = rusty_vorbis::decode::Decoder::new(ident, comment, setup)
            .map_err(|e| Error::invalid(format!("vorbis headers: {e}")))?;
        let channels = dec.channels();
        self.state = Some(State {
            map: ffmpeg_channel_map(channels),
            timeline: Timeline::new(1i64 << dec.ident().blocksize_0),
            channels: channels as u16,
            sample_rate: dec.sample_rate(),
            dec,
        });
        Ok(())
    }

    fn send_packet(&mut self, packet: &Packet) -> Result<()> {
        let st = self
            .state
            .as_mut()
            .ok_or_else(|| Error::invalid("vorbis decode: not configured"))?;

        if packet.data.is_empty() {
            // A zero-length packet carries no audio (and does not prime the
            // overlap): only its granule, if any, moves the position.
            if let Some(end) = packet.end_position {
                st.timeline.position = end;
            }
            return Ok(());
        }
        let decoded = st
            .dec
            .decode(&packet.data)
            .map_err(|e| Error::invalid(format!("vorbis decode: {e}")))?;
        // A packet that decoded has a well-formed header.
        let blocksize =
            st.dec
                .packet_blocksize(&packet.data)
                .map_err(|e| Error::invalid(format!("vorbis decode: {e}")))? as i64;
        let samples = st.timeline.packet(packet, blocksize, decoded);
        if samples == 0 {
            return Ok(()); // some packets carry no output samples
        }

        // The decoder holds planar f32 in Vorbis channel order; interleave
        // straight into the frame's bytes, in FFmpeg's order.
        let mut interleaved = vec![0u8; samples * st.map.len() * 4];
        let dec = &st.dec;
        interleave_into(|c| dec.channel(c), &st.map, &mut interleaved);
        self.queue.push_back(Frame::Audio(AudioFrame {
            sample_rate: st.sample_rate,
            channels: st.channels,
            format: SampleFormat::F32,
            planes: vec![interleaved],
            samples,
            pts: packet.pts,
        }));
        Ok(())
    }

    fn receive_frame(&mut self) -> Result<Frame> {
        if let Some(frame) = self.queue.pop_front() {
            return Ok(frame);
        }
        if self.eof {
            Err(Error::Eof)
        } else {
            Err(Error::Again)
        }
    }

    fn flush(&mut self) {
        self.eof = true;
    }
}

// ---------------------------------------------------------------------------
// Encoder — a thin wrapper over rusty_vorbis's native streaming encoder.
// ---------------------------------------------------------------------------

/// The rff-trait Vorbis encoder, delegating to [`rusty_vorbis::VorbisEncoder`].
/// The header/audio packet stream (three setup headers first, then audio with
/// granule pts) is the inner encoder's, unchanged.
pub struct VorbisEncoder {
    inner: rusty_vorbis::VorbisEncoder,
}

impl VorbisEncoder {
    pub fn new() -> Self {
        VorbisEncoder {
            inner: rusty_vorbis::VorbisEncoder::default(),
        }
    }

    /// The three Vorbis setup headers for the configured stream.
    pub fn headers(&self) -> Vec<Vec<u8>> {
        self.inner.headers()
    }

    /// The three setup headers packed as length-prefixed `extradata` (`u32 LE len + bytes`
    /// each) — the format the Ogg muxer and the Vorbis decoder both use.
    pub fn extradata(&self) -> Vec<u8> {
        self.inner.extradata()
    }
}

impl Default for VorbisEncoder {
    fn default() -> Self {
        VorbisEncoder::new()
    }
}

impl Encoder for VorbisEncoder {
    fn configure(&mut self, options: &Dictionary) -> Result<()> {
        if let Some(b) = options.get_int("b") {
            if b > 0 {
                self.inner.set_bitrate_bps(b as i32);
            }
        }
        // `-q:a` / `-qscale:a` (Vorbis quality, −1..=10) takes precedence when present.
        if let Some(q) = options.get_int("q").or_else(|| options.get_int("qscale")) {
            self.inner
                .set_quality(rusty_vorbis::quality01_from_vorbis_q(q as f64));
        }
        Ok(())
    }

    fn send_frame(&mut self, frame: &Frame) -> Result<()> {
        let Frame::Audio(a) = frame else {
            return Err(Error::invalid("vorbis encode: expected an audio frame"));
        };
        let ch = a.channels.max(1) as usize;
        let n = a.samples;
        // Convert to the typed interleaved slice — the same sample math the encoder
        // used when it ingested rff frames directly, so output stays byte-identical.
        let enc = match a.format {
            SampleFormat::S16 => {
                let d = &a.planes[0];
                let mut pcm = Vec::with_capacity(n * ch);
                for i in 0..n * ch {
                    let o = i * 2;
                    pcm.push(i16::from_le_bytes([d[o], d[o + 1]]));
                }
                self.inner.push_pcm_s16(&pcm, a.channels, a.sample_rate)
            }
            SampleFormat::F32 => {
                let d = &a.planes[0];
                let mut pcm = Vec::with_capacity(n * ch);
                for i in 0..n * ch {
                    let o = i * 4;
                    pcm.push(f32::from_le_bytes([d[o], d[o + 1], d[o + 2], d[o + 3]]));
                }
                self.inner.push_pcm_f32(&pcm, a.channels, a.sample_rate)
            }
            _ => return Err(Error::invalid("vorbis encode: unsupported sample format")),
        };
        enc.map_err(map_err)
    }

    fn receive_packet(&mut self) -> Result<Packet> {
        let ep = self.inner.next_packet().map_err(map_err)?;
        let mut pkt = Packet::from_data(0, ep.data);
        pkt.pts = Some(ep.pts);
        pkt.duration = ep.duration;
        Ok(pkt)
    }

    fn flush(&mut self) {
        self.inner.finish();
    }
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn registers_vorbis_codec() {
        let mut reg = CodecRegistry::new();
        register(&mut reg);
        assert!(reg.find_decoder(rff_core::CodecId::Vorbis).is_ok());
        assert!(reg.find_encoder(rff_core::CodecId::Vorbis).is_ok());
    }

    #[test]
    fn unpack_headers_roundtrips() {
        let mut packed = Vec::new();
        for h in [b"abc".as_slice(), b"", b"defgh"] {
            packed.extend_from_slice(&(h.len() as u32).to_le_bytes());
            packed.extend_from_slice(h);
        }
        let out = unpack_headers(&packed);
        assert_eq!(out, [b"abc".as_slice(), b"", b"defgh"]);
        // A truncated trailing blob is dropped, not read past the end.
        packed.extend_from_slice(&9u32.to_le_bytes());
        packed.extend_from_slice(b"xy");
        assert_eq!(unpack_headers(&packed).len(), 3);
    }

    #[test]
    fn configure_rejects_missing_headers() {
        let mut dec = VorbisDecoder::default();
        assert!(dec.configure(&CodecParams::default()).is_err());
    }

    /// Encode `n` frames of a stereo tone with the in-house encoder; returns the
    /// packed extradata and the audio packets.
    fn encoded_tone(n: usize) -> (Vec<u8>, Vec<Packet>) {
        let mut enc = VorbisEncoder::new();
        let mut plane = Vec::with_capacity(n * 2 * 4);
        for i in 0..n {
            for f in [0.02f32, 0.031] {
                plane.extend_from_slice(&(0.4 * (f * i as f32).sin()).to_le_bytes());
            }
        }
        enc.send_frame(&Frame::Audio(AudioFrame {
            sample_rate: 44_100,
            channels: 2,
            format: SampleFormat::F32,
            planes: vec![plane],
            samples: n,
            pts: Some(0),
        }))
        .unwrap();
        enc.flush();
        let mut packets = Vec::new();
        while let Ok(p) = enc.receive_packet() {
            packets.push(p);
        }
        // The first three packets are the setup headers.
        let audio = packets.split_off(3);
        (enc.extradata(), audio)
    }

    fn decode_all(extradata: &[u8], packets: &[Packet]) -> Vec<AudioFrame> {
        let mut dec = VorbisDecoder::default();
        dec.configure(&CodecParams {
            extradata: extradata.to_vec(),
            ..Default::default()
        })
        .unwrap();
        let mut out = Vec::new();
        for p in packets {
            dec.send_packet(p).unwrap();
            while let Ok(Frame::Audio(af)) = dec.receive_frame() {
                out.push(af);
            }
        }
        dec.flush();
        while let Ok(Frame::Audio(af)) = dec.receive_frame() {
            out.push(af);
        }
        out
    }

    /// Output is float, as FFmpeg's decoder's is: not quantised to 16 bits.
    #[test]
    fn decodes_to_unquantised_f32() {
        let (extradata, packets) = encoded_tone(8192);
        let frames = decode_all(&extradata, &packets);
        assert!(!frames.is_empty());
        let mut off_grid = 0usize;
        for af in &frames {
            assert_eq!(af.format, SampleFormat::F32);
            assert_eq!(af.planes[0].len(), af.samples * af.channels as usize * 4);
            for b in af.planes[0].chunks_exact(4) {
                let s = f32::from_le_bytes([b[0], b[1], b[2], b[3]]);
                assert!(s.is_finite() && s.abs() <= 1.5, "{s}");
                if (s * 32768.0).fract() != 0.0 {
                    off_grid += 1;
                }
            }
        }
        assert!(off_grid > 1000, "only {off_grid} samples off the s16 grid");
    }

    /// FFmpeg order for every channel count Vorbis defines (checked against
    /// libavcodec's `ff_vorbis_channel_layout_offsets` and an FFmpeg 8.1 decode
    /// of a 5.1 file with a distinct tone per channel).
    #[test]
    fn channel_map_matches_ffmpeg() {
        assert_eq!(ffmpeg_channel_map(1), [0]);
        assert_eq!(ffmpeg_channel_map(2), [0, 1]);
        assert_eq!(ffmpeg_channel_map(3), [0, 2, 1]);
        assert_eq!(ffmpeg_channel_map(4), [0, 1, 2, 3]);
        assert_eq!(ffmpeg_channel_map(5), [0, 2, 1, 3, 4]);
        assert_eq!(ffmpeg_channel_map(6), [0, 2, 1, 5, 3, 4]);
        assert_eq!(ffmpeg_channel_map(7), [0, 2, 1, 6, 5, 3, 4]);
        assert_eq!(ffmpeg_channel_map(8), [0, 2, 1, 7, 5, 6, 3, 4]);
        assert_eq!(ffmpeg_channel_map(9), (0..9).collect::<Vec<_>>());
        for n in 1..=8 {
            let mut m = ffmpeg_channel_map(n);
            m.sort_unstable();
            assert_eq!(m, (0..n).collect::<Vec<_>>(), "{n}ch map is a permutation");
        }
    }

    /// 5.1: Vorbis FL FC FR RL RR LFE → FFmpeg FL FR FC LFE BL BR.
    #[test]
    fn interleave_reorders_5_1_to_ffmpeg_order() {
        // Plane k holds the constant k (FL=0, FC=1, FR=2, RL=3, RR=4, LFE=5).
        let pcm: Vec<Vec<f32>> = (0..6).map(|k| vec![k as f32; 2]).collect();
        let mut bytes = vec![0u8; 2 * 6 * 4];
        interleave_into(|c| &pcm[c], &ffmpeg_channel_map(6), &mut bytes);
        let got: Vec<f32> = bytes
            .chunks_exact(4)
            .map(|b| f32::from_le_bytes([b[0], b[1], b[2], b[3]]))
            .collect();
        let frame = [0.0, 2.0, 1.0, 5.0, 3.0, 4.0]; // FL FR FC LFE BL BR
        assert_eq!(got, [frame, frame].concat());
    }

    /// The 1- and 2-channel fast paths interleave the same as the general one.
    #[test]
    fn interleave_mono_and_stereo() {
        let pcm: Vec<Vec<f32>> = vec![vec![1.0, 2.0, 3.0], vec![-1.0, -2.0, -3.0]];
        let floats = |b: &[u8]| -> Vec<f32> {
            b.chunks_exact(4)
                .map(|b| f32::from_le_bytes([b[0], b[1], b[2], b[3]]))
                .collect()
        };
        let mut mono = vec![0u8; 2 * 4];
        interleave_into(|c| &pcm[c], &[0], &mut mono);
        assert_eq!(floats(&mono), [1.0, 2.0]);
        let mut stereo = vec![0u8; 3 * 2 * 4];
        interleave_into(|c| &pcm[c], &[0, 1], &mut stereo);
        assert_eq!(floats(&stereo), [1.0, -1.0, 2.0, -2.0, 3.0, -3.0]);
    }

    /// A zero-length packet yields nothing and does not use up the first
    /// (priming) packet: the output is the same as without it.
    #[test]
    fn empty_packets_are_ignored() {
        let (extradata, packets) = encoded_tone(4096);
        let mut with_empty = vec![Packet::from_data(0, Vec::new())];
        for p in &packets {
            with_empty.push(p.clone());
            with_empty.push(Packet::from_data(0, Vec::new()));
        }
        let flat = |fs: Vec<AudioFrame>| -> Vec<u8> {
            fs.into_iter().flat_map(|f| f.planes[0].clone()).collect()
        };
        assert_eq!(
            flat(decode_all(&extradata, &with_empty)),
            flat(decode_all(&extradata, &packets))
        );
    }

    #[test]
    fn end_trim_cuts_only_the_final_packet() {
        let mut p = Packet::from_data(0, Vec::new());
        // Mid-stream: never cut.
        assert_eq!(end_trim(1000, &p, 512), 512);
        p.end_position = Some(1200);
        assert_eq!(end_trim(1000, &p, 512), 512);
        // Final packet: cut back to the granule.
        p.flags.end_of_stream = true;
        assert_eq!(end_trim(1000, &p, 512), 200);
        assert_eq!(end_trim(1000, &p, 100), 100, "never extends");
        assert_eq!(end_trim(1200, &p, 512), 0, "padding == packet: all of it");
        // Padding longer than the packet: FFmpeg's decode.c ignores it.
        assert_eq!(end_trim(1300, &p, 512), 512);
        // No granule on the final packet: nothing to trim to.
        p.end_position = None;
        assert_eq!(end_trim(1000, &p, 512), 512);
    }

    /// Lay `packets` out as Ogg would: every `per_page`-th packet (and the
    /// last) completes a page and carries the granule after it; the last is
    /// end-of-stream with `final_granule`.
    fn paginate(packets: &mut [Packet], sizes: &[usize], per_page: usize, final_granule: i64) {
        let mut granule = 0i64;
        let n = packets.len();
        for (i, (p, &size)) in packets.iter_mut().zip(sizes).enumerate() {
            granule += size as i64;
            if i + 1 == n {
                p.end_position = Some(final_granule);
                p.flags.end_of_stream = true;
            } else if (i + 1) % per_page == 0 {
                p.end_position = Some(granule);
            }
        }
    }

    #[test]
    fn final_packet_is_trimmed_to_the_last_granule() {
        let (extradata, mut packets) = encoded_tone(10_000);
        let untrimmed = decode_all(&extradata, &packets);
        let total: usize = untrimmed.iter().map(|f| f.samples).sum();
        // Per-packet granule durations, `prev_blocksize/4 + blocksize/4` (the
        // first packet adds none) — how an Ogg muxer counts.
        let st = {
            let mut d = VorbisDecoder::default();
            d.configure(&CodecParams {
                extradata: extradata.clone(),
                ..Default::default()
            })
            .unwrap();
            d.state.unwrap()
        };
        let blocksizes: Vec<usize> = packets
            .iter()
            .map(|p| st.dec.packet_blocksize(&p.data).unwrap())
            .collect();
        let mut sizes = vec![0];
        sizes.extend(blocksizes.windows(2).map(|w| (w[0] + w[1]) / 4));
        let granule_total: usize = sizes.iter().sum();
        assert!(granule_total <= total, "{granule_total} > {total}");
        let last = *sizes.last().unwrap();
        assert!(last > 100 && total > 4096, "last {last} total {total}");

        // Multi-page: output stops exactly at the final granule.
        let want = granule_total - 100;
        paginate(&mut packets, &sizes, 4, want as i64);
        let frames = decode_all(&extradata, &packets);
        let got: usize = frames.iter().map(|f| f.samples).sum();
        assert_eq!(got, want);
        // The kept samples are the untrimmed ones, unchanged.
        let flat = |fs: &[AudioFrame]| {
            fs.iter()
                .flat_map(|f| f.planes[0].clone())
                .collect::<Vec<u8>>()
        };
        assert_eq!(flat(&frames)[..], flat(&untrimmed)[..want * 2 * 4]);

        // One page (only the last packet has a granule): FFmpeg counts from 0
        // crediting the first packet with (bs0 + its blocksize) / 4 samples it
        // never outputs, so it ends that much short of the granule.
        for p in packets.iter_mut() {
            p.end_position = None;
        }
        packets.last_mut().unwrap().end_position = Some(want as i64);
        let ident = st.dec.ident();
        let (bs0, bs1) = (1i64 << ident.blocksize_0, 1i64 << ident.blocksize_1);
        let phantom = (bs0 + blocksizes[0] as i64) / 4;
        assert!(
            phantom == bs0 / 2 || phantom == (bs0 + bs1) / 4,
            "{phantom}"
        );
        let got: usize = decode_all(&extradata, &packets)
            .iter()
            .map(|f| f.samples)
            .sum();
        assert_eq!(got as i64, want as i64 - phantom);
    }

    /// A page boundary just after a long block that a short one follows: our
    /// decoder has already output that block's `(long - short)/4` tail, which
    /// FFmpeg's decoder (and the granule) only count with the next packet.
    /// The end trim must still land on the final granule.
    #[test]
    fn end_trim_accounts_for_the_early_long_block_tail() {
        // A 48 kHz stream (bs 256/2048) with blocks S S L L S S: FFmpeg's
        // durations 0(+128 credited), 128, 576, 1024, 576, 128, granule 1728
        // after the 4th packet and 2400 on the last (32 samples of end
        // padding). Our decoder outputs 0, 128, 576, 1472, 128, 128 — 448
        // ahead of FFmpeg after the 4th. Taken from an FFmpeg 8.1 libvorbis
        // encode (`-page_duration 20000`), which FFmpeg decodes to 2400.
        let blocks = [256i64, 256, 2048, 2048, 256, 256];
        let decoded = [0usize, 128, 576, 1472, 128, 128];
        let ends = [None, None, None, Some(1728), None, Some(2400)];
        let mut t = Timeline::new(256);
        let mut kept = Vec::new();
        for i in 0..6 {
            let mut p = Packet::from_data(0, Vec::new());
            p.end_position = ends[i];
            p.flags.end_of_stream = i == 5;
            kept.push(t.packet(&p, blocks[i], decoded[i]));
        }
        assert_eq!(kept, [0, 128, 576, 1472, 128, 96]);
        assert_eq!(kept.iter().sum::<usize>(), 2400);

        // Same stream, but the last block is long and announces a short one
        // that never comes: its early tail is not in FFmpeg's output either.
        let blocks = [256i64, 256, 2048, 2048, 2048];
        let decoded = [0usize, 128, 576, 1024, 1472];
        let ends = [None, None, None, Some(1728), Some(2752)];
        let mut t = Timeline::new(256);
        let mut kept = 0;
        for i in 0..5 {
            let mut p = Packet::from_data(0, Vec::new());
            p.end_position = ends[i];
            p.flags.end_of_stream = i == 4;
            kept += t.packet(&p, blocks[i], decoded[i]);
        }
        assert_eq!(kept, 2752);
    }

    #[test]
    fn decode_before_configure_errors() {
        let mut dec = VorbisDecoder::default();
        assert!(dec.send_packet(&Packet::from_data(0, vec![0; 4])).is_err());
    }
}
