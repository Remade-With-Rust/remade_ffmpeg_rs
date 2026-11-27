//! RLE: byte-plane split + delta predictor, then a byte run-length code.
//!
//! The token code is shared with DWA's RLE section ([`unpack`], [`pack`]).

use crate::error::{Error, Result};

const MIN_RUN: usize = 3;
const MAX_RUN: usize = 127;

/// Decode run-length tokens into exactly `out_len` bytes. A signed count
/// byte `n < 0` is followed by `-n` literal bytes; `n >= 0` by one byte
/// repeated `n + 1` times.
#[cfg(test)]
pub(crate) fn unpack(src: &[u8], out_len: usize) -> Result<Vec<u8>> {
    let mut out = Vec::new();
    unpack_into(src, out_len, &mut out)?;
    Ok(out)
}

/// [`unpack`] into a reused buffer.
pub(crate) fn unpack_into(src: &[u8], out_len: usize, out: &mut Vec<u8>) -> Result<()> {
    // Every byte is written on success (checked below), so a reused
    // buffer is not re-zeroed.
    if out.len() > out_len {
        out.truncate(out_len);
    } else {
        out.resize(out_len, 0);
    }
    let n = unpack_slice(src, out)?;
    if n != out_len {
        return Err(Error::invalid(format!(
            "RLE data decodes to {n} bytes, expected {out_len}"
        )));
    }
    Ok(())
}

/// Decode run-length tokens into the front of `out`, returning how many
/// bytes they produced. Errors (in the same order and with the same text as
/// a growing-`Vec` decoder) when a run would pass the end of `src` or of
/// `out`. Writing into a sized slice keeps the per-token work to one bounds
/// check and one copy or fill (a `Vec` re-checked its length and capacity
/// for every token).
fn unpack_slice(src: &[u8], out: &mut [u8]) -> Result<usize> {
    let mut i = 0;
    let mut o = 0;
    while i < src.len() {
        let n = src[i] as i8;
        i += 1;
        if n < 0 {
            let n = usize::from(n.unsigned_abs());
            let Some(lit) = src.get(i..i + n) else {
                return Err(Error::invalid("RLE literal run past end of data"));
            };
            let Some(d) = out.get_mut(o..o + n) else {
                return Err(Error::invalid("RLE data decodes past its size"));
            };
            d.copy_from_slice(lit);
            i += n;
            o += n;
        } else {
            let n = n as usize + 1;
            let Some(&v) = src.get(i) else {
                return Err(Error::invalid("RLE repeat run past end of data"));
            };
            let Some(d) = out.get_mut(o..o + n) else {
                return Err(Error::invalid("RLE data decodes past its size"));
            };
            i += 1;
            d.fill(v);
            o += n;
        }
    }
    Ok(o)
}

/// Encode bytes as run-length tokens (byte-identical to OpenEXR's and the
/// `exr` crate's encoder).
pub(crate) fn pack(data: &[u8]) -> Vec<u8> {
    let mut out = Vec::with_capacity(data.len() + data.len() / 64 + 2);
    let n = data.len();
    let isa = crate::simd::isa();
    let mut start = 0;
    let mut end = 1;
    while start < n {
        end = run_end(data, start, crate::simd::rle_run_end(isa, data, start, end));
        if end - start >= MIN_RUN {
            out.push((end - start - 1) as u8);
            out.push(data[start]);
            start = end;
            continue;
        }
        end = literal_end(
            data,
            start,
            crate::simd::rle_literal_end(isa, data, start, end),
        );
        out.push((start as isize - end as isize) as u8);
        out.extend_from_slice(&data[start..end]);
        start = end;
        end += 1;
    }
    out
}

/// The end of the repeat run of `data[start]` scanned from `end`: the first
/// position that differs, at most 128 bytes after `start`. `end` may equal
/// `start` (after a repeat run), in which case the first step only re-reads
/// `data[start]`; signed arithmetic keeps OpenEXR's bound. The oracle of
/// `simd::rle_run_end`, which advances `end` first.
pub(crate) fn run_end(data: &[u8], start: usize, mut end: usize) -> usize {
    while end < data.len()
        && data[start] == data[end]
        && (end as isize - start as isize - 1) < MAX_RUN as isize
    {
        end += 1;
    }
    end
}

/// The end of the literal run scanned from `end`: the first position that
/// starts three equal bytes, at most 127 bytes after `start`, or the end of
/// the data. The oracle of `simd::rle_literal_end`.
pub(crate) fn literal_end(data: &[u8], start: usize, mut end: usize) -> usize {
    let n = data.len();
    while end < n
        && (end + 1 >= n
            || data[end] != data[end + 1]
            || end + 2 >= n
            || data[end + 1] != data[end + 2])
        && end - start < MAX_RUN
    {
        end += 1;
    }
    end
}

/// Un-RLE and undo the predictor into `scratch.a` (even bytes, then odd).
pub(crate) fn decompress_halves(
    data: &[u8],
    raw_size: usize,
    scratch: &mut super::Scratch,
) -> Result<()> {
    unpack_into(data, raw_size, &mut scratch.a)?;
    super::zip::undo_predictor(&mut scratch.a);
    Ok(())
}

