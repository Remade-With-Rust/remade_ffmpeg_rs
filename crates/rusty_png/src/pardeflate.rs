//! Multi-threaded DEFLATE for a single image (pigz-style block splitting).
//!
//! # Why this exists
//!
//! The stage profiler puts DEFLATE at **77–82%** of encode at `Compression::Fast`
//! and **94–99.5%** at `Default`/`Best`. Nothing outside DEFLATE can move the
//! encode benchmark, and at a matched filter and matched size we measured
//! ffmpeg's zlib **1.09–1.45× ahead** of ours single-threaded. Out-optimising
//! zlib's inner loop is a poor bet; splitting the work is not — and ffmpeg's PNG
//! encoder is single-threaded for a single image, so it cannot answer this.
//!
//! # How the stream stays valid
//!
//! Concatenating independent *zlib* streams would be invalid. Instead this
//! builds ONE zlib stream by hand, exactly as `pigz` does:
//!
//! ```text
//!   [zlib header] [raw DEFLATE blocks from worker 0] ... [worker N-1, BFINAL] [Adler-32]
//! ```
//!
//! Each worker compresses with `zlib_header = false` (raw DEFLATE) and ends with
//! `FlushCompress::Full`, which closes the current block on a byte boundary and
//! resets the dictionary — so the next worker's output can simply follow. Only
//! the final worker uses `Finish`. The Adler-32 is over the whole *uncompressed*
//! stream, so it is computed independently of the split.
//!
//! # Why blocks are sized, not counted
//!
//! The compression loss depends on bytes per block, not on how many blocks there
//! are. Measured (pessimistically — independent streams, no dictionary priming):
//!
//! | filtered bytes | bytes/block | size delta |
//! |---|---|---|
//! | 24.9 MB | 1.04 MB | **+0.11%** |
//! | 2.35 MB | 98 KB | +1.64% |
//! | 1.44 MB | 60 KB | **+7.44%** |
//!
//! So blocks are never smaller than [`PAR_MIN_BLOCK`], and an image too small to
//! yield two such blocks is compressed serially and pays nothing.

use std::io::Write;

use flate2::{write::DeflateEncoder, Compress, Compression, FlushCompress};

/// Smallest block worth splitting off, in filtered bytes.
///
/// At ~1 MB/block the measured size cost is +0.11%; at 98 KB it is +1.64% and at
/// 60 KB it is +7.44%. 1 MiB keeps every split comfortably in the first regime.
pub const PAR_MIN_BLOCK: usize = 1 << 20;

/// How many blocks to split `filtered_len` into, given a thread budget.
///
/// Returns 1 when the image cannot yield at least two full-size blocks, which is
/// the case that would otherwise pay the +1.6%…+7.4% penalty for nothing.
pub fn block_count(filtered_len: usize, threads: usize) -> usize {
    // Platforms with no thread support: `std::thread::scope` COMPILES on
    // wasm32-unknown-unknown but `spawn` is unsupported there and panics at
    // runtime. A `cargo check --target wasm32-unknown-unknown` therefore passes
    // while the code is still broken, which is exactly the kind of gap a
    // compile-only gate misses. Force serial so the panic is unreachable rather
    // than merely unlikely.
    //
    // (The default path already resolved to 1 by accident, because
    // `available_parallelism()` returns `Err` here — but "safe by accident" is
    // not safe: an explicit `-threads 8` walked straight into it.)
    #[cfg(all(target_family = "wasm", not(target_feature = "atomics")))]
    {
        let _ = (filtered_len, threads);
        return 1;
    }
    #[cfg(not(all(target_family = "wasm", not(target_feature = "atomics"))))]
    {
        if threads <= 1 || filtered_len < 2 * PAR_MIN_BLOCK {
            return 1;
        }
        (filtered_len / PAR_MIN_BLOCK).min(threads).max(1)
    }
}

fn adler32(data: &[u8]) -> u32 {
    use simd_adler32::Adler32;
    let mut h = Adler32::new();
    h.write(data);
    h.finish()
}

/// Compress `filtered` (whole PNG scanline stream, filter bytes included) into a
/// single zlib stream using up to `threads` workers.
///
/// `row_stride` must divide `filtered.len()` — blocks are cut on scanline
/// boundaries so each worker sees whole rows.
pub fn compress_parallel(
    filtered: &[u8],
    row_stride: usize,
    level: u32,
    threads: usize,
) -> std::io::Result<Vec<u8>> {
    let mut out = Vec::with_capacity(filtered.len() / 2 + 1024);
    compress_parallel_to(&mut out, filtered, row_stride, level, threads)?;
    Ok(out)
}

