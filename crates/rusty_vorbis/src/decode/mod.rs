//! Pure-Rust **Vorbis I decoder** (packet level, Ogg-agnostic).
//!
//! Construct a [`Decoder`] from the three header packets (identification,
//! comment, setup), then feed it audio packets in stream order:
//!
//! ```no_run
//! # fn demo(ident: &[u8], comment: &[u8], setup: &[u8], packets: &[Vec<u8>]) -> rusty_vorbis::Result<()> {
//! use rusty_vorbis::decode::Decoder;
//! let mut dec = Decoder::new(ident, comment, setup)?;
//! let mut pcm = vec![0.0f32; dec.max_frames_per_packet() * dec.channels()];
//! for p in packets {
//!     let frames = dec.decode_interleaved(p, &mut pcm)?;
//!     let _samples = &pcm[..frames * dec.channels()]; // interleaved f32
//! }
//! # Ok(()) }
//! ```
//!
//! **Output semantics** (identical to lewton and FFmpeg): the first audio
//! packet after construction (or [`Decoder::reset`]) yields **0** frames — it
//! only primes the overlap — and every later packet yields the overlapped half
//! between the previous and the current window centres
//! (`prev_blocksize/4 + cur_blocksize/4` for a well-formed stream). Trimming the
//! stream's end (and any leading skip) by granule position is the container's
//! job. Samples are `f32`, nominally in [-1, 1], unclipped.
//!
//! **Channel order** is Vorbis native order (Vorbis I §4.3.9), *not*
//! FFmpeg/WAV order: 1 mono; 2 L R; 3 L C R; 4 FL FR RL RR; 5 FL C FR RL RR;
//! 6 FL C FR RL RR LFE; 7 FL C FR SL SR RC LFE; 8 FL C FR SL SR RL RR LFE.
//! Reorder in the adapter if the consumer wants another layout.
//!
//! **No per-packet allocation**: all scratch is sized at construction from the
//! headers. **Errors, never panics**: malformed packets return
//! [`crate::Error::InvalidData`] and leave the decoder state unchanged; hostile
//! headers are rejected or bounded (see `header`).
//!
//! **Where this decoder deliberately differs from lewton 0.10.2** (all found
//! by the synthetic-stream gate; none is reachable from a libvorbis stream):
//! lewton's fast IMDCT is wrong at block sizes 64 and 128 (its own slow IMDCT
//! agrees with ours); lewton swaps the magnitude/angle roles of a coupling
//! step whose magnitude channel number is above the angle's; lewton reads only
//! 7 of the 8 residue cascade books. We follow the spec (and FFmpeg) in each.
//!
//! Correctness is gated against lewton 0.10.2 and FFmpeg 8.1 (libvorbis
//! streams, q-1..q10, 1-8 channels, 8-96 kHz): sample-exact frame counts and a
//! max abs difference around 1e-7 (float operation order differs, so the gate
//! is a tolerance plus SNR, not bit-exactness).

mod bits;
mod floor;
mod header;
mod imdct;
pub mod prof;
mod residue;

pub use header::{CommentHeader, IdentHeader};

use crate::{Error, Result};
use bits::BitReader;
use floor::FloorDecode;
use header::{ilog, Floor, Setup};
use imdct::Dct4;

/// Per-channel floor outcome for the current packet.
#[derive(Clone, Copy, PartialEq)]
enum ChFloor {
    Unused,
    One,
    Zero(u64),
}

/// The Vorbis window slope `sin(π/2 · sin²(π/2 · (x+½)/n))`, x in 0..n,
/// computed in f64 (FFmpeg ships the same curve as precomputed tables).
fn window_slope(n: usize) -> Vec<f32> {
    use std::f64::consts::FRAC_PI_2;
    (0..n)
        .map(|x| {
            let v = (FRAC_PI_2 * (x as f64 + 0.5) / n as f64).sin();
            (FRAC_PI_2 * v * v).sin() as f32
        })
        .collect()
}

/// A Vorbis I decoder. See the [module docs](self).
pub struct Decoder {
    ident: IdentHeader,
    comment: CommentHeader,
    setup: Setup,
    channels: usize,
    /// Block sizes `[short, long]`.
    bs: [usize; 2],
    mode_bits: u32,
    /// Per mapping, per submap: the channels it carries, in channel order.
    submap_channels: Vec<Vec<Vec<u8>>>,
    dct: [Dct4; 2],
    /// Window slopes for half-lengths `bs[0]/2` and `bs[1]/2`.
    slope: [Vec<f32>; 2],

