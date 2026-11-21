//! TIFF LZW, both the TIFF 6 form (MSB-first, "early change") and the
//! pre-TIFF-6 form libtiff calls `LZW_COMPAT` (LSB-first, no early change).
//!
//! The decoder writes into a fixed-size output and stops when it is full, so
//! its memory and time are bounded by the expected chunk size whatever the
//! input holds.

use crate::error::{TiffError, TiffResult};

const CLEAR: u16 = 256;
const EOI: u16 = 257;
const FIRST: u16 = 258;
const MAX_CODES: usize = 4096;

struct BitReader<'a> {
    data: &'a [u8],
    pos: usize,
    acc: u64,
    nbits: u32,
    msb: bool,
}

impl BitReader<'_> {
    /// Next `width`-bit code (9..=12), or `None` at end of data.
    ///
    /// The accumulator holds `nbits` unread bits (MSB-first: the low
    /// `nbits` bits; LSB-first: the low bits, consumed from the bottom).
    /// It is refilled 32 bits at a time while four bytes remain, so a code
    /// costs one test and one shift instead of a loop over bytes.
    #[inline]
    fn read(&mut self, width: u32) -> Option<u16> {
        if self.nbits < width {
            if let Some(w) = self.data.get(self.pos..self.pos + 4) {
                let w: [u8; 4] = w.try_into().unwrap();
                if self.msb {
                    self.acc = (self.acc << 32) | u64::from(u32::from_be_bytes(w));
                } else {
                    self.acc |= u64::from(u32::from_le_bytes(w)) << self.nbits;
                }
                self.pos += 4;
                self.nbits += 32;
            } else {
                while self.nbits < width {
                    let b = *self.data.get(self.pos)?;
                    self.pos += 1;
                    if self.msb {
                        self.acc = (self.acc << 8) | u64::from(b);
                    } else {
                        self.acc |= u64::from(b) << self.nbits;
                    }
                    self.nbits += 8;
                }
            }
        }
        let mask = (1u64 << width) - 1;
        self.nbits -= width;
        let code = if self.msb {
            // Bits above `nbits + width` are already consumed: masking the
            // code is enough, the accumulator never needs clearing (it holds
            // at most 31 + 32 bits, so the shift never overflows).
            (self.acc >> self.nbits) & mask
        } else {
            let c = self.acc & mask;
            self.acc >>= width;
            c
        };
        Some(code as u16)
    }
}

/// Decode a TIFF LZW strip/tile into `out`, which must be filled exactly.
#[cfg(test)]
pub(crate) fn decode(data: &[u8], out: &mut [u8]) -> TiffResult<()> {
    let mut v = Vec::new();
    decode_append(data, &mut v, out.len())?;
    out.copy_from_slice(&v);
    Ok(())
}

/// Decode a TIFF LZW strip/tile, appending exactly `n` bytes to `out`.
///
/// Every code's string is, by construction, an earlier stretch of this same
/// output: code `k` added after emitting `prev` is `string(prev)` plus the
/// next string's first byte, which is exactly the `len(prev) + 1` bytes
/// starting where `prev` was emitted. So instead of a prefix/suffix table
/// walked back to front one byte at a time, each code records where it was
/// first emitted and how long it is, and emitting it is a copy from there.
///
/// The output is sized once (one zero-fill, about one instruction a byte)
/// and written by index: a `Vec::push` per byte, with its capacity check and
/// length update, measured several times that.
///
/// Old-style (LSB-first) data is detected the way libtiff does it: a first
/// byte of 0 with the low bit of the second byte set.
///
/// A strip that ends early (its data runs out, or an end-of-information code
/// arrives, before `n` bytes) keeps the bytes it did decode and the rest stay
/// zero, as the `tiff` crate does: damaged files commonly lose a strip's last
/// byte or two, and refusing the whole image for that loses every good pixel.
/// An invalid code is still an error. Returns the bytes really decoded (the
/// caller bounds how much zero-fill an image may hold).
pub(crate) fn decode_append(data: &[u8], out: &mut Vec<u8>, n: usize) -> TiffResult<usize> {
    let base = out.len();
    out.try_reserve_exact(n)
        .map_err(|_| TiffError::limits(format!("cannot allocate {n} bytes")))?;
    out.resize(base + n, 0);
    decode_into(data, &mut out[base..])
}