/// [`compress_parallel`] writing straight to `w` instead of returning a `Vec`.
///
/// The layout this module builds — `[header] [blocks in order] [Adler-32]` —
/// needs nothing that depends on the total compressed size: the header is a
/// function of `level` alone and the checksum is over the *uncompressed* input.
/// So the whole stream can go out as it is produced.
///
/// That matters because the returning form made **three** full-size copies of
/// the compressed data: each worker's own `Vec`, then a `raw` buffer
/// concatenating all of them, then `assemble` copying `raw` again to put two
/// header bytes in front. With the encoder's own copy into the writer on top,
/// a parallel encode held roughly four copies of the compressed stream at once.
/// Here the workers' buffers are the only ones, and each is dropped the moment
/// it has been written.
pub fn compress_parallel_to<W: Write>(
    w: &mut W,
    filtered: &[u8],
    row_stride: usize,
    level: u32,
    threads: usize,
) -> std::io::Result<()> {
    let rows = filtered.len() / row_stride;
    let nblocks = block_count(filtered.len(), threads);

    w.write_all(&zlib_header(level))?;

    if nblocks <= 1 {
        // Serial path — identical bytes to the ordinary encoder.
        let mut e = DeflateEncoder::new(&mut *w, Compression::new(level));
        e.write_all(filtered)?;
        e.finish()?;
    } else {
        let rows_per = rows.div_ceil(nblocks);
        let mut ranges = Vec::with_capacity(nblocks);
        let mut r0 = 0usize;
        while r0 < rows {
            let r1 = (r0 + rows_per).min(rows);
            ranges.push((r0 * row_stride, r1 * row_stride));
            r0 = r1;
        }
        let last = ranges.len() - 1;

        // Scoped threads: the block count is small and bounded, and this keeps
        // the crate free of a work-stealing runtime it does not otherwise need.
        //
        // Workers are joined IN ORDER and each block is written and dropped as
        // it arrives. Joining in order is not a scheduling constraint — the
        // threads all run concurrently regardless — it is what lets the bytes
        // leave in stream order without staging them somewhere first.
        std::thread::scope(|s| -> std::io::Result<()> {
            let handles: Vec<_> = ranges
                .iter()
                .enumerate()
                .map(|(i, &(a, b))| {
                    let chunk = &filtered[a..b];
                    s.spawn(move || compress_block(chunk, level, i == last))
                })
                .collect();
            for h in handles {
                let part = h.join().unwrap()?;
                w.write_all(&part)?;
            }
            Ok(())
        })?;
    }

    w.write_all(&adler32(filtered).to_be_bytes())?;
    Ok(())
}

/// One block as RAW DEFLATE. Non-final blocks end with `Full`, which closes the
/// block on a byte boundary and resets the dictionary so the next block's output
/// can be appended verbatim.
fn compress_block(chunk: &[u8], level: u32, is_last: bool) -> std::io::Result<Vec<u8>> {
    // The crate is `#![forbid(unsafe_code)]`, so this drains through an ordinary
    // scratch buffer rather than writing into a Vec's spare capacity. The extra
    // copy is 64 KiB at a time against a multi-megabyte block — irrelevant next
    // to DEFLATE itself, and it keeps the no-unsafe promise intact.
    const SCRATCH: usize = 64 * 1024;
    let mut c = Compress::new(Compression::new(level), false);
    let mut out = Vec::with_capacity(chunk.len() / 2 + 64);
    let mut buf = vec![0u8; SCRATCH];
    let mut input = chunk;

    while !input.is_empty() {
        let (before_in, before_out) = (c.total_in(), c.total_out());
        c.compress(input, &mut buf, FlushCompress::None)
            .map_err(std::io::Error::other)?;
        let produced = (c.total_out() - before_out) as usize;
        let consumed = (c.total_in() - before_in) as usize;
        out.extend_from_slice(&buf[..produced]);
        input = &input[consumed..];
        if consumed == 0 && produced == 0 {
            break;
        }
    }

    // Non-final blocks end on a byte boundary with the dictionary reset, so the
    // next block's raw output can be appended verbatim.
    let flush = if is_last {
        FlushCompress::Finish
    } else {
        FlushCompress::Full
    };
    loop {
        let before_out = c.total_out();
        let status = c
            .compress(&[], &mut buf, flush)
            .map_err(std::io::Error::other)?;
        let produced = (c.total_out() - before_out) as usize;
        out.extend_from_slice(&buf[..produced]);
        match status {
            flate2::Status::StreamEnd => break,
            _ if produced == 0 => break,
            _ => {}
        }
    }
    Ok(out)
}

