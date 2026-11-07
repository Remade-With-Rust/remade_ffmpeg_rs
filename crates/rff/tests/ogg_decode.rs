//! Ogg Vorbis / Opus decode against FFmpeg 8.1 (issue #49): float output, the
//! stream end trimmed to the final page's granule position, and channels in
//! FFmpeg's order. The expected frame counts and per-channel RMS were taken
//! from `ffmpeg -i <fixture> -c:a pcm_f32le` (libavcodec's native decoders);
//! the fixtures were encoded by FFmpeg 8.1 (libvorbis / libopus):
//!
//! - `vorbis_stereo_43972.ogg`: 440/660 Hz, 44.1 kHz, `-q:a 5
//!   -page_duration 100000` — 43,972 frames over many pages, not a multiple
//!   of the block size.
//! - `vorbis_5_1_58368.ogg`: 5.1 at 48 kHz, a different tone per channel.
//! - `vorbis_one_page.ogg`: 0.3 s mono — all audio on one (EOS) page, where
//!   FFmpeg ends (blocksize_0 + first blocksize) / 4 samples short of the
//!   granule.
//! - `vorbis_early_tail_2400.ogg`: 0.05 s mono at 48 kHz, q0, `-page_duration
//!   20000` — a page ends on a long block that a short one follows, so the
//!   decoder's output runs `(2048 - 256) / 4` samples ahead of the granule
//!   there; the end trim must still stop at FFmpeg's 2,400.
//! - `opus_stereo_96000.ogg`: 2 s, 48 kHz, 64 kb/s, pre-skip 312.
//! - `opus_stereo_59259.opus`: 1.23456 s — not a multiple of 20 ms.
//! - `opus_one_page.opus`: 0.1 s mono, all audio on the EOS page.
//!
//! The damaged-page cases (issue #59) flip one payload byte of a mid-file
//! page in memory; their expected values are FFmpeg 8.1's decode of the same
//! damaged bytes, which drops the page ("CRC mismatch!") and goes on.

use std::path::PathBuf;

use rff::codec::CodecParams;
use rff::core::{CodecId, Error, Frame, SampleFormat};
use rff::Engine;

fn testdata(name: &str) -> PathBuf {
    std::path::Path::new(env!("CARGO_MANIFEST_DIR"))
        .join("testdata")
        .join(name)
}

struct Decoded {
    codec: CodecId,
    channels: usize,
    sample_rate: u32,
    /// Interleaved f32.
    samples: Vec<f32>,
}

impl Decoded {
    fn frames(&self) -> usize {
        self.samples.len() / self.channels
    }

    fn channel(&self, c: usize) -> impl Iterator<Item = f64> + '_ {
        self.samples
            .iter()
            .skip(c)
            .step_by(self.channels)
            .map(|&s| s as f64)
    }

    fn rms(&self, c: usize) -> f64 {
        (self.channel(c).map(|s| s * s).sum::<f64>() / self.frames() as f64).sqrt()
    }
}

/// Demux + decode a fixture through the engine's registries.
fn decode(name: &str) -> Decoded {
    decode_bytes(std::fs::read(testdata(name)).expect("fixture"))
}

/// A fixture with one payload bit flipped in the middle of the body of the
/// page with sequence number `seq`, so that page fails its CRC.
fn damaged(name: &str, seq: u32) -> Vec<u8> {
    let mut file = std::fs::read(testdata(name)).expect("fixture");
    let mut at = 0;
    while file[at..].starts_with(b"OggS") {
        let nsegs = file[at + 26] as usize;
        let body_at = at + 27 + nsegs;
        let body: usize = file[at + 27..body_at].iter().map(|&l| l as usize).sum();
        if u32::from_le_bytes(file[at + 18..at + 22].try_into().unwrap()) == seq {
            file[body_at + body / 2] ^= 0x01;
            return file;
        }
        at = body_at + body;
    }
    panic!("{name}: no page {seq}");
}

