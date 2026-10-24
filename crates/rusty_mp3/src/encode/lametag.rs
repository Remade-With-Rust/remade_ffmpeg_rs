//! The **LAME-style Info/Xing tag frame** and the gapless arithmetic behind it.
//!
//! An MP3 stream cannot say where its audio starts or ends: the encoder's
//! analysis filterbank and MDCT delay the signal, and a frame always carries a
//! whole `samples_per_frame`. The de-facto fix is LAME's: the first frame of the
//! file is a silent frame whose main-data area holds a Xing header (`Xing` for
//! VBR, `Info` for CBR: frame count, byte count, seek TOC) followed by the LAME
//! extension, whose 12 + 12 bit field gives the **encoder delay** (samples of
//! priming before the first input sample) and the **padding** (samples after
//! the last one). FFmpeg's demuxer (and the players and browsers built on it)
//! reads it and trims the decode back to exactly the input.
//!
//! Conventions, as FFmpeg's `mp3dec.c` applies them (and LAME writes them):
//!
//! * the frame count EXCLUDES the tag frame itself (the demuxer skips it and
//!   takes `frames * spf` as the decoded length);
//! * the byte count INCLUDES it (it is compared against the stream size);
//! * decoded samples `[delay + 529, frames*spf - padding + 529)` are kept: 529
//!   is the decoder's own delay (528 + 1), so `delay + input + padding` must be
//!   exactly `frames * spf`, and `padding >= 529` for the last input sample to
//!   be decoded at all.

use crate::frame::ChannelMode;
use crate::header::{FrameHeader, MpegVersion};
use crate::tables;

/// Samples of priming this encoder emits before the first input sample, in the
/// LAME/FFmpeg sense (the decoder's 529 comes on top). The analysis filterbank
/// is applied to each granule as it arrives -- there is no psychoacoustic
/// look-ahead -- so this is the analysis filterbank and MDCT overlap alone.
/// Measured by cross-correlation: input sample `k` decodes (FFmpeg, no
/// trimming) at sample `k + 1057` = `k + ENCODER_DELAY + DECODER_DELAY`, at
/// every rate, channel layout, bitrate and rate mode.
pub const ENCODER_DELAY: usize = 528;

/// The decoder-side delay (synthesis filterbank + IMDCT overlap) every Layer III
/// decoder adds, as FFmpeg's demuxer and LAME count it (`528 + 1`).
pub const DECODER_DELAY: usize = 529;

/// The fewest audio frames a stream carries after the tag frame. FFmpeg's
/// demuxer only accepts a stream once it finds two consecutive frames ("Failed
/// to find two consecutive MPEG audio frames" otherwise), so even a one-sample
/// input is encoded into two.
pub const MIN_AUDIO_FRAMES: usize = 2;

/// Version string in the LAME extension. Readers key their LAME-tag parsing on
/// the first four bytes (FFmpeg honours the delay/padding field only after
/// `LAME`, `Lavf` or `Lavc`); the remaining five name this encoder, so a tool
/// that prints the string does not attribute the stream to LAME's own release.
const ENCODER_STRING: &[u8; 9] = b"LAMErusty";

/// Xing header flags: frame count, byte count, TOC, quality indicator.
const XING_FLAGS: u32 = 0x0F;
/// Xing header (tag + flags + frames + bytes + 100-byte TOC + quality) plus the
/// 36-byte LAME extension -- the bytes a tag frame must hold after its side info.
const TAG_BYTES: usize = 4 + 4 + 4 + 4 + 100 + 4 + 36;
/// TOC sampling buckets (LAME's `VbrTag.c` / FFmpeg's `XING_NUM_BAGS`).
const NUM_BAGS: usize = 400;

/// Audio frames needed for `input` samples (per channel) behind `delay`
/// samples of priming: every input sample plus the decoder's delay must fit,
/// and never fewer than [`MIN_AUDIO_FRAMES`].
#[must_use]
pub fn frames_for(input: u64, delay: usize, spf: usize) -> u64 {
    let spf = spf.max(1) as u64;
    (input + (delay + DECODER_DELAY) as u64)
        .div_ceil(spf)
        .max(MIN_AUDIO_FRAMES as u64)
}

