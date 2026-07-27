//! VP9 encoder — the `Encoder` trait bridge (Floor 3, brick C3).
//!
//! Wraps [`FrameEncoder`] in the `rff_codec::Encoder` send/receive interface and
//! converts an incoming YUV 4:2:0 [`VideoFrame`] (display size) into the coded
//! grid (rounded up to 8, edge-replicated) the frame encoder expects.

use std::collections::VecDeque;

use rff_codec::Encoder;
use rff_core::{Dictionary, Error, Frame, Packet, PixelFormat, Result, VideoFrame};

use super::frameenc::FrameEncoder;

/// Frame-level rate controller (R2): a leaky-bucket feedback that nudges the
/// per-frame `qindex` toward a target bits-per-frame. Higher `qindex` ⇒ coarser
/// quantization ⇒ fewer bits, so an over-budget frame raises `q` for the next.
struct RateCtl {
    target_per_frame: f64, // bits
    q: f64,                // current qindex (kept fractional for smooth control)
}

impl RateCtl {
    /// Pick the qindex for the next frame.
    fn qindex(&self) -> u32 {
        self.q.round().clamp(4.0, 220.0) as u32
    }
    /// Feed back the bits a frame actually spent.
    fn update(&mut self, actual_bits: f64) {
        // Integral control: accumulate the relative over/undershoot into q.
        let err = (actual_bits - self.target_per_frame) / self.target_per_frame;
        self.q = (self.q + 10.0 * err.clamp(-1.0, 4.0)).clamp(4.0, 220.0);
    }
}

/// Concatenate coded frames into a VP9 superframe: the frames back-to-back followed
/// by a superframe index (marker, each frame's byte length, marker). A hidden ALT-REF
/// must ride in a superframe with the next shown frame, else a lenient decoder emits it
/// as its own displayed frame.
fn pack_superframe(frames: &[Vec<u8>]) -> Vec<u8> {
    let max = frames.iter().map(|f| f.len()).max().unwrap_or(0);
    let mag: usize = if max < (1 << 8) {
        1
    } else if max < (1 << 16) {
        2
    } else if max < (1 << 24) {
        3
    } else {
        4
    };
    let marker = 0xc0u8 | (((mag - 1) as u8) << 3) | ((frames.len() - 1) as u8);
    let mut out = Vec::new();
    for f in frames {
        out.extend_from_slice(f);
    }
    out.push(marker);
    for f in frames {
        let sz = f.len();
        for b in 0..mag {
            out.push(((sz >> (8 * b)) & 0xff) as u8);
        }
    }
    out.push(marker);
    out
}

/// SAD of the `bs×bs` luma block at `(bx,by)` in `anchor` against `neigh` shifted by
/// integer `(mvr,mvc)`, clamping reference reads to the plane border.
#[allow(clippy::too_many_arguments)]
fn tf_sad(
    anchor: &[u16],
    neigh: &[u16],
    w: usize,
    h: usize,
    bx: usize,
    by: usize,
    bs: usize,
    mvr: i32,
    mvc: i32,
) -> u64 {
    let mut sad = 0u64;
    for dy in 0..bs {
        let ay = by + dy;
        if ay >= h {
            break;
        }
        for dx in 0..bs {
            let ax = bx + dx;
            if ax >= w {
                break;
            }
            let sx = (ax as i32 + mvc).clamp(0, w as i32 - 1) as usize;
            let sy = (ay as i32 + mvr).clamp(0, h as i32 - 1) as usize;
            let d = anchor[ay * w + ax] as i64 - neigh[sy * w + sx] as i64;
            sad += d.unsigned_abs();
        }
    }
    sad
}

/// Best integer-pel MV `(row,col)` aligning `neigh`'s block to `anchor`'s (full search
/// ±`range`, ties toward the shorter vector).
#[allow(clippy::too_many_arguments)]
fn tf_search(
    anchor: &[u16],
    neigh: &[u16],
    w: usize,
    h: usize,
    bx: usize,
    by: usize,
    bs: usize,
    range: i32,
) -> (i32, i32) {
    let mut best = (0i32, 0i32);
    let mut best_sad = tf_sad(anchor, neigh, w, h, bx, by, bs, 0, 0);
    for mvr in -range..=range {
        for mvc in -range..=range {
            let sad = tf_sad(anchor, neigh, w, h, bx, by, bs, mvr, mvc);
            let shorter = mvr.abs() + mvc.abs() < best.0.abs() + best.1.abs();
            if sad < best_sad || (sad == best_sad && shorter) {
                best_sad = sad;
                best = (mvr, mvc);
            }
        }
    }
    best
}

/// Mean absolute consecutive-frame luma difference over a group (subsampled) —
/// a cheap motion proxy. High motion ⇒ the hidden ALT-REF pays off (temporal
/// bracketing helps prediction); low motion ⇒ the ARF is pure overhead. Measured
/// crossover ≈ 8 (akiyo 0.4 / foreman 5.2 code plain; bus 21 / crowd_run 12 use ARF).
fn group_motion(frames: &[([Vec<u16>; 3], u32, u32)]) -> f64 {
    if frames.len() < 2 {
        return 0.0;
    }
    let (mut tot, mut cnt) = (0u64, 0u64);
    for w in frames.windows(2) {
        let (a, b) = (&w[0].0[0], &w[1].0[0]);
        let n = a.len().min(b.len());
        let mut i = 0;
        while i < n {
            tot += (a[i] as i64 - b[i] as i64).unsigned_abs();
            cnt += 1;
            i += 37; // stride-subsample for speed
        }
    }
    tot as f64 / cnt.max(1) as f64
}

/// Simplified libvpx temporal filter (`vp9_temporal_filter`): denoise the ALT-REF
/// `anchor` by blending each `neighbor` — motion-compensated to the anchor — with a
/// per-pixel weight that decays with the aligned difference, so static regions are
/// averaged (noise cancels) while moving/occluded regions keep the anchor. Luma drives
/// the 16×16 motion; chroma reuses the halved MV over the co-located 8×8 block. The
/// result is a cleaner long-term reference: on noisy content it both codes cheaper and
/// predicts the group's P frames better. Purely a source transform — no bitstream
/// effect, so the encode/decode path is unchanged.
fn temporal_filter(
    anchor: &[Vec<u16>; 3],
    neighbors: &[&[Vec<u16>; 3]],
    cw: usize,
    ch: usize,
    strength: f64,
) -> [Vec<u16>; 3] {
    const BS: usize = 16;
    const RANGE: i32 = 8;
    const MAX_W: f64 = 16.0;
    let (cwc, chc) = (cw / 2, ch / 2);
    // Post-alignment a matched pixel differs only by noise, so the kernel must stay
    // wide enough that such neighbors get real weight (else the anchor dominates and
    // nothing is averaged). `strength` scales the Gaussian σ; occlusions still fall off.
    let sigma = (strength * 6.0).max(1.0);
    let two_sig2 = 2.0 * sigma * sigma;
    let weight = |mc: u16, a: u16| -> f64 {
        let d = mc as f64 - a as f64;
        MAX_W * (-(d * d) / two_sig2).exp()
    };
    let mut out = [anchor[0].clone(), anchor[1].clone(), anchor[2].clone()];
    let mut by = 0;
    while by < ch {
        let mut bx = 0;
        while bx < cw {
            // Luma accumulators for this 16×16 block (anchor weighted MAX_W).
            let mut acc = [0f64; BS * BS];
            let mut wsum = [0f64; BS * BS];
            // Chroma accumulators for the co-located 8×8 block, per plane.
            let mut cacc = [[0f64; 64]; 2];
            let mut cwsum = [[0f64; 64]; 2];
            for dy in 0..BS {
                for dx in 0..BS {
                    let (ax, ay) = (bx + dx, by + dy);
                    if ax < cw && ay < ch {
                        acc[dy * BS + dx] = anchor[0][ay * cw + ax] as f64 * MAX_W;
                        wsum[dy * BS + dx] = MAX_W;
                    }
                }
            }
            let (cbx, cby) = (bx / 2, by / 2);
            for dy in 0..8 {
                for dx in 0..8 {
                    let (ax, ay) = (cbx + dx, cby + dy);
                    if ax < cwc && ay < chc {
                        for p in 0..2 {
                            cacc[p][dy * 8 + dx] = anchor[p + 1][ay * cwc + ax] as f64 * MAX_W;
                            cwsum[p][dy * 8 + dx] = MAX_W;
                        }
                    }
                }
            }
            for nb in neighbors {
                let (mvr, mvc) = tf_search(&anchor[0], &nb[0], cw, ch, bx, by, BS, RANGE);
                for dy in 0..BS {
                    for dx in 0..BS {
                        let (ax, ay) = (bx + dx, by + dy);
                        if ax >= cw || ay >= ch {
                            continue;
                        }
                        let sx = (ax as i32 + mvc).clamp(0, cw as i32 - 1) as usize;
                        let sy = (ay as i32 + mvr).clamp(0, ch as i32 - 1) as usize;
                        let mc = nb[0][sy * cw + sx];
                        let wgt = weight(mc, anchor[0][ay * cw + ax]);
                        acc[dy * BS + dx] += mc as f64 * wgt;
                        wsum[dy * BS + dx] += wgt;
                    }
                }
                let (cmr, cmc) = (mvr / 2, mvc / 2);
                for dy in 0..8 {
                    for dx in 0..8 {
                        let (ax, ay) = (cbx + dx, cby + dy);
                        if ax >= cwc || ay >= chc {
                            continue;
                        }
                        let sx = (ax as i32 + cmc).clamp(0, cwc as i32 - 1) as usize;
                        let sy = (ay as i32 + cmr).clamp(0, chc as i32 - 1) as usize;
                        for p in 0..2 {
                            let mc = nb[p + 1][sy * cwc + sx];
                            let wgt = weight(mc, anchor[p + 1][ay * cwc + ax]);
                            cacc[p][dy * 8 + dx] += mc as f64 * wgt;
                            cwsum[p][dy * 8 + dx] += wgt;
                        }
                    }
                }
            }
            for dy in 0..BS {
                for dx in 0..BS {
                    let (ax, ay) = (bx + dx, by + dy);
                    if ax < cw && ay < ch {
                        out[0][ay * cw + ax] =
                            (acc[dy * BS + dx] / wsum[dy * BS + dx]).round() as u16;
                    }
                }
            }
            for dy in 0..8 {
                for dx in 0..8 {
                    let (ax, ay) = (cbx + dx, cby + dy);
                    if ax < cwc && ay < chc {
                        for p in 0..2 {
                            out[p + 1][ay * cwc + ax] =
                                (cacc[p][dy * 8 + dx] / cwsum[p][dy * 8 + dx]).round() as u16;
                        }
                    }
                }
            }
            bx += BS;
        }
        by += BS;
    }
    out
}

