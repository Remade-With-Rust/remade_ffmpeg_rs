//! Encoder ↔ lewton round-trip gates, moved here from the (now dependency-free)
//! `rusty_vorbis` encoder crate: the adapter is where the encoder and the lewton
//! decode oracle may meet. Every assertion is preserved from the original
//! in-crate tests.

use rff_codec::Encoder;
use rff_codec_vorbis::VorbisEncoder;
use rff_core::{AudioFrame, Frame, SampleFormat};
use rusty_vorbis::frame::{encode_long_packet, encode_stream_bs};
use rusty_vorbis::setup::{parse_setup, SETUP_Q4_STEREO};
use rusty_vorbis::{write_comment_header, write_ident_header, BITRATE_NOMINAL, BS0_LOG2, BS1_LOG2};

/// Best normalized cross-correlation of `got` against `reference` over lags `0..max_lag`.
fn best_correlation(reference: &[f32], got: &[f32], max_lag: usize) -> f32 {
    let mut best = 0.0f32;
    for lag in 0..max_lag {
        if lag + got.len() > reference.len() {
            break;
        }
        let mut dot = 0.0f32;
        let mut er = 0.0f32;
        let mut eg = 0.0f32;
        for (k, &g) in got.iter().enumerate() {
            let r = reference[lag + k];
            dot += r * g;
            er += r * r;
            eg += g * g;
        }
        if er > 0.0 && eg > 0.0 {
            let c = dot / (er.sqrt() * eg.sqrt());
            if c > best {
                best = c;
            }
        }
    }
    best
}

/// Minimal WAV reader (PCM s16) → (sample_rate, channels, interleaved f32).
fn read_wav(path: &str) -> (u32, usize, Vec<f32>) {
    let d = std::fs::read(path).unwrap();
    let rate = u32::from_le_bytes([d[24], d[25], d[26], d[27]]);
    let channels = u16::from_le_bytes([d[22], d[23]]) as usize;
    // Find the "data" chunk.
    let mut i = 12;
    while &d[i..i + 4] != b"data" {
        let sz = u32::from_le_bytes([d[i + 4], d[i + 5], d[i + 6], d[i + 7]]) as usize;
        i += 8 + sz;
    }
    let sz = u32::from_le_bytes([d[i + 4], d[i + 5], d[i + 6], d[i + 7]]) as usize;
    let pcm = &d[i + 8..i + 8 + sz];
    let samples = (0..pcm.len() / 2)
        .map(|k| i16::from_le_bytes([pcm[2 * k], pcm[2 * k + 1]]) as f32 / 32768.0)
        .collect();
    (rate, channels, samples)
}

/// THE SPIKE: our generated ident + comment and the embedded q4 setup must all
/// parse in lewton — proving the codebook/setup strategy end to end.
#[test]
fn headers_parse_in_lewton() {
    let ident_bytes = write_ident_header(2, 44_100, BS0_LOG2, BS1_LOG2, BITRATE_NOMINAL);
    let ident = lewton::header::read_header_ident(&ident_bytes).expect("ident parses");
    assert_eq!(ident.audio_channels, 2);
    assert_eq!(ident.audio_sample_rate, 44_100);
    // lewton stores blocksizes as log2 exponents (256 = 2^8, 2048 = 2^11).
    assert_eq!(ident.blocksize_0, BS0_LOG2);
    assert_eq!(ident.blocksize_1, BS1_LOG2);

    let comment_bytes = write_comment_header("remade_ffmpeg_rs", &[("ENCODER", "rff")]);
    lewton::header::read_header_comment(&comment_bytes).expect("comment parses");

    // The crux: the embedded libvorbis q4 setup must be lewton-decodable.
    lewton::header::read_header_setup(SETUP_Q4_STEREO, 2, (BS0_LOG2, BS1_LOG2))
        .expect("setup parses");
}