/// Trailing padding (samples) once `input` samples behind `delay` are encoded
/// into [`frames_for`] frames: `frames * spf - delay - input`, always at least
/// [`DECODER_DELAY`] (so the last input sample is decoded) and below
/// `DECODER_DELAY + spf` except at the minimum-frame floor.
#[must_use]
pub fn padding_for(input: u64, delay: usize, spf: usize) -> u64 {
    frames_for(input, delay, spf) * spf as u64 - delay as u64 - input
}

/// Extra priming (zero samples ahead of the input) a stream of only `input`
/// samples needs so FFmpeg trims it exactly.
///
/// FFmpeg's demuxer attaches the start skip (`delay + 529`) to the FIRST packet
/// and the end trim to each packet reaching past the input -- and every such
/// side-data packet overwrites the decoder's pending skip. So if the skip spans
/// past the first frame (it does on MPEG-2/2.5: 1057 > 576) and a later frame
/// still inside it also carries end trim -- an input under 95 samples -- the
/// rest of the skip is lost and the output grows by up to a frame. Priming
/// just enough that the input ends no earlier than the second frame does
/// (`delay + 529 + input >= 2 * 576`) keeps the end trim off every frame the
/// skip still covers. Only meaningful while no frame has been encoded
/// (`input < spf`); zero for MPEG-1 and for any input long enough not to hit
/// the case.
#[must_use]
pub fn extra_priming(input: u64, spf: usize) -> usize {
    let spf = spf.max(1) as u64;
    // Bad iff some frame k >= 1 starts inside the skip and ends past the input.
    let bad = |skip: u64| (1..=skip / spf).any(|k| k * spf < skip && (k + 1) * spf > skip + input);
    let skip = (ENCODER_DELAY + DECODER_DELAY) as u64;
    (0..spf).find(|&p| !bad(skip + p)).unwrap_or(0) as usize
}

/// CRC-16 as the LAME tag uses it (polynomial 0x8005 reflected, i.e. 0xA001,
/// LSB first; FFmpeg's `AV_CRC_16_ANSI_LE`), continued from `crc`.
#[must_use]
pub fn crc16(mut crc: u16, data: &[u8]) -> u16 {
    for &b in data {
        crc ^= u16::from(b);
        for _ in 0..8 {
            crc = if crc & 1 == 1 {
                (crc >> 1) ^ 0xA001
            } else {
                crc >> 1
            };
        }
    }
    crc
}

/// Running totals over the audio frames that follow the tag frame: what the
/// Xing header, the TOC and the LAME extension's music length/CRC describe.
#[derive(Debug, Clone)]
pub struct TagStats {
    frames: u32,
    /// Stream bytes so far, tag frame included (the Xing byte count's unit).
    bytes: u64,
    music_crc: u16,
    // TOC sampling (LAME/FFmpeg): every `want` frames, record the byte position;
    // when the 400 buckets fill, keep every other one and double `want`.
    bag: Vec<u64>,
    want: u32,
    seen: u32,
}

impl TagStats {
    /// Start the totals for a stream whose tag frame is `tag_frame_len` bytes.
    #[must_use]
    pub fn new(tag_frame_len: usize) -> Self {
        Self {
            frames: 0,
            bytes: tag_frame_len as u64,
            music_crc: 0,
            bag: Vec::with_capacity(NUM_BAGS),
            want: 1,
            seen: 0,
        }
    }

    /// Account one audio frame, in stream order.
    pub fn add_frame(&mut self, frame: &[u8]) {
        self.frames = self.frames.saturating_add(1);
        self.bytes += frame.len() as u64;
        self.music_crc = crc16(self.music_crc, frame);
        self.seen += 1;
        if self.seen == self.want {
            self.seen = 0;
            self.bag.push(self.bytes);
            if self.bag.len() == NUM_BAGS {
                let kept: Vec<u64> = self.bag.iter().skip(1).step_by(2).copied().collect();
                self.bag.clear();
                self.bag.extend(kept);
                self.want *= 2;
            }
        }
    }