/// Parse an ffmpeg-style bitrate (`"2M"`, `"128k"`, `"500000"`) into bits/sec.
fn parse_bitrate_bps(s: &str) -> Option<f64> {
    let s = s.trim();
    if let Some(n) = s.strip_suffix(['k', 'K']) {
        n.trim().parse::<f64>().ok().map(|x| x * 1_000.0)
    } else if let Some(n) = s.strip_suffix(['m', 'M']) {
        n.trim().parse::<f64>().ok().map(|x| x * 1_000_000.0)
    } else {
        s.parse::<f64>().ok()
    }
}

/// In-house VP9 encoder: the first frame is a key frame, subsequent frames are
/// P frames (ZEROMV, single-reference LAST) against the previous reconstruction.
pub struct Vp9Encoder {
    qindex: u32,
    packets: VecDeque<Packet>,
    eof: bool,
    /// Previous frame's reconstruction (coded size) + its dimensions, used as the
    /// LAST reference; `None` ⇒ the next frame is coded as a key frame.
    reference: Option<([Vec<u16>; 3], u32, u32)>,
    /// The most recent key frame's reconstruction (+ dims), installed as the GOLDEN
    /// reference on every P frame (a stable long-term anchor the per-block RD may pick).
    golden: Option<([Vec<u16>; 3], u32, u32)>,
    /// Active when `-b:v` sets a target bitrate; overrides the fixed `qindex`.
    rc: Option<RateCtl>,
    /// ALT-REF lookahead group size (`-lag N`, 0 ⇒ off). When >1, frames are buffered
    /// and each group is coded key/P… + a hidden future ALT-REF shown last.
    lag: usize,
    /// Two-pass rate control (`-pass 2` / `twopass=1` with `-b:v`): buffer the clip,
    /// probe its size, then encode at a global constant qindex that hits the target.
    twopass: bool,
    /// ALT-REF temporal-filter strength (`arnr-strength`, 0 ⇒ off). Denoises the hidden
    /// ALT-REF source by blending motion-compensated neighbor frames.
    tf_strength: f64,
    /// Buffered input frames (coded-size YUV) awaiting an ALT-REF group or two-pass flush.
    lookahead: VecDeque<([Vec<u16>; 3], u32, u32)>,
    /// Physical VP9 reference slots (0..2 used) for the cross-GOP ALT-REF chain, and the
    /// ping-pong assignment of GOLDEN (previous ARF) / ALTREF (current ARF) slots. Only
    /// the first group is key-started; later groups chain through these slots.
    slots: [Option<[Vec<u16>; 3]>; 3],
    golden_slot: usize,
    arf_slot: usize,
    /// Frame dims of the current chain; a change forces a fresh key-started group.
    group_dims: Option<(u32, u32)>,
    /// F3 context chaining (`VP9_CHAIN`): a companion copy of our own conformant
    /// decoder tracks the adapted FrameContext + prev-frame MVs; each new P frame
    /// chains onto them (error_resilient=0). Bit-exact adaptation by construction.
    chain: bool,
    companion: Option<Box<crate::Vp9Decoder>>,
    /// Previous chained frame was a shown, same-size P (the decoder's
    /// `use_prev_mvs` precondition).
    chain_prev_p: bool,
    /// Reconstruction of the most recent slotted-coded frame (for the recon oracle).
    last_recon: Option<[Vec<u16>; 3]>,
    /// Frame counter for the `VP9_RECON_CHECK` oracle.
    recon_check_n: usize,
    /// Persistent oracle decoder (holds the reference chain).
    recon_check_dec: Option<Box<crate::Vp9Decoder>>,
    /// Active while coding an ALT-REF group with chaining on.
    group_chain: bool,
    /// Active during two-pass PASS 2: chaining engages (pass 1 is a throwaway
    /// probe that must not touch the companion).
    pass2_chaining: bool,
    /// Content-adaptive-lag motion threshold: ALT-REF groups below this mean
    /// inter-frame luma diff are coded as plain chained P frames instead (the ARF
    /// is overhead on low-motion content). `VP9_LAG_MOTION_THRESH` overrides.
    lag_motion_thresh: f64,
    /// ARF q scale (`VP9_ARF_QSCALE`): the hidden ALT-REF is a long-term reference,
    /// so libvpx codes it at LOWER q (higher quality) — the extra bits pay off across
    /// every P frame that predicts from it. 1.0 = code at the frame q (the old,
    /// net-loss behavior); <1 = the arf boost.
    arf_qscale: f64,
    /// Encoder speed preset (`-cpu-used`/`-speed`, 0 = best quality/slowest .. 4 =
    /// fastest). Higher levels progressively drop RD tools (sub-8×8, forward-prob
    /// two-pass, trellis, tx-search) for a graceful quality→speed trade. See
    /// [`FrameEncoder::set_speed`](super::frameenc::FrameEncoder::set_speed).
    speed: u32,
    /// Lever 2 — time-budget controller target: the per-frame decision-pass wall
    /// time (µs) the content-adaptive dispatch should hold. `Some` engages the
    /// controller (`VP9_DISPATCH_BUDGET=<ms>`); then `dispatch_q_state` is
    /// steered per frame to hit this, making per-frame
    /// encode time content-INVARIANT (bus/mobile route more to the variance
    /// partition, akiyo less) rather than merely flatter. `None` = fixed-q dispatch.
    dispatch_budget_us: Option<u64>,
    /// Lever 2 — the controller's live route fraction, persisted across frames
    /// (a `FrameEncoder` is per-frame, so the state lives here). Fed into each
    /// frame via [`FrameEncoder::set_dispatch_q`] and nudged after by the measured
    /// decision time. Seeded at 0.5; the integral update drives steady-state error
    /// to zero when the budget is reachable within `[0, 0.95]`.
    dispatch_q_state: f64,
}