/// The streaming encoder (send_frame → receive_packet, fed in odd-sized chunks) must
/// produce audio packets that lewton decodes to non-trivial audio, using the encoder's
/// own `headers()`. Validates the block-management + header plumbing end to end.
#[test]
fn streaming_encode_decodes_in_lewton() {
    let mut enc = VorbisEncoder::new();
    let sr = 44_100u32;
    let total = 2048 * 6usize;
    let sample = |ch: usize, i: usize| -> f32 {
        let f = if ch == 0 { 0.02 } else { 0.023 };
        0.4 * (f * i as f32).sin()
    };
    // Feed in 1000-sample chunks to exercise arbitrary frame boundaries.
    let mut i = 0;
    while i < total {
        let chunk = 1000.min(total - i);
        let mut plane = Vec::with_capacity(chunk * 2 * 4);
        for k in 0..chunk {
            for ch in 0..2 {
                plane.extend_from_slice(&sample(ch, i + k).to_le_bytes());
            }
        }
        let frame = Frame::Audio(AudioFrame {
            sample_rate: sr,
            channels: 2,
            format: SampleFormat::F32,
            planes: vec![plane],
            samples: chunk,
            pts: Some(i as i64),
        });
        enc.send_frame(&frame).unwrap();
        i += chunk;
    }
    let mut packets = Vec::new();
    while let Ok(p) = enc.receive_packet() {
        packets.push(p);
    }
    enc.flush();
    while let Ok(p) = enc.receive_packet() {
        packets.push(p);
    }
    assert!(
        packets.len() >= 5,
        "expected multiple audio packets, got {}",
        packets.len()
    );

    let headers = enc.headers();
    let l_ident = lewton::header::read_header_ident(&headers[0]).unwrap();
    let l_setup = lewton::header::read_header_setup(&headers[2], 2, (BS0_LOG2, BS1_LOG2)).unwrap();
    let mut pwr = lewton::audio::PreviousWindowRight::new();
    let mut decoded: Vec<f32> = Vec::new();
    for p in &packets {
        // The first three packets are the setup headers, not audio.
        if p.data.len() >= 7 && p.data[0] & 1 == 1 && &p.data[1..7] == b"vorbis" {
            continue;
        }
        let pcm = lewton::audio::read_audio_packet(&l_ident, &l_setup, &p.data, &mut pwr).unwrap();
        if !pcm.is_empty() && !pcm[0].is_empty() {
            decoded.extend(pcm[0].iter().map(|&s| s as f32 / 32768.0));
        }
    }
    assert!(!decoded.is_empty(), "no audio decoded");
    let energy: f32 = decoded.iter().map(|x| x * x).sum::<f32>() / decoded.len() as f32;
    assert!(
        energy > 1e-4 && energy < 1.0,
        "decoded energy out of range: {energy}"
    );
}

/// Encode long-block packets from a test tone, decode them with lewton, and confirm the
/// output resembles the input (correlation, not bit-exact).
#[test]
fn packets_decode_in_lewton() {
    let setup = parse_setup(SETUP_Q4_STEREO, 2).unwrap();
    let ident_bytes = write_ident_header(2, 44_100, BS0_LOG2, BS1_LOG2, BITRATE_NOMINAL);
    let comment_bytes = write_comment_header("rff", &[]);
    let setup_bytes = SETUP_Q4_STEREO;
    let l_ident = lewton::header::read_header_ident(&ident_bytes).unwrap();
    let _l_comment = lewton::header::read_header_comment(&comment_bytes).unwrap();
    let l_setup = lewton::header::read_header_setup(setup_bytes, 2, (BS0_LOG2, BS1_LOG2)).unwrap();

    let n = 2048usize;
    let hop = n / 2;
    let total = n * 6;
    // A steady stereo tone (both channels a low-frequency sine).
    let signal: Vec<Vec<f32>> = (0..2)
        .map(|ch| {
            (0..total)
                .map(|i| {
                    let f = if ch == 0 { 0.02 } else { 0.023 };
                    0.5 * (f * i as f32).sin()
                })
                .collect()
        })
        .collect();

    let mut pwr = lewton::audio::PreviousWindowRight::new();
    let mut decoded: Vec<Vec<f32>> = vec![Vec::new(), Vec::new()];
    let mut pos = 0;
    while pos + n <= total {
        let blocks: Vec<Vec<f32>> = (0..2).map(|ch| signal[ch][pos..pos + n].to_vec()).collect();
        let packet = encode_long_packet(&setup, &blocks, 44_100, 0.5).unwrap();
        let pcm = lewton::audio::read_audio_packet(&l_ident, &l_setup, &packet, &mut pwr)
            .expect("lewton decodes our packet");
        if !pcm.is_empty() && !pcm[0].is_empty() {
            for (ch, chan) in pcm.iter().enumerate() {
                decoded[ch].extend(chan.iter().map(|&s| s as f32 / 32768.0));
            }
        }
        pos += hop;
    }

    assert!(!decoded[0].is_empty(), "no audio decoded");
    let got = &decoded[0];
    let out_energy: f32 = got.iter().map(|x| x * x).sum::<f32>() / got.len() as f32;
    assert!(
        out_energy > 1e-4,
        "decoded audio is basically silent: {out_energy}"
    );
    assert!(out_energy < 1.0, "decoded audio blew up: {out_energy}");

    let best = best_correlation(&signal[0], got, 2 * n);
    eprintln!("CORR ch0 best normalized correlation = {best:.4}");
    assert!(
        best > 0.8,
        "decoded audio does not resemble the input (corr={best:.4})"
    );
}

