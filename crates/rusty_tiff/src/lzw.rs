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
    /// Next `width`-bit code, or `None` at end of data.
    #[inline]
    fn read(&mut self, width: u32) -> Option<u16> {
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
        let mask = (1u64 << width) - 1;
        let code = if self.msb {
            (self.acc >> (self.nbits - width)) & mask
        } else {
            let c = self.acc & mask;
            self.acc >>= width;
            c
        };
        self.nbits -= width;
        if self.msb {
            self.acc &= (1u64 << self.nbits) - 1;
        }
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

/// Strings this long or shorter are copied byte by byte; longer ones with
/// one `extend_from_within` (a runtime-length memcpy call costs more than a
/// few bytes).
const INLINE_COPY: usize = 8;

/// Decode a TIFF LZW strip/tile, appending exactly `n` bytes to `out`.
///
/// Every code's string is, by construction, an earlier stretch of this same
/// output: code `k` added after emitting `prev` is `string(prev)` plus the
/// next string's first byte, which is exactly the `len(prev) + 1` bytes
/// starting where `prev` was emitted. So instead of a prefix/suffix table
/// walked back to front one byte at a time, each code records where it was
/// first emitted and how long it is, and emitting it is a copy from there.
///
/// Old-style (LSB-first) data is detected the way libtiff does it: a first
/// byte of 0 with the low bit of the second byte set.
pub(crate) fn decode_append(data: &[u8], out: &mut Vec<u8>, n: usize) -> TiffResult<()> {
    out.try_reserve_exact(n)
        .map_err(|_| TiffError::limits(format!("cannot allocate {n} bytes")))?;
    let base = out.len();
    let target = base + n;
    let old_style = data.len() >= 2 && data[0] == 0 && (data[1] & 1) != 0;
    let mut br = BitReader {
        data,
        pos: 0,
        acc: 0,
        nbits: 0,
        msb: !old_style,
    };
    // Per code >= 258: absolute start of its first emission in `out`, and
    // its length. Codes < 256 are their own byte.
    let mut start = vec![0usize; MAX_CODES];
    let mut len = vec![0u16; MAX_CODES];
    let early = u16::from(!old_style);
    let mut width = 9u32;
    let mut next = FIRST;
    // The previous code's emission: (code, start, length).
    let mut prev: Option<(u16, usize, u16)> = None;

    while out.len() < target {
        let Some(code) = br.read(width) else {
            return Err(TiffError::format(format!(
                "LZW data ended after {} of {n} bytes",
                out.len() - base
            )));
        };
        if code == CLEAR {
            width = 9;
            next = FIRST;
            prev = None;
            continue;
        }
        if code == EOI {
            return Err(TiffError::format(format!(
                "LZW end-of-information after {} of {n} bytes",
                out.len() - base
            )));
        }
        let at = out.len();
        let Some((_, pstart, plen)) = prev else {
            // First code after a clear (or at the start): must be a literal.
            if code > 255 {
                return Err(TiffError::format("LZW: non-literal code after clear"));
            }
            out.push(code as u8);
            prev = Some((code, at, 1));
            continue;
        };
        let room = target - at;
        let elen: u16 = if code < 256 {
            out.push(code as u8);
            1
        } else if code < next {
            let (s0, l) = (start[code as usize], len[code as usize]);
            copy_back(out, s0, usize::from(l).min(room));
            l
        } else if code == next && (next as usize) < MAX_CODES {
            // KwKwK: the string is prev + first(prev).
            let l = usize::from(plen);
            copy_back(out, pstart, l.min(room));
            if l < room {
                let b = out[pstart];
                out.push(b);
            }
            plen.saturating_add(1)
        } else {
            return Err(TiffError::format(format!("LZW: invalid code {code}")));
        };
        // Add prev + first(this string): plen + 1 bytes from prev's start.
        if (next as usize) < MAX_CODES {
            start[next as usize] = pstart;
            len[next as usize] = plen.saturating_add(1);
            next += 1;
            if width < 12 && next >= (1u16 << width) - early {
                width += 1;
            }
        }
        prev = Some((code, at, elen));
    }
    out.truncate(target);
    Ok(())
}

/// Append `l` bytes copied from `out[from..]` (always fully before the end).
#[inline]
fn copy_back(out: &mut Vec<u8>, from: usize, l: usize) {
    if l <= INLINE_COPY {
        for k in 0..l {
            let b = out[from + k];
            out.push(b);
        }
    } else {
        out.extend_from_within(from..from + l);
    }
}

struct BitWriter {
    out: Vec<u8>,
    acc: u32,
    nbits: u32,
}

impl BitWriter {
    fn put(&mut self, code: u16, width: u32) {
        self.acc = (self.acc << width) | u32::from(code);
        self.nbits += width;
        while self.nbits >= 8 {
            self.nbits -= 8;
            self.out.push((self.acc >> self.nbits) as u8);
        }
        self.acc &= (1 << self.nbits) - 1;
    }
    fn finish(mut self) -> Vec<u8> {
        if self.nbits > 0 {
            self.out.push((self.acc << (8 - self.nbits)) as u8);
        }
        self.out
    }
}

/// Encode with TIFF 6 LZW (MSB-first, early change, clear at start and when
/// the table fills).
pub(crate) fn encode(data: &[u8]) -> Vec<u8> {
    const HSIZE: usize = 1 << 14;
    let mut keys = vec![u32::MAX; HSIZE];
    let mut vals = vec![0u16; HSIZE];
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
    let mut cur = u16::from(b0);
    for &b in rest {
        let key = (u32::from(cur) << 8) | u32::from(b);
        let mut h = (key.wrapping_mul(0x9E37_79B1) >> 18) as usize & (HSIZE - 1);
        let found = loop {
            if keys[h] == key {
                break Some(vals[h]);
            }
            if keys[h] == u32::MAX {
                break None;
            }
            h = (h + 1) & (HSIZE - 1);
        };
        if let Some(code) = found {
            cur = code;
            continue;
        }
        w.put(cur, width);
        keys[h] = key;
        vals[h] = next;
        next += 1;
        // Encoder side of early change: libtiff's `free_ent > maxcode`.
        if next >= 4094 {
            w.put(CLEAR, width);
            keys.fill(u32::MAX);
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
        // Decode into a large buffer: decode() errors at EOI with the count.
        match decode(data, &mut out) {
            Err(TiffError::Format(m)) if m.contains("end-of-information") => {
                m.split_whitespace().nth(3)?.parse().ok()
            }
            _ => None,
        }
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
