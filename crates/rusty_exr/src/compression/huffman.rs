//! The 16-bit Huffman coder used by PIZ and by DWA's AC stream
//! (Christian Rouet's coder from OpenEXR's `ImfHuf`).
//!
//! Stream layout: five little-endian u32 (`im`, `iM`, table bytes, data
//! bits, 0), the packed code-length table for symbols `im..=iM`, then the
//! MSB-first code bits. Symbol `iM` is the run code: it is followed by an
//! 8-bit count of extra copies of the previous symbol.

use std::cmp::Ordering;
use std::collections::BinaryHeap;

use crate::error::{Error, Result};

const ENC_SIZE: usize = (1 << 16) + 1;
const DEC_BITS: u32 = 14;
const DEC_SIZE: usize = 1 << DEC_BITS;
const SHORT_ZEROCODE_RUN: u64 = 59;
const LONG_ZEROCODE_RUN: u64 = 63;
const SHORTEST_LONG_RUN: u64 = 2 + LONG_ZEROCODE_RUN - SHORT_ZEROCODE_RUN;
const LONGEST_LONG_RUN: u64 = 255 + SHORTEST_LONG_RUN;

#[inline]
fn hlen(c: u64) -> u64 {
    c & 63
}
#[inline]
fn hcode(c: u64) -> u64 {
    c >> 6
}

/// Turn code lengths (one per symbol) into `length | code << 6` entries.
fn canonical(table: &mut [u64]) -> Result<()> {
    let mut n = [0u64; 59];
    for &l in table.iter() {
        if l > 58 {
            return Err(Error::invalid("Huffman code length over 58"));
        }
        n[l as usize] += 1;
    }
    let mut c = 0u64;
    for i in (1..=58).rev() {
        let nc = (c + n[i]) >> 1;
        n[i] = c;
        c = nc;
    }
    for e in table.iter_mut() {
        let l = *e as usize;
        if l > 0 {
            *e = l as u64 | (n[l] << 6);
            n[l] += 1;
        }
    }
    Ok(())
}

// ---------------------------------------------------------------------------
// Decoding.

struct BitIn<'a> {
    data: &'a [u8],
    byte: usize,
    buf: u64,
    count: u32,
}

impl<'a> BitIn<'a> {
    fn new(data: &'a [u8]) -> Self {
        Self {
            data,
            byte: 0,
            buf: 0,
            count: 0,
        }
    }
    #[inline]
    fn refill(&mut self) {
        while self.count <= 56 {
            let b = if self.byte < self.data.len() {
                self.data[self.byte]
            } else {
                0
            };
            self.byte += 1;
            self.buf |= u64::from(b) << (56 - self.count);
            self.count += 8;
        }
    }
    /// The next `n` bits (1..=58), MSB first. Requires a refill first.
    #[inline]
    fn peek(&self, n: u32) -> u64 {
        self.buf >> (64 - n)
    }
    #[inline]
    fn consume(&mut self, n: u32) {
        self.buf <<= n;
        self.count -= n;
    }
}

/// Unpack the code-length table.
fn unpack_table(
    c: &mut BitIn<'_>,
    limit_bits: u64,
    im: usize,
    imax: usize,
) -> Result<(Vec<u64>, u64)> {
    let mut table = vec![0u64; ENC_SIZE];
    let mut used = 0u64;
    let mut take = |c: &mut BitIn<'_>, n: u32| -> Result<u64> {
        used += u64::from(n);
        if used > limit_bits {
            return Err(Error::invalid("Huffman table runs past the end of data"));
        }
        c.refill();
        let v = c.peek(n);
        c.consume(n);
        Ok(v)
    };
    let mut i = im;
    while i <= imax {
        let l = take(c, 6)?;
        table[i] = l;
        if l == LONG_ZEROCODE_RUN {
            let run = (take(c, 8)? + SHORTEST_LONG_RUN) as usize;
            if i + run > imax + 1 {
                return Err(Error::invalid("Huffman table longer than declared"));
            }
            table[i..i + run].iter_mut().for_each(|v| *v = 0);
            i += run;
        } else if l >= SHORT_ZEROCODE_RUN {
            let run = (l - SHORT_ZEROCODE_RUN + 2) as usize;
            if i + run > imax + 1 {
                return Err(Error::invalid("Huffman table longer than declared"));
            }
            table[i..i + run].iter_mut().for_each(|v| *v = 0);
            i += run;
        } else {
            i += 1;
        }
    }
    canonical(&mut table)?;
    Ok((table, used))
}