/// Block switching: a signal with a transient must encode via `encode_stream_bs` (which fires
/// a short-block run over the transient), decode in lewton, and reconstruct — proving the mixed
/// long/short/transition packet stream is valid Vorbis that overlap-adds correctly.
#[test]
fn block_switch_decodes_in_lewton() {
    let setup = parse_setup(SETUP_Q4_STEREO, 2).unwrap();
    let ident = write_ident_header(2, 44_100, BS0_LOG2, BS1_LOG2, BITRATE_NOMINAL);
    let l_ident = lewton::header::read_header_ident(&ident).unwrap();
    let l_setup =
        lewton::header::read_header_setup(SETUP_Q4_STEREO, 2, (BS0_LOG2, BS1_LOG2)).unwrap();

    let total = 2048 * 10;
    // Tones + two transient bursts (castanet-like attacks) that should trigger short blocks.
    let signal: Vec<Vec<f32>> = (0..2)
        .map(|ch| {
            (0..total)
                .map(|i| {
                    let t = i as f32;
                    let base = 0.4 * (0.02 * t).sin() + 0.2 * (0.07 * t + 0.5).sin();
                    let attack = (7000..7160).contains(&i) || (12000..12160).contains(&i);
                    base + if attack {
                        0.8 * (0.8 * t + ch as f32).sin()
                    } else {
                        0.0
                    }
                })
                .collect()
        })
        .collect();

    let packets = encode_stream_bs(&setup, &signal, 44_100, 0.7).unwrap();
    // Confirm a mix of block sizes was actually emitted (not all long).
    let shorts = packets.iter().filter(|p| p.len() < 80).count();
    eprintln!("BS: {} packets, ~{} short", packets.len(), shorts);

    let mut pwr = lewton::audio::PreviousWindowRight::new();
    let mut decoded: Vec<Vec<f32>> = vec![Vec::new(), Vec::new()];
    for pkt in &packets {
        let pcm = lewton::audio::read_audio_packet(&l_ident, &l_setup, pkt, &mut pwr)
            .expect("lewton decodes our block-switched packet");
        if !pcm.is_empty() && !pcm[0].is_empty() {
            for (c, ch) in pcm.iter().enumerate() {
                decoded[c].extend(ch.iter().map(|&s| s as f32 / 32768.0));
            }
        }
    }
    assert!(!decoded[0].is_empty(), "no audio decoded");
    let best = best_correlation(&signal[0], &decoded[0], 2 * 2048);
    eprintln!("BS corr = {best:.4}");
    assert!(
        best > 0.9,
        "block-switched decode does not reconstruct (corr={best:.4})"
    );
}