/// The 2-byte zlib container header for `level`.
///
/// Split out from the old `assemble` so it can be written before the blocks
/// exist: it depends only on the level, which is why the stream is emittable
/// front-to-back at all.
fn zlib_header(level: u32) -> [u8; 2] {
    // CMF: deflate, 32 KiB window. FLG: level hint + check bits so
    // (CMF<<8 | FLG) % 31 == 0.
    let cmf = 0x78u8;
    let flevel = match level {
        0..=1 => 0u8,
        2..=5 => 1,
        6 => 2,
        _ => 3,
    };
    let mut flg = flevel << 6;
    let rem = ((cmf as u16) << 8 | flg as u16) % 31;
    if rem != 0 {
        flg += (31 - rem) as u8;
    }
    [cmf, flg]
}

#[cfg(test)]
mod tests {
    use super::*;
    use std::io::Read;

    fn roundtrip(data: &[u8], stride: usize, threads: usize) {
        let z = compress_parallel(data, stride, 6, threads).expect("compress");
        let mut d = flate2::read::ZlibDecoder::new(&z[..]);
        let mut back = Vec::new();
        d.read_to_end(&mut back).expect("the stream must be valid zlib");
        assert_eq!(back, data, "threads={threads}: round-trip mismatch");
    }

    /// The whole point: a split stream must still be ONE valid zlib stream that
    /// any decoder reads back byte-for-byte.
    #[test]
    fn parallel_stream_is_valid_zlib_and_lossless() {
        let stride = 4096;
        let rows = 900; // ~3.7 MB, enough for several blocks
        let mut data = Vec::with_capacity(stride * rows);
        for r in 0..rows {
            data.push(1u8);
            for i in 1..stride {
                data.push(((r * 31 + i * 7) % 251) as u8);
            }
        }
        for threads in [1usize, 2, 3, 4, 8] {
            roundtrip(&data, stride, threads);
        }
    }

    /// Small inputs must NOT be split — that is where the size penalty lives.
    #[test]
    fn small_inputs_stay_serial() {
        assert_eq!(block_count(1024, 8), 1);
        assert_eq!(block_count(PAR_MIN_BLOCK, 8), 1, "one block's worth is not splittable");
        assert_eq!(block_count(2 * PAR_MIN_BLOCK, 8), 2);
        assert_eq!(block_count(64 * PAR_MIN_BLOCK, 8), 8, "capped by the thread budget");
        assert_eq!(block_count(64 * PAR_MIN_BLOCK, 1), 1, "single thread never splits");
    }
}

#[cfg(test)]
mod encoder_integration {
    /// End-to-end gate: a PNG encoded through the PARALLEL path must decode to
    /// exactly the same pixels as the serial one, and must remain a valid PNG
    /// for any decoder — not merely for ours. Sizes may differ (block
    /// boundaries reset the dictionary); pixels may not.
    #[test]
    fn parallel_encode_matches_serial_pixels() {
        let (w, h, ch) = (900usize, 1400usize, 3usize);
        let mut px = Vec::with_capacity(w * h * ch);
        for y in 0..h {
            for x in 0..w {
                px.push(((x * 7 + y * 3) % 256) as u8);
                px.push(((x ^ y) % 256) as u8);
                px.push(((x * y) % 251) as u8);
            }
        }

        let encode = |threads: usize| -> Vec<u8> {
            let mut out = Vec::new();
            {
                let mut e = crate::Encoder::new(&mut out, w as u32, h as u32);
                e.set_color(crate::ColorType::Rgb);
                e.set_depth(crate::BitDepth::Eight);
                e.set_compression(crate::Compression::Default);
                e.set_filter(crate::FilterType::Up);
                e.set_parallel(threads);
                e.write_header().unwrap().write_image_data(&px).unwrap();
            }
            out
        };

        let decode = |bytes: &[u8]| -> Vec<u8> {
            let d = crate::Decoder::new(std::io::Cursor::new(bytes.to_vec()));
            let mut r = d.read_info().unwrap();
            let mut buf = vec![0; r.output_buffer_size()];
            let info = r.next_frame(&mut buf).unwrap();
            buf.truncate(info.buffer_size());
            buf
        };

        let serial = encode(1);
        assert_eq!(decode(&serial), px, "serial path must be lossless");

        for threads in [2usize, 4, 8] {
            let par = encode(threads);
            assert_eq!(
                decode(&par),
                px,
                "threads={threads}: parallel encode lost pixels"
            );
            // Sanity: it really did take the parallel branch on an input this
            // size, so a silently-serial fallback cannot pass as a success.
            let blocks = super::block_count((w * ch + 1) * h, threads);
            assert!(blocks > 1, "threads={threads}: expected a split, got {blocks}");
        }
    }
}