    /// Audio frames accounted so far.
    #[must_use]
    pub fn frames(&self) -> u32 {
        self.frames
    }

    /// The 100-entry seek table: entry `i` is the byte position of `i`% of the
    /// duration, as a fraction of the stream size in 1/256ths.
    fn toc(&self) -> [u8; 100] {
        let mut toc = [0u8; 100];
        if self.bag.is_empty() || self.bytes == 0 {
            return toc;
        }
        for (i, t) in toc.iter_mut().enumerate().skip(1) {
            let j = i * self.bag.len() / 100;
            *t = (256 * self.bag[j] / self.bytes).min(255) as u8;
        }
        toc
    }
}

/// The header of the tag frame for a stream with header `stream`: same version,
/// rate and channel layout, no CRC, and the stream's bitrate unless the tag does
/// not fit in a frame that small -- then the smallest larger bitrate that holds
/// it (decoders do not require the tag frame's bitrate to match; FFmpeg's own
/// muxer chooses it the same way).
#[must_use]
pub fn tag_header(stream: &FrameHeader) -> FrameHeader {
    let mut h = stream.clone();
    h.crc_protected = false;
    h.padding = false;
    // Signalled like the audio frames after it (the encoder marks every stereo
    // frame joint stereo), so the whole stream shares one channel mode.
    if h.channel_mode.channels() == 2 {
        h.channel_mode = ChannelMode::JointStereo {
            ms_stereo: false,
            intensity_stereo: false,
        };
    }
    let table: &[u32; 16] = if h.version == MpegVersion::V1 {
        &tables::BITRATE_V1_L3
    } else {
        &tables::BITRATE_V2_L3
    };
    let need = 4 + h.side_info_len() + TAG_BYTES;
    let start = table[1..15]
        .iter()
        .position(|&b| b >= stream.bitrate_kbps)
        .unwrap_or(0)
        + 1;
    for &kbps in &table[start..15] {
        h.bitrate_kbps = kbps;
        if h.frame_size() >= need {
            return h;
        }
    }
    h.bitrate_kbps = table[14];
    h
}

/// What the LAME extension records about the encode.
#[derive(Debug, Clone, Copy)]
pub struct TagInfo {
    /// `Xing` (VBR) rather than `Info` (CBR).
    pub vbr: bool,
    /// Samples of priming before the first input sample.
    pub delay: usize,
    /// Samples of padding after the last input sample.
    pub padding: usize,
}