/// Short-code table entry: `symbol << 8 | length`; 0 = empty; long-code
/// slots hold `LONG | index` into the long-candidate lists.
const LONG: u32 = 0x8000_0000;

struct DecTable {
    short: Vec<u32>,
    /// Candidate symbols for each long-code slot, in symbol order.
    long: Vec<Vec<u32>>,
}

fn build_dec_table(table: &[u64], im: usize, imax: usize) -> Result<DecTable> {
    let mut short = vec![0u32; DEC_SIZE];
    let mut long: Vec<Vec<u32>> = Vec::new();
    for (sym, &e) in table.iter().enumerate().take(imax + 1).skip(im) {
        let c = hcode(e);
        let l = hlen(e);
        if l == 0 {
            continue;
        }
        if c >> l != 0 {
            return Err(Error::invalid("Huffman code exceeds its length"));
        }
        if l > u64::from(DEC_BITS) {
            let slot = (c >> (l - u64::from(DEC_BITS))) as usize;
            let s = short[slot];
            if s == 0 {
                short[slot] = LONG | long.len() as u32;
                long.push(vec![sym as u32]);
            } else if s & LONG != 0 {
                long[(s & !LONG) as usize].push(sym as u32);
            } else {
                return Err(Error::invalid(
                    "Huffman long code collides with a short one",
                ));
            }
        } else {
            let start = (c << (u64::from(DEC_BITS) - l)) as usize;
            let n = 1usize << (u64::from(DEC_BITS) - l);
            let v = ((sym as u32) << 8) | l as u32;
            for s in &mut short[start..start + n] {
                if *s != 0 {
                    return Err(Error::invalid("Huffman short codes collide"));
                }
                *s = v;
            }
        }
    }
    Ok(DecTable { short, long })
}

/// Decode a Huffman stream into exactly `out_len` u16 values.
pub(crate) fn decompress(data: &[u8], out_len: usize) -> Result<Vec<u16>> {
    if data.len() < 20 {
        return Err(Error::invalid("Huffman header truncated"));
    }
    let rd = |i: usize| u32::from_le_bytes([data[i], data[i + 1], data[i + 2], data[i + 3]]);
    let im = rd(0) as usize;
    let imax = rd(4) as usize;
    let n_bits = u64::from(rd(12));
    if im >= ENC_SIZE || imax >= ENC_SIZE || im > imax {
        return Err(Error::invalid("Huffman symbol range"));
    }
    let body = &data[20..];
    if n_bits.div_ceil(8) > body.len() as u64 {
        return Err(Error::invalid("Huffman bit count exceeds the data"));
    }
    let mut c = BitIn::new(body);
    let (table, table_bits) = unpack_table(&mut c, body.len() as u64 * 8, im, imax)?;
    // The code bits start at the byte after the table.
    let table_bytes = table_bits.div_ceil(8) as usize;
    let code = &body[table_bytes..];
    if n_bits > code.len() as u64 * 8 {
        return Err(Error::invalid("Huffman bit count exceeds the data"));
    }
    let dec = build_dec_table(&table, im, imax)?;
    let rlc = imax as u32;

    let mut out: Vec<u16> = Vec::with_capacity(out_len);
    let mut bits = BitIn::new(&code[..n_bits.div_ceil(8) as usize]);
    let mut left = n_bits;
    while left > 0 {
        bits.refill();
        let e = dec.short[bits.peek(DEC_BITS) as usize];
        let (sym, l) = if e & LONG == 0 {
            let l = e & 0xff;
            if l == 0 {
                return Err(Error::invalid("invalid Huffman code"));
            }
            (e >> 8, l)
        } else {
            let mut found = None;
            for &s in &dec.long[(e & !LONG) as usize] {
                let te = table[s as usize];
                let l = hlen(te) as u32;
                if u64::from(l) <= left && bits.peek(l) == hcode(te) {
                    found = Some((s, l));
                    break;
                }
            }
            found.ok_or_else(|| Error::invalid("invalid Huffman code"))?
        };
        if u64::from(l) > left {
            return Err(Error::invalid("Huffman code runs past the bit count"));
        }
        bits.consume(l);
        left -= u64::from(l);
        if sym == rlc {
            if left < 8 {
                return Err(Error::invalid("Huffman run count runs past the bit count"));
            }
            bits.refill();
            let n = bits.peek(8) as usize;
            bits.consume(8);
            left -= 8;
            let Some(&last) = out.last() else {
                return Err(Error::invalid("Huffman run with nothing to repeat"));
            };
            if out.len() + n > out_len {
                return Err(Error::invalid("Huffman data longer than expected"));
            }
            out.resize(out.len() + n, last);
        } else {
            if out.len() >= out_len {
                return Err(Error::invalid("Huffman data longer than expected"));
            }
            out.push(sym as u16);
        }
    }
    if out.len() != out_len {
        return Err(Error::invalid(format!(
            "Huffman data decodes to {} values, expected {out_len}",
            out.len()
        )));
    }
    Ok(out)
}

