//! The rff-vp9 side: encode and decode the corpus IN-PROCESS, through the same
//! public codec registry the `ffmpeg` CLI uses, with no options set — so this
//! arm really is "our defaults", not a hand-tuned configuration.
//!
//! Both profilers are runtime-gated ([`enc_prof::set_enabled`] /
//! [`dec_prof::set_enabled`]) rather than compile-time, so `speed` and `stages`
//! come from ONE binary: the speed pass leaves the taps disabled, where each
//! costs a single predictable relaxed load, and the stages pass turns them on.

use rff_codec::CodecRegistry;
use rff_codec_vp9::prof as dec_prof;
use rff_core::{CodecId, Dictionary, Error, Frame, Packet, PixelFormat, VideoFrame};
use std::time::{Duration, Instant};

use crate::y4m::Clip;

/// Our encoder's stage profiler lives inside the codec crate's private `encode`
/// module; the crate re-exports the reader API for exactly this harness.
pub use rff_codec_vp9::encode_prof as enc_prof;

fn registry() -> CodecRegistry {
    let mut r = CodecRegistry::new();
    rff_codec_vp9::register(&mut r);
    r
}

fn video_frame(clip: &Clip, i: usize) -> Frame {
    let f = &clip.frames[i];
    Frame::Video(VideoFrame {
        width: clip.width as u32,
        height: clip.height as u32,
        format: PixelFormat::Yuv420p,
        planes: vec![f.y.clone(), f.u.clone(), f.v.clone()],
        strides: vec![clip.width, clip.width.div_ceil(2), clip.width.div_ceil(2)],
        pts: Some(i as i64),
    })
}

/// One encode pass. Returns (elapsed, concatenated packet payloads, packet count).
fn encode_once(clip: &Clip, frames: usize) -> Result<(Duration, Vec<Vec<u8>>), String> {
    encode_once_cfg(clip, frames, &Dictionary::new())
}

/// One encode pass at an EXPLICIT configuration — the matched-operating-point
/// path. `encode_once` is this with an empty dictionary, so the default arm and
/// the matched arm run byte-identical code and differ only in the options.
fn encode_once_cfg(
    clip: &Clip,
    frames: usize,
    opts: &Dictionary,
) -> Result<(Duration, Vec<Vec<u8>>), String> {
    let reg = registry();
    let mut enc = reg
        .find_encoder(CodecId::Vp9)
        .map_err(|e| format!("find_encoder: {e}"))?;
    if !opts.is_empty() {
        enc.configure(opts).map_err(|e| format!("configure: {e}"))?;
    }
    let mut out: Vec<Vec<u8>> = Vec::new();
    let t = Instant::now();
    for i in 0..frames {
        let f = video_frame(clip, i);
        enc.send_frame(&f).map_err(|e| format!("send_frame: {e}"))?;
        loop {
            match enc.receive_packet() {
                Ok(p) => out.push(p.data),
                Err(Error::Again) | Err(Error::Eof) => break,
                Err(e) => return Err(format!("receive_packet: {e}")),
            }
        }
    }
    enc.flush();
    loop {
        match enc.receive_packet() {
            Ok(p) => out.push(p.data),
            Err(Error::Again) | Err(Error::Eof) => break,
            Err(e) => return Err(format!("flush/receive_packet: {e}")),
        }
    }
    Ok((t.elapsed(), out))
}

/// Best-of-N encode at our DEFAULT settings. Returns (best wall, packets).
///
/// Best-of-N, not mean: the fastest pass is the one least disturbed by the OS
/// scheduler and by turbo/thermal excursions, so it is the most repeatable
/// statistic — which is the whole point of a fixed corpus.
pub fn encode_speed(clip: &Clip, frames: usize, reps: usize) -> Result<(Duration, Vec<Vec<u8>>), String> {
    encode_speed_cfg(clip, frames, reps, &Dictionary::new())
}

/// Best-of-N encode at an explicit configuration (the `pareto` pass).
pub fn encode_speed_cfg(
    clip: &Clip,
    frames: usize,
    reps: usize,
    opts: &Dictionary,
) -> Result<(Duration, Vec<Vec<u8>>), String> {
    let mut best = Duration::MAX;
    let mut kept = Vec::new();
    for _ in 0..reps.max(1) {
        let (d, pkts) = encode_once_cfg(clip, frames, opts)?;
        if d < best {
            best = d;
            kept = pkts;
        }
    }
    Ok((best, kept))
}