/// Decode into `out`; returns how many bytes were produced (fewer than
/// `out.len()` when the data or an end-of-information code ends it early).
fn decode_into(data: &[u8], out: &mut [u8]) -> TiffResult<usize> {
    let n = out.len();
    let old_style = data.len() >= 2 && data[0] == 0 && (data[1] & 1) != 0;
    let mut br = BitReader {
        data,
        pos: 0,
        acc: 0,
        nbits: 0,
        msb: !old_style,
    };
    // Per code >= 258: start of its first emission in `out`, and its length.
    let mut start = vec![0u32; MAX_CODES];
    let mut len = vec![0u16; MAX_CODES];
    let early = u16::from(!old_style);
    let mut width = 9u32;
    let mut next = FIRST;
    let mut limit = (1u16 << width) - early;
    // The previous code's emission: start and length (0 = no previous code).
    let (mut pstart, mut plen) = (0usize, 0u16);
    let mut pos = 0usize;

    while pos < n {
        let Some(code) = br.read(width) else {
            return Ok(ended(out, pos));
        };
        if code == CLEAR {
            width = 9;
            next = FIRST;
            limit = (1u16 << width) - early;
            plen = 0;
            continue;
        }
        if code == EOI {
            return Ok(ended(out, pos));
        }
        if plen == 0 {
            // First code after a clear (or at the start): must be a literal.
            if code > 255 {
                return Err(TiffError::format("LZW: non-literal code after clear"));
            }
            out[pos] = code as u8;
            (pstart, plen) = (pos, 1);
            pos += 1;
            continue;
        }
        let at = pos;
        let room = n - at;
        let elen: u16 = if code < 256 {
            out[pos] = code as u8;
            pos += 1;
            1
        } else if code < next {
            let (s0, l) = (start[code as usize] as usize, len[code as usize]);
            let take = usize::from(l).min(room);
            copy_back(out, s0, at, take);
            pos += take;
            l
        } else if code == next && (next as usize) < MAX_CODES {
            // KwKwK: the string is prev + first(prev).
            let l = usize::from(plen);
            let take = l.min(room);
            copy_back(out, pstart, at, take);
            pos += take;
            if l < room {
                out[pos] = out[pstart];
                pos += 1;
            }
            plen.saturating_add(1)
        } else {
            return Err(TiffError::format(format!("LZW: invalid code {code}")));
        };
        // Add prev + first(this string): plen + 1 bytes from prev's start.
        if (next as usize) < MAX_CODES {
            start[next as usize] = pstart as u32;
            len[next as usize] = plen.saturating_add(1);
            next += 1;
            if width < 12 && next >= limit {
                width += 1;
                limit = (1u16 << width) - early;
            }
        }
        (pstart, plen) = (at, elen);
    }
    Ok(n)
}

/// A strip that ends at `pos` short of `out.len()`: clear the up to 15
/// bytes an overcopy may have written past it, so the rest reads as zero.
#[cold]
fn ended(out: &mut [u8], pos: usize) -> usize {
    let n = out.len();
    out[pos..n.min(pos + 16)].fill(0);
    pos
}

/// Copy `l` bytes from `out[from..]` to `out[at..]`; the source lies wholly
/// before `at`, so the two never overlap.
#[inline]
fn copy_back(out: &mut [u8], from: usize, at: usize, l: usize) {
    let (done, rest) = out.split_at_mut(at);
    if l <= 16 && from + 16 <= at && rest.len() >= 16 {
        // Overcopy: one constant 16-byte copy. The bytes past `l` land on
        // output the next codes overwrite (decode_into re-zeroes them if
        // the strip ends early, so a short strip still reads as zeros).
        rest[..16].copy_from_slice(&done[from..from + 16]);
        return;
    }
    let src = &done[from..from + l];
    let dst = &mut rest[..l];
    if l <= 16 {
        for (d, s) in dst.iter_mut().zip(src) {
            *d = *s;
        }
    } else {
        dst.copy_from_slice(src);
    }
}

/// MSB-first bit packer. Codes collect in a 64-bit accumulator and leave
/// four bytes at a time, so a 9..12-bit code costs a shift and an or rather
/// than a loop of single-byte pushes.
struct BitWriter {
    out: Vec<u8>,
    acc: u64,
    nbits: u32,
}