impl Default for Vp9Encoder {
    fn default() -> Vp9Encoder {
        Vp9Encoder {
            qindex: 64, // a middle-quality default
            packets: VecDeque::new(),
            eof: false,
            reference: None,
            golden: None,
            rc: None,
            lag: 0,
            twopass: false,
            tf_strength: 3.0, // default ARNR strength; only used when lag>1
            lookahead: VecDeque::new(),
            slots: [None, None, None],
            golden_slot: 1,
            arf_slot: 2,
            group_dims: None,
            // Default speed preset. 3 = the balanced default (2026-07-18): on top
            // of speed 2 it adds the libvpx-cpu3-derived speed features (split
            // early-termination, adaptive mode-skip, 64×64 partition gate) for
            // +1.02% BD-rate at ~1.3× faster — a clean Pareto step. Speed 2 (the
            // prior default, byte-identical to the pinned corpus) and speed 0 (the
            // exhaustive-RD anchor) remain selectable. `-speed`/`-cpu-used N` overrides.
            speed: 3,
            chain: std::env::var("VP9_NO_CHAIN").is_err(), // F3: corpus-gated −11.15% BD
            companion: None,
            chain_prev_p: false,
            last_recon: None,
            recon_check_n: 0,
            recon_check_dec: None,
            group_chain: false,
            pass2_chaining: false,
            lag_motion_thresh: std::env::var("VP9_LAG_MOTION_THRESH")
                .ok()
                .and_then(|v| v.parse().ok())
                .unwrap_or(8.0),
            // 0.5 = the ARF q-boost (measured -8.87% BD vs plain IPPP on 1080p
            // motion). 1.0 restores the old un-boosted, net-loss ARF.
            arf_qscale: std::env::var("VP9_ARF_QSCALE")
                .ok()
                .and_then(|v| v.parse().ok())
                .unwrap_or(0.5),
            dispatch_budget_us: std::env::var("VP9_DISPATCH_BUDGET")
                .ok()
                .and_then(|v| v.parse::<f64>().ok())
                .filter(|ms| *ms > 0.0)
                .map(|ms| (ms * 1000.0) as u64),
            dispatch_q_state: 0.5,
        }
    }
}

/// Copy a `u8` plane (display size) into a coded-size `u16` buffer, replicating
/// the last in-frame row/column into the padding (libvpx `extend_frame`).
fn to_coded(plane: &[u8], stride: usize, dw: usize, dh: usize, cw: usize, ch: usize) -> Vec<u16> {
    let mut out = vec![0u16; cw * ch];
    for y in 0..ch {
        let sy = y.min(dh - 1);
        for x in 0..cw {
            let sx = x.min(dw - 1);
            out[y * cw + x] = plane[sy * stride + sx] as u16;
        }
    }
    out
}

impl Encoder for Vp9Encoder {
    fn configure(&mut self, options: &Dictionary) -> Result<()> {
        // `-qp N` sets the VP9 qindex directly (0..255); `-crf N` maps a 0..63
        // quality onto it. qindex 0 would mean lossless — clamp away from it.
        if let Some(qp) = options.get("qp").and_then(|v| v.parse::<u32>().ok()) {
            self.qindex = qp.min(255);
        } else if let Some(crf) = options.get("crf").and_then(|v| v.parse::<u32>().ok()) {
            self.qindex = (crf * 4).clamp(1, 255);
        } else if let Some(q) = options.get("q").and_then(|v| v.parse::<u32>().ok()) {
            self.qindex = q.min(255);
        }
        // `-b:v RATE` engages rate control toward `RATE` bits/sec. The per-frame
        // budget needs a frame rate; honour `-r`/`framerate`, else assume 30 fps.
        if let Some(bps) = options.get("b").and_then(parse_bitrate_bps) {
            let fps = options
                .get("framerate")
                .or_else(|| options.get("r"))
                .and_then(|v| v.parse::<f64>().ok())
                .filter(|&f| f > 0.0)
                .unwrap_or(30.0);
            self.rc = Some(RateCtl {
                target_per_frame: bps / fps,
                q: self.qindex as f64,
            });
        }
        // `-lag N` (aka lag-in-frames) turns on ALT-REF lookahead with a group size of
        // `N` (each group is coded key/P… + one hidden future ALT-REF shown last).
        if let Some(lag) = options
            .get("lag")
            .or_else(|| options.get("lag-in-frames"))
            .and_then(|v| v.parse::<usize>().ok())
        {
            self.lag = lag.min(32);
        }
        // ALT-REF temporal-filter strength (`arnr-strength`, 0 disables).
        if let Some(s) = options
            .get("arnr-strength")
            .or_else(|| options.get("tf"))
            .and_then(|v| v.parse::<f64>().ok())
        {
            self.tf_strength = s.max(0.0);
        }
        // `-dispatch-budget MS` engages the content-adaptive time-budget controller:
        // per frame, the variance-partition route fraction is steered to hold the
        // decision pass near MS milliseconds — a per-frame latency/throughput target
        // that caps encode time on complex content while easy content stays full-RD.
        // 0/absent = off. Overrides the `VP9_DISPATCH_BUDGET` env default.
        if let Some(ms) = options
            .get("dispatch-budget")
            .and_then(|v| v.parse::<f64>().ok())
        {
            self.dispatch_budget_us = if ms > 0.0 {
                Some((ms * 1000.0) as u64)
            } else {
                None
            };
        }
        // Two-pass: `-pass 2` (ffmpeg-style; `-pass 1` is a discardable analysis pass we
        // fold into pass 2 internally) or an explicit `twopass=1`. Needs `-b:v`.
        if options.get("pass").map(|v| v.trim()) == Some("2")
            || options.get("twopass").map(|v| v.trim()) == Some("1")
        {
            self.twopass = true;
        }
        // Speed preset: `-cpu-used N` / `-speed N` (0 best..4 fastest), à la libvpx.
        // Higher = progressively drop RD tools for a graceful quality→speed trade.
        if let Some(sp) = options
            .get("cpu-used")
            .or_else(|| options.get("speed"))
            .or_else(|| options.get("quality"))
            .and_then(|v| v.parse::<u32>().ok())
        {
            // 0–3 = the RD-quality ladder; 4–6 = the realtime rungs (content-adaptive
            // variance-partition dispatcher, rising route fraction). See `set_speed`.
            self.speed = sp.min(6);
        }
        Ok(())
    }

    fn send_frame(&mut self, frame: &Frame) -> Result<()> {
        let vf: &VideoFrame = match frame {
            Frame::Video(v) => v,
            Frame::Audio(_) => {
                return Err(Error::unsupported(
                    "vp9 encode: audio frame on a video codec",
                ))
            }
        };
        if vf.format != PixelFormat::Yuv420p {
            return Err(Error::unsupported(format!(
                "vp9 encode: needs yuv420p, got `{}` (convert with -vf format=yuv420p)",
                vf.format.name()
            )));
        }
        let (w, h) = (vf.width as usize, vf.height as usize);
        let mi_cols = (w + 7) >> 3;
        let mi_rows = (h + 7) >> 3;
        let (cw, ch) = (mi_cols * 8, mi_rows * 8);
        let (cwc, chc) = (cw / 2, ch / 2);
        let (dwc, dhc) = (w.div_ceil(2), h.div_ceil(2));

        let y = to_coded(&vf.planes[0], vf.strides[0], w, h, cw, ch);
        let u = to_coded(&vf.planes[1], vf.strides[1], dwc, dhc, cwc, chc);
        let v = to_coded(&vf.planes[2], vf.strides[2], dwc, dhc, cwc, chc);

        if self.twopass {
            // Two-pass needs the whole clip before it can solve for the qindex — buffer.
            self.lookahead.push_back(([y, u, v], vf.width, vf.height));
            return Ok(());
        }
        if self.lag > 1 {
            // ALT-REF lookahead: buffer, and emit a group once it is `lag` frames long.
            self.lookahead.push_back(([y, u, v], vf.width, vf.height));
            if self.lookahead.len() >= self.lag {
                let group: Vec<_> = self.lookahead.drain(..).collect();
                self.code_altref_group(group);
            }
            return Ok(());
        }

        // Default path: code immediately as KEY (first frame / resize) or P (+ GOLDEN).
        let bytes = self.code_frame([y, u, v], vf.width, vf.height, None);
        self.packets.push_back(Packet::from_data(0, bytes));
        Ok(())
    }

    fn receive_packet(&mut self) -> Result<Packet> {
        if let Some(p) = self.packets.pop_front() {
            Ok(p)
        } else if self.eof {
            Err(Error::Eof)
        } else {
            Err(Error::Again)
        }
    }

    fn flush(&mut self) {
        if !self.lookahead.is_empty() {
            let group: Vec<_> = self.lookahead.drain(..).collect();
            if self.twopass {
                self.two_pass_encode(group);
            } else {
                self.code_altref_group(group);
            }
        }
        self.eof = true;
    }
}

impl Vp9Encoder {
    /// Pick the qindex for the next frame (rate control, else the fixed value).
    fn next_qindex(&self) -> u32 {
        match &self.rc {
            Some(rc) => rc.qindex(),
            None => self.qindex,
        }
    }

    /// Lever 2 (time-budget controller) — feed the current adapted route fraction
    /// into a frame encoder before it encodes. No-op unless the budget is engaged.
    fn budget_apply(&self, enc: &mut FrameEncoder) {
        if self.dispatch_budget_us.is_some() {
            enc.set_dispatch_q(self.dispatch_q_state);
        }
    }

