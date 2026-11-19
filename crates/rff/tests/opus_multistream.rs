//! Multichannel Ogg Opus (mapping families 1 and 255) through the engine,
//! against libopus 1.6.1 (issue #125).
//!
//! The fixtures' packets come from libopus's multistream encoder, written to
//! Ogg with an OpusHead carrying the family, stream counts and mapping table
//! (pre-skip 312, final granule cutting the last packet). Each `.r16` holds
//! what rff must output — libopus's `opus_multistream_decode_float` of the
//! same packets, pre-skip dropped, cut at the final granule, family 1
//! reordered from Vorbis to FFmpeg order as libavcodec does — as an FNV-1a
//! hash of the whole float output plus every 4th frame as rounded s16:
//!
//! - `opus_ms_5_1`: 5.1 from libopus's surround encoder (coupled streams
//!   CELT, centre/LFE hybrid), 20 ms.
//! - `opus_ms_7_1_60ms`: 7.1 CELT in 60 ms (multi-frame) packets.
//! - `opus_ms_3_0_silk`: 3.0 SILK, which rusty-opus decodes bit-exactly.
//! - `opus_ms_f255`: family 255, 6 outputs from one coupled and two mono
//!   streams; output 3 silent, outputs 4 and 5 the same mono stream.
//!
//! The generator lives in rusty-opus `tools/ms_oracle`. Set
//! `RFF_OPUS_ORACLE=<its out/ dir>` to run its whole corpus through rff too.

use std::path::{Path, PathBuf};

use rff::codec::CodecParams;
use rff::core::{CodecId, Error, Frame};
use rff::Engine;

fn testdata(name: &str) -> PathBuf {
    Path::new(env!("CARGO_MANIFEST_DIR"))
        .join("testdata")
        .join(name)
}

/// Demux + decode an Ogg file; returns (channels, interleaved f32).
fn decode(file: Vec<u8>) -> (usize, Vec<f32>) {
    let engine = Engine::new();
    let mut dem = engine
        .formats
        .open_demuxer("ogg", Box::new(std::io::Cursor::new(file)))
        .unwrap();
    let streams = dem.read_header().unwrap();
    let s = &streams[0];
    assert_eq!(s.codec_id, CodecId::Opus);
    let mut dec = engine.codecs.find_decoder(s.codec_id).unwrap();
    dec.configure(&CodecParams {
        codec_id: s.codec_id,
        sample_rate: s.sample_rate,
        channels: s.channels,
        sample_format: s.sample_format,
        extradata: s.extradata.clone(),
        ..Default::default()
    })
    .unwrap();
    let mut samples = Vec::new();
    let mut drain = |dec: &mut Box<dyn rff::codec::Decoder>| loop {
        match dec.receive_frame() {
            Ok(Frame::Audio(af)) => {
                assert_eq!(af.channels, s.channels);
                samples.extend(
                    af.planes[0]
                        .chunks_exact(4)
                        .map(|b| f32::from_le_bytes([b[0], b[1], b[2], b[3]])),
                );
            }
            Ok(_) => panic!("video frame from an audio decoder"),
            Err(Error::Again) | Err(Error::Eof) => break,
            Err(e) => panic!("decode: {e}"),
        }
    };
    loop {
        match dem.read_packet() {
            Ok(p) => {
                dec.send_packet(&p).unwrap();
                drain(&mut dec);
            }
            Err(Error::Eof) => break,
            Err(e) => panic!("demux: {e}"),
        }
    }
    dec.flush();
    drain(&mut dec);
    (s.channels as usize, samples)
}

fn fnv1a64(samples: &[f32]) -> u64 {
    let mut h = 0xcbf2_9ce4_8422_2325u64;
    for s in samples {
        for b in s.to_le_bytes() {
            h ^= u64::from(b);
            h = h.wrapping_mul(0x0100_0000_01b3);
        }
    }
    h
}

fn u32le(b: &[u8], at: usize) -> u32 {
    u32::from_le_bytes(b[at..at + 4].try_into().unwrap())
}

