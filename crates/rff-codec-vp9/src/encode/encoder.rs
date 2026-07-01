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
        // Two-pass: `-pass 2` (ffmpeg-style; `-pass 1` is a discardable analysis pass we
        // fold into pass 2 internally) or an explicit `twopass=1`. Needs `-b:v`.
        if options.get("pass").map(|v| v.trim()) == Some("2")
            || options.get("twopass").map(|v| v.trim()) == Some("1")
        {
            self.twopass = true;
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
        let recon = enc.recon_owned();
        if is_key {
            self.golden = Some((recon.clone(), w, h));
        }
        self.reference = Some((recon, w, h));
        bytes
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

        // Pass 2: emit at the derived qindex (fresh reference chain).
        self.reference = None;
        self.golden = None;
        for (i, (c, w, h)) in frames.into_iter().enumerate() {
            let q = if i == 0 { q_key } else { q_inter };
            let b = self.code_frame_q(c, w, h, None, q);
            self.packets.push_back(Packet::from_data(0, b));
        }
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
    fn code_altref_group(&mut self, frames: Vec<([Vec<u16>; 3], u32, u32)>) {
        let n = frames.len();
        if n == 0 {
            return;
        }
        let (w, h) = (frames[0].1, frames[0].2);
        // The first group, or any resize, restarts with a key frame + fresh slots.
        let need_key = self.slots[0].is_none() || self.group_dims != Some((w, h));
        self.group_dims = Some((w, h));

        // Groups too short for a hidden ALT-REF: code plainly (key iff needed, then P…).
        if n <= 2 {
            for (i, (c, fw, fh)) in frames.into_iter().enumerate() {
                let b = if need_key && i == 0 {
                    self.code_key_slotted(c, fw, fh)
                } else {
                    self.code_p_slotted(c, fw, fh, false)
                };
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

        // First shown P (F_i0) packed WITH the hidden ARF into one superframe.
        let (c1, w1, h1) = frames[i0].clone();
        let pb1 = self.code_p_slotted(c1, w1, h1, true);
        self.packets
            .push_back(Packet::from_data(0, pack_superframe(&[ab, pb1])));

        // Remaining shown P frames F_{i0+1}..F_{n-2}.
        for f in frames.iter().take(n - 1).skip(i0 + 1) {
            let (ci, iw, ih) = f.clone();
            let pb = self.code_p_slotted(ci, iw, ih, true);
            self.packets.push_back(Packet::from_data(0, pb));
        }

        // Display the ALT-REF, then swap GOLDEN↔ALTREF so this ARF anchors the next group.
        self.packets.push_back(Packet::from_data(
            0,
            FrameEncoder::encode_show_existing_frame(self.arf_slot as u32),
        ));
        std::mem::swap(&mut self.golden_slot, &mut self.arf_slot);
    }

    /// Key frame: fills every reference slot with its reconstruction.
    fn code_key_slotted(&mut self, coded: [Vec<u16>; 3], w: u32, h: u32) -> Vec<u8> {
        let q = self.next_qindex();
        let mut enc = FrameEncoder::new(w, h, q, coded, None);
        let bytes = enc.encode_frame();
        let recon = enc.recon_owned();
        self.slots = [Some(recon.clone()), Some(recon.clone()), Some(recon)];
        bytes
    }

    /// Hidden ALT-REF: predicts from LAST(slot0)/GOLDEN(golden_slot), refreshes
    /// `arf_slot`. Returns the coded bytes (stores its recon in `arf_slot`).
    fn code_arf_slotted(&mut self, coded: [Vec<u16>; 3], w: u32, h: u32) -> Vec<u8> {
        let q = self.next_qindex();
        let idx = [0, self.golden_slot, self.arf_slot];
        let mut enc = FrameEncoder::new(w, h, q, coded, self.slots[0].clone());
        enc.set_golden(self.slots[self.golden_slot].clone().unwrap());
        enc.set_ref_frame_idx(idx);
        enc.set_hidden_altref(self.arf_slot);
        let bytes = enc.encode_frame();
        self.slots[self.arf_slot] = Some(enc.recon_owned());
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
        self.slots[0] = Some(enc.recon_owned());
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