    /// Lever 2 — after a frame encodes, steer the route fraction toward the target
    /// decision-pass time from the measured cost. The key frame's intra cost is
    /// anomalous, so it feeds q in but never drives the controller.
    fn budget_update(&mut self, enc: &FrameEncoder, is_key: bool) {
        if let Some(budget) = self.dispatch_budget_us {
            if !is_key {
                // Integral controller on q: it accumulates the normalized error, so
                // steady-state error → 0 when the target is reachable within [0,0.95].
                let err = (enc.decision_us() as f64 - budget as f64) / budget as f64;
                const K: f64 = 0.4; // gain: settles in a few frames without ringing
                let prev = self.dispatch_q_state;
                self.dispatch_q_state = (prev + K * err).clamp(0.0, 0.95);
                if std::env::var("VP9_BUDGET_DEBUG").is_ok() {
                    eprintln!(
                        "BUDGET decision={:.1}ms target={:.1}ms q {:.3}->{:.3}",
                        enc.decision_us() as f64 / 1000.0,
                        budget as f64 / 1000.0,
                        prev,
                        self.dispatch_q_state
                    );
                }
            }
        }
    }

    /// Code one shown KEY or P frame at the single-pass qindex, feeding the rate
    /// controller the bits it spent.
    fn code_frame(
        &mut self,
        coded: [Vec<u16>; 3],
        w: u32,
        h: u32,
        altref: Option<&[Vec<u16>; 3]>,
    ) -> Vec<u8> {
        let q = self.next_qindex();
        let bytes = self.code_frame_q(coded, w, h, altref, q);
        if let Some(rc) = &mut self.rc {
            rc.update(bytes.len() as f64 * 8.0);
        }
        bytes
    }

    /// Code one shown KEY or P frame at an *explicit* qindex, chaining the reference:
    /// a P installs the GOLDEN anchor (and the group's ALT-REF, if any), refreshes
    /// LAST, and updates the chain. Returns the coded bytes (no rate-control feedback —
    /// the two-pass driver measures bits itself).
    fn code_frame_q(
        &mut self,
        coded: [Vec<u16>; 3],
        w: u32,
        h: u32,
        altref: Option<&[Vec<u16>; 3]>,
        qindex: u32,
    ) -> Vec<u8> {
        let reference = match self.reference.take() {
            Some((planes, rw, rh)) if rw == w && rh == h => Some(planes),
            _ => None,
        };
        let is_key = reference.is_none();
        let mut enc = FrameEncoder::new(w, h, qindex, coded, reference);
        enc.set_speed(self.speed);
        self.budget_apply(&mut enc);
        let chaining = self.chain && ((self.lag == 0 && !self.twopass) || self.pass2_chaining);
        if chaining && !is_key {
            if let Some(c) = &self.companion {
                let mvs = if self.chain_prev_p {
                    c.prev_mvs.clone()
                } else {
                    None
                };
                enc.set_chain(c.frame_contexts[0].clone(), mvs);
            }
        }
        if !is_key {
            if let Some((g, gw, gh)) = &self.golden {
                if *gw == w && *gh == h {
                    enc.set_golden(g.clone());
                }
            }
            if let Some(a) = altref {
                enc.set_altref(a.clone());
            }
        }
        let bytes = enc.encode_frame();
        self.budget_update(&enc, is_key);
        if chaining {
            use rff_codec::Decoder as _;
            let comp = self
                .companion
                .get_or_insert_with(|| Box::new(crate::Vp9Decoder::default()));
            let ok = comp
                .send_packet(&Packet::from_data(0, bytes.clone()))
                .and_then(|_| comp.receive_frame())
                .is_ok();
            if ok {
                self.chain_prev_p = !is_key;
            } else {
                // Companion desync: fail safe back to independent frames.
                self.chain = false;
                self.companion = None;
                self.chain_prev_p = false;
            }
        }
        let recon = enc.recon_owned();
        // Encoder/decoder reconstruction oracle (`VP9_RECON_CHECK=1`). The encoder feeds
        // its OWN `recon` forward as the next frame's reference, so if that ever differs
        // from what a decoder produces from the emitted bytes, prediction diverges and
        // the error compounds frame over frame — the worst failure mode this encoder
        // has, and invisible to both the decoder conformance vectors (they test the
        // decoder against libvpx streams) and a byte-diff (the bitstream is perfectly
        // self-consistent; it is the encoder's belief about it that is wrong).
        //
        // Our decoder is 315/315 bit-exact against libvpx, so it is a trustworthy oracle.
        if std::env::var("VP9_RECON_CHECK").is_ok() {
            self.recon_check(&bytes, &recon, w, h);
        }
        if is_key {
            self.golden = Some((recon.clone(), w, h));
        }
        self.reference = Some((recon, w, h));
        bytes
    }

    /// Feed one coded frame to the oracle decoder and, if it emits a picture, diff that
    /// picture against the reconstruction the encoder believes it produced.
    ///
    /// `expect = None` means "this frame is HIDDEN (an ALT-REF) — advance the decoder's
    /// state but do not compare", because a hidden frame emits no picture. The ARF is
    /// verified later, when `show_existing_frame` displays it and `expect` is its recon.
    /// Without this the whole ALT-REF path was invisible to the oracle: `recon_check` ran
    /// only from `code_frame_q`, and the lag>1 path goes through `code_altref_group`.
    fn recon_verify(&mut self, bytes: &[u8], expect: Option<&[Vec<u16>; 3]>, w: u32, h: u32) {
        if std::env::var("VP9_RECON_CHECK").is_err() {
            return;
        }
        match expect {
            Some(r) => self.recon_check(bytes, r, w, h),
            None => {
                use rff_codec::Decoder as _;
                let dec = self
                    .recon_check_dec
                    .get_or_insert_with(|| Box::new(crate::Vp9Decoder::default()));
                let _ = dec.send_packet(&Packet::from_data(0, bytes.to_vec()));
                while dec.receive_frame().is_ok() {}
                eprintln!("RECON_CHECK frame {}: hidden (fed, not compared)", self.recon_check_n);
                self.recon_check_n += 1;
            }
        }
    }

    /// Decode `bytes` with a scratch decoder and compare against the encoder's own
    /// reconstruction. Reports the first differing sample per frame; counts frames.
    fn recon_check(&mut self, bytes: &[u8], recon: &[Vec<u16>; 3], w: u32, h: u32) {
        use rff_codec::Decoder as _;
        // PERSISTENT across frames. A fresh decoder per frame holds no reference
        // buffers, so every inter frame decodes to garbage and the check reports a
        // 100% mismatch that says nothing about the encoder.
        let dec = self
            .recon_check_dec
            .get_or_insert_with(|| Box::new(crate::Vp9Decoder::default()));
        let got = dec
            .send_packet(&Packet::from_data(0, bytes.to_vec()))
            .ok()
            .and_then(|_| dec.receive_frame().ok());
        let Some(rff_core::Frame::Video(vf)) = got else {
            eprintln!("RECON_CHECK frame {}: DECODE FAILED", self.recon_check_n);
            self.recon_check_n += 1;
            return;
        };
        let mut bad = 0usize;
        let mut first = None;
        for p in 0..3 {
            let (pw, ph) = if p == 0 {
                (w as usize, h as usize)
            } else {
                ((w as usize).div_ceil(2), (h as usize).div_ceil(2))
            };
            // The encoder's recon is stored at the CODED size (mi-aligned); the decoded
            // frame is the display crop, so compare only the visible region row by row.
            let cw = recon[p].len() / ph.max(1);
            for y in 0..ph {
                for x in 0..pw {
                    let e = recon[p].get(y * cw + x).copied().unwrap_or(0) as u8;
                    let d = vf.planes[p].get(y * vf.strides[p] + x).copied().unwrap_or(0);
                    if e != d {
                        bad += 1;
                        if first.is_none() {
                            first = Some((p, x, y, e, d));
                        }
                    }
                }
            }
        }
        if bad > 0 {
            let (p, x, y, e, d) = first.unwrap();
            eprintln!(
                "RECON_CHECK frame {}: {} samples DIVERGE (first plane{} ({},{}) enc={} dec={})",
                self.recon_check_n, bad, p, x, y, e, d
            );
            // Level 2: a 64x64-superblock map of where luma diverges, so the pattern
            // (tile column, SB column, scattered, edge) is visible instead of guessed.
            if std::env::var("VP9_RECON_CHECK").ok().as_deref() == Some("2") {
                let (pw, ph) = (w as usize, h as usize);
                let cw = recon[0].len() / ph.max(1);
                let (sbx, sby) = (pw.div_ceil(64), ph.div_ceil(64));
                let mut map = vec![0u32; sbx * sby];
                for yy in 0..ph {
                    for xx in 0..pw {
                        let e = recon[0].get(yy * cw + xx).copied().unwrap_or(0) as u8;
                        let dv = vf.planes[0].get(yy * vf.strides[0] + xx).copied().unwrap_or(0);
                        if e != dv {
                            map[(yy / 64) * sbx + xx / 64] += 1;
                        }
                    }
                }
                eprintln!("  SB map ({}x{} superblocks, '.'=clean, #=count/410):", sbx, sby);
                for r in 0..sby {
                    let row: String = (0..sbx)
                        .map(|c| match map[r * sbx + c] {
                            0 => '.',
                            n if n < 410 => '1',
                            n if n < 1640 => '2',
                            n if n < 3277 => '3',
                            _ => '#',
                        })
                        .collect();
                    eprintln!("   {r:2} {row}");
                }
            }
        } else {
            eprintln!("RECON_CHECK frame {}: ok", self.recon_check_n);
        }
        self.recon_check_n += 1;
    }