    // --- persistent overlap state ---
    has_prev: bool,
    prev_len: usize,
    /// `channels x bs[1]/2`, channel `c` at `c * bs[1]/2`.
    prev: Vec<f32>,

    // --- per-packet scratch (sized once) ---
    /// Spectra, `channels x n/2` with stride `n/2` of the current block.
    spectra: Vec<f32>,
    /// Residue-2 interleaved vector / unfold scratch (`channels x bs[1]/2`).
    interleave: Vec<f32>,
    /// Decoded PCM, planar, channel `c` at `c * max_frames`.
    pcm: Vec<f32>,
    /// Max frames one packet yields: `(3*bs[1] - bs[0]) / 4` (a long block
    /// between a long and a short neighbour).
    max_frames: usize,
    pcm_frames: usize,
    floor_y: Vec<u32>,
    floor0: Vec<Vec<f32>>,
    ch_floor: Vec<ChFloor>,
    no_residue: Vec<bool>,
    skip: Vec<bool>,
    classes: Vec<u8>,
    dct_out: Vec<f32>,
    fft_scratch: Vec<[f32; 2]>,
    blk: Vec<f32>,
}

impl Decoder {
    /// Build a decoder from the identification, comment and setup header
    /// packets.
    pub fn new(ident: &[u8], comment: &[u8], setup: &[u8]) -> Result<Self> {
        let ident = header::parse_ident(ident)?;
        let comment = header::parse_comment(comment)?;
        let setup = header::parse_setup(setup, &ident)?;
        let channels = ident.channels as usize;
        let bs = [1usize << ident.blocksize_0, 1usize << ident.blocksize_1];
        let half = bs[1] / 2;
        let max_frames = (3 * bs[1] - bs[0]) / 4;
        let submap_channels = setup
            .mappings
            .iter()
            .map(|m| {
                (0..m.submap_floor.len())
                    .map(|s| {
                        (0..channels as u8)
                            .filter(|&c| m.mux[c as usize] as usize == s)
                            .collect()
                    })
                    .collect()
            })
            .collect();
        Ok(Decoder {
            mode_bits: ilog(setup.modes.len() as u64 - 1),
            submap_channels,
            dct: [Dct4::new(bs[0] / 2), Dct4::new(bs[1] / 2)],
            slope: [window_slope(bs[0] / 2), window_slope(bs[1] / 2)],
            has_prev: false,
            prev_len: 0,
            prev: vec![0.0; channels * half],
            spectra: vec![0.0; channels * half],
            interleave: vec![0.0; channels * half],
            pcm: vec![0.0; channels * max_frames],
            max_frames,
            pcm_frames: 0,
            floor_y: vec![0; channels * 65],
            floor0: vec![Vec::new(); channels],
            ch_floor: vec![ChFloor::Unused; channels],
            no_residue: vec![false; channels],
            skip: Vec::with_capacity(channels),
            classes: Vec::new(),
            dct_out: vec![0.0; half],
            fft_scratch: vec![[0.0; 2]; half / 2],
            blk: vec![0.0; bs[1]],
            ident,
            comment,
            setup,
            channels,
            bs,
        })
    }

    /// Number of channels (Vorbis native order, see the module docs).
    pub fn channels(&self) -> usize {
        self.channels
    }

    pub fn sample_rate(&self) -> u32 {
        self.ident.sample_rate
    }

    /// The parsed identification header.
    pub fn ident(&self) -> &IdentHeader {
        &self.ident
    }

    /// The parsed comment header (vendor + user comments).
    pub fn comment(&self) -> &CommentHeader {
        &self.comment
    }

    /// Upper bound on the frames (samples per channel) one packet can yield:
    /// `(3 * long_blocksize - short_blocksize) / 4` (a long block whose left
    /// neighbour is long and right neighbour short).
    pub fn max_frames_per_packet(&self) -> usize {
        self.max_frames
    }

    /// Forget the overlap state (after a seek): the next packet yields 0
    /// frames, like the first packet of a stream.
    pub fn reset(&mut self) {
        self.has_prev = false;
        self.pcm_frames = 0;
    }

