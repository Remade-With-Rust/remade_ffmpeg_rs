//! Streaming encode (`AacEncoder::set_streaming`, #111): packets as frames
//! complete, a few frames of memory, and the priming / padding an MP4 edit
//! list needs.

use rusty_aac::{AacDecoder, AacEncoder, AacEncoderConfig, AudioSpecificConfig};

const RATE: u32 = 48_000;

fn lcg(s: &mut u64) -> f32 {
    *s = s
        .wrapping_mul(6364136223846793005)
        .wrapping_add(1442695040888963407);
    ((*s >> 33) as f32 / (1u64 << 31) as f32) - 1.0
}

/// Tones with a quiet opening and percussive bursts (so short blocks occur),
/// interleaved.
fn signal(frames: usize, channels: usize, seed: u64) -> Vec<f32> {
    let mut s = seed;
    let mut out = Vec::with_capacity(frames * channels);
    for i in 0..frames {
        let t = i as f32 / RATE as f32;
        let quiet = if i < RATE as usize / 4 { 0.02 } else { 1.0 };
        let burst = if i % 9000 < 200 {
            lcg(&mut s) * 0.8
        } else {
            0.0
        };
        for c in 0..channels {
            let tone = (t * 440.0 * (c + 1) as f32 * std::f32::consts::TAU).sin() * 0.3;
            out.push((tone + burst) * quiet);
        }
    }
    out
}

fn whole(cfg: AacEncoderConfig, pcm: &[f32], ch: u16) -> Vec<(Vec<u8>, i64)> {
    let mut e = AacEncoder::new(cfg);
    e.push_pcm(pcm, ch, RATE).unwrap();
    e.finish();
    std::iter::from_fn(|| e.next_packet().ok().map(|p| (p.data, p.pts))).collect()
}

/// Push in pseudo-random chunks, draining after each push. Returns the
/// packets and how many came out before `finish`.
fn streamed(
    cfg: AacEncoderConfig,
    pcm: &[f32],
    ch: u16,
    seed: u64,
) -> (Vec<(Vec<u8>, i64)>, usize, AacEncoder) {
    let mut e = AacEncoder::new(cfg);
    e.set_streaming(true);
    let mut s = seed;
    let n = pcm.len() / ch as usize;
    let (mut out, mut at) = (Vec::new(), 0);
    while at < n {
        let take = (1 + ((lcg(&mut s).abs() * 5000.0) as usize)).min(n - at);
        e.push_pcm(&pcm[at * ch as usize..(at + take) * ch as usize], ch, RATE)
            .unwrap();
        while let Ok(p) = e.next_packet() {
            out.push((p.data, p.pts));
        }
        at += take;
    }
    let early = out.len();
    e.finish();
    while let Ok(p) = e.next_packet() {
        out.push((p.data, p.pts));
    }
    (out, early, e)
}

fn decode(packets: &[(Vec<u8>, i64)], ch: u16) -> Vec<f32> {
    let mut d = AacDecoder::with_config(AudioSpecificConfig {
        object_type: 2,
        sample_rate: RATE,
        channels: ch,
    });
    packets
        .iter()
        .flat_map(|(data, pts)| d.decode(data, Some(*pts)).unwrap().samples)
        .collect()
}

/// SNR in dB of `decoded` against `input` placed `offset` frames in.
fn snr_at(input: &[f32], decoded: &[f32], ch: usize, offset: usize) -> f64 {
    let (mut sig, mut err) = (0f64, 0f64);
    for (i, &x) in input.iter().enumerate() {
        let y = decoded.get(i + offset * ch).copied().unwrap_or(0.0);
        sig += f64::from(x) * f64::from(x);
        err += f64::from(x - y) * f64::from(x - y);
    }
    10.0 * (sig / err.max(1e-30)).log10()
}

#[test]
fn with_the_absolute_detector_streaming_is_byte_identical_to_finish() {
    // Frame-demand allocation needs a whole reservoir segment ahead, so a
    // streaming encode codes every frame at its constant-rate share.
    let cfg = AacEncoderConfig {
        relative_transients: false,
        frame_demand: 0.0,
        ..Default::default()
    };
    for ch in [1u16, 2, 6] {
        for frames in [1usize, 500, 1024, 1025, 3 * 1024 + 1, 48_007] {
            let pcm = signal(frames, ch as usize, frames as u64);
            let want = whole(cfg, &pcm, ch);
            let (got, _, _) = streamed(cfg, &pcm, ch, 7 + frames as u64);
            assert_eq!(got, want, "{ch} ch, {frames} frames");
        }
    }
}

#[test]
fn packets_come_out_while_the_pcm_arrives() {
    let pcm = signal(20 * 1024, 2, 1);
    let mut e = AacEncoder::default();
    e.set_streaming(true);
    let mut early = 0;
    for chunk in pcm.chunks(2 * 1024) {
        e.push_pcm(chunk, 2, RATE).unwrap();
        while e.next_packet().is_ok() {
            early += 1;
        }
    }
    // Two frames of lookahead behind 20 complete ones.
    assert_eq!(early, 18);
    e.finish();
    let rest = std::iter::from_fn(|| e.next_packet().ok()).count();
    assert_eq!(early + rest, 21, "20 frames plus the flush frame");
}