pub(crate) fn compress(raw: &[u8], work: &mut super::Work) -> Vec<u8> {
    pack(super::zip::prepare_into(raw, &mut work.prep))
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn scan_twins_match_scalar() {
        let mut seed = 0x0e1e_0e1eu32;
        let mut next = || {
            seed ^= seed << 13;
            seed ^= seed >> 17;
            seed ^= seed << 5;
            seed
        };
        for case in 0..300 {
            let n = 1 + (next() % 700) as usize;
            // Runs of random lengths over a small alphabet, so every run and
            // literal length (and the 127/128 caps) shows up.
            let mut data = Vec::with_capacity(n);
            while data.len() < n {
                let v = (next() % [2, 3, 256][case % 3]) as u8;
                let len = 1 + (next() % [3, 40, 300][(case / 3) % 3]) as usize;
                data.extend(std::iter::repeat_n(v, len.min(n - data.len())));
            }
            for start in 0..n {
                for end in [start, start + 1, start + 2] {
                    if end > n {
                        continue;
                    }
                    let want_r = run_end(&data, start, end);
                    let want_l = literal_end(&data, start, end.max(start + 1).min(n));
                    for isa in crate::simd::available() {
                        let r = crate::simd::rle_run_end(isa, &data, start, end);
                        assert!(r <= want_r && r >= end);
                        assert_eq!(
                            run_end(&data, start, r),
                            want_r,
                            "{isa:?} run {start} {end}"
                        );
                        let e0 = end.max(start + 1).min(n);
                        let l = crate::simd::rle_literal_end(isa, &data, start, e0);
                        assert!(l <= want_l && l >= e0);
                        assert_eq!(
                            literal_end(&data, start, l),
                            want_l,
                            "{isa:?} lit {start} {end}"
                        );
                    }
                }
            }
        }
    }

    /// The growing-`Vec` decoder `unpack_into` replaced: the oracle.
    fn unpack_reference(src: &[u8], out_len: usize) -> Result<Vec<u8>> {
        let mut out = Vec::new();
        let mut i = 0;
        while i < src.len() {
            let n = src[i] as i8;
            i += 1;
            if n < 0 {
                let n = usize::from(n.unsigned_abs());
                if i + n > src.len() {
                    return Err(Error::invalid("RLE literal run past end of data"));
                }
                if out.len() + n > out_len {
                    return Err(Error::invalid("RLE data decodes past its size"));
                }
                out.extend_from_slice(&src[i..i + n]);
                i += n;
            } else {
                let n = n as usize + 1;
                if i >= src.len() {
                    return Err(Error::invalid("RLE repeat run past end of data"));
                }
                if out.len() + n > out_len {
                    return Err(Error::invalid("RLE data decodes past its size"));
                }
                let v = src[i];
                i += 1;
                out.resize(out.len() + n, v);
            }
        }
        if out.len() != out_len {
            return Err(Error::invalid(format!(
                "RLE data decodes to {} bytes, expected {out_len}",
                out.len()
            )));
        }
        Ok(out)
    }

    #[test]
    fn unpack_matches_reference_on_damaged_streams() {
        let mut seed = 0x1234_5678u32;
        let mut next = || {
            seed ^= seed << 13;
            seed ^= seed >> 17;
            seed ^= seed << 5;
            seed
        };
        for case in 0..4000 {
            let len = (next() % 600) as usize;
            let data: Vec<u8> = (0..len).map(|i| if next() % 3 == 0 { (i / 7) as u8 } else { next() as u8 }).collect();
            let mut p = pack(&data);
            // Damage: flip, truncate or extend some streams.
            match case % 4 {
                1 if !p.is_empty() => {
                    let k = next() as usize % p.len();
                    p[k] ^= 1 << (next() % 8);
                }
                2 => p.truncate(next() as usize % (p.len() + 1)),
                3 => p.push(next() as u8),
                _ => {}
            }
            for want in [len, len.saturating_sub(1), len + 3] {
                let mut out = vec![9u8; 5];
                let got = unpack_into(&p, want, &mut out).map(|()| out);
                let refr = unpack_reference(&p, want);
                assert_eq!(format!("{got:?}"), format!("{refr:?}"), "case {case} want {want}");
            }
        }
    }

    #[test]
    fn round_trip() {
        let cases: Vec<Vec<u8>> = vec![
            vec![],
            vec![1],
            vec![1, 1],
            vec![1, 1, 1],
            vec![0; 1000],
            (0..=255u8).collect(),
            [vec![5; 200], (0..50u8).collect(), vec![7, 7, 8, 8, 8, 9]].concat(),
        ];
        for c in cases {
            let p = pack(&c);
            assert_eq!(unpack(&p, c.len()).unwrap(), c);
            let z = compress(&c, &mut crate::compression::Work::default());
            let mut s = crate::compression::Scratch::default();
            decompress_halves(&z, c.len(), &mut s).unwrap();
            let mut back = vec![0; c.len()];
            crate::compression::zip::interleave(&s.a, &mut back);
            assert_eq!(back, c);
        }
    }
}