    /// Two-pass rate control: pass 1 codes every buffered frame at a probe qindex to
    /// measure the clip's true size, then a single global qindex is derived (from the
    /// `bits ≈ 2^(-q/Q_PER_2X)` model) that lands the pass-2 total on the target — a
    /// constant-quality encode that hits the size, without single-pass's startup
    /// transient or per-frame swings. Key frames get a small qindex bonus (they anchor
    /// the group). Both passes reset the reference chain (their recon differs by q).
    fn two_pass_encode(&mut self, frames: Vec<([Vec<u16>; 3], u32, u32)>) {
        let n = frames.len();
        if n == 0 {
            return;
        }
        let target_per_frame = self.rc.as_ref().map(|rc| rc.target_per_frame);
        // No target ⇒ nothing to solve for; fall back to fixed-q coding.
        let Some(tpf) = target_per_frame else {
            for (c, w, h) in frames {
                let b = self.code_frame_q(c, w, h, None, self.qindex);
                self.packets.push_back(Packet::from_data(0, b));
            }
            return;
        };
        const Q_PROBE: u32 = 128;
        const Q_PER_2X: f64 = 100.0; // qindex step that ~halves the coded size (measured)
        const KEY_BONUS: f64 = 16.0; // key frames coded a little finer

        // Pass 1: probe the true size at a fixed qindex.
        self.reference = None;
        self.golden = None;
        let mut probe_bits = 0.0f64;
        for (c, w, h) in &frames {
            let b = self.code_frame_q(c.clone(), *w, *h, None, Q_PROBE);
            probe_bits += b.len() as f64 * 8.0;
        }
        let target_total = tpf * n as f64;
        let ratio = (probe_bits / target_total).clamp(1.0 / 32.0, 32.0);
        let q2 = (Q_PROBE as f64 + Q_PER_2X * ratio.log2()).clamp(4.0, 220.0);
        let q_key = (q2 - KEY_BONUS).clamp(4.0, 220.0).round() as u32;
        let q_inter = q2.round() as u32;
        if std::env::var("VP9_2PASS_DBG").is_ok() {
            eprintln!(
                "2pass: probe_bits={probe_bits:.0} target_total={target_total:.0} ratio={ratio:.3} q2={q2:.1} q_key={q_key} q_inter={q_inter}"
            );
        }

        // Pass 2: emit at the derived qindex (fresh reference chain). Context
        // chaining engages here (pass 1 was a throwaway probe): fresh companion,
        // each frame coded against the previous frame's adapted context/MVs.
        self.reference = None;
        self.golden = None;
        self.companion = None;
        self.chain_prev_p = false;
        self.pass2_chaining = self.chain;
        for (i, (c, w, h)) in frames.into_iter().enumerate() {
            let q = if i == 0 { q_key } else { q_inter };
            let b = self.code_frame_q(c, w, h, None, q);
            self.packets.push_back(Packet::from_data(0, b));
        }
        self.pass2_chaining = false;
    }

    /// Code a display-order group as an ALT-REF GOP. The **first** group is key-started
    /// (KEY fills all slots); **subsequent** groups chain — no key frame, they predict
    /// from the previous group's reconstructed frames via the physical ref slots.
    ///
    /// Slot ping-pong (LAST=slot0 rolls; GOLDEN=previous ARF; ALTREF=new ARF): each
    /// group codes the hidden ALT-REF (F_last) into `arf_slot`, its shown P frames
    /// (which may reference LAST/GOLDEN/ALTREF) refreshing slot0, then a
    /// `show_existing_frame(arf_slot)`. GOLDEN and ALTREF slots swap for the next group,
    /// so the just-coded ARF becomes the next group's GOLDEN anchor.
    /// Chain args for the next group frame from the persistent companion decoder.
    fn chain_args(
        &self,
    ) -> Option<(
        crate::decode::FrameContext,
        Option<std::sync::Arc<Vec<crate::mv::MvRef>>>,
    )> {
        let c = self.companion.as_ref()?;
        let temporal_ok = !c.last_intra_only && c.last_show_frame && !c.last_frame_key;
        let mvs = if temporal_ok { c.prev_mvs.clone() } else { None };
        Some((c.frame_contexts[0].clone(), mvs))
    }

    /// Feed one emitted coded frame to the persistent companion, advancing its
    /// adapted-context / temporal-MV / ref-slot state. A hidden ALT-REF decodes
    /// then yields `Again` (no shown output) - not a desync. A hard error fails
    /// safe to independent frames.
    fn companion_feed(&mut self, bytes: &[u8]) {
        if !self.group_chain {
            return;
        }
        use rff_codec::Decoder as _;
        let comp = self
            .companion
            .get_or_insert_with(|| Box::new(crate::Vp9Decoder::default()));
        if comp
            .send_packet(&Packet::from_data(0, bytes.to_vec()))
            .is_err()
        {
            self.group_chain = false;
            self.chain = false;
            self.companion = None;
            return;
        }
        loop {
            match comp.receive_frame() {
                Ok(_) => {}
                Err(Error::Again) | Err(Error::Eof) => break,
                Err(_) => {
                    self.group_chain = false;
                    self.chain = false;
                    self.companion = None;
                    return;
                }
            }
        }
    }

    fn code_altref_group(&mut self, frames: Vec<([Vec<u16>; 3], u32, u32)>) {
        let n = frames.len();
        if n == 0 {
            return;
        }
        let (w, h) = (frames[0].1, frames[0].2);
        // The first group, or any resize, restarts with a key frame + fresh slots.
        let need_key = self.slots[0].is_none() || self.group_dims != Some((w, h));
        self.group_dims = Some((w, h));
        self.group_chain = self.chain && !self.twopass;

        // Content-adaptive lag: a hidden ALT-REF pays off on high-motion content
        // (bus -27% BD) but is pure overhead on static/low-motion (akiyo +31%),
        // so code low-motion groups as plain chained P frames. Also the fallback
        // for groups too short for an ARF.
        let plain = n <= 2 || group_motion(&frames) < self.lag_motion_thresh;
        if plain {
            for (i, (c, fw, fh)) in frames.into_iter().enumerate() {
                let b = if need_key && i == 0 {
                    self.code_key_slotted(c, fw, fh)
                } else {
                    self.code_p_slotted(c, fw, fh, false)
                };
                self.companion_feed(&b);
                let r = self.last_recon.take();
                self.recon_verify(&b, r.as_ref(), fw, fh);
                self.packets.push_back(Packet::from_data(0, b));
            }
            return;
        }

        // First shown frame of this group; a key-started group consumes F0 as the key.
        let mut i0 = 0;
        if need_key {
            self.golden_slot = 1;
            self.arf_slot = 2;
            let (c0, kw, kh) = frames[0].clone();
            let kb = self.code_key_slotted(c0, kw, kh);
            self.companion_feed(&kb);
            let r = self.last_recon.clone();
            self.recon_verify(&kb, r.as_ref(), kw, kh);
            self.packets.push_back(Packet::from_data(0, kb));
            i0 = 1;
        }

        // Hidden ALT-REF = the group's last frame, temporally filtered with the frames
        // just before it. References LAST/GOLDEN, refreshes `arf_slot`; displayed last.
        let (aw, ah) = (frames[n - 1].1, frames[n - 1].2);
        let carf = if self.tf_strength > 0.0 {
            let window = (n - 1).saturating_sub(4).max(i0);
            let neighbors: Vec<&[Vec<u16>; 3]> =
                frames[window..n - 1].iter().map(|f| &f.0).collect();
            let cw = (aw as usize).div_ceil(8) * 8;
            let ch = (ah as usize).div_ceil(8) * 8;
            temporal_filter(&frames[n - 1].0, &neighbors, cw, ch, self.tf_strength)
        } else {
            frames[n - 1].0.clone()
        };
        let ab = self.code_arf_slotted(carf, aw, ah);
        self.companion_feed(&ab);
        // The ARF is HIDDEN: feed it so the oracle's decoder tracks the reference slots,
        // and keep its recon to compare when `show_existing_frame` finally displays it.
        let arf_recon = self.last_recon.take();
        self.recon_verify(&ab, None, aw, ah);

        // First shown P (F_i0) packed WITH the hidden ARF into one superframe.
        let (c1, w1, h1) = frames[i0].clone();
        let pb1 = self.code_p_slotted(c1, w1, h1, true);
        self.companion_feed(&pb1);
        let r = self.last_recon.take();
        self.recon_verify(&pb1, r.as_ref(), w1, h1);
        self.packets
            .push_back(Packet::from_data(0, pack_superframe(&[ab, pb1])));

        // Remaining shown P frames F_{i0+1}..F_{n-2}.
        for f in frames.iter().take(n - 1).skip(i0 + 1) {
            let (ci, iw, ih) = f.clone();
            let pb = self.code_p_slotted(ci, iw, ih, true);
            self.companion_feed(&pb);
            let r = self.last_recon.take();
            self.recon_verify(&pb, r.as_ref(), iw, ih);
            self.packets.push_back(Packet::from_data(0, pb));
        }

        // Display the ALT-REF, then swap GOLDEN↔ALTREF so this ARF anchors the next group.
        let se = FrameEncoder::encode_show_existing_frame(self.arf_slot as u32);
        self.companion_feed(&se);
        // show_existing displays the hidden ARF — the one chance to verify it.
        self.recon_verify(&se, arf_recon.as_ref(), aw, ah);
        self.packets.push_back(Packet::from_data(0, se));
        std::mem::swap(&mut self.golden_slot, &mut self.arf_slot);
    }

