//! Deflate (compression 8 and 32946) through flate2's pure-Rust zlib-rs
//! backend. Both codes carry a zlib-wrapped stream.

use flate2::{Compress, Compression, Decompress, FlushCompress, FlushDecompress, Status};

use crate::error::{TiffError, TiffResult};

/// Inflate into `out`, which must be filled exactly. Output beyond `out` is
/// never produced (the inflater writes only into the slice), so a
/// decompression bomb costs at most `out.len()` bytes.
pub(crate) fn decode(data: &[u8], out: &mut [u8]) -> TiffResult<()> {
    let mut d = Decompress::new(true);
    loop {
        let in_before = d.total_in();
        let out_before = d.total_out();
        let ip = in_before as usize;
        let op = out_before as usize;
        if op >= out.len() {
            return Ok(());
        }
        let status = d
            .decompress(&data[ip..], &mut out[op..], FlushDecompress::None)
            .map_err(|e| TiffError::format(format!("Deflate: {e}")))?;
        let progressed = d.total_in() != in_before || d.total_out() != out_before;
        if (d.total_out() as usize) >= out.len() {
            return Ok(());
        }
        match status {
            Status::StreamEnd => {
                return Err(TiffError::format(format!(
                    "Deflate stream ended after {} of {} bytes",
                    d.total_out(),
                    out.len()
                )))
            }
            Status::Ok | Status::BufError => {
                if !progressed {
                    return Err(TiffError::format(format!(
                        "Deflate data ended after {} of {} bytes",
                        d.total_out(),
                        out.len()
                    )));
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
