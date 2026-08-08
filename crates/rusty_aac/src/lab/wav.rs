//! Minimal RIFF/WAVE I/O — **piece 1** of the ffmpeg comparison harness.
//!
//! The corpus lives as in-memory `f32`. To hand a clip to another encoder, or to
//! score a decoded result with an external oracle, it has to become a file and
//! come back. Nothing else in the crate needed that, so nothing else had it.
//!
//! Deliberately small: PCM `s16` and IEEE `f32` only, which is what
//! `ffmpeg` emits and what `scipy.io.wavfile` (and therefore PEAQ) reads.
//! **s16 is the default for anything PEAQ will see** — the reference PEAQ driver
//! scales float input by 32768 when it detects `|x| <= 1.5`, so a float WAV whose
//! peak happens to exceed that is silently treated as already-scaled. Writing s16
//! removes the guess.

use std::io::{self, Read, Write};
use std::path::Path;

/// A decoded WAV file.
#[derive(Debug, Clone)]
pub struct Wav {
    pub sample_rate: u32,
    pub channels: u16,
    /// Interleaved samples, normalised to `[-1, 1]`.
    pub samples: Vec<f32>,
}

impl Wav {
    /// Samples per channel.
    pub fn frames(&self) -> usize {
        self.samples.len() / self.channels.max(1) as usize
    }

    /// Channel 0 as mono.
    pub fn mono(&self) -> Vec<f32> {
        if self.channels <= 1 {
            self.samples.clone()
        } else {
            self.samples
                .iter()
                .step_by(self.channels as usize)
                .copied()
                .collect()
        }
    }
}

fn u32le(b: &[u8]) -> u32 {
    u32::from_le_bytes([b[0], b[1], b[2], b[3]])
}
fn u16le(b: &[u8]) -> u16 {
    u16::from_le_bytes([b[0], b[1]])
}

/// Read a RIFF/WAVE file (PCM s16 or IEEE f32).
pub fn read(path: impl AsRef<Path>) -> io::Result<Wav> {
    let mut d = Vec::new();
    std::fs::File::open(path.as_ref())?.read_to_end(&mut d)?;
    if d.len() < 44 || &d[0..4] != b"RIFF" || &d[8..12] != b"WAVE" {
        return Err(io::Error::new(io::ErrorKind::InvalidData, "not a RIFF/WAVE file"));
    }
    let (mut tag, mut channels, mut sample_rate, mut bits) = (1u16, 1u16, 44100u32, 16u16);
    let mut data: &[u8] = &[];
    let mut pos = 12usize;
    while pos + 8 <= d.len() {
        let id = &d[pos..pos + 4];
        let sz = u32le(&d[pos + 4..pos + 8]) as usize;
        let end = (pos + 8 + sz).min(d.len());
        let body = &d[pos + 8..end];
        if id == b"fmt " && body.len() >= 16 {
            tag = u16le(&body[0..2]);
            channels = u16le(&body[2..4]).max(1);
            sample_rate = u32le(&body[4..8]);
            bits = u16le(&body[14..16]);
            // WAVE_FORMAT_EXTENSIBLE carries the real tag in the GUID's first two bytes.
            if tag == 0xFFFE && body.len() >= 26 {
                tag = u16le(&body[24..26]);
            }
        } else if id == b"data" {
            data = body;
        }
        pos += 8 + sz + (sz & 1); // chunks are word-aligned
    }
    if data.is_empty() {
        return Err(io::Error::new(io::ErrorKind::InvalidData, "no data chunk"));
    }

    let samples: Vec<f32> = match (tag, bits) {
        (1, 16) => data
            .chunks_exact(2)
            .map(|c| i16::from_le_bytes([c[0], c[1]]) as f32 / 32768.0)
            .collect(),
        (1, 32) => data
            .chunks_exact(4)
            .map(|c| i32::from_le_bytes([c[0], c[1], c[2], c[3]]) as f32 / 2147483648.0)
            .collect(),
        (3, 32) => data
            .chunks_exact(4)
            .map(|c| f32::from_le_bytes([c[0], c[1], c[2], c[3]]))
            .collect(),
        _ => {
            return Err(io::Error::new(
                io::ErrorKind::InvalidData,
                format!("unsupported WAV format tag {tag} / {bits} bits"),
            ))
        }
    };
    Ok(Wav {
        sample_rate,
        channels,
        samples,
    })
}