#[test]
fn default_streaming_decodes_as_well_as_the_whole_clip() {
    for ch in [1u16, 2] {
        let pcm = signal(5 * RATE as usize, ch as usize, 3);
        let want = whole(AacEncoderConfig::default(), &pcm, ch);
        let (got, early, enc) = streamed(AacEncoderConfig::default(), &pcm, ch, 11);
        assert_eq!(got.len(), want.len());
        // At most the two lookahead frames, a partial frame and the flush frame wait for `finish`.
        assert!(
            early + 4 >= got.len(),
            "streamed, not held to the end: {early} of {}",
            got.len()
        );
        let same = got.iter().zip(&want).filter(|(a, b)| a == b).count();
        let p = enc.priming_samples() as usize;
        let (s_whole, s_stream) = (
            snr_at(&pcm, &decode(&want, ch), ch as usize, p),
            snr_at(&pcm, &decode(&got, ch), ch as usize, p),
        );
        println!(
            "{ch} ch: {same}/{} packets identical, SNR whole {s_whole:.2} dB, streaming {s_stream:.2} dB",
            got.len()
        );
        assert!((s_whole - s_stream).abs() < 0.5, "{s_whole} vs {s_stream}");
    }
}

#[test]
fn priming_and_padding_place_the_input_in_the_decoded_output() {
    let n = 10_000;
    let pcm = signal(n, 1, 5);
    let (packets, _, enc) = streamed(AacEncoderConfig::default(), &pcm, 1, 2);
    let (priming, padding) = (
        enc.priming_samples() as usize,
        enc.padding_samples().unwrap() as usize,
    );
    assert_eq!(priming, 1024);
    let decoded = decode(&packets, 1);
    assert_eq!(
        decoded.len(),
        priming + n + padding,
        "decoded = priming + input + padding"
    );
    assert_eq!((priming + n + padding) % 1024, 0);
    let at = snr_at(&pcm, &decoded, 1, priming);
    assert!(at > 15.0, "input sits {priming} samples in: {at:.1} dB");
    assert!(at > snr_at(&pcm, &decoded, 1, 0) + 10.0);
    assert!(at > snr_at(&pcm, &decoded, 1, 2 * priming) + 10.0);
    // The whole-clip encode reports the same.
    let mut e = AacEncoder::default();
    e.push_pcm(&pcm, 1, RATE).unwrap();
    assert_eq!(e.padding_samples(), None, "not known before finish");
    e.finish();
    assert_eq!(e.padding_samples(), Some(padding as u32));
}

#[test]
fn streaming_must_be_chosen_before_the_first_push() {
    let pcm = signal(4096, 1, 9);
    let mut e = AacEncoder::default();
    e.push_pcm(&pcm, 1, RATE).unwrap();
    e.set_streaming(true); // too late: ignored
    assert!(e.next_packet().is_err());
    e.push_pcm(&pcm, 1, RATE).unwrap();
    assert!(e.next_packet().is_err(), "still the whole-clip encoder");
}

/// Streaming against the whole-clip encode on the lab corpus (every class, its
/// own rate and channels): packets byte-identical, and the NMR of channel 0.
/// `cargo test -p rusty_aac --features lab --test streaming -- --nocapture`.
#[cfg(feature = "lab")]
#[test]
fn lab_corpus_streaming_against_the_whole_clip() {
    use rusty_aac::lab::corpus::corpus;
    use rusty_aac::lab::quality::track_nmr;
    for sig in corpus() {
        let ch = sig.channels;
        let encode = |stream: bool| {
            let mut e = AacEncoder::default();
            e.set_streaming(stream);
            for chunk in sig.pcm.chunks(4096 * ch as usize) {
                e.push_pcm(chunk, ch, sig.sample_rate).unwrap();
            }
            e.finish();
            std::iter::from_fn(|| e.next_packet().ok().map(|p| (p.data, p.pts))).collect::<Vec<_>>()
        };
        let (want, got) = (encode(false), encode(true));
        assert_eq!(want.len(), got.len());
        let same = want.iter().zip(&got).filter(|(a, b)| a == b).count();
        let mono = |packets: &[(Vec<u8>, i64)]| -> Vec<f32> {
            let mut d = AacDecoder::with_config(AudioSpecificConfig {
                object_type: 2,
                sample_rate: sig.sample_rate,
                channels: ch,
            });
            let all: Vec<f32> = packets
                .iter()
                .flat_map(|(data, pts)| d.decode(data, Some(*pts)).unwrap().samples)
                .collect();
            all.iter()
                .step_by(ch as usize)
                .skip(1024)
                .take(sig.frames())
                .copied()
                .collect()
        };
        let orig = sig.mono();
        let (a, b) = (
            track_nmr(&orig, &mono(&want), sig.sample_rate),
            track_nmr(&orig, &mono(&got), sig.sample_rate),
        );
        println!(
            "{:<12} {same:>4}/{:<4} identical  NMR whole {:+.3} dB  streaming {:+.3} dB",
            sig.name(),
            want.len(),
            a.mean_nmr_db,
            b.mean_nmr_db
        );
        assert!(
            (a.mean_nmr_db - b.mean_nmr_db).abs() < 0.25,
            "{}",
            sig.name()
        );
    }
}
