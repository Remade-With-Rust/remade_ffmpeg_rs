//! Property tests for `rusty_aac`'s documented invariants (use-protection-please H-28).
//!
//! Dependency-free on purpose: a seeded xorshift generator drives each property
//! over many cases, so the crate's audited dev-dependency closure (cargo-vet,
//! 14/14) does not grow. Every property is deterministic — a failure reproduces
//! from the printed case index.
//!
//! | Invariant (where documented)                                         | Property |
//! |----------------------------------------------------------------------|----------|
//! | The decoder never panics on any bytes or any config (threat model)    | P1 |
//! | ADTS header write → parse is the identity (`write_adts_header` docs)  | P2 |
//! | AudioSpecificConfig write → parse is the identity                     | P3 |
//! | Encode → decode preserves rate, channel count and duration            | P4 |
//! | Chunking and planar vs interleaved pushes give one bitstream          | P5 |
//! | Hostile PCM never panics the encoder, and its output decodes          | P6 |
//! | LOAS-wrapped access units decode exactly as the raw units do          | P7 |

use rusty_aac::latm::{write_loas_frame, LatmDecoder};
use rusty_aac::{
    parse_adts, parse_audio_specific_config, write_adts_header, write_audio_specific_config,
    AacDecoder, AacEncoder, AacEncoderConfig, AdtsHeader, AudioSpecificConfig, SAMPLE_RATES,
};

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
    #[allow(clippy::cast_possible_truncation, reason = "a byte from the generator")]
    fn byte(&mut self) -> u8 {
        self.next() as u8
    }
}

fn tone(n: usize, ch: usize, sr: u32) -> Vec<f32> {
    (0..n * ch)
        .map(|i| {
            let (t, c) = ((i / ch) as f32, (i % ch) as f32);
            (t * (440.0 + 110.0 * c) * std::f32::consts::TAU / sr as f32).sin() * 0.3
        })
        .collect()
}

fn encode(pcm: &[f32], ch: u16, sr: u32) -> Vec<Vec<u8>> {
    let mut e = AacEncoder::new(AacEncoderConfig::default());
    e.push_pcm(pcm, ch, sr).unwrap();
    e.finish();
    let mut out = Vec::new();
    while let Ok(p) = e.next_packet() {
        out.push(p.data);
    }
    out
}

fn adts(au: &[u8], sr: u32, ch: u16) -> Vec<u8> {
    let hdr = AdtsHeader {
        object_type: 2,
        sample_rate: sr,
        channels: ch,
        frame_length: au.len() + 7,
        header_len: 7,
    };
    [write_adts_header(&hdr), au.to_vec()].concat()
}

/// P1: random bytes, mutated real frames and random configs never panic the
/// decoder (they decode or return a typed error).
#[test]
fn p1_decoder_never_panics() {
    let mut rng = Rng(0x9E37_79B9_7F4A_7C15);
    let real: Vec<Vec<u8>> = encode(&tone(8192, 2, 44_100), 2, 44_100)
        .iter()
        .map(|au| adts(au, 44_100, 2))
        .collect();
    for case in 0..600 {
        let mut dec = AacDecoder::new();
        let packet: Vec<u8> = if case % 2 == 0 {
            (0..rng.below(2048)).map(|_| rng.byte()).collect()
        } else {
            let mut p = real[rng.below(real.len() as u64) as usize].clone();
            for _ in 0..=rng.below(8) {
                let i = rng.below(p.len() as u64) as usize;
                p[i] ^= 1 << rng.below(8);
            }
            p
        };
        let _ = dec.decode(&packet, None);
        // A random AudioSpecificConfig, then the same packet as a raw unit.
        let asc: Vec<u8> = (0..=rng.below(12)).map(|_| rng.byte()).collect();
        if let Ok(mut d) = AacDecoder::with_config_bytes(&asc) {
            let _ = d.decode(&packet, None);
        }
    }
}

/// P2: every legal ADTS header survives write → parse unchanged.
#[test]
fn p2_adts_header_round_trip() {
    for &sr in &SAMPLE_RATES {
        for ch in 1..=7u16 {
            for len in [7usize, 8, 100, 1000, 8191] {
                let hdr = AdtsHeader {
                    object_type: 2,
                    sample_rate: sr,
                    channels: ch,
                    frame_length: len,
                    header_len: 7,
                };
                let back = parse_adts(&write_adts_header(&hdr)).unwrap();
                assert_eq!(
                    (
                        back.object_type,
                        back.sample_rate,
                        back.channels,
                        back.frame_length
                    ),
                    (2, sr, ch, len),
                    "sr {sr} ch {ch} len {len}"
                );
            }
        }
    }
}

/// P3: `AudioSpecificConfig` write → parse is the identity for the object types
/// and layouts the writer supports.
#[test]
fn p3_audio_specific_config_round_trip() {
    for object_type in [1u8, 2, 3, 4] {
        for &sample_rate in &SAMPLE_RATES {
            for channels in 1..=7u16 {
                let cfg = AudioSpecificConfig {
                    object_type,
                    sample_rate,
                    channels,
                };
                let back = parse_audio_specific_config(&write_audio_specific_config(&cfg)).unwrap();
                assert_eq!(
                    (back.object_type, back.sample_rate, back.channels),
                    (object_type, sample_rate, channels)
                );
            }
        }
    }
}