fn decode_bytes(file: Vec<u8>) -> Decoded {
    let engine = Engine::new();
    let mut dem = engine
        .formats
        .open_demuxer("ogg", Box::new(std::io::Cursor::new(file)))
        .unwrap();
    let streams = dem.read_header().unwrap();
    let s = &streams[0];
    assert_eq!(
        s.sample_format,
        Some(SampleFormat::F32),
        "advertised format"
    );
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
                assert_eq!(af.format, SampleFormat::F32);
                assert_eq!(af.channels, s.channels);
                assert_eq!(af.planes[0].len(), af.samples * af.channels as usize * 4);
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
    Decoded {
        codec: s.codec_id,
        channels: s.channels as usize,
        sample_rate: s.sample_rate,
        samples,
    }
}

fn assert_rms(d: &Decoded, want: &[f64]) {
    assert_eq!(d.channels, want.len());
    for (c, &w) in want.iter().enumerate() {
        let got = d.rms(c);
        assert!(
            (got - w).abs() < 1e-6,
            "ch{c} rms {got:.9} vs FFmpeg {w:.9}"
        );
    }
}

/// Goertzel power of `x` at `freq`.
fn power_at(x: impl Iterator<Item = f64>, freq: f64, rate: f64) -> f64 {
    let k = 2.0 * (2.0 * std::f64::consts::PI * freq / rate).cos();
    let (mut s1, mut s2) = (0.0, 0.0);
    for v in x {
        let s0 = v + k * s1 - s2;
        s2 = s1;
        s1 = s0;
    }
    s1 * s1 + s2 * s2 - k * s1 * s2
}

#[test]
fn vorbis_multi_page_ends_at_the_final_granule() {
    let d = decode("vorbis_stereo_43972.ogg");
    assert_eq!(d.codec, CodecId::Vorbis);
    assert_eq!(d.frames(), 43_972, "FFmpeg decodes 43,972 frames");
    assert_rms(&d, &[0.088984951, 0.08902861]);
    // Float, not s16 in disguise.
    let off_grid = d
        .samples
        .iter()
        .filter(|s| (**s * 32768.0).fract() != 0.0)
        .count();
    assert!(
        off_grid > d.samples.len() / 2,
        "{off_grid} off the s16 grid"
    );
}

/// The same packets through lewton's own Ogg reader, which trims the stream
/// end itself: the engine's demuxer + adapter must give the same samples.
/// The adapter decodes with the in-house `rusty_vorbis` decoder, whose float
/// operation order differs from lewton's, so the samples agree to within
/// float rounding (max abs difference ~1e-7), not bit for bit; the sample
/// count — the demuxer's end trim — must match exactly.
#[test]
fn vorbis_matches_lewtons_own_ogg_reader() {
    let d = decode("vorbis_stereo_43972.ogg");
    let file = std::fs::File::open(testdata("vorbis_stereo_43972.ogg")).unwrap();
    let mut rdr = lewton::inside_ogg::OggStreamReader::new(file).unwrap();
    let mut want = Vec::new();
    while let Some(pcm) = rdr.read_dec_packet_generic::<Vec<Vec<f32>>>().unwrap() {
        for i in 0..pcm.first().map_or(0, Vec::len) {
            want.extend(pcm.iter().map(|ch| ch[i]));
        }
    }
    assert_eq!(d.samples.len(), want.len(), "sample count");
    let max_diff = d
        .samples
        .iter()
        .zip(&want)
        .map(|(a, b)| (a - b).abs())
        .fold(0.0f32, f32::max);
    assert!(max_diff <= 1e-6, "max abs diff vs lewton {max_diff:e}");
}

/// 5.1: FFmpeg order is FL FR FC LFE BL BR, Vorbis's is FL FC FR RL RR LFE.
/// Each channel of the fixture carries its own tone; FFmpeg's decode puts
/// 300, 400, 200, (LFE), 600 and 700 Hz on FL, FR, FC, LFE, BL, BR.
#[test]
fn vorbis_5_1_comes_out_in_ffmpeg_channel_order() {
    let d = decode("vorbis_5_1_58368.ogg");
    assert_eq!(d.frames(), 58_368);
    assert_rms(
        &d,
        &[
            0.088831679,
            0.088941945,
            0.089055486,
            0.004386011,
            0.08904442,
            0.088951527,
        ],
    );
    let tones = [300.0, 400.0, 200.0, 600.0, 700.0];
    for (c, want) in [(0, 300.0), (1, 400.0), (2, 200.0), (4, 600.0), (5, 700.0)] {
        let loudest = tones
            .iter()
            .copied()
            .max_by(|&a, &b| {
                let pa = power_at(d.channel(c), a, d.sample_rate as f64);
                let pb = power_at(d.channel(c), b, d.sample_rate as f64);
                pa.total_cmp(&pb)
            })
            .unwrap();
        assert_eq!(loudest, want, "channel {c}");
    }
    // The LFE is the quiet one, fourth as in FFmpeg (Vorbis puts it last).
    let quietest = (0..6).min_by(|&a, &b| d.rms(a).total_cmp(&d.rms(b)));
    assert_eq!(quietest, Some(3));
}

