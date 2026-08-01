use super::{RowData, Worker};
use crate::decode::decoder::MAX_COMPONENTS;
use crate::decode::error::Result;
use crate::decode::idct::dequantize_and_idct_block;
use crate::decode::parser::Component;
use alloc::sync::Arc;
use alloc::vec;
use alloc::vec::Vec;
use core::mem;

pub struct ImmediateWorker {
    offsets: [usize; MAX_COMPONENTS],
    results: Vec<Vec<u8>>,
    components: Vec<Option<Component>>,
    quantization_tables: Vec<Option<Arc<[u16; 64]>>>,
    /// Last coefficient buffer this worker finished with, offered back to the
    /// caller so it can be refilled rather than reallocated.
    spare: Option<Vec<i16>>,
    /// A block held back so it can be paired with its right-hand neighbour for
    /// the two-block AVX2 IDCT. `(component, block_y, block_x, coefficients)`.
    pending: Option<(usize, usize, usize, [i16; 64])>,
}

impl Default for ImmediateWorker {
    fn default() -> Self {
        ImmediateWorker {
            offsets: [0; MAX_COMPONENTS],
            results: vec![Vec::new(); MAX_COMPONENTS],
            components: vec![None; MAX_COMPONENTS],
            quantization_tables: vec![None; MAX_COMPONENTS],
            spare: None,
            pending: None,
        }
    }
}

/// `RUSTY_JPEG_ABLATE=planezero` — restore the unconditional plane memset.
pub(crate) fn ablate_plane_zero() -> bool {
    use std::sync::OnceLock;
    static V: OnceLock<bool> = OnceLock::new();
    *V.get_or_init(|| {
        std::env::var("RUSTY_JPEG_ABLATE")
            .map(|v| v.split(',').any(|t| t == "planezero"))
            .unwrap_or(false)
    })
}

/// `RUSTY_JPEG_ABLATE=planeinit` — see `start_immediate`.
pub(crate) fn ablate_plane_init() -> bool {
    use std::sync::OnceLock;
    static V: OnceLock<bool> = OnceLock::new();
    *V.get_or_init(|| {
        std::env::var("RUSTY_JPEG_ABLATE")
            .map(|v| v.split(',').any(|t| t == "planeinit"))
            .unwrap_or(false)
    })
}

impl ImmediateWorker {
    pub fn start_immediate(&mut self, data: RowData) {
        let _s = crate::prof::scope(crate::prof::Stage::DecPlaneInit);
        assert!(self.results[data.index].is_empty());
        // `RUSTY_JPEG_ABLATE=planeinit` skips sizing/zeroing the output plane.
        // Only valid together with `stores`, since nothing may write into it.
        if crate::decode::idct::ablate_stores() && ablate_plane_init() {
            self.components[data.index] = Some(data.component);
            self.quantization_tables[data.index] = Some(data.quantization_table);
            return;
        }

        self.offsets[data.index] = 0;
        let needed = data.component.block_size.width as usize
            * data.component.block_size.height as usize
            * data.component.dct_scale
            * data.component.dct_scale;

        // Refill a recycled allocation when the caller supplied one: its pages
        // are already resident, which is the entire cost being avoided.
        //
        // When it is ALREADY the right length, leave it completely alone. The
        // plane is exactly the block grid and every block writes its own 8x8,
        // so the decode overwrites all of it -- the `clear()` + `resize(.., 0)`
        // this replaces was a full 3.1 MB memset per 1080p frame whose every
        // byte was then overwritten.
        //
        // Note what this does and does not risk. The bytes stay INITIALISED
        // (they are last frame's pixels), so there is no undefined behaviour and
        // nothing uninitialised can escape; the only exposure would be stale
        // pixels if a block were somehow not written, and a scan that fails to
        // decode returns `Err` rather than handing back a plane.
        // `RUSTY_JPEG_ABLATE=planezero` forces the old always-memset behaviour,
        // so the change can be A/B'd inside ONE binary instead of against a
        // number from an earlier session.
        match data.recycled {
            Some(buf) if buf.len() == needed && !ablate_plane_zero() => {
                self.results[data.index] = buf;
            }
            Some(mut buf) => {
                buf.clear();
                buf.resize(needed, 0u8);
                self.results[data.index] = buf;
            }
            None => self.results[data.index].resize(needed, 0u8),
        }
        self.components[data.index] = Some(data.component);
        self.quantization_tables[data.index] = Some(data.quantization_table);
    }