/// P4: encoding then decoding preserves the rate, the channel count and (at
/// least) the duration, for every channel count and a spread of rates.
#[test]
#[cfg_attr(miri, ignore = "runs real encodes: too slow to interpret under Miri")]
fn p4_encode_decode_preserves_shape() {
    for (sr, ch) in [
        (44_100u32, 1u16),
        (48_000, 2),
        (22_050, 3),
        (32_000, 6),
        (8_000, 2),
    ] {
        let n = 4096 + 333;
        let aus = encode(&tone(n, ch as usize, sr), ch, sr);
        let mut dec = AacDecoder::new();
        let mut samples = 0usize;
        for au in &aus {
            let a = dec.decode(&adts(au, sr, ch), None).unwrap();
            assert_eq!((a.sample_rate, a.channels), (sr, ch), "sr {sr} ch {ch}");
            samples += a.samples.len() / ch as usize;
        }
        assert!(samples >= n, "sr {sr} ch {ch}: {samples} < {n}");
    }
}

/// P5: the bitstream does not depend on how the PCM was pushed — chunk sizes,
/// or planar instead of interleaved.
#[test]
#[cfg_attr(miri, ignore = "runs real encodes: too slow to interpret under Miri")]
fn p5_push_shape_invariance() {
    let (sr, ch, n) = (44_100u32, 2usize, 6000);
    let pcm = tone(n, ch, sr);
    let whole = encode(&pcm, 2, sr);
    let mut rng = Rng(0x2545_F491_4F6C_DD1D);
    for case in 0..6 {
        let mut e = AacEncoder::new(AacEncoderConfig::default());
        let mut pos = 0;
        while pos < n {
            let k = (1 + rng.below(1500) as usize).min(n - pos);
            let chunk = &pcm[pos * ch..(pos + k) * ch];
            if case % 2 == 0 {
                e.push_pcm(chunk, 2, sr).unwrap();
            } else {
                let l: Vec<f32> = chunk.iter().step_by(2).copied().collect();
                let r: Vec<f32> = chunk.iter().skip(1).step_by(2).copied().collect();
                e.push_pcm_planar(&[&l, &r], sr).unwrap();
            }
            pos += k;
        }
        e.finish();
        let mut got = Vec::new();
        while let Ok(p) = e.next_packet() {
            got.push(p.data);
        }
        assert_eq!(got, whole, "case {case}");
    }
}

/// P6: hostile PCM — NaN, ±Inf, subnormals, 1e30 — never panics the encoder,
/// every block fits the decoder input buffer (6144 bits per channel), and what
/// it emits decodes. (Before the input clamp, ±Inf produced a 9441-byte block.)
#[test]
#[cfg_attr(miri, ignore = "runs real encodes: too slow to interpret under Miri")]
fn p6_hostile_pcm() {
    // An absurd bitrate must not lift the per-block ceiling either.
    let mut e = AacEncoder::new(AacEncoderConfig {
        bitrate_bps: u32::MAX,
        ..AacEncoderConfig::default()
    });
    let mut rng = Rng(0x1234_5678_9ABC_DEF1);
    let noise: Vec<f32> = (0..4096 * 2)
        .map(|_| (rng.below(2000) as f32 - 1000.0) / 1000.0)
        .collect();
    e.push_pcm(&noise, 2, 48_000).unwrap();
    e.finish();
    while let Ok(p) = e.next_packet() {
        assert!(
            p.data.len() <= 768 * 2,
            "{} B block at u32::MAX bps",
            p.data.len()
        );
    }
    let specials = [
        f32::NAN,
        f32::INFINITY,
        f32::NEG_INFINITY,
        1e30,
        -1e30,
        f32::MIN_POSITIVE / 2.0,
        0.0,
    ];
    let mut rng = Rng(0xD1B5_4A32_D192_ED03);
    for case in 0..8 {
        let ch = 1 + (case % 3) as u16;
        let pcm: Vec<f32> = (0..3000 * ch as usize)
            .map(|_| match rng.below(4) {
                0 => specials[rng.below(specials.len() as u64) as usize],
                1 => f32::from_bits(rng.next() as u32),
                _ => (rng.below(2000) as f32 - 1000.0) / 1000.0,
            })
            .collect();
        let aus = encode(&pcm, ch, 48_000);
        let mut dec = AacDecoder::new();
        for (i, au) in aus.iter().enumerate() {
            assert!(
                au.len() <= 768 * ch as usize,
                "case {case}: {} B block",
                au.len()
            );
            if let Err(e) = dec.decode(&adts(au, 48_000, ch), None) {
                panic!(
                    "case {case}: packet {i} of {} ({} B): {e}",
                    aus.len(),
                    au.len()
                );
            }
        }
    }
}

/// P7: wrapping access units in LOAS (with an in-band `StreamMuxConfig`) changes
/// nothing about the decoded PCM.
#[test]
#[cfg_attr(miri, ignore = "runs real encodes: too slow to interpret under Miri")]
fn p7_loas_equals_raw() {
    let (sr, ch) = (48_000u32, 2u16);
    let aus = encode(&tone(5000, ch as usize, sr), ch, sr);
    let cfg = AudioSpecificConfig {
        object_type: 2,
        sample_rate: sr,
        channels: ch,
    };
    let mut raw = AacDecoder::with_config(cfg);
    let mut latm = LatmDecoder::new();
    for au in &aus {
        let want = raw.decode(au, None).unwrap();
        let (got, used) = latm.decode(&write_loas_frame(&cfg, au), None).unwrap();
        assert!(used > 0);
        assert_eq!(
            got.samples.iter().map(|v| v.to_bits()).collect::<Vec<_>>(),
            want.samples.iter().map(|v| v.to_bits()).collect::<Vec<_>>()
        );
    }
}