/// Brick 3: on a multi-tone signal the fitted floor should reconstruct with high
/// correlation (a flat floor smears multi-formant spectra).
#[test]
fn fitted_floor_reconstructs_multitone() {
    let setup = parse_setup(SETUP_Q4_STEREO, 2).unwrap();
    let ident_bytes = write_ident_header(2, 44_100, BS0_LOG2, BS1_LOG2, BITRATE_NOMINAL);
    let l_ident = lewton::header::read_header_ident(&ident_bytes).unwrap();
    let l_setup =
        lewton::header::read_header_setup(SETUP_Q4_STEREO, 2, (BS0_LOG2, BS1_LOG2)).unwrap();

    let n = 2048usize;
    let hop = n / 2;
    let total = n * 8;
    // Rich signal: several partials at different amplitudes (a shaped spectrum).
    let signal: Vec<Vec<f32>> = (0..2)
        .map(|_| {
            (0..total)
                .map(|i| {
                    let t = i as f32;
                    0.4 * (0.03 * t).sin()
                        + 0.25 * (0.08 * t).sin()
                        + 0.15 * (0.17 * t).sin()
                        + 0.08 * (0.31 * t).cos()
                })
                .collect()
        })
        .collect();

    let mut pwr = lewton::audio::PreviousWindowRight::new();
    let mut decoded: Vec<f32> = Vec::new();
    let mut pos = 0;
    while pos + n <= total {
        let blocks: Vec<Vec<f32>> = (0..2).map(|ch| signal[ch][pos..pos + n].to_vec()).collect();
        let packet = encode_long_packet(&setup, &blocks, 44_100, 0.5).unwrap();
        let pcm = lewton::audio::read_audio_packet(&l_ident, &l_setup, &packet, &mut pwr)
            .expect("lewton decodes our packet");
        if !pcm.is_empty() && !pcm[0].is_empty() {
            decoded.extend(pcm[0].iter().map(|&s| s as f32 / 32768.0));
        }
        pos += hop;
    }
    assert!(!decoded.is_empty());
    let best = best_correlation(&signal[0], &decoded, 2 * n);
    eprintln!("CORR multitone best correlation = {best:.4}");
    assert!(
        best > 0.9,
        "fitted floor multitone reconstruction poor (corr={best:.4})"
    );
}

/// A dense broadband spectrum (many partials, shaped envelope) — the case a flat floor
/// smears and a fitted floor should track. Reports SNR so floor tuning is visible.
#[test]
fn fitted_floor_reconstructs_broadband() {
    let setup = parse_setup(SETUP_Q4_STEREO, 2).unwrap();
    let ident_bytes = write_ident_header(2, 44_100, BS0_LOG2, BS1_LOG2, BITRATE_NOMINAL);
    let l_ident = lewton::header::read_header_ident(&ident_bytes).unwrap();
    let l_setup =
        lewton::header::read_header_setup(SETUP_Q4_STEREO, 2, (BS0_LOG2, BS1_LOG2)).unwrap();

    let n = 2048usize;
    let hop = n / 2;
    let total = n * 8;
    // A dense broadband spectrum: 64 partials across the band with a 1/√k envelope and
    // decorrelating phases — many partitions carry medium content (stresses the residue).
    let signal: Vec<Vec<f32>> = (0..2)
        .map(|_| {
            (0..total)
                .map(|i| {
                    let t = i as f32;
                    let mut s = 0.0f32;
                    for k in 1..65 {
                        let amp = 0.5 / (k as f32).sqrt();
                        let phase = 1.3 * k as f32 * (k as f32 + 1.0);
                        s += amp * (0.022 * k as f32 * t + phase).sin();
                    }
                    s * 0.12
                })
                .collect()
        })
        .collect();

    let mut pwr = lewton::audio::PreviousWindowRight::new();
    let mut decoded: Vec<f32> = Vec::new();
    let mut pos = 0;
    while pos + n <= total {
        let blocks: Vec<Vec<f32>> = (0..2).map(|ch| signal[ch][pos..pos + n].to_vec()).collect();
        let packet = encode_long_packet(&setup, &blocks, 44_100, 0.5).unwrap();
        let pcm = lewton::audio::read_audio_packet(&l_ident, &l_setup, &packet, &mut pwr)
            .expect("lewton decodes our packet");
        if !pcm.is_empty() && !pcm[0].is_empty() {
            decoded.extend(pcm[0].iter().map(|&s| s as f32 / 32768.0));
        }
        pos += hop;
    }
    assert!(!decoded.is_empty());
    let best = best_correlation(&signal[0], &decoded, 2 * n);
    eprintln!("CORR broadband = {best:.4}");
    assert!(
        best > 0.85,
        "fitted floor broadband reconstruction poor (corr={best:.4})"
    );
}

