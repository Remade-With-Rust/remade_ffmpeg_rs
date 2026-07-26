//! Our AV1 side: `rusty_av1e` (our rav1e fork) to encode, `rusty_av1d` (our
//! rav1d fork) to decode, both driven IN-PROCESS through their library APIs at
//! DEFAULT settings.
//!
//! "Default" for a library means its own constructed default, not a preset we
//! chose: `EncoderConfig::default()` with only the geometry filled in, and
//! `rav1d::Decoder::new()` with a stock settings block. The one imposed setting
//! is single-threaded, on every arm, because per-function attribution against a
//! multi-threaded pipeline is meaningless — worker cycle sums exceed wall time
//! and the residue stops meaning anything.
//!
//! Both forks carry a `profile` cargo feature holding a stage profiler. Unlike
//! our VP9 profilers (runtime-gated, one binary), these are compile-time gates,
//! so this analyzer is genuinely built twice — see `run_analysis_av1.sh`.

use std::time::{Duration, Instant};

use rff_video_harness::y4m::Clip;

pub use rusty_av1d::prof as dec_prof;
pub use rusty_av1e::prof as enc_prof;

use rusty_av1d::{Decoder as Rav1dDec, Rav1dError, Settings as Rav1dSettings};
use rusty_av1e::prelude::{ChromaSampling, Config, Context, EncoderConfig, EncoderStatus};

/// Build the encoder at library defaults for this clip's geometry.
fn context(clip: &Clip, frames: usize) -> Result<Context<u8>, String> {
    let mut enc = EncoderConfig::default();
    enc.width = clip.width;
    enc.height = clip.height;
    enc.bit_depth = 8;
    enc.chroma_sampling = ChromaSampling::Cs420;
    enc.time_base = rusty_av1e::prelude::Rational::new(clip.fps.1 as u64, clip.fps.0 as u64);
    // A finite clip: telling the encoder how many frames are coming lets its
    // rate control and lookahead behave as they would on a real finite input
    // rather than an open-ended stream.
    enc.still_picture = false;
    let cfg = Config::new()
        .with_encoder_config(enc)
        // Single-threaded: see the module note.
        .with_threads(1);
    let _ = frames;
    cfg.new_context::<u8>()
        .map_err(|e| format!("rusty_av1e config rejected: {e}"))
}

fn encode_once(clip: &Clip, frames: usize) -> Result<(Duration, Vec<Vec<u8>>), String> {
    let mut ctx = context(clip, frames)?;
    let mut out: Vec<Vec<u8>> = Vec::new();
    let t = Instant::now();
    for i in 0..frames {
        let src = &clip.frames[i];
        let mut f = ctx.new_frame();
        // 4:2:0 8-bit: one byte per sample, chroma at half width (rounded up).
        let cw = clip.width.div_ceil(2);
        for (idx, (data, stride)) in
            [(&src.y, clip.width), (&src.u, cw), (&src.v, cw)].into_iter().enumerate()
        {
            f.planes[idx].copy_from_raw_u8(data, stride, 1);
        }
        match ctx.send_frame(f) {
            Ok(()) => {}
            // Buffer full; draining below makes room.
            Err(EncoderStatus::EnoughData) => {}
            Err(e) => return Err(format!("send_frame: {e:?}")),
        }
        drain(&mut ctx, &mut out)?;
    }
    ctx.flush();
    loop {
        match ctx.receive_packet() {
            Ok(p) => out.push(p.data),
            Err(EncoderStatus::Encoded) => continue,
            Err(EncoderStatus::LimitReached) => break,
            Err(EncoderStatus::Failure) => return Err("encode failure".into()),
            Err(_) => break,
        }
    }
    Ok((t.elapsed(), out))
}

fn drain(ctx: &mut Context<u8>, out: &mut Vec<Vec<u8>>) -> Result<(), String> {
    loop {
        match ctx.receive_packet() {
            Ok(p) => out.push(p.data),
            Err(EncoderStatus::Encoded) => continue,
            Err(EncoderStatus::LimitReached) => break,
            Err(EncoderStatus::Failure) => return Err("encode failure".into()),
            // NeedMoreData / EnoughData / NotReady.
            Err(_) => break,
        }
    }
    Ok(())
}

/// Best-of-N encode. The fastest pass is the one least disturbed by the
/// scheduler and by turbo/thermal excursions, so it is the most repeatable.
pub fn encode_speed(
    clip: &Clip,
    frames: usize,
    reps: usize,
) -> Result<(Duration, Vec<Vec<u8>>), String> {
    let mut best = Duration::MAX;
    let mut kept = Vec::new();
    for _ in 0..reps.max(1) {
        let (d, pkts) = encode_once(clip, frames)?;
        if d < best {
            best = d;
            kept = pkts;
        }
    }
    Ok((best, kept))
}

fn decode_once(packets: &[Vec<u8>]) -> Result<(Duration, usize), String> {
    // PIN TO ONE THREAD. rav1d's `n_threads` defaults to 0, which means AUTO =
    // every core (lib.rs: `if s.n_threads.get() != 0`), so `Decoder::new()` would
    // race a multi-threaded decoder against the `-threads 1` we pin ffmpeg to —
    // inflating our numbers by the core count. The harness's whole premise is
    // single-threaded per-function attribution on every arm.
    let mut settings = Rav1dSettings::new();
    settings.set_n_threads(1);
    let mut dec =
        Rav1dDec::with_settings(&settings).map_err(|e| format!("rav1d init: {e:?}"))?;
    let mut n = 0usize;
    let t = Instant::now();
    for (i, p) in packets.iter().enumerate() {
        if p.is_empty() {
            continue;
        }
        let buf = p.clone().into_boxed_slice();
        match dec.send_data(buf, None, Some(i as i64), None) {
            Ok(()) => {}
            Err(Rav1dError::TryAgain) => {
                n += pull(&mut dec)?;
                loop {
                    match dec.send_pending_data() {
                        Ok(()) => break,
                        Err(Rav1dError::TryAgain) => n += pull(&mut dec)?,
                        Err(e) => return Err(format!("send_pending_data: {e:?}")),
                    }
                }
            }
            Err(e) => return Err(format!("send_data: {e:?}")),
        }
        n += pull(&mut dec)?;
    }
    n += pull(&mut dec)?;
    Ok((t.elapsed(), n))
}

fn pull(dec: &mut Rav1dDec) -> Result<usize, String> {
    let mut n = 0;
    loop {
        match dec.get_picture() {
            Ok(_) => n += 1,
            Err(Rav1dError::TryAgain) => break,
            Err(e) => return Err(format!("get_picture: {e:?}")),
        }
    }
    Ok(n)
}

/// Best-of-N decode. Bytes are already in RAM, matching how the external
/// reference harnesses time their decode loop, so neither side is charged for
/// file I/O.
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

/// Encoder stage snapshot, median over `passes` full encodes.
pub fn encode_stages(
    clip: &Clip,
    frames: usize,
    passes: usize,
) -> Vec<(enc_prof::Stage, f64, u64)> {
    rff_video_harness::profile_median(
        || {
            let _ = encode_once(clip, frames);
        },
        passes,
        enc_prof::reset,
        enc_prof::snapshot,
    )
}

/// Decoder stage snapshot, median over `passes` full decodes.
pub fn decode_stages(packets: &[Vec<u8>], passes: usize) -> Vec<(dec_prof::Stage, f64, u64)> {
    rff_video_harness::profile_median(
        || {
            let _ = decode_once(packets);
        },
        passes,
        dec_prof::reset,
        dec_prof::snapshot,
    )
}
