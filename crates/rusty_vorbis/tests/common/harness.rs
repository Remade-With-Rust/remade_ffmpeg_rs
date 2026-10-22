//! Shared decode-and-compare harness for the correctness gate (tests only).
//! Requires `mod ogg;` (tests/common/ogg.rs) at the crate root.

#![allow(dead_code)]

use crate::ogg::{read_packets, OggPacket};
use rusty_vorbis::decode::Decoder;

/// Planar decoded audio.
pub struct Pcm {
    pub channels: usize,
    pub rate: u32,
    /// `chans[c][frame]`.
    pub chans: Vec<Vec<f32>>,
}

impl Pcm {
    pub fn frames(&self) -> usize {
        self.chans.first().map_or(0, Vec::len)
    }
    fn truncate(&mut self, n: usize) {
        for c in &mut self.chans {
            c.truncate(n);
        }
    }
}

/// The final granule of the stream (end-trim target), if any page carried one.
fn last_granule(pk: &[OggPacket]) -> Option<i64> {
    pk.iter().rev().find(|p| p.granule >= 0).map(|p| p.granule)
}

/// True when every audio packet completes on the same (final) page — FFmpeg's
/// Ogg demuxer mis-trims such streams (it credits the first, silent packet with
/// a duration), so its frame count there is a container artefact.
pub fn single_audio_page(file: &[u8]) -> bool {
    let pk = read_packets(file);
    pk.len() > 3 && pk[3..pk.len() - 1].iter().all(|p| p.granule < 0)
}

/// Decode with the in-house decoder: every packet through
/// [`Decoder::decode_interleaved`], then the container end-trim by granule.
pub fn decode_ours(file: &[u8]) -> Result<Pcm, String> {
    let pk = read_packets(file);
    if pk.len() < 3 {
        return Err("fewer than 3 packets".into());
    }
    let mut dec = Decoder::new(&pk[0].data, &pk[1].data, &pk[2].data).map_err(|e| e.to_string())?;
    let ch = dec.channels();
    let mut buf = vec![0.0f32; dec.max_frames_per_packet() * ch];
    let mut chans = vec![Vec::new(); ch];
    for p in &pk[3..] {
        let n = dec
            .decode_interleaved(&p.data, &mut buf)
            .map_err(|e| e.to_string())?;
        for f in 0..n {
            for c in 0..ch {
                chans[c].push(buf[f * ch + c]);
            }
        }
    }
    let mut pcm = Pcm {
        channels: ch,
        rate: dec.sample_rate(),
        chans,
    };
    if let Some(g) = last_granule(&pk) {
        if (g as usize) < pcm.frames() {
            pcm.truncate(g as usize);
        }
    }
    Ok(pcm)
}

/// Decode with lewton 0.10.2 (packet by packet, same end-trim).
pub fn decode_lewton(file: &[u8]) -> Result<Pcm, String> {
    use lewton::audio::{read_audio_packet_generic, PreviousWindowRight};
    use lewton::header::{read_header_ident, read_header_setup};
    let pk = read_packets(file);
    let ident = read_header_ident(&pk[0].data).map_err(|e| format!("{e:?}"))?;
    let setup = read_header_setup(
        &pk[2].data,
        ident.audio_channels,
        (ident.blocksize_0, ident.blocksize_1),
    )
    .map_err(|e| format!("{e:?}"))?;
    let mut pwr = PreviousWindowRight::new();
    let ch = ident.audio_channels as usize;
    let mut chans = vec![Vec::new(); ch];
    for p in &pk[3..] {
        let s: Vec<Vec<f32>> = read_audio_packet_generic(&ident, &setup, &p.data, &mut pwr)
            .map_err(|e| format!("{e:?}"))?;
        for (c, v) in s.into_iter().enumerate() {
            chans[c].extend(v);
        }
    }
    let mut pcm = Pcm {
        channels: ch,
        rate: ident.audio_sample_rate,
        chans,
    };
    if let Some(g) = last_granule(&pk) {
        if (g as usize) < pcm.frames() {
            pcm.truncate(g as usize);
        }
    }
    Ok(pcm)
}

/// Decode with the `ffmpeg` on PATH (native vorbis decoder) to f32. FFmpeg
/// emits its own channel order; channels are returned as FFmpeg orders them.
pub fn decode_ffmpeg(path: &std::path::Path, channels: usize, rate: u32) -> Option<Pcm> {
    let out = std::process::Command::new("ffmpeg")
        .args(["-v", "error", "-i"])
        .arg(path)
        .args(["-f", "f32le", "-c:a", "pcm_f32le", "-"])
        .output()
        .ok()?;
    if !out.status.success() {
        return None;
    }
    let s: Vec<f32> = out
        .stdout
        .chunks_exact(4)
        .map(|b| f32::from_le_bytes([b[0], b[1], b[2], b[3]]))
        .collect();
    let frames = s.len() / channels;
    let chans = (0..channels)
        .map(|c| (0..frames).map(|f| s[f * channels + c]).collect())
        .collect();
    Some(Pcm {
        channels,
        rate,
        chans,
    })
}

/// FFmpeg's Vorbis channel placement: FFmpeg channel `i` carries Vorbis
/// channel `FFMPEG_FROM_VORBIS[ch-1][i]` (libavcodec `ff_vorbis_channel_layout_offsets`).
pub const FFMPEG_FROM_VORBIS: [&[usize]; 8] = [
    &[0],
    &[0, 1],
    &[0, 2, 1],
    &[0, 1, 2, 3],
    &[0, 2, 1, 3, 4],
    &[0, 2, 1, 5, 3, 4],
    &[0, 2, 1, 6, 5, 3, 4],
    &[0, 2, 1, 7, 5, 6, 3, 4],
];

/// Max abs difference and SNR (dB, reference power over error power).
pub struct Diff {
    pub max_abs: f64,
    pub snr_db: f64,
}

/// Compare `a` (Vorbis order) against `b`; `map[i]` = the channel of `a` that
/// `b`'s channel `i` holds. Compares the common frame prefix.
pub fn compare(a: &Pcm, b: &Pcm, map: &[usize]) -> Diff {
    let n = a.frames().min(b.frames());
    let (mut max_abs, mut sig, mut err) = (0.0f64, 0.0f64, 0.0f64);
    for (bi, &ai) in map.iter().enumerate() {
        for f in 0..n {
            let x = f64::from(b.chans[bi][f]);
            let d = f64::from(a.chans[ai][f]) - x;
            max_abs = max_abs.max(d.abs());
            sig += x * x;
            err += d * d;
        }
    }
    let snr_db = if err == 0.0 {
        f64::INFINITY
    } else {
        10.0 * (sig / err).log10()
    };
    Diff { max_abs, snr_db }
}
