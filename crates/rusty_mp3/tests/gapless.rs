//! Gapless output (issue #69): the encoder flushes every input sample through
//! its delay, and leads the stream with a LAME-style Info/Xing tag whose
//! delay/padding let FFmpeg (and other LAME-tag readers) trim the decode back
//! to exactly the input.
//!
//! FFmpeg itself is not available to `cargo test`, so these check the tag
//! against the arithmetic FFmpeg's `mp3dec.c` applies -- it keeps decoded
//! samples `[delay + 529, frames * spf - padding + 529)` -- and check with this
//! crate's decoder (which does not trim) that the input sits exactly there.
//! The fixtures are generated in-test, and the file uses only the crate's
//! 1.0 API (the tag is parsed and its CRCs checked here, independently), so it
//! runs -- and fails -- against the pre-fix encoder too.

use rusty_mp3::header::FrameHeader;
use rusty_mp3::{Mp3Decoder, Mp3Encoder, Mp3EncoderConfig};

/// The decoder delay FFmpeg's demuxer adds to the tag's encoder delay.
const DECODER_DELAY: usize = 529;
/// This encoder's delay (`rusty_mp3::encode::lametag::ENCODER_DELAY`).
const ENCODER_DELAY: usize = 528;

/// CRC-16/ARC (poly 0xA001 reflected, init 0): the LAME tag's two CRCs.
fn crc16(mut crc: u16, data: &[u8]) -> u16 {
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

fn cfg(bitrate_kbps: u32, vbr_quality: Option<f32>) -> Mp3EncoderConfig {
    Mp3EncoderConfig {
        bitrate_kbps,
        vbr_quality,
    }
}

fn encode(cfg: Mp3EncoderConfig, pcm: &[f32], ch: u16, sr: u32) -> Vec<u8> {
    let mut e = Mp3Encoder::new(cfg);
    e.push_pcm_f32(pcm, ch, sr).unwrap();
    e.finish();
    let mut out = Vec::new();
    while let Ok(p) = e.next_packet() {
        out.extend_from_slice(&p);
    }
    out
}

/// Deterministic broadband test signal: low-passed noise plus a tone per channel.
fn signal(n: usize, ch: usize, sr: u32) -> Vec<f32> {
    let mut s = 0x1234_5678u32;
    let mut lp = [0f32; 2];
    let mut out = Vec::with_capacity(n * ch);
    for i in 0..n {
        for (c, lp) in lp.iter_mut().enumerate().take(ch) {
            s = s.wrapping_mul(1_664_525).wrapping_add(1_013_904_223);
            let r = (s >> 8) as f32 / (1u32 << 24) as f32 * 2.0 - 1.0;
            *lp = 0.7 * *lp + 0.3 * r;
            let f = if c == 0 { 440.0 } else { 660.0 };
            let t = i as f32 / sr as f32;
            out.push(0.35 * *lp + 0.2 * (2.0 * std::f32::consts::PI * f * t).sin());
        }
    }
    out
}

/// The stream cut into frames.
fn frames(mp3: &[u8]) -> Vec<&[u8]> {
    let mut out = Vec::new();
    let mut p = 0;
    while p + 4 <= mp3.len() {
        let h = FrameHeader::parse([mp3[p], mp3[p + 1], mp3[p + 2], mp3[p + 3]])
            .unwrap_or_else(|_| panic!("no frame header at byte {p}"));
        let size = h.frame_size();
        assert!(p + size <= mp3.len(), "truncated frame at byte {p}");
        out.push(&mp3[p..p + size]);
        p += size;
    }
    assert_eq!(p, mp3.len(), "trailing bytes after the last frame");
    out
}

/// The fields of the leading tag frame.
#[derive(Debug)]
struct Tag {
    vbr: bool,
    frames: u32,
    bytes: u32,
    delay: usize,
    padding: usize,
    music_len: u32,
    music_crc: u16,
    crc_ok: bool,
}

fn parse_tag(frame: &[u8]) -> Tag {
    let h = FrameHeader::parse([frame[0], frame[1], frame[2], frame[3]]).unwrap();
    let o = 4 + h.side_info_len();
    let be32 = |p: usize| u32::from_be_bytes(frame[p..p + 4].try_into().unwrap());
    let vbr = match &frame[o..o + 4] {
        b"Xing" => true,
        b"Info" => false,
        other => panic!("no Info/Xing tag: {other:?}"),
    };
    assert_eq!(be32(o + 4), 0x0F, "flags: frames | bytes | TOC | quality");
    let l = o + 120;
    assert_eq!(
        &frame[l..l + 4],
        b"LAME",
        "FFmpeg keys the delay field on it"
    );
    let dp = u32::from_be_bytes([0, frame[l + 21], frame[l + 22], frame[l + 23]]);
    Tag {
        vbr,
        frames: be32(o + 8),
        bytes: be32(o + 12),
        delay: (dp >> 12) as usize,
        padding: (dp & 0xFFF) as usize,
        music_len: be32(l + 28),
        music_crc: u16::from_be_bytes([frame[l + 32], frame[l + 33]]),
        crc_ok: u16::from_be_bytes([frame[l + 34], frame[l + 35]]) == crc16(0, &frame[..l + 34]),
    }
}

/// Decode with this crate's decoder: per-channel samples, channel 0 only.
fn decode_ch0(mp3: &[u8]) -> (Vec<f32>, usize) {
    let mut d = Mp3Decoder::new();
    d.push(mp3);
    d.flush();
    let (mut out, mut ch) = (Vec::new(), 1);
    while let Ok(f) = d.next_frame() {
        ch = usize::from(f.channels);
        out.extend(f.samples.iter().step_by(ch));
    }
    (out, ch)
}

fn snr_db(want: &[f32], got: &[f32]) -> f64 {
    let sig: f64 = want.iter().map(|&x| f64::from(x) * f64::from(x)).sum();
    let err: f64 = want
        .iter()
        .zip(got)
        .map(|(&a, &b)| (f64::from(a) - f64::from(b)).powi(2))
        .sum();
    10.0 * (sig / err.max(1e-20)).log10()
}

/// Check one encode: the tag describes the stream exactly, FFmpeg's trim
/// arithmetic keeps exactly `n` samples, and those samples are the input.
fn check(mp3: &[u8], pcm: &[f32], n: usize, ch: usize, vbr: bool, label: &str) -> f64 {
    let fr = frames(mp3);
    let h = FrameHeader::parse([mp3[0], mp3[1], mp3[2], mp3[3]]).unwrap();
    let spf = h.version.samples_per_frame();
    let tag = parse_tag(fr[0]);
    assert_eq!(tag.vbr, vbr, "{label}: Xing for VBR, Info for CBR");
    assert!(tag.crc_ok, "{label}: tag CRC");
    assert_eq!(
        tag.frames as usize,
        fr.len() - 1,
        "{label}: frame count excludes the tag frame"
    );
    assert_eq!(
        tag.bytes as usize,
        mp3.len(),
        "{label}: byte count includes it"
    );
    assert_eq!(tag.music_len as usize, mp3.len(), "{label}: music length");
    let crc = fr[1..].iter().fold(0, |c, f| crc16(c, f));
    assert_eq!(tag.music_crc, crc, "{label}: music CRC");
    assert!(
        tag.frames >= 2,
        "{label}: FFmpeg needs two consecutive audio frames"
    );
    // FFmpeg keeps [delay + 529, frames*spf - padding + 529) of the decode.
    let full = tag.frames as usize * spf;
    assert_eq!(
        tag.delay + n + tag.padding,
        full,
        "{label}: delay + input + padding"
    );
    assert!(
        tag.padding >= DECODER_DELAY,
        "{label}: the last input sample is decoded"
    );
    // Every frame, the tag frame included, carries the same header fields
    // FFmpeg's sync check compares (all but bitrate, padding, mode extension).
    let mask = |f: &[u8]| u32::from_be_bytes([f[0], f[1], f[2], f[3]]) & 0xFFFE_0CCF;
    assert!(
        fr.iter().all(|f| mask(f) == mask(fr[0])),
        "{label}: consistent headers"
    );

    // This decoder does not trim: the tag frame decodes to one frame of silence,
    // then the input sits behind the delay.
    let (dec, dch) = decode_ch0(mp3);
    assert_eq!(dch, ch, "{label}: channels");
    assert_eq!(dec.len(), fr.len() * spf, "{label}: every frame decodes");
    let at = spf + tag.delay + DECODER_DELAY;
    assert!(
        at + n <= dec.len(),
        "{label}: input tail missing from the decode"
    );
    let want: Vec<f32> = pcm.iter().step_by(ch).copied().collect();
    snr_db(&want, &dec[at..at + n])
}

/// The issue's exact repro: 50 ms of DC at 0.25, mono 44.1 kHz, 192 kbps CBR.
/// Before the fix the tag had no LAME extension, counted itself among the
/// frames, and the 2 audio frames ended 958 samples before the input did.
#[test]
fn issue_69_repro_keeps_every_sample() {
    let pcm = vec![0.25f32; 2205];
    let mut enc = Mp3Encoder::new(cfg(192, None));
    enc.push_pcm_f32(&pcm, 1, 44100).unwrap();
    enc.finish();
    let mut out = Vec::new();
    while let Ok(p) = enc.next_packet() {
        out.extend_from_slice(&p);
    }
    let fr = frames(&out);
    let tag = parse_tag(fr[0]);
    assert_eq!(
        (tag.frames, tag.delay, tag.padding),
        (3, 528, 723),
        "{tag:?}"
    );
    assert_eq!(fr.len(), 4);
    // FFmpeg's decoded length: frames*spf - delay - padding.
    assert_eq!(tag.frames as usize * 1152 - tag.delay - tag.padding, 2205);
    // The DC is present up to the input's very end, then gone.
    let (dec, _) = decode_ch0(&out);
    let at = 1152 + ENCODER_DELAY + DECODER_DELAY;
    let mean = |s: &[f32]| s.iter().sum::<f32>() / s.len() as f32;
    let tail = mean(&dec[at + 2105..at + 2205]);
    assert!(
        (tail - 0.25).abs() < 0.03,
        "last 100 input samples decode to {tail}"
    );
    // ...and the flush frames decode to silence (the padding FFmpeg trims).
    let after = mean(&dec[at + 2300..at + 2399]).abs();
    assert!(
        after < 0.03,
        "the padding after the input decodes to {after}"
    );
    assert_eq!(enc.lame_tag_frame(), Some(fr[0]));
}

/// Lengths around the frame boundaries, every layout and mode.
#[test]
fn every_length_is_gapless_across_rates_layouts_and_modes() {
    let lens = [0usize, 1, 10, 94, 576, 1151, 1152, 1153, 2205, 6000];
    let rates = [44100u32, 32000, 48000, 22050, 16000, 24000, 11025, 8000];
    for &sr in &rates {
        let v1 = sr >= 32000;
        let modes: [(u32, Option<f32>); 3] = if v1 {
            [(128, None), (320, None), (0, Some(190.0))]
        } else {
            [(64, None), (160, None), (0, Some(100.0))]
        };
        for ch in [1usize, 2] {
            for &(kbps, q) in &modes {
                for &n in &lens {
                    let pcm = signal(n, ch, sr);
                    let mp3 = encode(cfg(kbps, q), &pcm, ch as u16, sr);
                    let label = format!("{sr} Hz {ch} ch {kbps} kbps {q:?} n={n}");
                    let snr = check(&mp3, &pcm, n, ch, q.is_some(), &label);
                    if n >= 1152 {
                        assert!(snr > 8.0, "{label}: aligned SNR {snr:.1} dB");
                    }
                }
            }
        }
    }
}

/// An input under 95 samples at an MPEG-2/2.5 rate is primed so it ends no
/// earlier than FFmpeg's start skip leaves the second frame (see
/// `encode::lametag::extra_priming`); longer ones and MPEG-1 keep the codec
/// delay.
#[test]
fn tiny_lsf_inputs_are_primed_past_the_second_frame() {
    let delay_of = |sr: u32, n: usize| {
        let mp3 = encode(cfg(0, None), &signal(n, 1, sr), 1, sr);
        parse_tag(frames(&mp3)[0]).delay
    };
    for n in [0, 1, 10, 94] {
        assert_eq!(delay_of(22050, n) + DECODER_DELAY + n, 2 * 576, "n {n}");
    }
    assert_eq!(delay_of(22050, 95), ENCODER_DELAY);
    assert_eq!(delay_of(44100, 10), ENCODER_DELAY);
}

/// Streaming: a caller that drains packets as they come gets the placeholder
/// tag first; overwriting it with `lame_tag_frame` yields exactly the stream a
/// drain-after-`finish` caller gets. Both the streaming (VBR, 320k) and the
/// buffered reservoir (CBR) paths.
#[test]
fn streamed_placeholder_patched_with_the_final_tag_equals_drain_after_finish() {
    let pcm = signal(30_000, 2, 44100);
    for c in [cfg(128, None), cfg(320, None), cfg(0, Some(190.0))] {
        let whole = encode(c, &pcm, 2, 44100);

        let mut e = Mp3Encoder::new(c);
        let mut packets = Vec::new();
        for chunk in pcm.chunks(2 * 1000) {
            e.push_pcm_f32(chunk, 2, 44100).unwrap();
            while let Ok(p) = e.next_packet() {
                packets.push(p);
            }
        }
        assert!(e.lame_tag_frame().is_none(), "no final tag before finish");
        e.finish();
        while let Ok(p) = e.next_packet() {
            packets.push(p);
        }
        let tag = e.lame_tag_frame().expect("final tag after finish").to_vec();
        assert_eq!(
            packets[0].len(),
            tag.len(),
            "placeholder is the final length"
        );
        packets[0] = tag;
        assert_eq!(packets.concat(), whole, "{c:?}");
    }
}

/// No push, no format: nothing is emitted. `finish` twice is harmless.
#[test]
fn finish_without_input_emits_nothing() {
    let mut e = Mp3Encoder::new(cfg(128, None));
    e.finish();
    e.finish();
    assert!(matches!(e.next_packet(), Err(rusty_mp3::Error::Eof)));
    assert!(e.lame_tag_frame().is_none());
}

/// `part2_3_length` is 12 bits: configurations whose frames offer a granule
/// more than 4095 bits (mono 320k at 32 kHz, mono 128/160k at 16 kHz) used to
/// wrap it and decode to noise.
#[test]
fn large_granule_budgets_stay_conformant() {
    for (sr, kbps) in [(32000u32, 320u32), (16000, 128), (16000, 160)] {
        let n = 20_000;
        let pcm = signal(n, 1, sr);
        let mp3 = encode(cfg(kbps, None), &pcm, 1, sr);
        let snr = check(&mp3, &pcm, n, 1, false, &format!("{sr} Hz {kbps}k"));
        assert!(snr > 20.0, "{sr} Hz mono {kbps}k: SNR {snr:.1} dB");
    }
}
