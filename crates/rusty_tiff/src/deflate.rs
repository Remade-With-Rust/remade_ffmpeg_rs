//! Deflate (compression 8 and 32946) through flate2's pure-Rust zlib-rs
//! backend. Both codes carry a zlib-wrapped stream.

use flate2::{Compress, Compression, Decompress, FlushCompress, FlushDecompress, Status};

use crate::error::{TiffError, TiffResult};

/// Inflate into `out`, which must be filled exactly.
#[cfg(test)]
pub(crate) fn decode(data: &[u8], out: &mut [u8]) -> TiffResult<()> {
    let mut v = Vec::new();
    decode_append(data, &mut v, out.len())?;
    out.copy_from_slice(&v);
    Ok(())
}

/// Inflate exactly `n` bytes onto the end of `out`, straight into its spare
/// capacity (no zero-fill first). Capacity is reserved for exactly `n`
/// more, so a decompression bomb costs at most `n` bytes; anything the
/// inflater writes past `n` (only possible if the allocator handed back
/// more capacity than asked) is truncated away.
pub(crate) fn decode_append(data: &[u8], out: &mut Vec<u8>, n: usize) -> TiffResult<()> {
    out.try_reserve_exact(n)
        .map_err(|_| TiffError::limits(format!("cannot allocate {n} bytes")))?;
    let base = out.len();
    let target = base + n;
    let mut d = Decompress::new(true);
    loop {
        if out.len() >= target {
            out.truncate(target);
            return Ok(());
        }
        let in_before = d.total_in();
        let out_before = out.len();
        let ip = in_before as usize;
        let status = d
            .decompress_vec(&data[ip..], out, FlushDecompress::None)
            .map_err(|e| TiffError::format(format!("Deflate: {e}")))?;
        let progressed = d.total_in() != in_before || out.len() != out_before;
        if out.len() >= target {
            out.truncate(target);
            return Ok(());
        }
        match status {
            Status::StreamEnd => {
                return Err(TiffError::format(format!(
                    "Deflate stream ended after {} of {n} bytes",
                    out.len() - base
                )))
            }
            Status::Ok | Status::BufError => {
                if !progressed {
                    return Err(TiffError::format(format!(
                        "Deflate data ended after {} of {n} bytes",
                        out.len() - base
                    )));
                }
                if out.len() == out.capacity() {
                    // Capacity exhausted below the target cannot happen with
                    // the reservation above, but never spin if it does.
                    out.try_reserve_exact(target - out.len())
                        .map_err(|_| TiffError::limits("cannot allocate"))?;
                }
            }
        }
    }
}

/// Deflate `data` into a zlib stream at `level` (0-9).
pub(crate) fn encode(data: &[u8], level: u32) -> Vec<u8> {
    let mut c = Compress::new(Compression::new(level.min(9)), true);
    let mut out = Vec::with_capacity(data.len() / 2 + 64);
    loop {
        let ip = c.total_in() as usize;
        if out.capacity() - out.len() < 64 {
            out.reserve(out.capacity().max(4096));
        }
        let st = c
            .compress_vec(&data[ip..], &mut out, FlushCompress::Finish)
            .unwrap_or(Status::StreamEnd);
        if st == Status::StreamEnd {
            break;
        }
    }
    out
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn round_trip() {
        let data: Vec<u8> = (0..100_000u32)
            .map(|i| (i % 251) as u8 ^ (i >> 9) as u8)
            .collect();
        for level in [0, 1, 6, 9] {
            let enc = encode(&data, level);
            let mut out = vec![0u8; data.len()];
            decode(&enc, &mut out).unwrap();
            assert_eq!(out, data);
        }
    }

    #[test]
    fn short_and_garbage() {
        let enc = encode(&[1, 2, 3], 6);
        let mut out = vec![0u8; 10];
        assert!(decode(&enc, &mut out).is_err());
        assert!(decode(&[0x78, 0x9c, 0xff, 0xff, 0xff], &mut out).is_err());
        assert!(decode(&[], &mut out).is_err());
    }
}