    /// Key frame: fills every reference slot with its reconstruction.
    fn code_key_slotted(&mut self, coded: [Vec<u16>; 3], w: u32, h: u32) -> Vec<u8> {
        let q = self.next_qindex();
        let mut enc = FrameEncoder::new(w, h, q, coded, None);
        enc.set_speed(self.speed);
        self.budget_apply(&mut enc);
        let bytes = enc.encode_frame();
        self.budget_update(&enc, true);
        let recon = enc.recon_owned();
        self.last_recon = Some(recon.clone());
        self.slots = [Some(recon.clone()), Some(recon.clone()), Some(recon)];
        bytes
    }

    /// Hidden ALT-REF: predicts from LAST(slot0)/GOLDEN(golden_slot), refreshes
    /// `arf_slot`. Returns the coded bytes (stores its recon in `arf_slot`).
    fn code_arf_slotted(&mut self, coded: [Vec<u16>; 3], w: u32, h: u32) -> Vec<u8> {
        // ARF q boost, RATE-AWARE. `arf_qscale` is a MULTIPLIER on qindex, so a fixed
        // 0.5 gives a mild absolute boost at low q and an enormous one at high q —
        // i.e. it spends hardest exactly when the budget is smallest. Measured on
        // akiyo_cif at crf 60 the hidden ARF took 5,481 B of a 7,845 B stream (70%)
        // while ordinary frames cost 31 B, so the group could not reach low rates at
        // all and BD-rate collapsed (+46% over the low-quality half of the ladder).
        //
        // Fade the boost out as qindex rises: full `arf_qscale` at q=0, none at the
        // top of the usable range. Measured BD vs no-ARF (+ = ARF worse):
        //   akiyo  qscale 0.5 -> +21.31%   1.0 -> +1.15%   rate-aware -> see below
        //   bus    qscale 0.5 -> +37.11%   1.0 -> +14.31%
        // `VP9_ARF_QSCALE_FLAT=1` restores the old flat multiplier (the A/B oracle).
        let base = self.next_qindex() as f64;
        let qs = if std::env::var("VP9_ARF_QSCALE_FLAT").is_ok() {
            self.arf_qscale
        } else {
            const Q_MAX: f64 = 220.0; // top of the qindex range rate control uses
            let headroom = ((Q_MAX - base) / Q_MAX).clamp(0.0, 1.0);
            1.0 - (1.0 - self.arf_qscale) * headroom
        };
        let q = ((base * qs).round() as u32).clamp(4, 255);
        let idx = [0, self.golden_slot, self.arf_slot];
        let mut enc = FrameEncoder::new(w, h, q, coded, self.slots[0].clone());
        enc.set_speed(self.speed);
        self.budget_apply(&mut enc);
        // Compound default-on for the whole ARF group (quality tier): the ARF frame does
        // LAST+GOLDEN, the shown P frames that reference it do true bi-prediction — kept
        // consistent across the group so libvpx accepts it. `VP9_NO_COMPOUND` opts out.
        enc.set_compound(self.speed <= 3);
        if self.group_chain {
            if let Some((fc, mvs)) = self.chain_args() {
                enc.set_chain(fc, mvs);
            }
        }
        enc.set_golden(self.slots[self.golden_slot].clone().unwrap());
        enc.set_ref_frame_idx(idx);
        enc.set_hidden_altref(self.arf_slot);
        let bytes = enc.encode_frame();
        self.budget_update(&enc, false);
        let arf_recon = enc.recon_owned();
        self.last_recon = Some(arf_recon.clone());
        self.slots[self.arf_slot] = Some(arf_recon);
        bytes
    }

    /// Shown P frame: predicts from LAST(slot0)/GOLDEN(golden_slot)/ALTREF(arf_slot when
    /// `with_altref`), refreshes LAST(slot0). Returns the coded bytes.
    fn code_p_slotted(
        &mut self,
        coded: [Vec<u16>; 3],
        w: u32,
        h: u32,
        with_altref: bool,
    ) -> Vec<u8> {
        let q = self.next_qindex();
        let idx = [0, self.golden_slot, self.arf_slot];
        let mut enc = FrameEncoder::new(w, h, q, coded, self.slots[0].clone());
        enc.set_speed(self.speed);
        self.budget_apply(&mut enc);
        // Compound default-on across the ARF group (a shown P referencing the future ARF
        // does true bi-prediction). Quality tier only. `VP9_NO_COMPOUND` opts out.
        enc.set_compound(self.speed <= 3);
        if self.group_chain && self.slots[0].is_some() {
            if let Some((fc, mvs)) = self.chain_args() {
                enc.set_chain(fc, mvs);
            }
        }
        if self.slots[0].is_some() {
            enc.set_golden(self.slots[self.golden_slot].clone().unwrap());
            if with_altref {
                if let Some(a) = &self.slots[self.arf_slot] {
                    enc.set_altref(a.clone());
                }
            }
            enc.set_ref_frame_idx(idx);
            enc.set_refresh_frame_flags(1); // refresh LAST (slot 0)
        }
        let bytes = enc.encode_frame();
        self.budget_update(&enc, self.slots[0].is_none());
        let recon = enc.recon_owned();
        self.last_recon = Some(recon.clone());
        self.slots[0] = Some(recon);
        bytes
    }
}

#[cfg(test)]
mod tests {
    use super::*;
    use rff_codec::Decoder;
    use rff_core::CodecId;

    #[test]
    fn encoder_trait_roundtrips_through_registry() {
        // A 96×64 YUV420p frame through the registered encoder, then the
        // registered decoder; the decode must be valid (a key frame of the right
        // size). Bit-exactness vs the recon is covered by frameenc's tests.
        let (w, h) = (96u32, 64u32);
        let ylen = (w * h) as usize;
        let clen = ((w / 2) * (h / 2)) as usize;
        let vf = VideoFrame {
            width: w,
            height: h,
            format: PixelFormat::Yuv420p,
            planes: vec![
                (0..ylen).map(|i| (i % 256) as u8).collect(),
                vec![128u8; clen],
                vec![128u8; clen],
            ],
            strides: vec![w as usize, (w / 2) as usize, (w / 2) as usize],
            pts: None,
        };

        let mut reg = rff_codec::CodecRegistry::new();
        crate::register(&mut reg);
        let mut enc = reg.find_encoder(CodecId::Vp9).unwrap();
        enc.configure(&Dictionary::new()).unwrap();
        enc.send_frame(&Frame::Video(vf)).unwrap();
        enc.flush();
        let pkt = enc.receive_packet().unwrap();
        assert!(!pkt.data.is_empty());
        // First three bytes: frame marker (10) + profile 0 + show_existing 0 +
        // key_frame bit 0 ... → byte 0 high bits 0b100... ; just confirm it decodes.

        let mut dec = reg.find_decoder(CodecId::Vp9).unwrap();
        dec.send_packet(&pkt).unwrap();
        let Frame::Video(out) = dec.receive_frame().unwrap() else {
            panic!("video")
        };
        assert_eq!((out.width, out.height), (w, h));
        assert_eq!(out.format, PixelFormat::Yuv420p);
    }

