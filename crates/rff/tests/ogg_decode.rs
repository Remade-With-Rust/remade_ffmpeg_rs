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
    let engine = Engine::new();
    let file = std::fs::read(testdata(name)).expect("fixture");
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
/// end itself: the engine's demuxer + adapter must agree sample for sample.
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
    assert_eq!(d.samples, want);
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