/// One decode pass over already-in-memory packets.
fn decode_once(packets: &[Vec<u8>]) -> Result<(Duration, usize), String> {
    let reg = registry();
    let mut dec = reg
        .find_decoder(CodecId::Vp9)
        .map_err(|e| format!("find_decoder: {e}"))?;
    let mut n = 0usize;
    let t = Instant::now();
    for (i, p) in packets.iter().enumerate() {
        let pkt = Packet {
            data: p.clone(),
            pts: Some(i as i64),
            ..Default::default()
        };
        dec.send_packet(&pkt).map_err(|e| format!("send_packet: {e}"))?;
        loop {
            match dec.receive_frame() {
                Ok(_) => n += 1,
                Err(Error::Again) | Err(Error::Eof) => break,
                Err(e) => return Err(format!("receive_frame: {e}")),
            }
        }
    }
    Ok((t.elapsed(), n))
}

/// Best-of-N decode with OUR decoder. Bytes are already in RAM, matching how the
/// libvpx reference harness times its decode loop (it slurps the IVF first), so
/// neither side is charged for file I/O.
pub fn decode_speed(packets: &[Vec<u8>], reps: usize) -> Result<(Duration, usize), String> {
    let mut best = Duration::MAX;
    let mut n = 0;
    for _ in 0..reps.max(1) {
        let (d, k) = decode_once(packets)?;
        n = k;
        best = best.min(d);
    }
    Ok((best, n))
}

/// Median-of-N stage snapshot around `run`, for either profiler.
///
/// Median rather than best: a stage table is a vector of correlated numbers, and
/// taking the minimum of each independently would produce a table that sums to
/// less than any single pass actually took.
pub fn profile_median<const K: usize>(
    mut run: impl FnMut(),
    passes: usize,
    reset: fn(),
    snapshot: fn() -> [(f64, u64); K],
) -> [(f64, u64); K] {
    let passes = passes.max(1);
    let mut per: Vec<[(f64, u64); K]> = Vec::with_capacity(passes);
    for _ in 0..passes {
        reset();
        run();
        per.push(snapshot());
    }
    let mid = passes / 2;
    let mut out = [(0.0f64, 0u64); K];
    for (i, o) in out.iter_mut().enumerate() {
        let mut ms: Vec<f64> = per.iter().map(|s| s[i].0).collect();
        ms.sort_by(|a, b| a.partial_cmp(b).unwrap_or(std::cmp::Ordering::Equal));
        *o = (ms[mid], per[mid][i].1);
    }
    out
}

/// Encoder stage snapshot for one clip, median over `passes` full encodes.
pub fn encode_stages(clip: &Clip, frames: usize, passes: usize) -> [(f64, u64); enc_prof::N] {
    encode_stages_cfg(clip, frames, passes, &Dictionary::new())
}

/// Encoder stage snapshot at an EXPLICIT configuration. Attribution is only
/// comparable against the reference when both sides encode the same operating
/// point — otherwise the arm emitting more coefficients inflates its own
/// coefficient stages for reasons that have nothing to do with efficiency.
pub fn encode_stages_cfg(
    clip: &Clip,
    frames: usize,
    passes: usize,
    opts: &Dictionary,
) -> [(f64, u64); enc_prof::N] {
    enc_prof::set_enabled(true);
    let s = profile_median(
        || {
            let _ = encode_once_cfg(clip, frames, opts);
        },
        passes,
        enc_prof::reset,
        enc_prof::snapshot,
    );
    enc_prof::set_enabled(false);
    s
}

/// Decoder stage snapshot for one clip, median over `passes` full decodes.
pub fn decode_stages(packets: &[Vec<u8>], passes: usize) -> [(f64, u64); dec_prof::N] {
    dec_prof::set_enabled(true);
    let s = profile_median(
        || {
            let _ = decode_once(packets);
        },
        passes,
        dec_prof::reset,
        dec_prof::snapshot,
    );
    dec_prof::set_enabled(false);
    s
}

/// Wrap our packets in an IVF container so the external decoders (ffmpeg's
/// ffvp9, the libvpx twin) can be pointed at exactly the stream we produced.
pub fn to_ivf(clip: &Clip, packets: &[Vec<u8>]) -> Vec<u8> {
    let mut out = Vec::with_capacity(packets.iter().map(|p| p.len() + 12).sum::<usize>() + 32);
    out.extend_from_slice(b"DKIF");
    out.extend_from_slice(&0u16.to_le_bytes()); // version
    out.extend_from_slice(&32u16.to_le_bytes()); // header length
    out.extend_from_slice(b"VP90");
    out.extend_from_slice(&(clip.width as u16).to_le_bytes());
    out.extend_from_slice(&(clip.height as u16).to_le_bytes());
    out.extend_from_slice(&clip.fps.0.to_le_bytes()); // time base denominator
    out.extend_from_slice(&clip.fps.1.to_le_bytes()); // time base numerator
    out.extend_from_slice(&(packets.len() as u32).to_le_bytes());
    out.extend_from_slice(&0u32.to_le_bytes());
    for (i, p) in packets.iter().enumerate() {
        out.extend_from_slice(&(p.len() as u32).to_le_bytes());
        out.extend_from_slice(&(i as u64).to_le_bytes());
        out.extend_from_slice(p);
    }
    out
}