    /// Frames the packet would add to the window sequence (`prev/4 + cur/4`
    /// style), from its header bits only — without decoding. Note the first
    /// packet after construction / [`Self::reset`] still yields 0.
    pub fn packet_frames(&self, packet: &[u8]) -> Result<usize> {
        let (_, _, lws, rws, _) = self.packet_layout(packet)?;
        Ok(rws - lws)
    }

    /// Parse the packet type, mode and window flags:
    /// `(mode, n, left_win_start, right_win_start, right_win_end)`.
    fn packet_layout(&self, packet: &[u8]) -> Result<(usize, usize, usize, usize, usize)> {
        let mut br = BitReader::new(packet);
        let (mode, n, lws, rws, rwe, _) = self.read_packet_header(&mut br)?;
        Ok((mode, n, lws, rws, rwe))
    }

    #[allow(clippy::type_complexity)]
    fn read_packet_header(
        &self,
        br: &mut BitReader,
    ) -> Result<(usize, usize, usize, usize, usize, usize)> {
        if br.read_bool() {
            return Err(Error::invalid("vorbis: header packet in the audio stream"));
        }
        let mode = br.read(self.mode_bits) as usize;
        if br.eop() {
            return Err(Error::invalid("vorbis: truncated audio packet"));
        }
        let Some(m) = self.setup.modes.get(mode) else {
            return Err(Error::invalid("vorbis: bad mode number"));
        };
        let n = self.bs[m.blockflag as usize];
        let (prev_long, next_long) = if m.blockflag {
            let p = br.read_bool();
            let q = br.read_bool();
            if br.eop() {
                return Err(Error::invalid("vorbis: truncated audio packet"));
            }
            (p, q)
        } else {
            (true, true)
        };
        let s = self.bs[0];
        // Left window: start, and which slope (long = 1) it overlaps with.
        let (lws, left_slope) = if prev_long {
            (0, m.blockflag as usize)
        } else {
            ((n - s) / 4, 0)
        };
        let (rws, rwe) = if next_long {
            (n / 2, n)
        } else {
            ((3 * n - s) / 4, (3 * n + s) / 4)
        };
        Ok((mode, n, lws, rws, rwe, left_slope))
    }

    /// Decode one audio packet into the internal planar buffer; returns the
    /// frames produced (read them with [`Self::channel`]). On error the
    /// decoder state is unchanged.
    pub fn decode(&mut self, packet: &[u8]) -> Result<usize> {
        self.pcm_frames = 0;
        if packet.is_empty() {
            return Ok(0);
        }
        let frames = self.decode_core(packet)?;
        self.pcm_frames = frames;
        Ok(frames)
    }

    /// The last decoded packet's samples for channel `c` (planar).
    pub fn channel(&self, c: usize) -> &[f32] {
        let m = self.max_frames;
        &self.pcm[c * m..c * m + self.pcm_frames]
    }

    /// Decode one audio packet and write its samples **interleaved** into
    /// `out` (which must hold `frames * channels`; [`Self::max_frames_per_packet`]
    /// `* channels` always suffices). Returns the frames written.
    pub fn decode_interleaved(&mut self, packet: &[u8], out: &mut [f32]) -> Result<usize> {
        if out.len() < self.max_frames_per_packet() * self.channels {
            // Cheap exact check needs the layout; only parse it when short.
            if !packet.is_empty() {
                let need = if self.has_prev {
                    self.packet_frames(packet)?
                } else {
                    0
                };
                if out.len() < need * self.channels {
                    return Err(Error::invalid("vorbis: output buffer too small"));
                }
            }
        }
        let frames = self.decode(packet)?;
        let t0 = prof::now();
        self.interleave_into(frames, out);
        prof::add(prof::INTERLEAVE, t0);
        Ok(frames)
    }

    fn interleave_into(&self, frames: usize, out: &mut [f32]) {
        let ch = self.channels;
        let half = self.max_frames;
        if frames == 0 {
            return;
        }
        let out = &mut out[..frames * ch];
        match ch {
            1 => out.copy_from_slice(&self.pcm[..frames]),
            2 => {
                let (l, r) = (&self.pcm[..frames], &self.pcm[half..half + frames]);
                for ((o, &a), &b) in out.chunks_exact_mut(2).zip(l).zip(r) {
                    o[0] = a;
                    o[1] = b;
                }
            }
            _ => {
                // Frame-major: `ch` sequential read streams, one contiguous
                // write pass.
                let pcm = &self.pcm;
                for (f, o) in out.chunks_exact_mut(ch).enumerate() {
                    for (c, d) in o.iter_mut().enumerate() {
                        *d = pcm[c * half + f];
                    }
                }
            }
        }
    }