    pub fn append_row_immediate(&mut self, (index, data): (usize, Vec<i16>)) {
        // Convert coefficients from a MCU row to samples.

        let component = self.components[index].as_ref().unwrap();
        let quantization_table = self.quantization_tables[index].as_ref().unwrap();
        let block_count =
            component.block_size.width as usize * component.vertical_sampling_factor as usize;
        let line_stride = component.block_size.width as usize * component.dct_scale;

        assert_eq!(data.len(), block_count * 64);

        // Two horizontally adjacent full blocks can share one AVX2 instruction
        // stream (see `arch::avx2`). Worth pairing only when BOTH need a real
        // transform: a DC-only block is a fill, which is cheaper than half a
        // vectorized IDCT, and ~31% of blocks on photographic content are.
        let blocks_wide = component.block_size.width as usize;
        let pair_idct = if component.dct_scale == 8 && !crate::decode::idct::ablate_idct() {
            crate::decode::arch::get_dequantize_and_idct_block_8x8_pair()
        } else {
            None
        };

        // Track the block's grid position incrementally instead of recovering it
        // with `i % blocks_wide` / `i / blocks_wide`. Those are integer div/mod
        // by a RUNTIME width, so they do not strength-reduce to shifts -- ~49k
        // of each per 1080p frame.
        let mut i = 0;
        let (mut bx, mut by) = (0usize, 0usize);
        while i < block_count {
            let x = bx * component.dct_scale;
            let y = by * component.dct_scale;
            let coefficients: &[i16; 64] = data[i * 64..(i + 1) * 64].try_into().unwrap();

            crate::prof::bump(crate::prof::Count::DecBlocks, 1);
            let dc_only = crate::decode::idct::is_dc_only(coefficients);
            if dc_only {
                crate::prof::bump(crate::prof::Count::DecDcOnlyBlocks, 1);
            }

            // Pair with the next block when it exists, sits on the SAME block
            // row (so its origin is exactly `dct_scale` bytes along), and also
            // needs a full transform.
            if let Some(idct_pair) = pair_idct {
                if !dc_only && i + 1 < block_count && (i % blocks_wide) + 1 < blocks_wide {
                    let next: &[i16; 64] =
                        data[(i + 1) * 64..(i + 2) * 64].try_into().unwrap();
                    if !crate::decode::idct::is_dc_only(next) {
                        crate::prof::bump(crate::prof::Count::DecBlocks, 1);
                        crate::prof::bump(crate::prof::Count::DecIdctPairs, 1);
                        let _s = crate::prof::scope(crate::prof::Stage::DecIdct);
                        let output =
                            &mut self.results[index][self.offsets[index] + y * line_stride + x..];
                        #[allow(unsafe_code)]
                        unsafe {
                            idct_pair(
                                coefficients,
                                next,
                                quantization_table,
                                line_stride,
                                output,
                                component.dct_scale,
                            );
                        }
                        // The pair is only formed when `bx + 1 < blocks_wide`,
                        // so advancing by two crosses at most one row edge.
                        bx += 2;
                        if bx >= blocks_wide {
                            bx -= blocks_wide;
                            by += 1;
                        }
                        i += 2;
                        continue;
                    }
                }
            }

            let output = &mut self.results[index][self.offsets[index] + y * line_stride + x..];
            let _s = crate::prof::scope(crate::prof::Stage::DecIdct);
            if dc_only && component.dct_scale == 8 {
                // Already scanned above; going through `dequantize_and_idct_block`
                // here would scan all 63 AC coefficients a second time, and on
                // photographic content ~31% of blocks land in this branch.
                crate::decode::idct::fill_dc_only(
                    coefficients,
                    quantization_table,
                    line_stride,
                    output,
                );
            } else {
                dequantize_and_idct_block(
                    component.dct_scale,
                    coefficients,
                    quantization_table,
                    line_stride,
                    output,
                );
            }
            bx += 1;
            if bx == blocks_wide {
                bx = 0;
                by += 1;
            }
            i += 1;
        }

        self.offsets[index] += block_count * component.dct_scale * component.dct_scale;

        // This worker consumed the row synchronously, so the buffer is free
        // now. Keep it for the caller to refill instead of dropping it and
        // making them allocate + zero a replacement for every MCU row.
        self.spare = Some(data);
    }