    /// ALT-REF lookahead: a `-lag N` group codes KEY + a hidden future ALT-REF + P
    /// frames + a `show_existing_frame`, and must decode to `N` displayed frames that
    /// are pixel-identical across our decoder, libvpx, and ffmpeg. Set `VP9_ARF_OUT` to
    /// dump the IVF + our decoded YUV for the external comparison.
    #[test]
    fn altref_lookahead_structure_and_roundtrip() {
        use rff_core::CodecId;
        let (w, h) = (128u32, 96u32);
        let (cw, ch) = (w as usize, h as usize);
        let n = 8u32;
        let frame = |f: u32| -> VideoFrame {
            let s = f as usize;
            let y: Vec<u8> = (0..cw * ch)
                .map(|i| (((i % cw + s) ^ (i / cw)) % 200 + 20) as u8)
                .collect();
            let uv = vec![128u8; (cw / 2) * (ch / 2)];
            VideoFrame {
                width: w,
                height: h,
                format: PixelFormat::Yuv420p,
                planes: vec![y, uv.clone(), uv],
                strides: vec![cw, cw / 2, cw / 2],
                pts: Some(f as i64),
            }
        };
        let mut reg = rff_codec::CodecRegistry::new();
        crate::register(&mut reg);
        let mut enc = reg.find_encoder(CodecId::Vp9).unwrap();
        let mut opts = Dictionary::new();
        opts.set("lag", &n.to_string());
        enc.configure(&opts).unwrap();
        for f in 0..n {
            enc.send_frame(&Frame::Video(frame(f))).unwrap();
        }
        enc.flush();
        let mut packets = Vec::new();
        while let Ok(p) = enc.receive_packet() {
            packets.push(p.data);
        }
        // One group of n frames ⇒ KEY + superframe[ARF,P1] + (n-3) P + show_existing = n.
        assert_eq!(
            packets.len() as u32,
            n,
            "expected KEY + superframe(ARF,P1) + P… + show_existing"
        );

        // Decode with our decoder; a hidden ARF yields no displayed frame, the
        // show_existing yields the ARF's frame — so exactly n frames are displayed.
        let mut dec = reg.find_decoder(CodecId::Vp9).unwrap();
        let mut ours: Vec<VideoFrame> = Vec::new();
        for pkt in &packets {
            dec.send_packet(&Packet::from_data(0, pkt.clone())).unwrap();
            while let Ok(Frame::Video(vf)) = dec.receive_frame() {
                ours.push(vf);
            }
        }
        assert_eq!(ours.len() as u32, n, "displayed frame count");

        if let Ok(dir) = std::env::var("VP9_ARF_OUT") {
            let mut ivf = Vec::new();
            ivf.extend_from_slice(b"DKIF");
            ivf.extend_from_slice(&0u16.to_le_bytes());
            ivf.extend_from_slice(&32u16.to_le_bytes());
            ivf.extend_from_slice(b"VP90");
            ivf.extend_from_slice(&(w as u16).to_le_bytes());
            ivf.extend_from_slice(&(h as u16).to_le_bytes());
            ivf.extend_from_slice(&30u32.to_le_bytes());
            ivf.extend_from_slice(&1u32.to_le_bytes());
            ivf.extend_from_slice(&(packets.len() as u32).to_le_bytes());
            ivf.extend_from_slice(&0u32.to_le_bytes());
            for (i, b) in packets.iter().enumerate() {
                ivf.extend_from_slice(&(b.len() as u32).to_le_bytes());
                ivf.extend_from_slice(&(i as u64).to_le_bytes());
                ivf.extend_from_slice(b);
            }
            std::fs::write(format!("{dir}/arf.ivf"), &ivf).unwrap();
            // Our decoded frames, display order, planar 4:2:0 (display size).
            let mut raw = Vec::new();
            for vf in &ours {
                for (p, &(pw, ph)) in [
                    (w as usize, h as usize),
                    ((w / 2) as usize, (h / 2) as usize),
                    ((w / 2) as usize, (h / 2) as usize),
                ]
                .iter()
                .enumerate()
                {
                    for yy in 0..ph {
                        raw.extend_from_slice(
                            &vf.planes[p][yy * vf.strides[p]..yy * vf.strides[p] + pw],
                        );
                    }
                }
            }
            std::fs::write(format!("{dir}/arf.ours.yuv"), &raw).unwrap();
        }
    }

    /// Two-pass rate control: on a clip whose complexity varies over time, the encode
    /// should land near the requested size (better than single-pass, which overshoots
    /// at the start before the leaky bucket catches up) and decode cleanly.
    #[test]
    fn two_pass_hits_target_and_decodes() {
        use rff_core::CodecId;
        let (w, h) = (128u32, 96u32);
        let (cw, ch) = (w as usize, h as usize);
        let n = 16u32;
        let fps = 30.0;
        // First half smooth, second half busy — a moving-complexity clip so a global
        // (lookahead) allocation clearly beats a reactive one.
        let frame = |f: u32| -> VideoFrame {
            let busy = f >= n / 2;
            let y: Vec<u8> = (0..cw * ch)
                .map(|i| {
                    let (x, yy) = (i % cw, i / cw);
                    if busy {
                        (((x * 13) ^ (yy * 7) ^ (f as usize * 5)) % 256) as u8
                    } else {
                        ((x + yy) / 3 % 200) as u8
                    }
                })
                .collect();
            let uv = vec![128u8; (cw / 2) * (ch / 2)];
            VideoFrame {
                width: w,
                height: h,
                format: PixelFormat::Yuv420p,
                planes: vec![y, uv.clone(), uv],
                strides: vec![cw, cw / 2, cw / 2],
                pts: Some(f as i64),
            }
        };

        let target = "300k";
        let mut reg = rff_codec::CodecRegistry::new();
        crate::register(&mut reg);
        let mut enc = reg.find_encoder(CodecId::Vp9).unwrap();
        let mut opts = Dictionary::new();
        opts.set("b", target);
        opts.set("twopass", "1");
        enc.configure(&opts).unwrap();
        for f in 0..n {
            enc.send_frame(&Frame::Video(frame(f))).unwrap();
        }
        enc.flush();
        let mut total_bits = 0u64;
        let mut packets = Vec::new();
        while let Ok(p) = enc.receive_packet() {
            total_bits += p.data.len() as u64 * 8;
            packets.push(p.data);
        }
        let achieved = total_bits as f64 * fps / n as f64;
        eprintln!("two-pass: target=300000 bps, achieved={achieved:.0} bps");
        // Within ±35% of target — the qindex model is coarse but the global solve keeps
        // it in the ballpark (single-pass on this clip swings far wider at the start).
        assert!(
            (achieved - 300_000.0).abs() < 0.35 * 300_000.0,
            "two-pass missed target badly: {achieved:.0} bps"
        );

        // The stream must decode to all n frames.
        let mut dec = reg.find_decoder(CodecId::Vp9).unwrap();
        let mut shown = 0u32;
        for pkt in &packets {
            dec.send_packet(&Packet::from_data(0, pkt.clone())).unwrap();
            while let Ok(Frame::Video(_)) = dec.receive_frame() {
                shown += 1;
            }
        }
        assert_eq!(shown, n, "two-pass decoded frame count");
    }

    /// ALT-REF temporal filtering: on a static scene corrupted by per-frame noise, the
    /// filter averages the motion-compensated neighbors so the ALT-REF *recovers the
    /// clean signal*. The displayed ALT-REF (last frame, via `show_existing`) is then
    /// markedly closer to the noise-free ground truth than the raw noisy anchor is —
    /// higher PSNR-vs-clean — at no cost in group size.
    #[test]
    fn temporal_filter_denoises_altref() {
        use rff_core::CodecId;
        let (w, h) = (128u32, 96u32);
        let (cw, ch) = (w as usize, h as usize);
        let n = 8u32;
        // Clean static base + strong per-frame noise (uncorrelated frame-to-frame).
        let base = |x: usize, y: usize| (((x * 5) ^ (y * 3)) % 180 + 40) as i32;
        let clean: Vec<u8> = (0..cw * ch).map(|i| base(i % cw, i / cw) as u8).collect();
        let frame = |f: u32| -> VideoFrame {
            let mut s = 0x9E3779B9u32.wrapping_mul(f + 1).wrapping_add(1);
            let mut noise = move || {
                s ^= s << 13;
                s ^= s >> 17;
                s ^= s << 5;
                (s % 41) as i32 - 20
            };
            let y: Vec<u8> = (0..cw * ch)
                .map(|i| (base(i % cw, i / cw) + noise()).clamp(0, 255) as u8)
                .collect();
            let uv = vec![128u8; (cw / 2) * (ch / 2)];
            VideoFrame {
                width: w,
                height: h,
                format: PixelFormat::Yuv420p,
                planes: vec![y, uv.clone(), uv],
                strides: vec![cw, cw / 2, cw / 2],
                pts: Some(f as i64),
            }
        };
        // Encode a group, then decode; return (group bytes, PSNR of the last displayed
        // frame — the ALT-REF — against the clean ground truth).
        let run = |strength: &str| -> (usize, f64) {
            let mut reg = rff_codec::CodecRegistry::new();
            crate::register(&mut reg);
            let mut enc = reg.find_encoder(CodecId::Vp9).unwrap();
            let mut opts = Dictionary::new();
            opts.set("lag", &n.to_string());
            opts.set("qp", "48");
            opts.set("arnr-strength", strength);
            enc.configure(&opts).unwrap();
            for f in 0..n {
                enc.send_frame(&Frame::Video(frame(f))).unwrap();
            }
            enc.flush();
            let mut total = 0;
            let mut packets = Vec::new();
            while let Ok(p) = enc.receive_packet() {
                total += p.data.len();
                packets.push(p.data);
            }
            let mut dec = reg.find_decoder(CodecId::Vp9).unwrap();
            let mut last: Option<VideoFrame> = None;
            for pkt in &packets {
                dec.send_packet(&Packet::from_data(0, pkt.clone())).unwrap();
                while let Ok(Frame::Video(vf)) = dec.receive_frame() {
                    last = Some(vf);
                }
            }
            let vf = last.unwrap();
            let mut se = 0u64;
            for y in 0..ch {
                for x in 0..cw {
                    let d = clean[y * cw + x] as i64 - vf.planes[0][y * vf.strides[0] + x] as i64;
                    se += (d * d) as u64;
                }
            }
            let mse = se as f64 / (cw * ch) as f64;
            let psnr = 10.0 * (255.0f64 * 255.0 / mse).log10();
            (total, psnr)
        };
        let (on_bytes, on_psnr) = run("4");
        let (off_bytes, off_psnr) = run("0");
        eprintln!(
            "temporal filter: ALT-REF PSNR-vs-clean off={off_psnr:.2} dB on={on_psnr:.2} dB (+{:.2}); group bytes off={off_bytes} on={on_bytes}",
            on_psnr - off_psnr
        );
        // The filtered ALT-REF recovers the clean signal far better...
        assert!(
            on_psnr > off_psnr + 2.0,
            "temporal filter did not denoise: on={on_psnr:.2} off={off_psnr:.2}"
        );
        // ...and does not cost group size.
        assert!(
            on_bytes <= off_bytes,
            "tf grew the group: on={on_bytes} off={off_bytes}"
        );
    }

