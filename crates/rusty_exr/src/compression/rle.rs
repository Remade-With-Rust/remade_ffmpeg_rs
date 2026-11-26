//! RLE: byte-plane split + delta predictor, then a byte run-length code.
//!
//! The token code is shared with DWA's RLE section ([`unpack`], [`pack`]).

use crate::error::{Error, Result};

const MIN_RUN: usize = 3;
const MAX_RUN: usize = 127;

/// Decode run-length tokens into exactly `out_len` bytes. A signed count
/// byte `n < 0` is followed by `-n` literal bytes; `n >= 0` by one byte
/// repeated `n + 1` times.
pub(crate) fn unpack(src: &[u8], out_len: usize) -> Result<Vec<u8>> {
    let mut out = Vec::new();
    unpack_into(src, out_len, &mut out)?;
    Ok(out)
}

/// [`unpack`] into a reused buffer.
pub(crate) fn unpack_into(src: &[u8], out_len: usize, out: &mut Vec<u8>) -> Result<()> {
    out.clear();
    out.reserve_exact(out_len);
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
    Ok(())
}

/// Encode bytes as run-length tokens (byte-identical to OpenEXR's and the
/// `exr` crate's encoder).
pub(crate) fn pack(data: &[u8]) -> Vec<u8> {
    let mut out = Vec::with_capacity(data.len() + data.len() / 64 + 2);
    let n = data.len();
    let mut start = 0;
    let mut end = 1;
    while start < n {
        // `end` may equal `start` here (after a repeat run), in which case
        // the first step only re-reads `data[start]`; signed arithmetic
        // keeps OpenEXR's bound (a repeat run may reach 128 bytes).
        while end < n
            && data[start] == data[end]
            && (end as isize - start as isize - 1) < MAX_RUN as isize
        {
            end += 1;
        }
        if end - start >= MIN_RUN {
            out.push((end - start - 1) as u8);
            out.push(data[start]);
            start = end;
            continue;
        }
        while end < n
            && (end + 1 >= n
                || data[end] != data[end + 1]
                || end + 2 >= n
                || data[end + 1] != data[end + 2])
            && end - start < MAX_RUN
        {
            end += 1;
        }
        out.push((start as isize - end as isize) as u8);
        out.extend_from_slice(&data[start..end]);
        start = end;
        end += 1;
    }
    out
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

pub(crate) fn compress(raw: &[u8]) -> Vec<u8> {
    let mut tmp = vec![0u8; raw.len()];
    super::zip::deinterleave(raw, &mut tmp);
    super::zip::apply_predictor(&mut tmp);
    pack(&tmp)
}

#[cfg(test)]
mod tests {
    use super::*;

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
            let z = compress(&c);
            let mut s = crate::compression::Scratch::default();
            decompress_halves(&z, c.len(), &mut s).unwrap();
            let mut back = vec![0; c.len()];
            crate::compression::zip::interleave(&s.a, &mut back);
            assert_eq!(back, c);
        }
    }
}