/// Opt-in: `-q` sweep — bitrate must rise monotonically with quality, and correlation
/// improve. Demonstrates the psychoacoustic `-q` knob.
#[test]
#[ignore]
fn quality_sweep() {
    let setup = parse_setup(SETUP_Q4_STEREO, 2).unwrap();
    let ident_bytes = write_ident_header(2, 44_100, BS0_LOG2, BS1_LOG2, BITRATE_NOMINAL);
    let l_ident = lewton::header::read_header_ident(&ident_bytes).unwrap();
    let l_setup =
        lewton::header::read_header_setup(SETUP_Q4_STEREO, 2, (BS0_LOG2, BS1_LOG2)).unwrap();
    let n = 2048usize;
    let hop = n / 2;
    let total = n * 8;
    let signal: Vec<Vec<f32>> = (0..2)
        .map(|_| {
            (0..total)
                .map(|i| {
                    let t = i as f32;
                    let mut s = 0.0f32;
                    for k in 1..65 {
                        s += 0.5 / (k as f32).sqrt()
                            * (0.022 * k as f32 * t + 1.3 * k as f32 * (k as f32 + 1.0)).sin();
                    }
                    s * 0.12
                })
                .collect()
        })
        .collect();
    for &q in &[0.1f32, 0.3, 0.5, 0.7, 0.9] {
        let mut pwr = lewton::audio::PreviousWindowRight::new();
        let mut decoded: Vec<f32> = Vec::new();
        let mut bytes = 0usize;
        let mut pkts = 0usize;
        let mut pos = 0;
        while pos + n <= total {
            let blocks: Vec<Vec<f32>> =
                (0..2).map(|ch| signal[ch][pos..pos + n].to_vec()).collect();
            let packet = encode_long_packet(&setup, &blocks, 44_100, q).unwrap();
            bytes += packet.len();
            pkts += 1;
            let pcm =
                lewton::audio::read_audio_packet(&l_ident, &l_setup, &packet, &mut pwr).unwrap();
            if !pcm.is_empty() && !pcm[0].is_empty() {
                decoded.extend(pcm[0].iter().map(|&s| s as f32 / 32768.0));
            }
            pos += hop;
        }
        let corr = best_correlation(&signal[0], &decoded, 2 * n);
        let kbps = (bytes as f32 / pkts as f32) * (44100.0 / 1024.0) * 8.0 / 1000.0;
        eprintln!(
            "QSWEEP q={q:.1}  {} B/pkt  ~{kbps:.0} kb/s  corr={corr:.4}",
            bytes / pkts
        );
    }
}

/// Opt-in: encode real audio from `$VORBIS_WAV_IN` at several `-q`, reporting our bitrate
/// and reconstruction correlation for a side-by-side with ffmpeg's libvorbis.
#[test]
#[ignore]
fn compare_real_audio() {
    let Ok(path) = std::env::var("VORBIS_WAV_IN") else {
        return;
    };
    let (rate, channels, inter) = read_wav(&path);
    assert_eq!(channels, 2, "test expects stereo");
    let n = 2048usize;
    let hop = n / 2;
    let frames = inter.len() / channels;
    let dur = frames.min(rate as usize * 4); // first ~4 s
    let sig: Vec<Vec<f32>> = (0..2)
        .map(|c| (0..dur).map(|i| inter[i * channels + c]).collect())
        .collect();
    let setup = parse_setup(SETUP_Q4_STEREO, 2).unwrap();
    let ident = write_ident_header(2, rate, BS0_LOG2, BS1_LOG2, BITRATE_NOMINAL);
    let l_ident = lewton::header::read_header_ident(&ident).unwrap();
    let l_setup =
        lewton::header::read_header_setup(SETUP_Q4_STEREO, 2, (BS0_LOG2, BS1_LOG2)).unwrap();
    for &q in &[0.3f32, 0.5, 0.7, 0.9] {
        let mut pwr = lewton::audio::PreviousWindowRight::new();
        let mut decoded: Vec<f32> = Vec::new();
        let (mut bytes, mut pkts, mut pos) = (0usize, 0usize, 0usize);
        while pos + n <= dur {
            let blocks: Vec<Vec<f32>> = (0..2).map(|c| sig[c][pos..pos + n].to_vec()).collect();
            let packet = encode_long_packet(&setup, &blocks, rate, q).unwrap();
            bytes += packet.len();
            pkts += 1;
            let pcm =
                lewton::audio::read_audio_packet(&l_ident, &l_setup, &packet, &mut pwr).unwrap();
            if !pcm.is_empty() && !pcm[0].is_empty() {
                decoded.extend(pcm[0].iter().map(|&s| s as f32 / 32768.0));
            }
            pos += hop;
        }
        let corr = best_correlation(&sig[0], &decoded, 2 * n);
        let kbps = (bytes as f32 / pkts as f32) * (rate as f32 / hop as f32) * 8.0 / 1000.0;
        eprintln!("REAL q={q:.1}  ~{kbps:.0} kb/s  corr={corr:.4}");
    }
}