    fn decode_core(&mut self, packet: &[u8]) -> Result<usize> {
        let mut tp = prof::now();
        let mut br = BitReader::new(packet);
        let (mode, n, lws, rws, rwe, left_slope) = self.read_packet_header(&mut br)?;
        let n2 = n / 2;
        let ch = self.channels;
        let mapping_idx = self.setup.modes[mode].mapping as usize;

        // Overlap feasibility (lewton errors when the previous right half is
        // longer than this block's left slope; checked before any state change).
        if self.has_prev && self.prev_len > self.slope[left_slope].len() {
            return Err(Error::invalid("vorbis: inconsistent block flags"));
        }

        // 1. Floors.
        {
            let setup = &self.setup;
            let mapping = &setup.mappings[mapping_idx];
            for c in 0..ch {
                let submap = mapping.mux[c] as usize;
                let fl = &setup.floors[mapping.submap_floor[submap] as usize];
                let r = match fl {
                    Floor::One(f1) => {
                        match floor::floor1_decode(
                            f1,
                            &setup.books,
                            &mut br,
                            &mut self.floor_y[c * 65..c * 65 + 65],
                        ) {
                            FloorDecode::Used => ChFloor::One,
                            FloorDecode::Unused => ChFloor::Unused,
                            FloorDecode::BadPacket => {
                                return Err(Error::invalid("vorbis: undecodable floor"))
                            }
                        }
                    }
                    Floor::Zero(f0) => {
                        match floor::floor0_decode(f0, &setup.books, &mut br, &mut self.floor0[c]) {
                            Ok(amp) => ChFloor::Zero(amp),
                            Err(FloorDecode::BadPacket) => {
                                return Err(Error::invalid("vorbis: undecodable floor"))
                            }
                            Err(_) => ChFloor::Unused,
                        }
                    }
                };
                self.ch_floor[c] = r;
                self.no_residue[c] = r == ChFloor::Unused;
            }
            // Nonzero propagation across coupled pairs.
            for &(m, a) in &mapping.coupling {
                let (m, a) = (m as usize, a as usize);
                if !(self.no_residue[m] && self.no_residue[a]) {
                    self.no_residue[m] = false;
                    self.no_residue[a] = false;
                }
            }
        }

        tp = prof::add(prof::FLOOR_DECODE, tp);
        // 2. Residues, decoded straight into the spectra.
        let spectra = &mut self.spectra[..ch * n2];
        spectra.fill(0.0);
        tp = prof::add(prof::OTHER, tp);
        {
            let setup = &self.setup;
            let mapping = &setup.mappings[mapping_idx];
            for (s, chans) in self.submap_channels[mapping_idx].iter().enumerate() {
                if chans.is_empty() {
                    continue;
                }
                let res = &setup.residues[mapping.submap_residue[s] as usize];
                self.skip.clear();
                self.skip
                    .extend(chans.iter().map(|&c| self.no_residue[c as usize]));
                let bad = if res.kind == 2 {
                    if self.skip.iter().all(|&s| s) {
                        Ok(())
                    } else {
                        let nv = chans.len();
                        let iv = &mut self.interleave[..nv * n2];
                        iv.fill(0.0);
                        let r = residue::decode_inner(
                            res,
                            &setup.books,
                            &mut br,
                            nv * n2,
                            &[false],
                            iv,
                            &mut self.classes,
                        );
                        tp = prof::add(prof::RESIDUE, tp);
                        if r.is_ok() {
                            // De-interleave into the submap's channels.
                            if nv == 1 {
                                let c = chans[0] as usize;
                                spectra[c * n2..(c + 1) * n2].copy_from_slice(iv);
                            } else if nv == 2 {
                                // `chans` is ascending, so c0 < c1: two disjoint
                                // channel slices, zipped (vectorisable).
                                let (c0, c1) = (chans[0] as usize, chans[1] as usize);
                                let (lo, hi) = spectra.split_at_mut(c1 * n2);
                                let s0 = &mut lo[c0 * n2..(c0 + 1) * n2];
                                for ((a, b), pair) in
                                    s0.iter_mut().zip(&mut hi[..n2]).zip(iv.chunks_exact(2))
                                {
                                    *a = pair[0];
                                    *b = pair[1];
                                }
                            } else {
                                // Frame-major: one contiguous read pass, `nv`
                                // sequential write streams.
                                for (k, frame) in iv.chunks_exact(nv).enumerate() {
                                    for (&c, &v) in chans.iter().zip(frame) {
                                        spectra[c as usize * n2 + k] = v;
                                    }
                                }
                            }
                        }
                        tp = prof::add(prof::DEINTERLEAVE, tp);
                        r
                    }
                } else if chans.len() == ch {
                    // All channels in this submap, in order: decode in place.
                    residue::decode_inner(
                        res,
                        &setup.books,
                        &mut br,
                        n2,
                        &self.skip,
                        spectra,
                        &mut self.classes,
                    )
                } else {
                    let nv = chans.len();
                    let iv = &mut self.interleave[..nv * n2];
                    iv.fill(0.0);
                    let r = residue::decode_inner(
                        res,
                        &setup.books,
                        &mut br,
                        n2,
                        &self.skip,
                        iv,
                        &mut self.classes,
                    );
                    for (j, &c) in chans.iter().enumerate() {
                        spectra[c as usize * n2..(c as usize + 1) * n2]
                            .copy_from_slice(&iv[j * n2..(j + 1) * n2]);
                    }
                    r
                };
                tp = prof::add(prof::RESIDUE, tp);
                if bad.is_err() {
                    return Err(Error::invalid("vorbis: undecodable residue"));
                }
            }
        }

        // 3. Inverse coupling, last step first.
        for &(m, a) in self.setup.mappings[mapping_idx].coupling.iter().rev() {
            let (m, a) = (m as usize, a as usize);
            let (mv, av) = if m < a {
                let (lo, hi) = spectra.split_at_mut(a * n2);
                (&mut lo[m * n2..(m + 1) * n2], &mut hi[..n2])
            } else {
                let (lo, hi) = spectra.split_at_mut(m * n2);
                (&mut hi[..n2], &mut lo[a * n2..(a + 1) * n2])
            };
            for (pm, pa) in mv.iter_mut().zip(av.iter_mut()) {
                let (m, a) = (*pm, *pa);
                let (nm, na) = if m > 0.0 {
                    if a > 0.0 {
                        (m, m - a)
                    } else {
                        (m + a, m)
                    }
                } else if a > 0.0 {
                    (m, m + a)
                } else {
                    (m - a, m)
                };
                *pm = nm;
                *pa = na;
            }
        }

        tp = prof::add(prof::COUPLING, tp);
        // 4. Floor curves, IMDCT, overlap-add.
        let blockflag = self.setup.modes[mode].blockflag as usize;
        let half = self.bs[1] / 2;
        let out_len = rws - lws;
        let plen = self.prev_len;
        let new_prev_len = rwe - rws;
        let mapping = &self.setup.mappings[mapping_idx];
        for c in 0..ch {
            let spec = &mut spectra[c * n2..(c + 1) * n2];
            let t = &mut self.dct_out[..n2];
            match self.ch_floor[c] {
                ChFloor::Unused => t.fill(0.0),
                fl => {
                    let floor =
                        &self.setup.floors[mapping.submap_floor[mapping.mux[c] as usize] as usize];
                    match (floor, fl) {
                        (Floor::One(f1), _) => {
                            floor::floor1_apply(f1, &self.floor_y[c * 65..c * 65 + 65], spec)
                        }
                        (Floor::Zero(f0), ChFloor::Zero(amp)) => {
                            floor::floor0_apply(
                                f0,
                                &self.floor0[c],
                                amp,
                                &f0.cos_omega[blockflag],
                                spec,
                            );
                        }
                        _ => {}
                    }
                    tp = prof::add(prof::FLOOR_SYNTH, tp);
                    self.dct[blockflag].run(spec, t, &mut self.fft_scratch);
                    tp = prof::add(prof::IMDCT, tp);
                }
            }
            // Unfold y[lws..rwe] of the IMDCT into `blk`.
            let blk = &mut self.blk[..rwe - lws];
            unfold(t, n, lws, rwe, blk);
            let prev = &mut self.prev[c * half..c * half + half];
            if self.has_prev {
                let pcm = &mut self.pcm[c * self.max_frames..c * self.max_frames + out_len];
                let sl = &self.slope[left_slope][..plen];
                for ((((o, &x), &p), &w), &wr) in pcm[..plen]
                    .iter_mut()
                    .zip(&blk[..plen])
                    .zip(&prev[..plen])
                    .zip(sl)
                    .zip(sl.iter().rev())
                {
                    *o = x * w + p * wr;
                }
                pcm[plen..].copy_from_slice(&blk[plen..out_len]);
            }
            prev[..new_prev_len].copy_from_slice(&blk[rws - lws..rwe - lws]);
            tp = prof::add(prof::OLA, tp);
        }
        let _ = tp;
        let frames = if self.has_prev { out_len } else { 0 };
        self.has_prev = true;
        self.prev_len = new_prev_len;
        Ok(frames)
    }
}

