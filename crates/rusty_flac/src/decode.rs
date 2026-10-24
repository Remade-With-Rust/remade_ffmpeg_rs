//! In-house **FLAC decoder** — streaming per-frame, planar `i32` output.
//!
//! Replaces the claxon dependency: full subset coverage (CONSTANT / VERBATIM /
//! FIXED / LPC subframes, all four stereo assignments, wasted bits, Rice and
//! Rice2 partitions with escape codes), CRC-8 header and CRC-16 frame
//! verification, and 4/8/12/16/20/24/32-bit sample sizes.

use alloc::vec;
use alloc::vec::Vec;

use crate::bitio::BitReader;
use crate::crc::{crc16, crc8};
use crate::prof::{self, Ctr, Stage};

/// Parsed STREAMINFO.
#[derive(Debug, Clone, Default)]
pub struct StreamInfo {
    pub min_block_size: u32,
    pub max_block_size: u32,
    pub min_frame_size: u32,
    pub max_frame_size: u32,
    pub sample_rate: u32,
    pub channels: u32,
    pub bits_per_sample: u32,
    pub total_samples: u64,
    pub md5: [u8; 16],
}

#[derive(Debug, Clone, PartialEq, Eq)]
pub enum DecodeError {
    /// Not a FLAC stream (missing `fLaC` marker or STREAMINFO).
    NotFlac,
    /// Stream ended mid-structure.
    Truncated,
    /// A frame header failed to parse or its CRC-8 mismatched.
    BadFrameHeader(&'static str),
    /// Frame CRC-16 mismatched.
    BadFrameCrc,
    /// A subframe used a reserved/invalid coding.
    BadSubframe(&'static str),
    /// Frame parameters disagree with STREAMINFO (channels, rate).
    Inconsistent(&'static str),
}

impl core::fmt::Display for DecodeError {
    fn fmt(&self, f: &mut core::fmt::Formatter<'_>) -> core::fmt::Result {
        match self {
            DecodeError::NotFlac => write!(f, "flac: not a FLAC stream"),
            DecodeError::Truncated => write!(f, "flac: truncated stream"),
            DecodeError::BadFrameHeader(w) => write!(f, "flac: bad frame header ({w})"),
            DecodeError::BadFrameCrc => write!(f, "flac: frame CRC mismatch"),
            DecodeError::BadSubframe(w) => write!(f, "flac: bad subframe ({w})"),
            DecodeError::Inconsistent(w) => write!(f, "flac: inconsistent stream ({w})"),
        }
    }
}

impl core::error::Error for DecodeError {}

/// Streaming FLAC decoder over a complete in-memory stream.
pub struct Decoder<'a> {
    data: &'a [u8],
    /// Byte offset of the next frame's first byte.
    pos: usize,
    info: StreamInfo,
    /// Scratch: per-channel decode buffers (channel-assignment domain).
    scratch: Vec<Vec<i32>>,
}

impl<'a> Decoder<'a> {
    /// Parse the marker + metadata blocks. Fails fast on non-FLAC input.
    pub fn new(data: &'a [u8]) -> Result<Self, DecodeError> {
        if data.len() < 4 + 4 + 34 || &data[..4] != b"fLaC" {
            return Err(DecodeError::NotFlac);
        }
        let mut pos = 4usize;
        let mut info: Option<StreamInfo> = None;
        loop {
            if pos + 4 > data.len() {
                return Err(DecodeError::Truncated);
            }
            let hdr = data[pos];
            let last = hdr & 0x80 != 0;
            let btype = hdr & 0x7F;
            let len = ((data[pos + 1] as usize) << 16)
                | ((data[pos + 2] as usize) << 8)
                | data[pos + 3] as usize;
            pos += 4;
            if pos + len > data.len() {
                return Err(DecodeError::Truncated);
            }
            if btype == 0 {
                if len < 34 {
                    return Err(DecodeError::NotFlac);
                }
                info = Some(parse_streaminfo(&data[pos..pos + 34]));
            }
            pos += len;
            if last {
                break;
            }
        }
        let info = info.ok_or(DecodeError::NotFlac)?;
        if info.channels == 0 || info.channels > 8 {
            return Err(DecodeError::Inconsistent("channels"));
        }
        let scratch = vec![Vec::new(); info.channels as usize];
        Ok(Decoder {
            data,
            pos,
            info,
            scratch,
        })
    }

    pub fn streaminfo(&self) -> &StreamInfo {
        &self.info
    }

    /// Decode the next frame into `out` (planar, one Vec per channel; each
    /// channel's samples are APPENDED). Returns the frame's block size, or
    /// `None` at end of stream.
    pub fn next_frame(&mut self, out: &mut [Vec<i32>]) -> Result<Option<usize>, DecodeError> {
        let _frame_scope = prof::scope(Stage::Frame);
        let Some((bs, ch_assign)) = self.decode_frame()? else {
            return Ok(None);
        };
        // --- stereo reconstruction into out ---
        let decorr_scope = prof::scope(Stage::Decorr);
        if let [a, b] = &mut self.scratch[..] {
            decorrelate(ch_assign, &mut a[..bs], &mut b[..bs]);
        }
        drop(decorr_scope);
        let _out_scope = prof::scope(Stage::Output);
        for (o, s) in out.iter_mut().zip(&self.scratch) {
            o.extend_from_slice(&s[..bs]);
        }
        Ok(Some(bs))
    }

    /// Decode the next frame and APPEND it to `out` interleaved (frame by
    /// frame, channels in stream order), converted to `S`:
    ///
    /// - `i32`: the native-width sample values (what [`Decoder::next_frame`]
    ///   gives, interleaved);
    /// - `f32`: normalised to [-1, 1) as `sample / 2^(bits_per_sample - 1)`
    ///   (exact for up to 24-bit streams);
    /// - `i16`: streams of at most 16 bits, scaled up to the 16-bit grid
    ///   (`sample << (16 - bits_per_sample)`); a wider stream is an
    ///   [`DecodeError::Inconsistent`] error before anything is decoded.
    ///
    /// Stereo decorrelation, interleaving and the conversion are one pass
    /// over the decoded channels, so this is the cheapest way to interleaved
    /// PCM. Returns the frame's block size, or `None` at end of stream; on an
    /// error `out` is left as it was.
    pub fn next_frame_interleaved<S: Sample>(
        &mut self,
        out: &mut Vec<S>,
    ) -> Result<Option<usize>, DecodeError> {
        let _frame_scope = prof::scope(Stage::Frame);
        let scale = S::scale(self.info.bits_per_sample)?;
        let Some((bs, ch_assign)) = self.decode_frame()? else {
            return Ok(None);
        };
        let _out_scope = prof::scope(Stage::Output);
        write_interleaved(&self.scratch, bs, ch_assign, scale, out);
        Ok(Some(bs))
    }

    /// Parse and decode the next frame into `scratch` (coded channel
    /// domain: a stereo pair is still in its coded assignment), verifying
    /// both CRCs. Returns `(block size, channel assignment)`, or `None` at
    /// end of stream. `pos` advances only past a frame that decoded cleanly.
    fn decode_frame(&mut self) -> Result<Option<(usize, u32)>, DecodeError> {
        if self.pos >= self.data.len() {
            return Ok(None);
        }
        prof::count(Ctr::Frames, 1);
        let frame_start = self.pos;
        let hdr_scope = prof::scope(Stage::Header);
        let mut br = BitReader::new(&self.data[frame_start..]);

        // --- frame header ---
        let sync = br.read_bits(14).ok_or(DecodeError::Truncated)?;
        if sync != 0x3FFE {
            return Err(DecodeError::BadFrameHeader("sync"));
        }
        if br.read_bits(1).ok_or(DecodeError::Truncated)? != 0 {
            return Err(DecodeError::BadFrameHeader("reserved"));
        }
        let _variable_blocking = br.read_bits(1).ok_or(DecodeError::Truncated)?;
        let bs_code = br.read_bits(4).ok_or(DecodeError::Truncated)?;
        let sr_code = br.read_bits(4).ok_or(DecodeError::Truncated)?;
        let ch_assign = br.read_bits(4).ok_or(DecodeError::Truncated)?;
        let ss_code = br.read_bits(3).ok_or(DecodeError::Truncated)?;
        if br.read_bits(1).ok_or(DecodeError::Truncated)? != 0 {
            return Err(DecodeError::BadFrameHeader("reserved2"));
        }
        read_utf8(&mut br).ok_or(DecodeError::BadFrameHeader("frame number"))?;

        let bs = match bs_code {
            0 => return Err(DecodeError::BadFrameHeader("blocksize code 0")),
            1 => 192usize,
            2..=5 => 576usize << (bs_code - 2),
            6 => br.read_bits(8).ok_or(DecodeError::Truncated)? as usize + 1,
            7 => br.read_bits(16).ok_or(DecodeError::Truncated)? as usize + 1,
            _ => 256usize << (bs_code - 8),
        };
        match sr_code {
            0..=11 => {} // streaminfo / table rates — frame data doesn't need it
            12 => {
                br.read_bits(8).ok_or(DecodeError::Truncated)?;
            }
            13 | 14 => {
                br.read_bits(16).ok_or(DecodeError::Truncated)?;
            }
            _ => return Err(DecodeError::BadFrameHeader("sample-rate code 15")),
        }
        let bps = match ss_code {
            0 => self.info.bits_per_sample,
            1 => 8,
            2 => 12,
            4 => 16,
            5 => 20,
            6 => 24,
            7 => 32,
            _ => return Err(DecodeError::BadFrameHeader("sample-size code")),
        };

        // CRC-8 covers the header bytes up to (not including) the CRC byte.
        debug_assert!(br.is_byte_aligned());
        let hdr_len = br.byte_pos();
        let hcrc = br.read_bits(8).ok_or(DecodeError::Truncated)? as u8;
        if crc8(&self.data[frame_start..frame_start + hdr_len]) != hcrc {
            return Err(DecodeError::BadFrameHeader("crc8"));
        }

        // --- channels for this assignment ---
        let (nch, side_ch): (usize, [bool; 2]) = match ch_assign {
            0..=7 => ((ch_assign + 1) as usize, [false, false]),
            8 => (2, [false, true]),  // left/side
            9 => (2, [true, false]),  // side/right
            10 => (2, [false, true]), // mid/side
            _ => return Err(DecodeError::BadFrameHeader("channel assignment")),
        };
        if nch != self.info.channels as usize {
            return Err(DecodeError::Inconsistent("frame channels"));
        }
        drop(hdr_scope);
        prof::count(
            match ch_assign {
                8 => Ctr::LeftSide,
                9 => Ctr::SideRight,
                10 => Ctr::MidSide,
                _ => Ctr::Independent,
            },
            1,
        );

        // --- subframes ---
        for (c, buf) in self.scratch.iter_mut().take(nch).enumerate() {
            let is_side = c < 2 && side_ch[c];
            let sub_bps = bps + u32::from(is_side);
            // Size without re-zeroing what will be overwritten: shrink is
            // O(1), and growth only zero-fills the newly exposed tail once
            // per size change (block sizes are constant within a stream).
            if buf.len() != bs {
                buf.clear();
                buf.resize(bs, 0);
            }
            decode_subframe(&mut br, bs, sub_bps, buf)?;
        }

        // --- footer ---
        let crc_scope = prof::scope(Stage::Crc16);
        br.align_to_byte();
        let frame_len = br.byte_pos();
        let fcrc = br.read_bits(16).ok_or(DecodeError::Truncated)? as u16;
        prof::count(Ctr::CrcBytes, frame_len as u64);
        if crc16(&self.data[frame_start..frame_start + frame_len]) != fcrc {
            return Err(DecodeError::BadFrameCrc);
        }
        self.pos = frame_start + frame_len + 2;
        drop(crc_scope);
        Ok(Some((bs, ch_assign)))
    }
}

mod sealed {
    pub trait Sealed {}
    impl Sealed for i32 {}
    impl Sealed for f32 {}
    impl Sealed for i16 {}
}

/// Per-stream conversion parameters for a [`Sample`] type.
#[doc(hidden)]
#[derive(Clone, Copy, Debug)]
pub struct Scale {
    /// `1 / 2^(bps - 1)` (f32 output).
    inv: f32,
    /// `16 - bps` (i16 output).
    up: u32,
}

/// Output sample types for [`Decoder::next_frame_interleaved`]: `i32`
/// (native width), `f32` (normalised) and `i16` (streams of at most 16
/// bits). Sealed: the conversions belong to this crate.
pub trait Sample: Copy + Default + sealed::Sealed {
    #[doc(hidden)]
    fn scale(bits_per_sample: u32) -> Result<Scale, DecodeError>;
    #[doc(hidden)]
    fn from_native(v: i32, s: Scale) -> Self;
}

fn base_scale(bps: u32) -> Scale {
    Scale {
        inv: 1.0 / (1u64 << (bps.clamp(1, 32) - 1)) as f32,
        up: 16u32.saturating_sub(bps),
    }
}

impl Sample for i32 {
    fn scale(bps: u32) -> Result<Scale, DecodeError> {
        Ok(base_scale(bps))
    }
    #[inline(always)]
    fn from_native(v: i32, _: Scale) -> i32 {
        v
    }
}

impl Sample for f32 {
    fn scale(bps: u32) -> Result<Scale, DecodeError> {
        Ok(base_scale(bps))
    }
    #[inline(always)]
    fn from_native(v: i32, s: Scale) -> f32 {
        v as f32 * s.inv
    }
}

impl Sample for i16 {
    fn scale(bps: u32) -> Result<Scale, DecodeError> {
        if bps > 16 {
            return Err(DecodeError::Inconsistent(
                "i16 output needs a stream of at most 16 bits per sample",
            ));
        }
        Ok(base_scale(bps))
    }
    #[inline(always)]
    fn from_native(v: i32, s: Scale) -> i16 {
        (v << s.up) as i16
    }
}

/// Decorrelate (stereo), interleave and convert one decoded block from the
/// coded-channel scratch onto the end of `out`, in one pass.
fn write_interleaved<S: Sample>(
    chans: &[Vec<i32>],
    bs: usize,
    ch_assign: u32,
    scale: Scale,
    out: &mut Vec<S>,
) {
    let nch = chans.len();
    let start = out.len();
    // The fill is the block's size (cache-resident when the caller reuses
    // `out` per frame); every slot is then overwritten.
    out.resize(start + bs * nch, S::default());
    let dst = &mut out[start..];
    let cv = |v: i32| S::from_native(v, scale);
    match chans {
        [m] => {
            for (d, &v) in dst.iter_mut().zip(&m[..bs]) {
                *d = cv(v);
            }
        }
        [a, b] => {
            let pairs = dst.chunks_exact_mut(2).zip(a[..bs].iter().zip(&b[..bs]));
            match ch_assign {
                8 => {
                    // left/side: R = L - side
                    for (d, (&l, &s)) in pairs {
                        d[0] = cv(l);
                        d[1] = cv(l.wrapping_sub(s));
                    }
                }
                9 => {
                    // side/right: L = side + R
                    for (d, (&s, &r)) in pairs {
                        d[0] = cv(s.wrapping_add(r));
                        d[1] = cv(r);
                    }
                }
                10 => {
                    // mid/side (the exact i32 form: see `decorrelate`)
                    for (d, (&m, &s)) in pairs {
                        let r = m.wrapping_sub(s >> 1);
                        d[0] = cv(r.wrapping_add(s));
                        d[1] = cv(r);
                    }
                }
                _ => {
                    for (d, (&l, &r)) in pairs {
                        d[0] = cv(l);
                        d[1] = cv(r);
                    }
                }
            }
        }
        _ => {
            for (c, ch) in chans.iter().enumerate() {
                for (d, &v) in dst[c..].iter_mut().step_by(nch).zip(&ch[..bs]) {
                    *d = cv(v);
                }
            }
        }
    }
}

/// Undo a stereo channel assignment in place: `a`/`b` hold the coded
/// channel pair (left/side, side/right, mid/side) and come out as
/// left/right. Independent channels (`ch_assign` 0..=7) are untouched.
///
/// Mid/side in `i32`: with `mid = 2m + (s & 1)`, exactly
/// `(mid + s) >> 1 == m + (s >> 1) + (s & 1)` and
/// `(mid - s) >> 1 == m - (s >> 1)`, so `R = m - (s >> 1)` and `L = R + s`,
/// wrapping, equal the `i64` textbook form truncated to 32 bits for every
/// input (`decorrelate_ref` in the tests). That keeps the lanes 32-bit, so
/// the loops vectorize 4-wide.
fn decorrelate(ch_assign: u32, a: &mut [i32], b: &mut [i32]) {
    match ch_assign {
        8 => {
            // left/side: R = L - side
            for (l, s) in a.iter().zip(b.iter_mut()) {
                *s = l.wrapping_sub(*s);
            }
        }
        9 => {
            // side/right: L = side + R
            for (s, r) in a.iter_mut().zip(b.iter()) {
                *s = s.wrapping_add(*r);
            }
        }
        10 => {
            // mid/side
            for (m, s) in a.iter_mut().zip(b.iter_mut()) {
                let r = m.wrapping_sub(*s >> 1);
                *m = r.wrapping_add(*s);
                *s = r;
            }
        }
        _ => {}
    }
}

fn parse_streaminfo(b: &[u8]) -> StreamInfo {
    let mut br = BitReader::new(b);
    let min_block_size = br.read_bits(16).unwrap();
    let max_block_size = br.read_bits(16).unwrap();
    let min_frame_size = br.read_bits(24).unwrap();
    let max_frame_size = br.read_bits(24).unwrap();
    let sample_rate = br.read_bits(20).unwrap();
    let channels = br.read_bits(3).unwrap() + 1;
    let bits_per_sample = br.read_bits(5).unwrap() + 1;
    let hi = br.read_bits(4).unwrap() as u64;
    let lo = br.read_bits(32).unwrap() as u64;
    let total_samples = (hi << 32) | lo;
    let mut md5 = [0u8; 16];
    for m in md5.iter_mut() {
        *m = br.read_bits(8).unwrap() as u8;
    }
    StreamInfo {
        min_block_size,
        max_block_size,
        min_frame_size,
        max_frame_size,
        sample_rate,
        channels,
        bits_per_sample,
        total_samples,
        md5,
    }
}

/// FLAC's UTF-8-style frame/sample number (up to 7 bytes / 36 bits).
fn read_utf8(br: &mut BitReader) -> Option<u64> {
    let lead = br.read_bits(8)?;
    if lead & 0x80 == 0 {
        return Some(lead as u64);
    }
    let nconts = (lead as u8).leading_ones() - 1;
    if nconts == 0 || nconts > 6 {
        return None;
    }
    let mut val = (lead as u64) & (0x7F >> nconts);
    for _ in 0..nconts {
        let c = br.read_bits(8)?;
        if c & 0xC0 != 0x80 {
            return None;
        }
        val = (val << 6) | (c as u64 & 0x3F);
    }
    Some(val)
}

/// Decode one subframe into `out` (pre-sized to the block length; every slot
/// is written except explicit zero-runs, which are filled here).
fn decode_subframe(
    br: &mut BitReader,
    bs: usize,
    bps: u32,
    out: &mut [i32],
) -> Result<(), DecodeError> {
    debug_assert_eq!(out.len(), bs);
    prof::count(Ctr::Subframes, 1);
    let hdr_scope = prof::scope(Stage::SubHeader);
    if br.read_bits(1).ok_or(DecodeError::Truncated)? != 0 {
        return Err(DecodeError::BadSubframe("padding bit"));
    }
    let ty = br.read_bits(6).ok_or(DecodeError::Truncated)?;
    let wasted = if br.read_bits(1).ok_or(DecodeError::Truncated)? == 1 {
        br.read_unary().ok_or(DecodeError::Truncated)? + 1
    } else {
        0
    };
    if wasted >= bps {
        return Err(DecodeError::BadSubframe("wasted bits >= bps"));
    }
    let ebps = bps - wasted; // effective coded bit depth

    match ty {
        0b000000 => {
            drop(hdr_scope);
            let _s = prof::scope(Stage::Misc);
            prof::count(Ctr::Constant, 1);
            let v = br.read_signed(ebps).ok_or(DecodeError::Truncated)?;
            out.fill(v);
        }
        0b000001 => {
            drop(hdr_scope);
            let _s = prof::scope(Stage::Misc);
            prof::count(Ctr::Verbatim, 1);
            for slot in out.iter_mut() {
                *slot = br.read_signed(ebps).ok_or(DecodeError::Truncated)?;
            }
        }
        0b001000..=0b001100 => {
            let order = (ty & 0x07) as usize;
            if order > bs {
                return Err(DecodeError::BadSubframe("fixed order > blocksize"));
            }
            for slot in out[..order].iter_mut() {
                *slot = br.read_signed(ebps).ok_or(DecodeError::Truncated)?;
            }
            drop(hdr_scope);
            prof::count(Ctr::FixedSub, 1);
            decode_residual(br, bs, order, out)?;
            let _s = prof::scope(Stage::Fixed);
            restore_fixed(out, order);
        }
        0b100000..=0b111111 => {
            let order = ((ty & 0x1F) + 1) as usize;
            if order > bs {
                return Err(DecodeError::BadSubframe("lpc order > blocksize"));
            }
            for slot in out[..order].iter_mut() {
                *slot = br.read_signed(ebps).ok_or(DecodeError::Truncated)?;
            }
            let prec = br.read_bits(4).ok_or(DecodeError::Truncated)? + 1;
            if prec > 15 {
                return Err(DecodeError::BadSubframe("qlp precision 16"));
            }
            let shift = br.read_signed(5).ok_or(DecodeError::Truncated)?;
            if shift < 0 {
                return Err(DecodeError::BadSubframe("negative shift"));
            }
            let mut qlp = [0i32; 32];
            for q in qlp.iter_mut().take(order) {
                *q = br.read_signed(prec).ok_or(DecodeError::Truncated)?;
            }
            drop(hdr_scope);
            prof::count(Ctr::LpcSub, 1);
            prof::count(Ctr::LpcOrderSum, order as u64);
            prof::count(Ctr::LpcHighOrder, u64::from(order > 12));
            // Accumulator bound: |sum| < 2^(ebps-1) * 2^(prec-1) * order.
            let acc_bits = ebps + prec + (usize::BITS - (order - 1).leading_zeros());
            prof::count(
                if acc_bits <= 32 {
                    Ctr::LpcSamples32
                } else {
                    Ctr::LpcSamples64
                },
                (bs - order) as u64,
            );
            decode_residual(br, bs, order, out)?;
            let _s = prof::scope(Stage::Lpc);
            restore_lpc(out, &qlp[..order], shift as u32);
        }
        _ => return Err(DecodeError::BadSubframe("reserved type")),
    }

    if wasted > 0 {
        let _s = prof::scope(Stage::Misc);
        for v in out.iter_mut() {
            *v <<= wasted;
        }
    }
    Ok(())
}

/// Partitioned Rice residual: appends `bs - order` residuals to `out`
/// (which already holds the warm-up samples).
/// Partitioned Rice residual: fills `out[order..]` (warm-ups already in
/// `out[..order]`).
fn decode_residual(
    br: &mut BitReader,
    bs: usize,
    order: usize,
    out: &mut [i32],
) -> Result<(), DecodeError> {
    let _s = prof::scope(Stage::Residual);
    prof::count(Ctr::Residuals, (bs - order) as u64);
    let method = br.read_bits(2).ok_or(DecodeError::Truncated)?;
    let (pbits, escape) = match method {
        0 => (4u32, 15u32),
        1 => (5u32, 31u32),
        _ => return Err(DecodeError::BadSubframe("residual method")),
    };
    let po = br.read_bits(4).ok_or(DecodeError::Truncated)?;
    let n_part = 1usize << po;
    let psize = bs >> po;
    // Partition 0 is short by the warm-up samples, so it must not go negative,
    // and the block must split evenly.
    if bs % n_part != 0 || psize < order {
        return Err(DecodeError::BadSubframe("partition geometry"));
    }
    let mut w = order;
    for part in 0..n_part {
        let cnt = if part == 0 { psize - order } else { psize };
        let param = br.read_bits(pbits).ok_or(DecodeError::Truncated)?;
        prof::count(Ctr::Partitions, 1);
        if param == escape {
            prof::count(Ctr::EscapePartitions, 1);
            let raw = br.read_bits(5).ok_or(DecodeError::Truncated)?;
            if raw > 0 {
                for slot in &mut out[w..w + cnt] {
                    *slot = br.read_signed(raw).ok_or(DecodeError::Truncated)?;
                }
            } else {
                out[w..w + cnt].fill(0);
            }
            w += cnt;
        } else {
            br.read_rice_signed_run(param, &mut out[w..w + cnt])
                .ok_or(DecodeError::Truncated)?;
            w += cnt;
        }
    }
    debug_assert_eq!(w, bs);
    Ok(())
}

/// Invert the fixed polynomial predictors in place (samples after warm-up are
/// residuals on entry, samples on exit).
///
/// The predictor state (the last `order` samples) rides in registers, so a
/// sample's only dependency on the one before it is a scaled add — not a
/// store-to-load round trip through `buf`. Wrapping `i32` arithmetic is
/// exact mod 2^32 in any order, so this equals the textbook form bit for
/// bit (`restore_fixed_ref` in the tests).
fn restore_fixed(buf: &mut [i32], order: usize) {
    let (warm, rest) = buf.split_at_mut(order);
    match order {
        0 => {}
        1 => {
            let mut p1 = warm[0];
            for x in rest {
                p1 = x.wrapping_add(p1);
                *x = p1;
            }
        }
        2 => {
            let (mut p2, mut p1) = (warm[0], warm[1]);
            for x in rest {
                let v = x.wrapping_add(p1.wrapping_mul(2)).wrapping_sub(p2);
                *x = v;
                (p2, p1) = (p1, v);
            }
        }
        3 => {
            let (mut p3, mut p2, mut p1) = (warm[0], warm[1], warm[2]);
            for x in rest {
                let v = x
                    .wrapping_add(p3)
                    .wrapping_sub(p2.wrapping_mul(3))
                    .wrapping_add(p1.wrapping_mul(3));
                *x = v;
                (p3, p2, p1) = (p2, p1, v);
            }
        }
        4 => {
            let (mut p4, mut p3, mut p2, mut p1) = (warm[0], warm[1], warm[2], warm[3]);
            for x in rest {
                let v = x
                    .wrapping_sub(p4)
                    .wrapping_add(p3.wrapping_mul(4))
                    .wrapping_sub(p2.wrapping_mul(6))
                    .wrapping_add(p1.wrapping_mul(4));
                *x = v;
                (p4, p3, p2, p1) = (p3, p2, p1, v);
            }
        }
        _ => unreachable!(),
    }
}

/// Invert the LPC predictor in place. The recurrence is inherently serial,
/// but const-order specializations let the compiler keep the whole predictor
/// state in registers for the common orders.
fn restore_lpc(buf: &mut [i32], qlp: &[i32], shift: u32) {
    #[inline(always)]
    fn run<const ORDER: usize>(buf: &mut [i32], qlp: &[i32], shift: u32) {
        let mut coeffs = [0i32; 32];
        coeffs[..ORDER].copy_from_slice(&qlp[..ORDER]);
        for i in ORDER..buf.len() {
            let mut sum = 0i64;
            for j in 0..ORDER {
                sum += coeffs[j] as i64 * buf[i - 1 - j] as i64;
            }
            buf[i] = buf[i].wrapping_add((sum >> shift) as i32);
        }
    }
    match qlp.len() {
        1 => run::<1>(buf, qlp, shift),
        2 => run::<2>(buf, qlp, shift),
        3 => run::<3>(buf, qlp, shift),
        4 => run::<4>(buf, qlp, shift),
        5 => run::<5>(buf, qlp, shift),
        6 => run::<6>(buf, qlp, shift),
        7 => run::<7>(buf, qlp, shift),
        8 => run::<8>(buf, qlp, shift),
        9 => run::<9>(buf, qlp, shift),
        10 => run::<10>(buf, qlp, shift),
        11 => run::<11>(buf, qlp, shift),
        12 => run::<12>(buf, qlp, shift),
        order => {
            for i in order..buf.len() {
                let mut sum = 0i64;
                for (j, &c) in qlp.iter().enumerate() {
                    sum += c as i64 * buf[i - 1 - j] as i64;
                }
                buf[i] = buf[i].wrapping_add((sum >> shift) as i32);
            }
        }
    }
}

/// Decode a complete FLAC stream to `(StreamInfo, planar samples)`.
pub fn decode(data: &[u8]) -> Result<(StreamInfo, Vec<Vec<i32>>), DecodeError> {
    let mut dec = Decoder::new(data)?;
    let info = dec.streaminfo().clone();
    let mut out: Vec<Vec<i32>> = (0..info.channels)
        .map(|_| Vec::with_capacity(info.total_samples as usize))
        .collect();
    while dec.next_frame(&mut out)?.is_some() {}
    Ok((info, out))
}

#[cfg(test)]
mod tests {
    use super::*;
    use crate::encode::Encoder;

    fn test_signal(n: usize) -> (Vec<i32>, Vec<i32>) {
        let mut x = 7u64;
        let l: Vec<i32> = (0..n)
            .map(|i| {
                x = x
                    .wrapping_mul(6364136223846793005)
                    .wrapping_add(1442695040888963407);
                ((i as f64 * 0.11).sin() * 9000.0) as i32 + ((x >> 40) & 0x3F) as i32 - 32
            })
            .collect();
        let r: Vec<i32> = l.iter().map(|&v| v - (v >> 3)).collect();
        (l, r)
    }

    #[test]
    fn own_encode_decodes_exactly() {
        let (l, r) = test_signal(30_000);
        let mut enc = Encoder::new(44100, 2, 16).unwrap();
        enc.push_planar(&[&l, &r]).unwrap();
        let stream = enc.finish();

        let (info, chans) = decode(&stream).unwrap();
        assert_eq!(info.sample_rate, 44100);
        assert_eq!(info.channels, 2);
        assert_eq!(info.bits_per_sample, 16);
        assert_eq!(info.total_samples, 30_000);
        assert_eq!(chans[0], l);
        assert_eq!(chans[1], r);
    }

    #[test]
    fn matches_claxon_on_own_stream() {
        let (l, r) = test_signal(20_000);
        let mut enc = Encoder::new(48000, 2, 16).unwrap();
        enc.push_planar(&[&l, &r]).unwrap();
        let stream = enc.finish();

        let (_, ours) = decode(&stream).unwrap();

        let mut reader = claxon::FlacReader::new(std::io::Cursor::new(&stream)).unwrap();
        let ch = reader.streaminfo().channels as usize;
        let mut theirs = vec![Vec::new(); ch];
        let mut c = 0;
        for s in reader.samples() {
            theirs[c].push(s.unwrap());
            c = (c + 1) % ch;
        }
        assert_eq!(ours, theirs);
    }

    /// The original fixed restore (every sample reloaded from `buf`): the
    /// oracle for the register-carried loops.
    fn restore_fixed_ref(buf: &mut [i32], order: usize) {
        for i in order..buf.len() {
            let p = |k: usize| buf[i - k];
            let pred = match order {
                0 => 0,
                1 => p(1),
                2 => p(1).wrapping_mul(2).wrapping_sub(p(2)),
                3 => p(1)
                    .wrapping_mul(3)
                    .wrapping_sub(p(2).wrapping_mul(3))
                    .wrapping_add(p(3)),
                _ => p(1)
                    .wrapping_mul(4)
                    .wrapping_sub(p(2).wrapping_mul(6))
                    .wrapping_add(p(3).wrapping_mul(4))
                    .wrapping_sub(p(4)),
            };
            buf[i] = buf[i].wrapping_add(pred);
        }
    }

    /// Orders 0..=4, lengths from exactly `order` up, small and wrapping
    /// (full-range) values: the register-carried restore matches the oracle.
    #[test]
    fn restore_fixed_matches_reference() {
        let mut x = 0xDEAD_BEEF_0BAD_F00Du64;
        let mut rnd = move || {
            x = x
                .wrapping_mul(6364136223846793005)
                .wrapping_add(1442695040888963407);
            (x >> 32) as u32
        };
        for order in 0..=4usize {
            for trial in 0..40 {
                let n = order + (rnd() % 200) as usize;
                let buf: Vec<i32> = (0..n)
                    .map(|_| {
                        if trial % 3 == 0 {
                            rnd() as i32
                        } else {
                            rnd() as i32 >> 16
                        }
                    })
                    .collect();
                let (mut a, mut b) = (buf.clone(), buf);
                restore_fixed(&mut a, order);
                restore_fixed_ref(&mut b, order);
                assert_eq!(a, b, "order={order} trial={trial}");
            }
        }
    }

    /// The original stereo reconstruction (mid/side through `i64`): the
    /// oracle for the 32-bit `decorrelate`.
    fn decorrelate_ref(ch_assign: u32, a: &mut [i32], b: &mut [i32]) {
        for i in 0..a.len() {
            match ch_assign {
                8 => b[i] = a[i].wrapping_sub(b[i]),
                9 => a[i] = a[i].wrapping_add(b[i]),
                10 => {
                    let mid = ((a[i] as i64) << 1) | ((b[i] as i64) & 1);
                    let side = b[i] as i64;
                    a[i] = ((mid + side) >> 1) as i32;
                    b[i] = ((mid - side) >> 1) as i32;
                }
                _ => {}
            }
        }
    }

    /// Every assignment over random, extreme (i32::MIN / MAX / -1 / 0) and
    /// small values: identical to the `i64` oracle.
    #[test]
    fn decorrelate_matches_reference() {
        let mut x = 0x5555_AAAA_1234_5678u64;
        let mut rnd = move || {
            x = x
                .wrapping_mul(6364136223846793005)
                .wrapping_add(1442695040888963407);
            (x >> 32) as u32
        };
        let edge = [
            i32::MIN,
            i32::MIN + 1,
            -2,
            -1,
            0,
            1,
            2,
            i32::MAX - 1,
            i32::MAX,
        ];
        for ch_assign in [0u32, 1, 8, 9, 10] {
            let mut a: Vec<i32> = Vec::new();
            let mut b: Vec<i32> = Vec::new();
            for &p in &edge {
                for &q in &edge {
                    a.push(p);
                    b.push(q);
                }
            }
            for i in 0..5000 {
                let (p, q) = (rnd() as i32, rnd() as i32);
                let sh = if i % 2 == 0 { 0 } else { 15 };
                a.push(p >> sh);
                b.push(q >> sh);
            }
            let (mut a1, mut b1) = (a.clone(), b.clone());
            decorrelate(ch_assign, &mut a1, &mut b1);
            decorrelate_ref(ch_assign, &mut a, &mut b);
            assert_eq!((a1, b1), (a, b), "ch_assign={ch_assign}");
        }
    }

    /// `next_frame_interleaved` (i32 / f32 / i16) equals the planar
    /// `next_frame` output interleaved and converted by hand, frame by frame,
    /// for mono, stereo (the encoder picks every assignment over this
    /// signal) and 6 channels, 16- and 8-bit; i16 refuses a 24-bit stream.
    #[test]
    fn interleaved_matches_planar() {
        let (l, r) = test_signal(40_000);
        let streams: Vec<(Vec<u8>, u32)> = [1usize, 2, 6]
            .iter()
            .flat_map(|&ch| [16u32, 8].into_iter().map(move |bps| (ch, bps)))
            .map(|(ch, bps)| {
                let shift = 16 - bps;
                let chans: Vec<Vec<i32>> = (0..ch)
                    .map(|c| {
                        let src = if c % 2 == 0 { &l } else { &r };
                        src.iter().map(|&v| (v >> shift) + c as i32).collect()
                    })
                    .collect();
                let refs: Vec<&[i32]> = chans.iter().map(|c| c.as_slice()).collect();
                let mut enc = Encoder::new(44100, ch as u32, bps).unwrap();
                enc.push_planar(&refs).unwrap();
                (enc.finish(), bps)
            })
            .collect();
        for (stream, bps) in &streams {
            let mut planar = Decoder::new(stream).unwrap();
            let mut di = Decoder::new(stream).unwrap();
            let mut df = Decoder::new(stream).unwrap();
            let mut ds = Decoder::new(stream).unwrap();
            let nch = planar.streaminfo().channels as usize;
            let inv = 1.0 / (1u64 << (bps - 1)) as f32;
            let (mut oi, mut of, mut os) = (Vec::new(), Vec::new(), Vec::new());
            loop {
                let mut chans = vec![Vec::new(); nch];
                let n = planar.next_frame(&mut chans).unwrap();
                oi.clear();
                of.clear();
                os.clear();
                assert_eq!(di.next_frame_interleaved::<i32>(&mut oi).unwrap(), n);
                assert_eq!(df.next_frame_interleaved::<f32>(&mut of).unwrap(), n);
                assert_eq!(ds.next_frame_interleaved::<i16>(&mut os).unwrap(), n);
                let Some(n) = n else { break };
                for i in 0..n {
                    for c in 0..nch {
                        let v = chans[c][i];
                        assert_eq!(oi[i * nch + c], v);
                        assert_eq!(of[i * nch + c].to_bits(), (v as f32 * inv).to_bits());
                        assert_eq!(os[i * nch + c], (v << (16 - bps)) as i16);
                    }
                }
            }
        }
        // i16 refuses wider streams without consuming a frame.
        let mut enc = Encoder::new(48000, 1, 24).unwrap();
        enc.push_planar(&[&l[..1000]]).unwrap();
        let stream = enc.finish();
        let mut d = Decoder::new(&stream).unwrap();
        let mut o16: Vec<i16> = Vec::new();
        assert!(d.next_frame_interleaved::<i16>(&mut o16).is_err());
        let mut o32: Vec<i32> = Vec::new();
        assert_eq!(
            d.next_frame_interleaved::<i32>(&mut o32).unwrap(),
            Some(1000)
        );
        assert_eq!(o32, l[..1000]);
    }

    #[test]
    fn rejects_garbage() {
        assert!(decode(b"not a flac stream at all").is_err());
        assert!(decode(b"fLaC").is_err());
    }
}