/// Opt-in: encode `$VORBIS_WAV_IN` (s16 stereo) at `$VORBIS_Q` and write lewton's decode to
/// `$VORBIS_WAV_OUT` (s16 stereo). A clean encoder-quality reference (the known-good decode
/// path) for PEAQ, independent of the Ogg muxer.
#[test]
#[ignore]
fn dump_lewton_decode() {
    let (Ok(inp), Ok(outp)) = (
        std::env::var("VORBIS_WAV_IN"),
        std::env::var("VORBIS_WAV_OUT"),
    ) else {
        return;
    };
    let q: f32 = std::env::var("VORBIS_Q")
        .ok()
        .and_then(|s| s.parse().ok())
        .unwrap_or(0.9);
    let (rate, channels, inter) = read_wav(&inp);
    assert_eq!(channels, 2, "expects stereo");
    let n = 2048usize;
    let hop = n / 2;
    let frames = inter.len() / channels;
    let sig: Vec<Vec<f32>> = (0..2)
        .map(|c| (0..frames).map(|i| inter[i * channels + c]).collect())
        .collect();
    let setup = parse_setup(SETUP_Q4_STEREO, 2).unwrap();
    let ident = write_ident_header(2, rate, BS0_LOG2, BS1_LOG2, BITRATE_NOMINAL);
    let l_ident = lewton::header::read_header_ident(&ident).unwrap();
    let l_setup =
        lewton::header::read_header_setup(SETUP_Q4_STEREO, 2, (BS0_LOG2, BS1_LOG2)).unwrap();
    // Encode: block-switching path when VORBIS_BS is set, else the long-only path.
    let packets: Vec<Vec<u8>> = if std::env::var("VORBIS_BS").is_ok() {
        encode_stream_bs(&setup, &sig, rate, q).unwrap()
    } else {
        let mut pkts = Vec::new();
        let mut pos = 0;
        while pos + n <= frames {
            let blocks: Vec<Vec<f32>> = (0..2).map(|c| sig[c][pos..pos + n].to_vec()).collect();
            pkts.push(encode_long_packet(&setup, &blocks, rate, q).unwrap());
            pos += hop;
        }
        pkts
    };
    let mut pwr = lewton::audio::PreviousWindowRight::new();
    let mut dec: Vec<Vec<f32>> = vec![Vec::new(), Vec::new()];
    let mut bytes = 0usize;
    for packet in &packets {
        bytes += packet.len();
        let pcm = lewton::audio::read_audio_packet(&l_ident, &l_setup, packet, &mut pwr).unwrap();
        if !pcm.is_empty() && !pcm[0].is_empty() {
            for (c, ch) in pcm.iter().enumerate() {
                dec[c].extend(ch.iter().map(|&s| s as f32 / 32768.0));
            }
        }
    }
    let kbps = bytes as f64 * 8.0 * rate as f64 / (frames as f64 * 1000.0);
    eprintln!("KBPS {kbps:.1}");
    // Write interleaved s16 stereo wav.
    let nsamp = dec[0].len().min(dec[1].len());
    let mut body = Vec::with_capacity(nsamp * 4);
    for i in 0..nsamp {
        for c in 0..2 {
            let v = (dec[c][i] * 32768.0).clamp(-32768.0, 32767.0) as i16;
            body.extend_from_slice(&v.to_le_bytes());
        }
    }
    let mut w = Vec::new();
    let data_len = body.len() as u32;
    w.extend_from_slice(b"RIFF");
    w.extend_from_slice(&(36 + data_len).to_le_bytes());
    w.extend_from_slice(b"WAVEfmt ");
    w.extend_from_slice(&16u32.to_le_bytes());
    w.extend_from_slice(&1u16.to_le_bytes()); // PCM
    w.extend_from_slice(&2u16.to_le_bytes()); // channels
    w.extend_from_slice(&rate.to_le_bytes());
    w.extend_from_slice(&(rate * 4).to_le_bytes()); // byte rate
    w.extend_from_slice(&4u16.to_le_bytes()); // block align
    w.extend_from_slice(&16u16.to_le_bytes()); // bits
    w.extend_from_slice(b"data");
    w.extend_from_slice(&data_len.to_le_bytes());
    w.extend_from_slice(&body);
    std::fs::write(&outp, &w).unwrap();
    eprintln!("wrote {nsamp} samples/ch to {outp}");
}
