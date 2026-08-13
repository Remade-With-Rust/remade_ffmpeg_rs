//! In-house **FLAC decoder** — streaming per-frame, planar `i32` output.
//!
//! Replaces the claxon dependency: full subset coverage (CONSTANT / VERBATIM /
//! FIXED / LPC subframes, all four stereo assignments, wasted bits, Rice and
//! Rice2 partitions with escape codes), CRC-8 header and CRC-16 frame
//! verification, and 4/8/12/16/20/24/32-bit sample sizes.

use crate::bitio::BitReader;
use crate::crc::{crc16, crc8};

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

impl std::fmt::Display for DecodeError {
    fn fmt(&self, f: &mut std::fmt::Formatter<'_>) -> std::fmt::Result {
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

impl std::error::Error for DecodeError {}

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
        if self.pos >= self.data.len() {
            return Ok(None);
        }
        let frame_start = self.pos;
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
        br.align_to_byte();
        let frame_len = br.byte_pos();
        let fcrc = br.read_bits(16).ok_or(DecodeError::Truncated)? as u16;
        if crc16(&self.data[frame_start..frame_start + frame_len]) != fcrc {
            return Err(DecodeError::BadFrameCrc);
        }
        self.pos = frame_start + frame_len + 2;

        // --- stereo reconstruction into out ---
        match ch_assign {
            8 => {
                // left/side: R = L - side
                let (l, s) = self.scratch.split_at_mut(1);
                for i in 0..bs {
                    let lv = l[0][i];
                    let sv = s[0][i];
                    s[0][i] = lv.wrapping_sub(sv);
                }
            }
            9 => {
                // side/right: L = side + R
                let (s, r) = self.scratch.split_at_mut(1);
                for i in 0..bs {
                    s[0][i] = s[0][i].wrapping_add(r[0][i]);
                }
            }
            10 => {
                // mid/side: mid = (L+R)>>1 was stored; reconstruct exactly.
                let (m, s) = self.scratch.split_at_mut(1);
                for i in 0..bs {
                    let mid = ((m[0][i] as i64) << 1) | ((s[0][i] as i64) & 1);
                    let side = s[0][i] as i64;
                    m[0][i] = ((mid + side) >> 1) as i32;
                    s[0][i] = ((mid - side) >> 1) as i32;
                }
            }
            _ => {}
        }
        for (o, s) in out.iter_mut().zip(&self.scratch) {
            o.extend_from_slice(s);
        }
        Ok(Some(bs))
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
            let v = br.read_signed(ebps).ok_or(DecodeError::Truncated)?;
            out.fill(v);
        }
        0b000001 => {
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
            decode_residual(br, bs, order, out)?;
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
            decode_residual(br, bs, order, out)?;
            restore_lpc(out, &qlp[..order], shift as u32);
        }
        _ => return Err(DecodeError::BadSubframe("reserved type")),
    }

    if wasted > 0 {
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
        if param == escape {
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
            for slot in &mut out[w..w + cnt] {
                let u = br.read_rice(param).ok_or(DecodeError::Truncated)?;
                *slot = ((u >> 1) as i32) ^ -((u & 1) as i32); // un-zigzag
            }
            w += cnt;
        }
    }
    debug_assert_eq!(w, bs);
    Ok(())
}

/// Invert the fixed polynomial predictors in place (samples after warm-up are
/// residuals on entry, samples on exit).
fn restore_fixed(buf: &mut [i32], order: usize) {
    match order {
        0 => {}
        1 => {
            for i in 1..buf.len() {
                buf[i] = buf[i].wrapping_add(buf[i - 1]);
            }
        }
        2 => {
            for i in 2..buf.len() {
                buf[i] = buf[i]
                    .wrapping_add(buf[i - 1].wrapping_mul(2))
                    .wrapping_sub(buf[i - 2]);
            }
        }
        3 => {
            for i in 3..buf.len() {
                buf[i] = buf[i]
                    .wrapping_add(buf[i - 1].wrapping_mul(3))
                    .wrapping_sub(buf[i - 2].wrapping_mul(3))
                    .wrapping_add(buf[i - 3]);
            }
        }
        4 => {
            for i in 4..buf.len() {
                buf[i] = buf[i]
                    .wrapping_add(buf[i - 1].wrapping_mul(4))
                    .wrapping_sub(buf[i - 2].wrapping_mul(6))
                    .wrapping_add(buf[i - 3].wrapping_mul(4))
                    .wrapping_sub(buf[i - 4]);
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

    #[test]
    fn rejects_garbage() {
        assert!(decode(b"not a flac stream at all").is_err());
        assert!(decode(b"fLaC").is_err());
    }
}