// ---------------------------------------------------------------------------
// Encoding (byte-identical to the `exr` crate's encoder).

#[derive(PartialEq, Eq, Clone, Copy)]
struct HeapItem {
    pos: usize,
    freq: u64,
}

impl Ord for HeapItem {
    fn cmp(&self, other: &Self) -> Ordering {
        other
            .freq
            .cmp(&self.freq)
            .then_with(|| other.pos.cmp(&self.pos))
    }
}
impl PartialOrd for HeapItem {
    fn partial_cmp(&self, other: &Self) -> Option<Ordering> {
        Some(self.cmp(other))
    }
}

/// Build code lengths (then canonical codes) in place of `freq`.
/// Returns the symbol range `(im, iM)`.
fn build_enc_table(freq: &mut [u64]) -> Result<(usize, usize)> {
    let mut links: Vec<usize> = (0..ENC_SIZE).collect();
    let im = freq.iter().position(|&f| f != 0).unwrap_or(0);
    let mut imax = 0;
    let mut heap = BinaryHeap::new();
    for (i, &f) in freq.iter().enumerate().skip(im) {
        if f != 0 {
            heap.push(HeapItem { pos: i, freq: f });
            imax = i;
        }
    }
    imax += 1;
    freq[imax] = 1;
    heap.push(HeapItem { pos: imax, freq: 1 });
    let mut lens = vec![0u64; ENC_SIZE];
    let mut count = heap.len();
    while count > 1 {
        let low = heap.pop().expect("heap holds two or more");
        count -= 1;
        let high = {
            let mut second = heap.peek_mut().expect("heap holds one or more");
            second.freq += low.freq;
            second.pos
        };
        let mut i = high;
        loop {
            lens[i] += 1;
            if links[i] == i {
                links[i] = low.pos;
                break;
            }
            i = links[i];
        }
        let mut i = low.pos;
        loop {
            lens[i] += 1;
            if links[i] == i {
                break;
            }
            i = links[i];
        }
    }
    canonical(&mut lens)?;
    freq.copy_from_slice(&lens);
    Ok((im, imax))
}

struct BitOut {
    out: Vec<u8>,
    bits: u64,
    count: u64,
}

impl BitOut {
    #[inline]
    fn put(&mut self, n: u64, v: u64) {
        self.bits = (self.bits << n) | v;
        self.count += n;
        while self.count >= 8 {
            self.count -= 8;
            self.out.push((self.bits >> self.count) as u8);
        }
    }
    #[inline]
    fn code(&mut self, e: u64) {
        self.put(hlen(e), hcode(e));
    }
    fn flush(&mut self) {
        if self.count > 0 {
            self.out.push((self.bits << (8 - self.count)) as u8);
        }
    }
}