impl BitWriter {
    #[inline]
    fn put(&mut self, code: u16, width: u32) {
        // Bits above the low `nbits` are already written; they fall off the
        // top of the shift or out of the `as u32` below, so no masking.
        self.acc = (self.acc << width) | u64::from(code);
        self.nbits += width;
        if self.nbits >= 32 {
            self.nbits -= 32;
            self.out
                .extend_from_slice(&((self.acc >> self.nbits) as u32).to_be_bytes());
        }
    }
    fn finish(mut self) -> Vec<u8> {
        while self.nbits >= 8 {
            self.nbits -= 8;
            self.out.push((self.acc >> self.nbits) as u8);
        }
        if self.nbits > 0 {
            self.out.push((self.acc << (8 - self.nbits)) as u8);
        }
        self.out
    }
}

/// Hash table slots (a power of two, well above the 4094 live codes).
const HSIZE: usize = 1 << 14;
/// A key is `prefix code << 8 | byte`: under 2^20, since codes are < 4096.
const KEY_BITS: u32 = 20;

/// TIFF 6 LZW encoder (MSB-first, early change, clear at start and when the
/// table fills), reusable across strips and tiles.
///
/// The string table is an open-addressed hash of `(prefix, byte) -> code`.
/// Instead of refilling its 16 Ki slots at every clear code and for every
/// strip (a default strip is about 8 KB, so the refill cost more than the
/// coding), each stored key carries a generation number in its top bits and
/// a clear just starts a new generation: a slot holding an older generation
/// reads as empty. The slots are refilled only when the generation counter
/// wraps, once every 4095 clears.
pub(crate) struct LzwEncoder {
    // Fixed-size tables: a probe index is masked to `HSIZE - 1`, so with the
    // length in the type the bounds check on every probe disappears.
    keys: Box<[u32; HSIZE]>,
    vals: Box<[u16; HSIZE]>,
    /// Current generation, `1..(1 << (32 - KEY_BITS))`; 0 marks a slot
    /// never written since the last refill.
    gen: u32,
}

impl LzwEncoder {
    pub(crate) fn new() -> Self {
        Self {
            keys: vec![0u32; HSIZE].into_boxed_slice().try_into().unwrap(),
            vals: vec![0u16; HSIZE].into_boxed_slice().try_into().unwrap(),
            gen: 0,
        }
    }

    /// Forget every string: start a new generation.
    #[inline]
    fn clear(&mut self) {
        self.gen += 1;
        if self.gen == 1 << (32 - KEY_BITS) {
            self.keys.fill(0);
            self.gen = 1;
        }
    }

    /// Encode one strip or tile.
    pub(crate) fn encode(&mut self, data: &[u8]) -> Vec<u8> {
        self.clear();
        let mut w = BitWriter {
            out: Vec::with_capacity(data.len() / 2 + 16),
            acc: 0,
            nbits: 0,
        };
        let mut width = 9u32;
        let mut next = FIRST;
        w.put(CLEAR, width);
        let Some((&b0, rest)) = data.split_first() else {
            w.put(EOI, width);
            return w.finish();
        };
        let mut tag = self.gen << KEY_BITS;
        let mut cur = u16::from(b0);
        for &b in rest {
            let key = (u32::from(cur) << 8) | u32::from(b);
            let tagged = tag | key;
            let mut h = (key.wrapping_mul(0x9E37_79B1) >> 18) as usize & (HSIZE - 1);
            let found = loop {
                let k = self.keys[h];
                if k == tagged {
                    break Some(self.vals[h]);
                }
                // Keys are below 2^KEY_BITS, so anything under this
                // generation's tag was written by an earlier one: empty.
                if k < tag {
                    break None;
                }
                h = (h + 1) & (HSIZE - 1);
            };
            if let Some(code) = found {
                cur = code;
                continue;
            }
            w.put(cur, width);
            self.keys[h] = tagged;
            self.vals[h] = next;
            next += 1;
            // Encoder side of early change: libtiff's `free_ent > maxcode`.
            if next >= 4094 {
                w.put(CLEAR, width);
                self.clear();
                tag = self.gen << KEY_BITS;
                width = 9;
                next = FIRST;
            } else if next >= (1u16 << width) {
                width += 1;
            }
            cur = u16::from(b);
        }
        w.put(cur, width);
        // The decoder adds an entry after this code; mirror its width change.
        next += 1;
        if next >= 4094 {
            w.put(CLEAR, width);
            width = 9;
        } else if next >= (1u16 << width) {
            width += 1;
        }
        w.put(EOI, width);
        w.finish()
    }
}

