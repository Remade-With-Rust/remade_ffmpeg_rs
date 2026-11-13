//! Vorbis / Opus decode from Matroska / WebM against FFmpeg 8.1 (issue #58):
//! the last block's `DiscardPadding` trims the stream end, as FFmpeg's
//! demuxer + `decode.c` do. Expected frame counts and RMS were taken from
//! `ffmpeg -i <fixture> -f f32le -` (libavcodec's native decoders); the
//! fixtures were made by FFmpeg 8.1:
//!
//! - `vorbis_mono_27221.webm`: 1.2345 s of 440 Hz at 22.05 kHz, libvorbis
//!   `-q:a 0`; the last block carries 427 samples of `DiscardPadding`.
//! - `opus_mono_20741.mka`: 0.4321 s of 440 Hz, libopus 64 kb/s; pre-skip 312
//!   (from the `OpusHead` CodecPrivate), `DiscardPadding` 67 samples.

use std::path::PathBuf;

use rff::codec::CodecParams;
use rff::core::{CodecId, Error, Frame, SampleFormat};
use rff::Engine;

fn testdata(name: &str) -> PathBuf {
    std::path::Path::new(env!("CARGO_MANIFEST_DIR"))
        .join("testdata")
        .join(name)
}

/// Demux + decode a mono fixture through the engine's registries; returns
/// the codec, the packets' total discard padding and the samples.
fn decode(name: &str) -> (CodecId, u64, Vec<f32>) {
    let engine = Engine::new();
    let file = std::fs::read(testdata(name)).expect("fixture");
    let mut dem = engine
        .formats
        .open_demuxer("matroska", Box::new(std::io::Cursor::new(file)))
        .unwrap();
    let streams = dem.read_header().unwrap();
    let s = &streams[0];
    assert_eq!(s.channels, 1);
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
    let mut padding = 0u64;
    let mut drain = |dec: &mut Box<dyn rff::codec::Decoder>| loop {
        match dec.receive_frame() {
            Ok(Frame::Audio(af)) => {
                assert_eq!(af.format, SampleFormat::F32);
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
                padding += p.discard_padding as u64;
                dec.send_packet(&p).unwrap();
                drain(&mut dec);
            }
            Err(Error::Eof) => break,
            Err(e) => panic!("demux: {e}"),
        }
    }
    dec.flush();
    drain(&mut dec);
    (s.codec_id, padding, samples)
}

fn rms(x: &[f32]) -> f64 {
    (x.iter().map(|&s| s as f64 * s as f64).sum::<f64>() / x.len() as f64).sqrt()
}

#[test]
fn vorbis_webm_ends_where_ffmpeg_does() {
    let (codec, padding, d) = decode("vorbis_mono_27221.webm");
    assert_eq!(codec, CodecId::Vorbis);
    assert_eq!(padding, 427, "DiscardPadding, in samples");
    assert_eq!(
        d.len(),
        27_221,
        "FFmpeg decodes 27,221 frames (27,648 untrimmed)"
    );
    assert!((rms(&d) - 0.088314788).abs() < 1e-6, "rms {}", rms(&d));
}

#[test]
fn opus_mka_pre_skip_and_discard_padding() {
    let (codec, padding, d) = decode("opus_mono_20741.mka");
    assert_eq!(codec, CodecId::Opus);
    assert_eq!(padding, 67);
    assert_eq!(
        d.len(),
        20_741,
        "FFmpeg decodes 20,741 frames (20,808 untrimmed)"
    );
    assert!((rms(&d) - 0.088309249).abs() < 1e-6, "rms {}", rms(&d));
}