/// Write interleaved `[-1, 1]` samples as PCM s16.
///
/// Clamped, not wrapped: a sample above full scale becomes full scale rather
/// than wrapping to the opposite polarity, which would inject a click the
/// external scorer would then charge to the encoder.
pub fn write_s16(
    path: impl AsRef<Path>,
    samples: &[f32],
    channels: u16,
    sample_rate: u32,
) -> io::Result<()> {
    let n_bytes = samples.len() * 2;
    let mut out = Vec::with_capacity(44 + n_bytes);
    out.extend_from_slice(b"RIFF");
    out.extend_from_slice(&((36 + n_bytes) as u32).to_le_bytes());
    out.extend_from_slice(b"WAVEfmt ");
    out.extend_from_slice(&16u32.to_le_bytes()); // fmt chunk size
    out.extend_from_slice(&1u16.to_le_bytes()); // PCM
    out.extend_from_slice(&channels.to_le_bytes());
    out.extend_from_slice(&sample_rate.to_le_bytes());
    let byte_rate = sample_rate * channels as u32 * 2;
    out.extend_from_slice(&byte_rate.to_le_bytes());
    out.extend_from_slice(&(channels * 2).to_le_bytes()); // block align
    out.extend_from_slice(&16u16.to_le_bytes()); // bits
    out.extend_from_slice(b"data");
    out.extend_from_slice(&(n_bytes as u32).to_le_bytes());
    for &s in samples {
        let v = (s.clamp(-1.0, 1.0) * 32767.0).round() as i16;
        out.extend_from_slice(&v.to_le_bytes());
    }
    std::fs::File::create(path.as_ref())?.write_all(&out)
}

#[cfg(test)]
mod tests {
    use super::*;

    fn tmp(name: &str) -> std::path::PathBuf {
        std::env::temp_dir().join(name)
    }

    /// Write then read must preserve the signal within s16 quantization.
    #[test]
    fn s16_round_trips() {
        let sr = 44100u32;
        let n = 4096;
        let sig: Vec<f32> = (0..n)
            .map(|i| 0.6 * (2.0 * std::f32::consts::PI * 440.0 * i as f32 / sr as f32).sin())
            .collect();
        let p = tmp("rusty_aac_wav_rt.wav");
        write_s16(&p, &sig, 1, sr).unwrap();
        let back = read(&p).unwrap();
        assert_eq!(back.sample_rate, sr);
        assert_eq!(back.channels, 1);
        assert_eq!(back.frames(), n);
        let worst = sig
            .iter()
            .zip(&back.samples)
            .map(|(a, b)| (a - b).abs())
            .fold(0f32, f32::max);
        assert!(worst < 1.0 / 16384.0, "s16 round-trip error {worst}");
        let _ = std::fs::remove_file(&p);
    }

    /// Stereo interleave order must survive.
    #[test]
    fn stereo_channels_stay_put() {
        let sr = 48000u32;
        let mut inter = Vec::new();
        for i in 0..1000 {
            inter.push(0.5); // L constant
            inter.push(if i % 2 == 0 { -0.25 } else { 0.25 }); // R alternating
        }
        let p = tmp("rusty_aac_wav_st.wav");
        write_s16(&p, &inter, 2, sr).unwrap();
        let back = read(&p).unwrap();
        assert_eq!(back.channels, 2);
        assert_eq!(back.frames(), 1000);
        for i in 0..1000 {
            assert!((back.samples[i * 2] - 0.5).abs() < 1e-3, "L moved at {i}");
        }
        let _ = std::fs::remove_file(&p);
    }

    /// Out-of-range input must clamp, never wrap. A wrap would put a full-scale
    /// opposite-polarity click into a file an external scorer then blames on the
    /// encoder.
    #[test]
    fn out_of_range_clamps_not_wraps() {
        let p = tmp("rusty_aac_wav_clip.wav");
        write_s16(&p, &[2.0, -2.0, 0.0], 1, 44100).unwrap();
        let back = read(&p).unwrap();
        assert!(back.samples[0] > 0.99, "positive overflow must clamp high");
        assert!(back.samples[1] < -0.99, "negative overflow must clamp low");
        let _ = std::fs::remove_file(&p);
    }

    /// The real corpus must be readable — it is what the headline ranking uses.
    #[test]
    fn reads_the_real_corpus_if_present() {
        let p = std::path::Path::new("../../corpus/corp_mus_piano.wav");
        if !p.exists() {
            return; // corpus not fetched in this checkout
        }
        let w = read(p).expect("real corpus clip must parse");
        assert!(w.frames() > 44100, "expected a multi-second clip");
        assert!(w.samples.iter().all(|v| v.is_finite()));
    }
}