    /// Inverse-transform one block straight into the plane.
    ///
    /// Holds a block back so horizontally adjacent pairs can go through the
    /// two-block AVX2 kernel, exactly as the row-batched path does.
    #[inline]
    pub(crate) fn fused_block_inner(&mut self, index: usize, block_y: usize, block_x: usize, coeffs: &[i16; 64]) {
        let component = self.components[index].as_ref().unwrap();
        let scale = component.dct_scale;
        let line_stride = component.block_size.width as usize * scale;

        crate::prof::bump(crate::prof::Count::DecBlocks, 1);
        let dc_only = crate::decode::idct::is_dc_only(coeffs);
        if dc_only {
            crate::prof::bump(crate::prof::Count::DecDcOnlyBlocks, 1);
        }
        #[cfg(feature = "counters")]
        {
            if coeffs[32..].iter().all(|&c| c == 0) {
                crate::prof::bump(crate::prof::Count::DecBottomHalfZero, 1);
            }
            if coeffs[8..].iter().all(|&c| c == 0) {
                crate::prof::bump(crate::prof::Count::DecTopRowOnly, 1);
            }
            let span = coeffs.iter().rposition(|&c| c != 0).unwrap_or(0);
            crate::prof::bump(crate::prof::Count::DecCoefSpanSum, span as u64);
        }

        // A DC-only block is a fill, cheaper than half a vectorized IDCT, so it
        // never joins a pair.
        if dc_only || scale != 8 {
            self.flush_pending(index);
            let off = block_y * scale * line_stride + block_x * scale;
            // Borrow the two fields separately. Cloning the Arc instead would
            // put an atomic refcount RMW on a path that runs ~49k times per
            // 1080p frame.
            let qt = self.quantization_tables[index].as_ref().unwrap();
            let out = &mut self.results[index][off..];
            let _s = crate::prof::scope(crate::prof::Stage::DecIdct);
            if dc_only && scale == 8 {
                crate::decode::idct::fill_dc_only(coeffs, qt, line_stride, out);
            } else {
                dequantize_and_idct_block(scale, coeffs, qt, line_stride, out);
            }
            return;
        }

        if let Some((pi, py, px, pcoeffs)) = self.pending.take() {
            if pi == index && py == block_y && px + 1 == block_x {
                if let Some(idct_pair) = crate::decode::arch::get_dequantize_and_idct_block_8x8_pair()
                {
                    let off = py * scale * line_stride + px * scale;
                    let qt = self.quantization_tables[index].as_ref().unwrap();
                    let out = &mut self.results[index][off..];
                    crate::prof::bump(crate::prof::Count::DecIdctPairs, 1);
                    let _s = crate::prof::scope(crate::prof::Stage::DecIdct);
                    #[allow(unsafe_code)]
                    unsafe {
                        idct_pair(&pcoeffs, coeffs, qt, line_stride, out, scale);
                    }
                    return;
                }
            }
            self.emit_single(pi, py, px, &pcoeffs);
        }
        self.pending = Some((index, block_y, block_x, *coeffs));
    }

    fn emit_single(&mut self, index: usize, block_y: usize, block_x: usize, coeffs: &[i16; 64]) {
        let component = self.components[index].as_ref().unwrap();
        let scale = component.dct_scale;
        let line_stride = component.block_size.width as usize * scale;
        let off = block_y * scale * line_stride + block_x * scale;
        let qt = self.quantization_tables[index].as_ref().unwrap();
        let out = &mut self.results[index][off..];
        let _s = crate::prof::scope(crate::prof::Stage::DecIdct);
        dequantize_and_idct_block(scale, coeffs, qt, line_stride, out);
    }

    fn flush_pending(&mut self, _index: usize) {
        if let Some((pi, py, px, pcoeffs)) = self.pending.take() {
            self.emit_single(pi, py, px, &pcoeffs);
        }
    }

    pub fn get_result_immediate(&mut self, index: usize) -> Vec<u8> {
        mem::take(&mut self.results[index])
    }
}

impl Worker for ImmediateWorker {
    fn reclaim_buffer(&mut self) -> Option<Vec<i16>> {
        self.spare.take()
    }

    fn start(&mut self, data: RowData) -> Result<()> {
        self.start_immediate(data);
        Ok(())
    }
    fn append_row(&mut self, row: (usize, Vec<i16>)) -> Result<()> {
        self.append_row_immediate(row);
        Ok(())
    }
    fn get_result(&mut self, index: usize) -> Result<Vec<u8>> {
        self.flush_pending(index);
        Ok(self.get_result_immediate(index))
    }

    fn supports_fused(&self) -> bool {
        true
    }

    fn as_immediate(&mut self) -> Option<&mut ImmediateWorker> {
        Some(self)
    }

}