/// Build the complete tag frame: a silent Layer III frame (all-zero side info,
/// so it decodes to silence where the tag is not understood) carrying the Xing
/// header and the LAME extension. `header` is from [`tag_header`].
#[must_use]
pub fn tag_frame(header: &FrameHeader, stats: &TagStats, info: &TagInfo) -> Vec<u8> {
    let size = header.frame_size();
    let mut f = Vec::with_capacity(size);
    f.extend_from_slice(&header.to_bytes());
    f.resize(4 + header.side_info_len(), 0);

    // Xing header.
    f.extend_from_slice(if info.vbr { b"Xing" } else { b"Info" });
    f.extend_from_slice(&XING_FLAGS.to_be_bytes());
    f.extend_from_slice(&stats.frames.to_be_bytes());
    let bytes = u32::try_from(stats.bytes).unwrap_or(u32::MAX);
    f.extend_from_slice(&bytes.to_be_bytes());
    f.extend_from_slice(&stats.toc());
    f.extend_from_slice(&0u32.to_be_bytes()); // quality indicator: not applicable

    // LAME extension (36 bytes).
    f.extend_from_slice(ENCODER_STRING);
    // Tag revision 0 | VBR method: 1 = CBR, 2 = ABR (the VBR mode targets an
    // average bitrate).
    f.push(if info.vbr { 2 } else { 1 });
    f.push(0); // lowpass: not recorded
    f.extend_from_slice(&[0; 8]); // ReplayGain peak + radio + audiophile: none
    f.push(0); // encoding flags + ATH type
               // ABR target / minimal bitrate: left unknown (0), as FFmpeg's muxer does.
               // Written as the bitrate, 128..=131 kbps put `00 00 80` into the frame --
               // an H.263 picture start code to FFmpeg's prober, which then claimed tiny
               // (< 7 frame) files, whose own mp3 probe score is low, as H.263 video.
    f.push(0);
    let delay = info.delay.min(0xFFF) as u32;
    let padding = info.padding.min(0xFFF) as u32;
    f.extend_from_slice(&((delay << 12) | padding).to_be_bytes()[1..]);
    // Misc: source sample rate (bits 6-7: <=32 kHz, 44.1, 48, >48) and stereo
    // mode (bits 2-4: 0 mono, 3 joint).
    let src_rate: u8 = match header.sample_rate {
        44100 => 1,
        48000 => 2,
        _ => 0,
    };
    let stereo: u8 = if header.channel_mode.channels() == 2 {
        3
    } else {
        0
    };
    f.push((src_rate << 6) | (stereo << 2));
    f.push(0); // MP3 gain
    f.extend_from_slice(&[0, 0]); // preset + surround
    f.extend_from_slice(&bytes.to_be_bytes()); // music length (tag frame included)
    f.extend_from_slice(&stats.music_crc.to_be_bytes());
    // Tag CRC: every frame byte before this field.
    let crc = crc16(0, &f);
    f.extend_from_slice(&crc.to_be_bytes());

    f.resize(size, 0);
    f
}

#[cfg(test)]
mod tests {
    use super::*;

    fn hdr(version: MpegVersion, sample_rate: u32, kbps: u32, mono: bool) -> FrameHeader {
        FrameHeader {
            version,
            crc_protected: false,
            bitrate_kbps: kbps,
            sample_rate,
            padding: false,
            channel_mode: if mono {
                ChannelMode::Mono
            } else {
                ChannelMode::Stereo
            },
            copyright: false,
            original: true,
            emphasis: 0,
        }
    }

    #[test]
    fn crc_matches_the_ansi_le_check_value() {
        // CRC-16/ARC ("123456789") = 0xBB3D.
        assert_eq!(crc16(0, b"123456789"), 0xBB3D);
    }

    #[test]
    fn frame_counts_cover_input_plus_both_delays() {
        for spf in [576usize, 1152] {
            for n in [0u64, 1, 575, 576, 1151, 1152, 1153, 2205, 10_000, 441_000] {
                let extra = if n < spf as u64 {
                    extra_priming(n, spf)
                } else {
                    0
                };
                let delay = ENCODER_DELAY + extra;
                let frames = frames_for(n, delay, spf);
                let pad = padding_for(n, delay, spf);
                assert!(frames >= MIN_AUDIO_FRAMES as u64);
                assert_eq!(delay as u64 + n + pad, frames * spf as u64);
                // The last input sample is decoded, and padding fits 12 bits.
                assert!(pad >= DECODER_DELAY as u64, "spf {spf} n {n}: pad {pad}");
                assert!(pad < 4096);
                // Minimal: one frame fewer would not hold the input.
                if frames > MIN_AUDIO_FRAMES as u64 {
                    assert!((frames - 1) * (spf as u64) < n + (delay + DECODER_DELAY) as u64);
                }
            }
        }
        // The issue #69 case: 2205 samples at 44.1 kHz -> 3 frames, 723 padding.
        assert_eq!(frames_for(2205, ENCODER_DELAY, 1152), 3);
        assert_eq!(padding_for(2205, ENCODER_DELAY, 1152), 723);
    }