/// Write `y[a..b]` of the unscaled IMDCT of block size `n` (see `imdct`) from
/// the DCT-IV output `t` into `dst[..b-a]`.
fn unfold(t: &[f32], n: usize, a: usize, b: usize, dst: &mut [f32]) {
    let n4 = n / 4;
    let n34 = 3 * n4;
    // Region 1: [0, n/4): t[n/4 + i].
    let (s, e) = (a.min(n4), b.min(n4));
    if s < e {
        dst[s - a..e - a].copy_from_slice(&t[n4 + s..n4 + e]);
    }
    // Region 2: [n/4, 3n/4): -t[3n/4 - 1 - i].
    let (s, e) = (a.max(n4).min(n34), b.max(n4).min(n34));
    if s < e {
        for (d, &v) in dst[s - a..e - a]
            .iter_mut()
            .zip(t[n34 - e..n34 - s].iter().rev())
        {
            *d = -v;
        }
    }
    // Region 3: [3n/4, n): -t[i - 3n/4].
    let (s, e) = (a.max(n34), b.max(n34));
    if s < e {
        for (d, &v) in dst[s - a..e - a].iter_mut().zip(&t[s - n34..e - n34]) {
            *d = -v;
        }
    }
}

#[cfg(test)]
mod tests {
    use super::*;