    /// Cross-GOP chaining: two `-lag 8` groups over 16 frames must contain exactly ONE
    /// key frame (the very first) — the second group chains through the reference slots
    /// with no key — yet still decode to 16 displayed frames that are pixel-identical
    /// across our decoder, libvpx, and ffmpeg. `VP9_XGOP_OUT` dumps for the external arm.
    #[test]
    fn cross_gop_chaining_no_extra_keyframe() {
        use rff_core::CodecId;
        let (w, h) = (128u32, 96u32);
        let (cw, ch) = (w as usize, h as usize);
        let n = 16u32;
        let frame = |f: u32| -> VideoFrame {
            let s = f as usize;
            let y: Vec<u8> = (0..cw * ch)
                .map(|i| (((i % cw + s) ^ (i / cw + s / 2)) % 220 + 18) as u8)
                .collect();
            let uv = vec![128u8; (cw / 2) * (ch / 2)];
            VideoFrame {
                width: w,
                height: h,
                format: PixelFormat::Yuv420p,
                planes: vec![y, uv.clone(), uv],
                strides: vec![cw, cw / 2, cw / 2],
                pts: Some(f as i64),
            }
        };
        let mut reg = rff_codec::CodecRegistry::new();
        crate::register(&mut reg);
        let mut enc = reg.find_encoder(CodecId::Vp9).unwrap();
        let mut opts = Dictionary::new();
        opts.set("lag", "8");
        enc.configure(&opts).unwrap();
        for f in 0..n {
            enc.send_frame(&Frame::Video(frame(f))).unwrap();
        }
        enc.flush();
        let mut packets = Vec::new();
        while let Ok(p) = enc.receive_packet() {
            packets.push(p.data);
        }
        // A frame is a key frame iff (not show_existing and frame_type=0), i.e. the
        // show_existing (bit3) and frame_type (bit2) bits of byte0 are both 0.
        let keyframes = packets.iter().filter(|p| p[0] & 0x0C == 0).count();
        assert_eq!(
            keyframes, 1,
            "exactly one key frame expected (chained groups)"
        );

        let mut dec = reg.find_decoder(CodecId::Vp9).unwrap();
        let mut ours: Vec<VideoFrame> = Vec::new();
        for pkt in &packets {
            dec.send_packet(&Packet::from_data(0, pkt.clone())).unwrap();
            while let Ok(Frame::Video(vf)) = dec.receive_frame() {
                ours.push(vf);
            }
        }
        assert_eq!(ours.len() as u32, n, "displayed frame count");

        if let Ok(dir) = std::env::var("VP9_XGOP_OUT") {
            let mut ivf = Vec::new();
            ivf.extend_from_slice(b"DKIF");
            ivf.extend_from_slice(&0u16.to_le_bytes());
            ivf.extend_from_slice(&32u16.to_le_bytes());
            ivf.extend_from_slice(b"VP90");
            ivf.extend_from_slice(&(w as u16).to_le_bytes());
            ivf.extend_from_slice(&(h as u16).to_le_bytes());
            ivf.extend_from_slice(&30u32.to_le_bytes());
            ivf.extend_from_slice(&1u32.to_le_bytes());
            ivf.extend_from_slice(&(packets.len() as u32).to_le_bytes());
            ivf.extend_from_slice(&0u32.to_le_bytes());
            for (i, b) in packets.iter().enumerate() {
                ivf.extend_from_slice(&(b.len() as u32).to_le_bytes());
                ivf.extend_from_slice(&(i as u64).to_le_bytes());
                ivf.extend_from_slice(b);
            }
            std::fs::write(format!("{dir}/xgop.ivf"), &ivf).unwrap();
            let mut raw = Vec::new();
            for vf in &ours {
                for (p, &(pw, ph)) in [
                    (w as usize, h as usize),
                    ((w / 2) as usize, (h / 2) as usize),
                    ((w / 2) as usize, (h / 2) as usize),
                ]
                .iter()
                .enumerate()
                {
                    for yy in 0..ph {
                        raw.extend_from_slice(
                            &vf.planes[p][yy * vf.strides[p]..yy * vf.strides[p] + pw],
                        );
                    }
                }
            }
            std::fs::write(format!("{dir}/xgop.ours.yuv"), &raw).unwrap();
        }
    }

    #[test]
    fn parse_bitrate_handles_suffixes() {
        assert_eq!(parse_bitrate_bps("2M"), Some(2_000_000.0));
        assert_eq!(parse_bitrate_bps("128k"), Some(128_000.0));
        assert_eq!(parse_bitrate_bps("500000"), Some(500_000.0));
        assert_eq!(parse_bitrate_bps("oops"), None);
    }

    /// R2 — `-b:v` drives the bitrate: a higher target spends more bits, and a low
    /// target is tracked (not wildly overshot). Robust to the clip's compressibility.
    #[test]
    fn rate_control_tracks_target_bitrate() {
        let (w, h) = (96u32, 96u32);
        let (cw, ch) = (w as usize, h as usize);
        let fps = 30.0;
        let n = 12u32;

        let frame = |f: u32| -> VideoFrame {
            let shift = f as usize; // a panning texture ⇒ real inter residual
            let y: Vec<u8> = (0..cw * ch)
                .map(|i| {
                    (((i % cw + shift).wrapping_mul(31) ^ (i / cw).wrapping_mul(57)) % 256) as u8
                })
                .collect();
            let uv = vec![128u8; (cw / 2) * (ch / 2)];
            VideoFrame {
                width: w,
                height: h,
                format: PixelFormat::Yuv420p,
                planes: vec![y, uv.clone(), uv],
                strides: vec![cw, cw / 2, cw / 2],
                pts: Some(f as i64),
            }
        };

        let run = |bitrate: &str| -> f64 {
            let mut reg = rff_codec::CodecRegistry::new();
            crate::register(&mut reg);
            let mut enc = reg.find_encoder(CodecId::Vp9).unwrap();
            let mut opts = Dictionary::new();
            opts.set("b", bitrate);
            enc.configure(&opts).unwrap();
            let mut total_bits = 0u64;
            for f in 0..n {
                enc.send_frame(&Frame::Video(frame(f))).unwrap();
                while let Ok(pkt) = enc.receive_packet() {
                    total_bits += pkt.data.len() as u64 * 8;
                }
            }
            total_bits as f64 * fps / n as f64
        };

        let lo = run("120k");
        let hi = run("3M");
        eprintln!("rate control: 120k→{lo:.0} bps, 3M→{hi:.0} bps");
        // A higher target spends more bits...
        assert!(
            hi > lo * 1.5,
            "no response to target: lo={lo:.0} hi={hi:.0}"
        );
        // ...and the low target is tracked, not blown past.
        assert!(
            lo < 120_000.0 * 2.5,
            "overshot the 120k target: {lo:.0} bps"
        );
    }
}
