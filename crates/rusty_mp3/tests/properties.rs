//! Property tests for `rusty_mp3`'s documented invariants (use-protection-please H-28).
//!
//! Dependency-free on purpose: a seeded xorshift generator drives each property
//! over many cases, so the crate's audited dev-dependency closure (cargo-vet,
//! 14/14) does not grow. Every property is deterministic -- a failure reproduces
//! from the printed case index.
//!
//! | Invariant (where documented)                                  | Property |
//! |---------------------------------------------------------------|----------|
//! | The decoder never panics on any byte slice (threat model)      | P1 |
//! | `decode_pipelined` equals the serial decoder on ANY input (its docs) | P2 |
//! | Header parse/serialise is a stable projection; parse never panics | P3 |
//! | Encoding then decoding preserves rate, channels and duration   | P4 |
//! | Every push entry point / chunking gives one bitstream (`push_with`) | P5 |
//! | Hostile PCM never panics the encoder (`MAX_INPUT_AMPLITUDE`)   | P6 |

use rusty_mp3::header::FrameHeader;
use rusty_mp3::{Mp3Decoder, Mp3Encoder, Mp3EncoderConfig};

struct Rng(u64);
impl Rng {
    fn next(&mut self) -> u64 {
        self.0 ^= self.0 << 13;
        self.0 ^= self.0 >> 7;
        self.0 ^= self.0 << 17;
        self.0
    }
    fn below(&mut self, n: u64) -> u64 {
        self.next() % n.max(1)
    }
}

const RATES: [u32; 9] = [8000, 11025, 12000, 16000, 22050, 24000, 32000, 44100, 48000];

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

fn decode_chunked(bytes: &[u8], chunk: usize) -> Vec<rusty_mp3::DecodedAudio> {
    let mut d = Mp3Decoder::new();
    let mut frames = Vec::new();
    for c in bytes.chunks(chunk.max(1)) {
        d.push(c);
        while let Ok(f) = d.next_frame() {
            frames.push(f);
        }
    }
    d.flush();
    while let Ok(f) = d.next_frame() {
        frames.push(f);
    }
    frames
}

fn tone(n: usize, ch: usize, sr: u32, seed: u64) -> Vec<f32> {
    let f = 200.0 + (seed % 3000) as f32;
    (0..n * ch)
        .map(|i| 0.4 * (2.0 * std::f32::consts::PI * f * (i / ch) as f32 / sr as f32).sin())
        .collect()
}

/// A short real stream to mutate (decoder properties start from valid syntax).
fn seed_stream() -> Vec<u8> {
    let cfg = Mp3EncoderConfig {
        bitrate_kbps: 128,
        vbr_quality: None,
    };
    encode(cfg, &tone(1152 * 6, 2, 44_100, 7), 2, 44_100)
}

/// Frames must be well-formed whatever went in.
fn check_frames(frames: &[rusty_mp3::DecodedAudio], case: usize) {
    for f in frames {
        assert!(
            f.channels == 1 || f.channels == 2,
            "case {case}: channels {}",
            f.channels
        );
        assert!(
            RATES.contains(&f.sample_rate),
            "case {case}: rate {}",
            f.sample_rate
        );
        assert_eq!(f.samples.len() % usize::from(f.channels), 0, "case {case}");
    }
}

/// P1: the decoder never panics on any byte slice -- random bytes biased toward
/// sync patterns, and mutated real frames -- pushed in random chunk sizes.
#[test]
fn p1_decoder_never_panics_on_any_bytes() {
    let mut r = Rng(0x9E37_79B9_7F4A_7C15);
    let base = seed_stream();
    for case in 0..1500 {
        let bytes: Vec<u8> = if case % 2 == 0 {
            let n = r.below(6000) as usize;
            (0..n)
                .map(|_| match r.below(5) {
                    0 => 0xFF,
                    1 => 0xFB,
                    2 => 0xF3,
                    _ => r.next() as u8,
                })
                .collect()
        } else {
            let mut b = base.clone();
            for _ in 0..=r.below(24) {
                let i = r.below(b.len() as u64) as usize;
                b[i] = r.next() as u8;
            }
            b.truncate(r.below(b.len() as u64 + 1) as usize);
            b
        };
        let chunk = 1 + r.below(2000) as usize;
        check_frames(&decode_chunked(&bytes, chunk), case);
    }
}

/// P2: the two-thread decoder equals the serial one, bit for bit, on any input.
#[test]
fn p2_pipelined_equals_serial() {
    let mut r = Rng(0xD1B5_4A32_D192_ED03);
    let base = seed_stream();
    for case in 0..120 {
        let mut b = base.clone();
        for _ in 0..r.below(12) {
            let i = r.below(b.len() as u64) as usize;
            b[i] ^= 1 << r.below(8);
        }
        let piped = rusty_mp3::decode_pipelined(&b);
        let serial = decode_chunked(&b, b.len());
        assert_eq!(piped.len(), serial.len(), "case {case}: frame count");
        for (a, s) in piped.iter().zip(&serial) {
            assert_eq!(
                (a.channels, a.sample_rate),
                (s.channels, s.sample_rate),
                "case {case}"
            );
            assert!(
                a.samples.len() == s.samples.len()
                    && a.samples
                        .iter()
                        .zip(&s.samples)
                        .all(|(x, y)| x.to_bits() == y.to_bits()),
                "case {case}: PCM differs"
            );
        }
    }
}