    /// lewton's `inverse_mdct_slow` (f64 here): DCT-IV of the first n/2, unfolded.
    fn imdct_slow(coeffs: &[f32]) -> Vec<f64> {
        let n = coeffs.len() * 2;
        let l = n / 2;
        let t: Vec<f64> = (0..l)
            .map(|i| {
                (0..l)
                    .map(|j| {
                        f64::from(coeffs[j])
                            * (std::f64::consts::PI / l as f64
                                * (i as f64 + 0.5)
                                * (j as f64 + 0.5))
                                .cos()
                    })
                    .sum()
            })
            .collect();
        let (n4, n34) = (n / 4, 3 * n / 4);
        (0..n)
            .map(|i| {
                if i < n4 {
                    t[n4 + i]
                } else if i < n34 {
                    -t[n34 - 1 - i]
                } else {
                    -t[i - n34]
                }
            })
            .collect()
    }

    #[test]
    fn imdct_unfold_matches_slow_for_all_vorbis_sizes() {
        for bits in 6..=13 {
            let n = 1usize << bits;
            let coeffs: Vec<f32> = (0..n / 2)
                .map(|i| ((i * 7919 % 1009) as f32 / 504.0 - 1.0) * 0.3)
                .collect();
            let want = imdct_slow(&coeffs);
            let dct = Dct4::new(n / 2);
            let mut t = vec![0.0; n / 2];
            let mut scratch = vec![[0.0; 2]; n / 4];
            dct.run(&coeffs, &mut t, &mut scratch);
            let mut got = vec![0.0; n];
            unfold(&t, n, 0, n, &mut got);
            let peak = want.iter().fold(0.0f64, |a, &b| a.max(b.abs()));
            for (i, (&g, &w)) in got.iter().zip(&want).enumerate() {
                assert!(
                    (f64::from(g) - w).abs() <= 2e-6 * peak,
                    "n={n} i={i}: {g} vs {w}"
                );
            }
        }
    }
}