#[test]
fn vorbis_single_page_matches_ffmpeg() {
    let d = decode("vorbis_one_page.ogg");
    // Granule 13,230; FFmpeg outputs 13,102 (= 13,230 - (256 + 256) / 4).
    assert_eq!(d.frames(), 13_102);
    assert_rms(&d, &[0.08895936]);
}

/// Ogg Opus: libopus's 312-sample pre-skip comes off the front and the final
/// granule (96,312) trims the back, so 2 s decodes to exactly 96,000 frames
/// (it was 96,960). FFmpeg reports Ogg Opus at 48 kHz.
#[test]
fn vorbis_end_trim_after_an_early_long_block_tail() {
    let d = decode("vorbis_early_tail_2400.ogg");
    assert_eq!(d.frames(), 2_400, "FFmpeg decodes 2,400 frames");
    assert_rms(&d, &[0.177820907]);
}

#[test]
fn opus_pre_skip_and_end_trim() {
    let d = decode("opus_stereo_96000.ogg");
    assert_eq!(d.codec, CodecId::Opus);
    assert_eq!(d.sample_rate, 48_000);
    assert_eq!(d.frames(), 96_000);
    assert_rms(&d, &[0.088061477, 0.08893406]);
}

/// 1.23456 s: 59,571 granule - 312 pre-skip = 59,259 frames, as in FFmpeg.
#[test]
fn opus_odd_length_matches_ffmpeg() {
    let d = decode("opus_stereo_59259.opus");
    assert_eq!(d.frames(), 59_259);
    assert_rms(&d, &[0.088196083, 0.088469157]);
}

/// All audio on the EOS page: FFmpeg still ends at granule - pre-skip.
#[test]
fn opus_single_page_matches_ffmpeg() {
    let d = decode("opus_one_page.opus");
    assert_eq!(d.frames(), 4_800); // granule 5,112 - 312
    assert_rms(&d, &[0.088420073]);
}

/// A flipped byte in page 5 of 10: FFmpeg drops the page's five packets and
/// decodes the rest; so must we, with no burst of noise where it was.
#[test]
fn vorbis_damaged_page_is_dropped_like_ffmpeg() {
    let d = decode_bytes(damaged("vorbis_stereo_43972.ogg", 5));
    assert_eq!(d.frames(), 38_852, "FFmpeg decodes 38,852 frames");
    assert_rms(&d, &[0.089206119, 0.08941548]);
    // The splice where the page was (two blocks overlapped that were never
    // neighbours) peaks at 0.16708 in FFmpeg's decode too; unchecked, the
    // corrupt page decoded to a 0.216 burst.
    let peak = d.samples.iter().fold(0f32, |m, s| m.max(s.abs()));
    assert!((peak - 0.167_075_8).abs() < 1e-6, "peak {peak}");
}

/// Opus, the first audio page lost: the pre-skip comes off the next one and
/// the final granule still trims the end (96,000 - 48,000 frames).
#[test]
fn opus_damaged_first_audio_page() {
    let d = decode_bytes(damaged("opus_stereo_96000.ogg", 2));
    assert_eq!(d.frames(), 48_000);
    assert_rms(&d, &[0.086382628, 0.086827187]);
}

/// Opus, the page before the EOS page lost: FFmpeg's position no longer
/// reaches the final granule, so it keeps the last packet whole (48,648).
#[test]
fn opus_damaged_last_full_page() {
    let d = decode_bytes(damaged("opus_stereo_96000.ogg", 3));
    assert_eq!(d.frames(), 48_648);
    assert_rms(&d, &[0.087359089, 0.088309663]);
}