/// P3: over every 32-bit pattern carrying the sync word (2^21 of them), header
/// parsing never panics, and for every pattern that parses, serialising and
/// re-parsing yields the same header (`parse` after `to_bytes` is a stable projection).
#[test]
fn p3_header_parse_serialise_is_stable() {
    let mut accepted = 0u32;
    for rest in 0..(1u32 << 21) {
        let word = (0x7FF << 21) | rest;
        let Ok(h) = FrameHeader::parse(word.to_be_bytes()) else {
            continue;
        };
        accepted += 1;
        let again = FrameHeader::parse(h.to_bytes()).expect("serialised header re-parses");
        assert_eq!(
            format!("{h:?}"),
            format!("{again:?}"),
            "pattern {word:#010x}"
        );
        assert!(
            h.frame_size() >= 4 && h.frame_size() <= 1441,
            "pattern {word:#010x}"
        );
    }
    assert!(accepted > 10_000, "only {accepted} headers parsed");
}

/// P4: encoding then decoding preserves sample rate, channel count and at least
/// the input duration, for random configurations (all nine rates, mono/stereo,
/// CBR and VBR) and lengths.
#[test]
fn p4_encode_decode_preserves_format_and_duration() {
    let mut r = Rng(0x2545_F491_4F6C_DD1D);
    for case in 0..24 {
        let sr = RATES[r.below(9) as usize];
        let ch = 1 + r.below(2) as u16;
        let n = 576 + r.below(1152 * 5) as usize;
        let vbr = r.below(4) == 0;
        let cfg = Mp3EncoderConfig {
            bitrate_kbps: [0, 32, 64, 128, 192][r.below(5) as usize],
            vbr_quality: vbr.then(|| rusty_mp3::vbr_quality_index(r.below(10) as f32)),
        };
        let mp3 = encode(cfg, &tone(n, usize::from(ch), sr, case), ch, sr);
        let frames = decode_chunked(&mp3, 1 + r.below(900) as usize);
        assert!(!frames.is_empty(), "case {case}: nothing decoded");
        check_frames(&frames, case as usize);
        for f in &frames {
            assert_eq!(
                (f.sample_rate, f.channels),
                (sr, ch),
                "case {case}: format changed"
            );
        }
        let per_ch: usize = frames
            .iter()
            .map(|f| f.samples.len() / usize::from(ch))
            .sum();
        assert!(
            per_ch >= n,
            "case {case}: decoded {per_ch} < input {n} samples/ch"
        );
    }
}

/// P5: random chunkings through every push entry point give one bitstream.
#[test]
fn p5_push_chunking_is_invariant() {
    let mut r = Rng(0xA076_1D64_78BD_642F);
    for case in 0..6 {
        let sr = [44_100u32, 22_050, 11_025][case % 3];
        let n = 1152 * 3 + r.below(2000) as usize;
        let pcm16: Vec<i16> = tone(n, 2, sr, case as u64)
            .iter()
            .map(|&x| (x * 32767.0) as i16)
            .collect();
        let cfg = || Mp3EncoderConfig {
            bitrate_kbps: 96,
            vbr_quality: None,
        };
        let mut whole = Mp3Encoder::new(cfg());
        whole.push_pcm_s16(&pcm16, 2, sr).unwrap();
        whole.finish();
        let mut want = Vec::new();
        while let Ok(p) = whole.next_packet() {
            want.extend_from_slice(&p);
        }
        let bytes: Vec<u8> = pcm16.iter().flat_map(|s| s.to_le_bytes()).collect();
        let mut e = Mp3Encoder::new(cfg());
        let mut at = 0;
        while at < bytes.len() {
            let take = ((1 + r.below(5000) as usize) * 4).min(bytes.len() - at);
            e.push_pcm_s16le(&bytes[at..at + take], 2, sr).unwrap();
            at += take;
        }
        e.finish();
        let mut got = Vec::new();
        while let Ok(p) = e.next_packet() {
            got.extend_from_slice(&p);
        }
        assert_eq!(got, want, "case {case}: chunked push changed the bitstream");
    }
}

/// P6: any f32 bit pattern, channel count and supported rate encodes without
/// panicking.
#[test]
fn p6_hostile_pcm_never_panics() {
    let mut r = Rng(0x1234_5678_9ABC_DEF1);
    for _ in 0..40 {
        let sr = RATES[r.below(9) as usize];
        let ch = 1 + r.below(2) as u16;
        let pcm: Vec<f32> = (0..(1152 * 3) * usize::from(ch))
            .map(|_| f32::from_bits(r.next() as u32))
            .collect();
        let cfg = Mp3EncoderConfig {
            bitrate_kbps: [32u32, 128, 320][r.below(3) as usize],
            vbr_quality: None,
        };
        let out = encode(cfg, &pcm, ch, sr);
        assert!(!out.is_empty());
    }
}