    #[test]
    fn extra_priming_only_for_tiny_lsf_inputs() {
        for n in 0..1152 {
            assert_eq!(extra_priming(n, 1152), 0, "MPEG-1 never needs it");
        }
        for n in 0..=94 {
            assert_eq!(extra_priming(n, 576), 95 - n as usize, "n {n}");
        }
        for n in 95..576 {
            assert_eq!(extra_priming(n, 576), 0, "n {n}");
        }
    }

    #[test]
    fn tag_frame_fits_every_layout_and_rate() {
        let cases = [
            (MpegVersion::V1, 32000, &tables::BITRATE_V1_L3),
            (MpegVersion::V1, 48000, &tables::BITRATE_V1_L3),
            (MpegVersion::V2, 24000, &tables::BITRATE_V2_L3),
            (MpegVersion::V2_5, 8000, &tables::BITRATE_V2_L3),
        ];
        for (v, sr, table) in cases {
            for &kbps in &table[1..15] {
                for mono in [true, false] {
                    let stream = hdr(v, sr, kbps, mono);
                    let h = tag_header(&stream);
                    let f = tag_frame(
                        &h,
                        &TagStats::new(h.frame_size()),
                        &TagInfo {
                            vbr: false,
                            delay: ENCODER_DELAY,
                            padding: 600,
                        },
                    );
                    assert_eq!(f.len(), h.frame_size());
                    assert!(h.bitrate_kbps >= kbps.min(table[14]));
                    let parsed = FrameHeader::parse([f[0], f[1], f[2], f[3]]).unwrap();
                    assert_eq!(parsed.to_bytes(), h.to_bytes());
                    assert_eq!(parsed.frame_size(), f.len());
                }
            }
        }
    }

    #[test]
    fn tag_fields_land_at_the_lame_offsets() {
        let h = tag_header(&hdr(MpegVersion::V1, 44100, 192, false));
        let mut stats = TagStats::new(h.frame_size());
        let audio = vec![0x55u8; 626];
        for _ in 0..7 {
            stats.add_frame(&audio);
        }
        let f = tag_frame(
            &h,
            &stats,
            &TagInfo {
                vbr: false,
                delay: 528,
                padding: 1000,
            },
        );
        let o = 4 + 32;
        let be32 = |p: usize| u32::from_be_bytes(f[p..p + 4].try_into().unwrap());
        assert_eq!(&f[o..o + 4], b"Info");
        assert_eq!(be32(o + 4), 0x0F);
        assert_eq!(be32(o + 8), 7, "frame count excludes the tag frame");
        let total = (h.frame_size() + 7 * 626) as u32;
        assert_eq!(be32(o + 12), total, "byte count includes the tag frame");
        let l = o + 120; // LAME extension
        assert_eq!(&f[l..l + 4], b"LAME");
        let dp = u32::from_be_bytes([0, f[l + 21], f[l + 22], f[l + 23]]);
        assert_eq!((dp >> 12, dp & 0xFFF), (528, 1000));
        assert_eq!(be32(l + 28), total);
        let mut crc = 0;
        for _ in 0..7 {
            crc = crc16(crc, &audio);
        }
        assert_eq!(u16::from_be_bytes([f[l + 32], f[l + 33]]), crc);
        assert_eq!(
            u16::from_be_bytes([f[l + 34], f[l + 35]]),
            crc16(0, &f[..l + 34]),
            "tag CRC covers every byte before it"
        );
        // TOC is monotone and starts at 0.
        let toc = &f[o + 16..o + 116];
        assert_eq!(toc[0], 0);
        assert!(toc.windows(2).all(|w| w[0] <= w[1]));
    }

    #[test]
    fn toc_sampling_stays_bounded_on_long_streams() {
        let mut s = TagStats::new(100);
        for _ in 0..1_000_000 {
            s.add_frame(&[0u8; 4]);
        }
        assert!(s.bag.len() < NUM_BAGS);
        let toc = s.toc();
        assert!(toc.windows(2).all(|w| w[0] <= w[1]));
        assert!(toc[50] >= 120 && toc[50] <= 136, "toc[50] = {}", toc[50]);
    }
}
