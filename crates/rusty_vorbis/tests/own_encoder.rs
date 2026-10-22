//! Correctness gate on the crate's OWN encoder output: the long-block
//! streaming `VorbisEncoder` and the block-switching `frame::encode_stream_bs`,
//! decoded by the in-house decoder vs lewton (always) and vs `ffmpeg` (when it
//! is on PATH).

#[path = "common/harness.rs"]
mod harness;
#[path = "common/ogg.rs"]
mod ogg;

use harness::{compare, decode_ffmpeg, decode_lewton, decode_ours, FFMPEG_FROM_VORBIS};
use rusty_vorbis::decode::Decoder;
use rusty_vorbis::{frame, setup, Error, VorbisEncoder, VorbisEncoderConfig};

/// A signal with tones, noise-like content and sharp attacks (to trigger
/// short blocks in the block-switching encoder).
fn signal(ch: usize, n: usize, sr: f32) -> Vec<Vec<f32>> {
    let mut seed = 0x1234_5678u32;
    (0..ch)
        .map(|c| {
            (0..n)
                .map(|i| {
                    seed = seed.wrapping_mul(1_664_525).wrapping_add(1_013_904_223);
                    let noise = (seed >> 9) as f32 / (1u32 << 23) as f32 - 0.5;
                    let t = i as f32 / sr;
                    // Near-silence, then a sudden loud burst every 0.37 s.
                    let ph = t % 0.37;
                    let env = if ph < 0.15 {
                        0.0
                    } else {
                        (-30.0 * (ph - 0.15)).exp()
                    };
                    0.02 * (2.0 * std::f32::consts::PI * (220.0 + 110.0 * c as f32) * t).sin()
                        + 1.6 * env * noise
                })
                .collect()
        })
        .collect()
}

/// Granules = cumulative decoded frames (the first packet yields 0).
fn granules(headers: &[Vec<u8>], audio: &[Vec<u8>]) -> Vec<i64> {
    let dec = Decoder::new(&headers[0], &headers[1], &headers[2]).unwrap();
    let mut g = 0i64;
    audio
        .iter()
        .enumerate()
        .map(|(i, p)| {
            if i > 0 {
                g += dec.packet_frames(p).unwrap() as i64;
            }
            g
        })
        .collect()
}

fn check(name: &str, headers: &[Vec<u8>], audio: &[Vec<u8>]) {
    let file = ogg::write_ogg(headers, audio, &granules(headers, audio));
    let ours = decode_ours(&file).unwrap();
    let lew = decode_lewton(&file).unwrap();
    assert_eq!(ours.frames(), lew.frames(), "{name}: frames vs lewton");
    let id: Vec<usize> = (0..ours.channels).collect();
    let dl = compare(&ours, &lew, &id);
    assert!(
        dl.max_abs <= 2e-6 && dl.snr_db >= 100.0,
        "{name}: vs lewton {:.3e}",
        dl.max_abs
    );
    let path = std::env::temp_dir().join(format!("rusty_vorbis_{name}_{}.ogg", std::process::id()));
    std::fs::write(&path, &file).unwrap();
    if let Some(ff) = decode_ffmpeg(&path, ours.channels, ours.rate) {
        assert_eq!(ours.frames(), ff.frames(), "{name}: frames vs ffmpeg");
        let df = compare(&ours, &ff, FFMPEG_FROM_VORBIS[ours.channels - 1]);
        assert!(
            df.max_abs <= 2e-6 && df.snr_db >= 100.0,
            "{name}: vs ffmpeg {:.3e}",
            df.max_abs
        );
        println!(
            "{name}: {} frames, max|d| vs lewton {:.2e}, vs ffmpeg {:.2e}",
            ours.frames(),
            dl.max_abs,
            df.max_abs
        );
    } else {
        println!(
            "{name}: {} frames, max|d| vs lewton {:.2e} (ffmpeg not available)",
            ours.frames(),
            dl.max_abs
        );
    }
    let _ = std::fs::remove_file(&path);
}

#[test]
fn streaming_encoder_output() {
    let sr = 44_100u32;
    let sig = signal(2, 44_100, sr as f32);
    let mut pcm = Vec::with_capacity(sig[0].len() * 2);
    for (&l, &r) in sig[0].iter().zip(&sig[1]) {
        pcm.push(l);
        pcm.push(r);
    }
    let mut enc = VorbisEncoder::new(VorbisEncoderConfig::default());
    enc.push_pcm_f32(&pcm, 2, sr).unwrap();
    enc.finish();
    let mut pk = Vec::new();
    loop {
        match enc.next_packet() {
            Ok(p) => pk.push(p.data),
            Err(Error::Eof) => break,
            Err(e) => panic!("{e}"),
        }
    }
    check("streaming", &pk[..3], &pk[3..]);
}

#[test]
fn block_switching_encoder_output() {
    let sr = 44_100u32;
    let sig = signal(2, 3 * 44_100 / 2, sr as f32);
    let tables = setup::parse_setup(setup::SETUP_Q4_STEREO, 2).unwrap();
    let audio = frame::encode_stream_bs(&tables, &sig, sr, 0.6).unwrap();
    let enc = VorbisEncoder::default();
    let mut headers = enc.headers();
    // `headers()` before any push reports 1 channel: rebuild the ident for stereo.
    headers[0] = rusty_vorbis::write_ident_header(
        2,
        sr,
        rusty_vorbis::BS0_LOG2,
        rusty_vorbis::BS1_LOG2,
        rusty_vorbis::BITRATE_NOMINAL,
    );
    // The stream must actually switch blocks for this gate to mean anything.
    let dec = Decoder::new(&headers[0], &headers[1], &headers[2]).unwrap();
    let shorts = audio
        .iter()
        .filter(|p| dec.packet_frames(p).unwrap() == 128)
        .count();
    assert!(shorts > 0, "no short blocks in the block-switching stream");
    check("blockswitch", &headers, &audio);
}