fn pack_table(enc: &[u64], im: usize, imax: usize, out: &mut Vec<u8>) {
    let mut w = BitOut {
        out: std::mem::take(out),
        bits: 0,
        count: 0,
    };
    let mut i = im;
    while i <= imax {
        let l = hlen(enc[i]);
        if l == 0 {
            let mut run = 1u64;
            while i < imax && run < LONGEST_LONG_RUN {
                if hlen(enc[i + 1]) > 0 {
                    break;
                }
                i += 1;
                run += 1;
            }
            if run >= 2 {
                if run >= SHORTEST_LONG_RUN {
                    w.put(6, LONG_ZEROCODE_RUN);
                    w.put(8, run - SHORTEST_LONG_RUN);
                } else {
                    w.put(6, SHORT_ZEROCODE_RUN + run - 2);
                }
                i += 1;
                continue;
            }
        }
        w.put(6, l);
        i += 1;
    }
    w.flush();
    *out = w.out;
}

#[inline]
fn send(w: &mut BitOut, sc: u64, run: u64, rc: u64) {
    if hlen(sc) + hlen(rc) + 8 < hlen(sc) * run {
        w.code(sc);
        w.code(rc);
        w.put(8, run);
    } else {
        for _ in 0..=run {
            w.code(sc);
        }
    }
}

/// Huffman-encode u16 values.
pub(crate) fn compress(data: &[u16]) -> Result<Vec<u8>> {
    if data.is_empty() {
        return Ok(Vec::new());
    }
    let mut enc = vec![0u64; ENC_SIZE];
    for &v in data {
        enc[v as usize] += 1;
    }
    let (im, imax) = build_enc_table(&mut enc)?;
    let mut out = vec![0u8; 20];
    pack_table(&enc, im, imax, &mut out);
    let table_len = out.len() - 20;
    let data_start = out.len();
    let mut w = BitOut {
        out,
        bits: 0,
        count: 0,
    };
    let rc = enc[imax];
    let mut s = data[0];
    let mut run = 0u64;
    for &v in &data[1..] {
        if s == v && run < 255 {
            run += 1;
        } else {
            send(&mut w, enc[s as usize], run, rc);
            run = 0;
        }
        s = v;
    }
    send(&mut w, enc[s as usize], run, rc);
    let n_bits = (w.out.len() - data_start) as u64 * 8 + w.count;
    w.flush();
    let mut out = w.out;
    for (k, v) in [im as u32, imax as u32, table_len as u32, n_bits as u32, 0]
        .into_iter()
        .enumerate()
    {
        out[k * 4..k * 4 + 4].copy_from_slice(&v.to_le_bytes());
    }
    Ok(out)
}

#[cfg(test)]
mod tests {
    use super::*;

    fn lcg(seed: &mut u32) -> u32 {
        *seed = seed.wrapping_mul(1_664_525).wrapping_add(1_013_904_223);
        *seed >> 8
    }

    #[test]
    fn round_trips() {
        let mut seed = 7;
        let cases: Vec<Vec<u16>> = vec![
            vec![0; 25],
            vec![65535; 300],
            (0..1000).map(|_| (lcg(&mut seed) % 7) as u16).collect(),
            (0..5000).map(|_| lcg(&mut seed) as u16).collect(),
            (0..3000)
                .map(|i| {
                    if i % 50 < 40 {
                        3
                    } else {
                        (lcg(&mut seed) % 900) as u16
                    }
                })
                .collect(),
            vec![1, 2],
        ];
        for c in cases {
            let z = compress(&c).unwrap();
            assert_eq!(decompress(&z, c.len()).unwrap(), c);
        }
    }

    #[test]
    fn rejects_wrong_length() {
        let c: Vec<u16> = (0..100).collect();
        let z = compress(&c).unwrap();
        assert!(decompress(&z, 99).is_err());
        assert!(decompress(&z, 101).is_err());
        assert!(decompress(&z[..z.len() - 1], 100).is_err());
    }
}