/// One-shot encode (tests).
#[cfg(test)]
pub(crate) fn encode(data: &[u8]) -> Vec<u8> {
    LzwEncoder::new().encode(data)
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn round_trip_various() {
        let mut seed = 1u32;
        for n in [0usize, 1, 2, 3, 255, 256, 1000, 70_000, 300_000] {
            let data: Vec<u8> = (0..n)
                .map(|i| {
                    seed = seed.wrapping_mul(1_103_515_245).wrapping_add(12345);
                    if i % 7 < 4 {
                        (i / 13) as u8
                    } else {
                        (seed >> 24) as u8
                    }
                })
                .collect();
            let enc = encode(&data);
            let mut out = vec![0u8; n];
            decode(&enc, &mut out).unwrap();
            assert_eq!(out, data, "n={n}");
        }
    }

    /// A reused encoder (generation-stamped table, wrapping past 4095
    /// clears) must emit exactly what a fresh one does for every strip.
    #[test]
    fn reused_encoder_matches_fresh_across_generation_wrap() {
        let mut enc = LzwEncoder::new();
        let mut seed = 11u32;
        for strip in 0..5000usize {
            let n = if strip % 500 == 0 { 20_000 } else { strip % 97 };
            let data: Vec<u8> = (0..n)
                .map(|i| {
                    seed = seed.wrapping_mul(1_103_515_245).wrapping_add(12345);
                    if i % 5 < 2 {
                        (i / 9) as u8
                    } else {
                        (seed >> 24) as u8
                    }
                })
                .collect();
            let got = enc.encode(&data);
            assert_eq!(got, encode(&data), "strip {strip}");
            let mut out = vec![0u8; n];
            decode(&got, &mut out).unwrap();
            assert_eq!(out, data, "strip {strip}");
        }
        assert!(enc.gen < 5000, "generation counter wrapped and refilled");
    }

    #[test]
    fn garbage_never_panics() {
        let mut seed = 7u32;
        for _ in 0..2000 {
            let n = (seed % 64) as usize;
            let data: Vec<u8> = (0..n)
                .map(|_| {
                    seed = seed.wrapping_mul(1_103_515_245).wrapping_add(12345);
                    (seed >> 16) as u8
                })
                .collect();
            let mut out = vec![0u8; 500];
            let _ = decode(&data, &mut out);
        }
    }
}

#[cfg(test)]
mod eoi_tests {
    use super::*;

    /// Decode until EOI; return the number of bytes before it.
    fn len_to_eoi(data: &[u8]) -> Option<usize> {
        let mut out = vec![0u8; 1 << 20];
        // A large buffer: EOI ends the decode early with the count.
        decode_into(data, &mut out).ok().filter(|&n| n < out.len())
    }

    #[test]
    fn short_strip_keeps_decoded_bytes_and_zero_fills() {
        let data: Vec<u8> = (0..512u32).map(|i| (i * 7 % 251 + 1) as u8).collect();
        let enc = encode(&data);
        for cut in 1..=3 {
            let mut out = vec![0xaa; data.len()];
            decode(&enc[..enc.len() - cut], &mut out).unwrap();
            let good = out.iter().zip(&data).take_while(|(a, b)| a == b).count();
            assert!(good >= data.len() - 16, "cut {cut}: {good}");
            assert!(out[good..].iter().all(|&b| b == 0), "cut {cut}");
        }
        // Asking for more than the strip holds: the tail is zero.
        let mut out = vec![0xaa; data.len() + 100];
        decode(&enc, &mut out).unwrap();
        assert_eq!(&out[..data.len()], &data[..]);
        assert!(out[data.len()..].iter().all(|&b| b == 0));
    }

    #[test]
    fn eoi_lands_after_the_last_byte() {
        let mut seed = 3u32;
        for n in 1..3000usize {
            let data: Vec<u8> = (0..n)
                .map(|i| {
                    seed = seed.wrapping_mul(1_103_515_245).wrapping_add(12345);
                    if (seed >> 20) % 4 == 0 {
                        (seed >> 24) as u8
                    } else {
                        (i / 50) as u8 & 1
                    }
                })
                .collect();
            assert_eq!(len_to_eoi(&encode(&data)), Some(n), "n={n}");
        }
    }
}