/// Decode `name.opus` and hold it to `name.r16`: the frame count exactly,
/// every stored sample within 1 s16 LSB, and bit for bit if `exact`.
fn check(name: &str, exact: bool) {
    let (channels, got) = decode(std::fs::read(testdata(&format!("{name}.opus"))).unwrap());
    let r16 = std::fs::read(testdata(&format!("{name}.r16"))).unwrap();
    assert_eq!(&r16[..4], b"R16\0");
    let step = u32le(&r16, 4) as usize;
    let hash = u64::from_le_bytes(r16[8..16].try_into().unwrap());
    let frames = u32le(&r16, 16) as usize;
    assert_eq!(r16[20] as usize, channels, "{name}: channels");
    assert_eq!(got.len(), frames * channels, "{name}: frames");
    let mut worst = 0i32;
    for (k, want) in r16[21..].chunks_exact(2 * channels).enumerate() {
        for c in 0..channels {
            let w = i16::from_le_bytes([want[2 * c], want[2 * c + 1]]) as i32;
            let g = (got[k * step * channels + c] * 32768.0)
                .round()
                .clamp(-32768.0, 32767.0) as i32;
            worst = worst.max((g - w).abs());
        }
    }
    assert!(worst <= 1, "{name}: {worst} s16 LSB from libopus");
    if exact {
        assert_eq!(fnv1a64(&got), hash, "{name}: not bit-exact with libopus");
    }
}

#[test]
fn opus_5_1_matches_libopus_in_ffmpeg_order() {
    check("opus_ms_5_1", false);
}

#[test]
fn opus_7_1_60ms_matches_libopus_in_ffmpeg_order() {
    check("opus_ms_7_1_60ms", false);
}

#[test]
fn opus_3_0_silk_is_bit_exact_with_libopus() {
    check("opus_ms_3_0_silk", true);
}

#[test]
fn opus_family_255_matches_libopus() {
    check("opus_ms_f255", false);
}

/// FFmpeg's order for a family-1 file with `channels` channels: output `i`
/// takes Vorbis-order channel `ORDER[i]` (libavcodec
/// `ff_vorbis_channel_layout_offsets`).
fn ffmpeg_order(channels: usize) -> Vec<usize> {
    match channels {
        3 => vec![0, 2, 1],
        5 => vec![0, 2, 1, 3, 4],
        6 => vec![0, 2, 1, 5, 3, 4],
        7 => vec![0, 2, 1, 6, 5, 3, 4],
        8 => vec![0, 2, 1, 7, 5, 6, 3, 4],
        n => (0..n).collect(),
    }
}

/// Every `.opus` in `$RFF_OPUS_ORACLE` (the full generator corpus, with its
/// untrimmed `.msp`/`.ref`): a per-config table (run with `--nocapture`).
/// Asserts only that each decodes to the right length.
#[test]
fn oracle_directory_table() {
    let Ok(dir) = std::env::var("RFF_OPUS_ORACLE") else {
        return;
    };
    let dir = Path::new(&dir);
    let mut names: Vec<String> = std::fs::read_dir(dir)
        .unwrap()
        .filter_map(|e| {
            let p = e.unwrap().path();
            (p.extension()? == "opus").then(|| p.file_stem().unwrap().to_string_lossy().into())
        })
        .collect();
    names.sort();
    println!(
        "{:<24} {:>3} {:>3} {:>7} {:>12} {:>9}  bit-exact",
        "config", "fam", "ch", "frames", "max|diff|", "s16 LSB"
    );
    for name in names {
        let msp = std::fs::read(dir.join(format!("{name}.msp"))).unwrap();
        // Ogg Opus decodes at 48 kHz; references at other rates don't apply.
        if u32le(&msp, 4) != 48_000 {
            continue;
        }
        let (ch, family) = (msp[8] as usize, msp[11]);
        let at = 12 + ch;
        let pre_skip = u16::from_le_bytes([msp[at], msp[at + 1]]) as usize;
        let input_len = u32le(&msp, at + 2) as usize;
        let reference: Vec<f32> = std::fs::read(dir.join(format!("{name}.ref")))
            .unwrap()
            .chunks_exact(4)
            .map(|b| f32::from_le_bytes([b[0], b[1], b[2], b[3]]))
            .collect();
        let order = if family == 1 {
            ffmpeg_order(ch)
        } else {
            (0..ch).collect()
        };
        let (channels, got) = decode(std::fs::read(dir.join(format!("{name}.opus"))).unwrap());
        assert_eq!(channels, ch);
        assert_eq!(got.len(), input_len * ch, "{name}: frames");
        let mut max = 0f32;
        let mut exact = true;
        for i in 0..input_len {
            for (c, &v) in order.iter().enumerate() {
                let (g, w) = (got[i * ch + c], reference[(pre_skip + i) * ch + v]);
                exact &= g.to_bits() == w.to_bits();
                max = max.max((g - w).abs());
            }
        }
        println!(
            "{:<24} {:>3} {:>3} {:>7} {:>12.3e} {:>9.3}  {}",
            name,
            family,
            ch,
            input_len,
            max,
            max * 32768.0,
            if exact { "yes" } else { "no" }
        );
    }
}
