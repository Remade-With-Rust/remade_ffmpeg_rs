//! Feature-gated stage profiler — where the time goes, at zero cost when off.
//!
//! Behind the `profile` feature. With it off, [`scope`] returns a zero-sized
//! guard whose `Drop` is empty, so the optimizer removes it entirely and the
//! shipped build is byte-identical. With it on, each scope accumulates TSC
//! cycles into a per-stage bucket.
//!
//! # Reading a dump
//!
//! Read it top-down and look at the **residue** first — `Total` minus the sum of
//! the named stages. That is where unnamed work hides, and it is usually the
//! most informative line in the table.
//!
//! But a stubborn residue is not automatically work: every scope costs about two
//! `rdtsc` reads, so a stage entered a million times inflates both its own bucket
//! and the residue. Before chasing a residue, compute `calls x ~20ns` and compare.
//! If they match, you are measuring the instrument, and decomposing further will
//! not help.
//!
//! Percentages are what to trust. For absolute throughput, run with the feature
//! **off** and time the whole operation.

/// Pipeline stages worth timing separately.
#[derive(Debug, Clone, Copy, PartialEq, Eq)]
#[repr(usize)]
pub enum Stage {
    /// Whole encode, wrapping everything below.
    Total = 0,
    /// Pulling source rows into per-component buffers (colour conversion for
    /// packed input; plane reads + chroma replication for planar).
    FillBuffers,
    /// Forward DCT.
    Fdct,
    /// Quantization.
    Quantize,
    /// Huffman symbol coding + bit writing.
    Entropy,
    /// The extra statistics pass that optimized Huffman tables require.
    HuffmanOptimize,
    /// Marker/header writing.
    Headers,
    /// Extracting one 8x8 block out of the row buffers (with chroma subsampling).
    GetBlock,
    /// Per-block-row buffer management: clears, edge padding, allocation.
    RowSetup,
    /// Decoder: entropy (Huffman) decode of one block's coefficients.
    DecEntropy,
    /// Decoder: dequantize + inverse DCT of one block.
    DecIdct,
    /// Decoder, **info tier**: the whole `decode_scan` call. Contains
    /// DecEntropy and DecIdct; excluded from sums.
    DecScan,
    /// Decoder: sizing/zeroing the per-component output planes.
    DecPlaneInit,
    /// Decoder: assembling the final image from the worker's planes.
    DecOutput,
    /// **Info tier.** Wraps the whole per-block body, so it CONTAINS GetBlock,
    /// Fdct and Quantize. Excluded from the residue arithmetic — counting it
    /// would double-count its children and drive the residue negative, which is
    /// exactly what happened the first time these scopes were added.
    /// `BlockBody - (GetBlock + Fdct + Quantize)` is the per-block glue.
    BlockBody,
    /// Decoder: obtaining the per-MCU-row coefficient buffer (reclaim + clear +
    /// resize, or a fresh allocation). Per MCU row per component, so the call
    /// count is in the hundreds per frame and the self-tax is negligible.
    DecMcuRowAlloc,
    /// Decoder, **info tier**: the whole per-MCU component/block loop. CONTAINS
    /// DecEntropy; `DecBlockLoop - DecEntropy` is the per-block glue.
    DecBlockLoop,
    /// Decoder, **info tier**: handing one finished MCU row to the worker.
    /// CONTAINS DecIdct; `DecRowDispatch - DecIdct` is the dispatch glue.
    DecRowDispatch,
}

impl Stage {
    /// Stages that nest inside others: shown, never summed.
    pub fn is_info(self) -> bool {
        matches!(
            self,
            Stage::BlockBody | Stage::DecScan | Stage::DecBlockLoop | Stage::DecRowDispatch
        )
    }
}

impl Stage {
    pub const COUNT: usize = 24;
    pub fn name(self) -> &'static str {
        match self {
            Stage::Total => "Total",
            Stage::FillBuffers => "FillBuffers",
            Stage::Fdct => "Fdct",
            Stage::Quantize => "Quantize",
            Stage::Entropy => "Entropy",
            Stage::HuffmanOptimize => "HuffmanOptimize",
            Stage::Headers => "Headers",
            Stage::GetBlock => "GetBlock",
            Stage::RowSetup => "RowSetup",
            Stage::DecEntropy => "DecEntropy",
            Stage::DecIdct => "DecIdct",
            Stage::DecScan => "[i] DecScan",
            Stage::DecPlaneInit => "DecPlaneInit",
            Stage::DecOutput => "DecOutput",
            Stage::BlockBody => "[i] BlockBody",
            Stage::DecMcuRowAlloc => "DecMcuRowAlloc",
            Stage::DecBlockLoop => "[i] DecBlockLoop",
            Stage::DecRowDispatch => "[i] DecRowDispatch",
        }
    }
    // Only the `profile` build walks the stage table by index.
    #[allow(dead_code)]
    fn from_index(i: usize) -> Stage {
        match i {
            0 => Stage::Total,
            1 => Stage::FillBuffers,
            2 => Stage::Fdct,
            3 => Stage::Quantize,
            4 => Stage::Entropy,
            5 => Stage::HuffmanOptimize,
            6 => Stage::Headers,
            7 => Stage::GetBlock,
            8 => Stage::RowSetup,
            9 => Stage::DecEntropy,
            10 => Stage::DecIdct,
            11 => Stage::DecScan,
            12 => Stage::DecPlaneInit,
            13 => Stage::DecOutput,
            14 => Stage::BlockBody,
            15 => Stage::DecMcuRowAlloc,
            16 => Stage::DecBlockLoop,
            _ => Stage::DecRowDispatch,
        }
    }
}

#[cfg(not(feature = "profile"))]
mod imp {
    use super::Stage;

    /// Zero-sized no-op guard. `Drop` is empty, so this compiles away.
    pub struct Guard;

    #[inline(always)]
    pub fn scope(_stage: Stage) -> Guard {
        Guard
    }

    #[inline(always)]
    pub fn reset() {}

    pub fn snapshot() -> [(f64, u64); Stage::COUNT] {
        [(0.0, 0); Stage::COUNT]
    }

    pub fn dump() -> alloc::string::String {
        alloc::string::String::from(
            "profiling disabled — rebuild with `--features profile` to get a breakdown\n",
        )
    }
}

#[cfg(feature = "profile")]
mod imp {
    use super::Stage;
    use alloc::format;
    use alloc::string::String;
    use alloc::vec::Vec;
    use core::sync::atomic::{AtomicU64, Ordering};

    #[allow(clippy::declare_interior_mutable_const)]
    const ZERO: AtomicU64 = AtomicU64::new(0);
    static CYCLES: [AtomicU64; Stage::COUNT] = [ZERO; Stage::COUNT];
    static CALLS: [AtomicU64; Stage::COUNT] = [ZERO; Stage::COUNT];

    /// `rdtsc` (~15 ns) rather than `Instant::now()` (~30 ns on Windows, where it
    /// is `QueryPerformanceCounter`). At these call counts the timer *is* the
    /// overhead, so halving it materially shrinks the phantom residue.
    #[inline(always)]
    fn now() -> u64 {
        #[cfg(all(feature = "profile", any(target_arch = "x86", target_arch = "x86_64")))]
        {
            // SAFETY: `_rdtsc` is a plain register read with no memory operands
            // and no preconditions. Only ever compiled into the dev-only
            // profiling build.
            #[allow(unsafe_code)]
            unsafe {
                #[cfg(target_arch = "x86")]
                use core::arch::x86::_rdtsc;
                #[cfg(target_arch = "x86_64")]
                use core::arch::x86_64::_rdtsc;
                _rdtsc()
            }
        }
        #[cfg(not(any(target_arch = "x86", target_arch = "x86_64")))]
        {
            0
        }
    }

    pub struct Guard {
        stage: usize,
        start: u64,
    }

    impl Drop for Guard {
        #[inline(always)]
        fn drop(&mut self) {
            let elapsed = now().wrapping_sub(self.start);
            CYCLES[self.stage].fetch_add(elapsed, Ordering::Relaxed);
            CALLS[self.stage].fetch_add(1, Ordering::Relaxed);
        }
    }

    #[inline(always)]
    pub fn scope(stage: Stage) -> Guard {
        Guard {
            stage: stage as usize,
            start: now(),
        }
    }

    pub fn reset() {
        for i in 0..Stage::COUNT {
            CYCLES[i].store(0, Ordering::Relaxed);
            CALLS[i].store(0, Ordering::Relaxed);
        }
    }

    /// `(cycles, calls)` per stage — raw, so callers can take medians over runs.
    pub fn snapshot() -> [(f64, u64); Stage::COUNT] {
        let mut out = [(0.0, 0); Stage::COUNT];
        for i in 0..Stage::COUNT {
            out[i] = (
                CYCLES[i].load(Ordering::Relaxed) as f64,
                CALLS[i].load(Ordering::Relaxed),
            );
        }
        out
    }

    /// Measure what one `scope()` actually costs on THIS machine, in cycles.
    ///
    /// Assuming a number here is how a residue gets chased that is really the
    /// instrument: two `rdtsc` reads plus two relaxed `fetch_add`s (`lock xadd`)
    /// is nowhere near free, and the true figure varies by CPU. Calibrate it.
    pub fn scope_cost_cycles() -> f64 {
        const ITERS: usize = 200_000;
        // Warm up, then time an empty scope in a loop and subtract the loop's
        // own cost measured the same way.
        let mut best = f64::MAX;
        for _ in 0..5 {
            let t0 = now();
            for _ in 0..ITERS {
                let g = scope(Stage::Headers);
                core::hint::black_box(&g);
            }
            let t1 = now();
            let with = t1.wrapping_sub(t0) as f64;

            let t0 = now();
            for _ in 0..ITERS {
                core::hint::black_box(0u64);
            }
            let t1 = now();
            let without = t1.wrapping_sub(t0) as f64;

            best = best.min((with - without) / ITERS as f64);
        }
        reset();
        best.max(0.0)
    }

    pub fn dump() -> String {
        let snap = snapshot();
        let total = snap[Stage::Total as usize].0.max(1.0);
        let mut s = String::from("stage             cycles%      Mcycles      calls\n");
        let mut named = 0.0;
        for i in 1..Stage::COUNT {
            let (cy, calls) = snap[i];
            if !Stage::from_index(i).is_info() {
                named += cy;
            }
            s.push_str(&format!(
                "{:<16} {:>7.2}% {:>12.1} {:>10}\n",
                Stage::from_index(i).name(),
                100.0 * cy / total,
                cy / 1e6,
                calls
            ));
        }
        let residue = total - named;
        let scopes: u64 = snap.iter().map(|(_, c)| *c).sum();
        s.push_str(&format!(
            "{:<16} {:>7.2}% {:>12.1} {:>10}\n",
            "residue",
            100.0 * residue / total,
            residue / 1e6,
            "-"
        ));
        s.push_str(&format!(
            "{:<16} {:>7.2}% {:>12.1} {:>10}\n",
            "Total",
            100.0,
            total / 1e6,
            snap[Stage::Total as usize].1
        ));

        // ---- probe-corrected view -------------------------------------------
        //
        // Each scope costs a MEASURED number of cycles, charged to the stage that
        // entered it. That cost is proportional to CALL COUNT, so it distorts
        // stages very unevenly: at 1.36M calls a per-block kernel carries the
        // charge 200,000x more than a per-frame one. Reading the raw column
        // therefore over-ranks fine-grained stages and invents residue. This is
        // the table to act on.
        let cost_per_scope = scope_cost_cycles();
        let mut corrected = [0f64; Stage::COUNT];
        let mut corrected_named = 0.0;
        for i in 1..Stage::COUNT {
            let (cy, calls) = snap[i];
            corrected[i] = (cy - calls as f64 * cost_per_scope).max(0.0);
            if !Stage::from_index(i).is_info() {
                corrected_named += corrected[i];
            }
        }
        let probe_total = scopes as f64 * cost_per_scope;
        let real_total = (total - probe_total).max(1.0);
        let real_residue = (real_total - corrected_named).max(0.0);
        s.push_str("\nprobe-corrected (subtract calls x measured scope cost):\n");
        let mut ranked: Vec<usize> = (1..Stage::COUNT)
            .filter(|i| !Stage::from_index(*i).is_info())
            .collect();
        ranked.sort_by(|a, b| corrected[*b].partial_cmp(&corrected[*a]).unwrap());
        for i in ranked {
            s.push_str(&format!(
                "{:<16} {:>7.2}% {:>12.1}\n",
                Stage::from_index(i).name(),
                100.0 * corrected[i] / real_total,
                corrected[i] / 1e6,
            ));
        }
        s.push_str(&format!(
            "{:<16} {:>7.2}% {:>12.1}\n",
            "residue",
            100.0 * real_residue / real_total,
            real_residue / 1e6,
        ));
        // The instrument's own cost, MEASURED on this machine rather than
        // assumed, so a residue can be judged instead of chased.
        let cost = scope_cost_cycles();
        let probe = scopes as f64 * cost;
        s.push_str(&format!(
            "\n{} scope entries x {:.1} cycles/scope = {:.1} Mcycles = {:.1}% of Total\n\
             \n\
             CAVEAT: {:.1} cycles/scope is measured in an EMPTY loop, so it is an\n\
             UPPER bound - in a real loop the two `lock xadd`s partly overlap with\n\
             surrounding work. The probe-corrected table above therefore\n\
             OVER-subtracts high-call stages. If a per-block kernel there reads\n\
             implausibly cheap, that is this effect, not a fast kernel.\n\
             The fix is fewer scopes, not a better estimate: coarsen per-block\n\
             scopes to per-block-row and the ambiguity goes away.\n",
            scopes,
            cost,
            probe / 1e6,
            100.0 * probe / total,
            cost,
        ));
        s
    }
}

pub use imp::{dump, reset, scope, snapshot, Guard};

/// Free-running counters for *work*, not time.
///
/// A deterministic count is immune to every timing artifact — scheduler
/// migration, frequency drift, cache state — so it is the first instrument to
/// reach for, ahead of any wall or cycle measurement. "Cycles per block" hides
/// how much work a block actually is; "symbols per block" and then "cycles per
/// symbol" is the number that can be reasoned about.
#[derive(Debug, Clone, Copy, PartialEq, Eq)]
#[repr(usize)]
pub enum Count {
    /// Huffman symbols emitted (DC + AC + ZRL + EOB).
    Symbols = 0,
    /// Calls into `write_bits`.
    BitWrites,
    /// Bits emitted, summed.
    Bits,
    /// Times the 64-bit bit buffer was flushed.
    BufferFlushes,
    /// Flushes that hit the slow byte-at-a-time path because of a 0xFF.
    StuffedFlushes,
    /// Non-zero AC coefficients coded.
    NonZeroAc,
    /// Decoder: blocks passed to the IDCT.
    DecBlocks,
    /// Decoder: of those, blocks whose AC coefficients are ALL zero, i.e. the
    /// output is one constant value and the full 8x8 transform is unnecessary.
    DecDcOnlyBlocks,
    /// Decoder: calls into the bit-accumulator refill (`read_bits`).
    DecRefills,
    /// Decoder: single bytes pulled from the reader inside those refills.
    DecBytesRead,
    /// Decoder: Huffman symbols decoded (`decode` + `decode_fast_ac` hits).
    DecSymbols,
    /// Decoder: `receive_extend` calls (raw coefficient bits).
    DecReceiveExtend,
    /// Decoder: refills served by the bulk path rather than the byte loop.
    DecBulkRefills,
    /// Decoder: `decode` calls resolved by the primary LUT in one step.
    DecLutHit,
    /// Decoder: `decode` calls whose code was longer than `LUT_BITS`, falling
    /// into the serial 9..16-bit search. This is the counter that decides
    /// whether widening the LUT is worth anything.
    DecLutMiss,
    /// Decoder: `decode_fast_ac` calls served by the AC LUT.
    DecFastAcHit,
    /// Decoder: `decode_fast_ac` calls that had to fall back.
    DecFastAcMiss,
    /// Decoder: block pairs put through the AVX2 two-block IDCT.
    DecIdctPairs,
}

impl Count {
    pub const COUNT: usize = 24;
    pub fn name(self) -> &'static str {
        match self {
            Count::Symbols => "symbols",
            Count::BitWrites => "bit_writes",
            Count::Bits => "bits",
            Count::BufferFlushes => "buffer_flushes",
            Count::StuffedFlushes => "stuffed_flushes",
            Count::NonZeroAc => "nonzero_ac",
            Count::DecBlocks => "dec_blocks",
            Count::DecDcOnlyBlocks => "dec_dc_only",
            Count::DecRefills => "dec_refills",
            Count::DecBytesRead => "dec_bytes_read",
            Count::DecSymbols => "dec_symbols",
            Count::DecReceiveExtend => "dec_receive_extend",
            Count::DecBulkRefills => "dec_bulk_refills",
            Count::DecLutHit => "lut_hit",
            Count::DecLutMiss => "lut_MISS",
            Count::DecFastAcHit => "fast_ac_hit",
            Count::DecFastAcMiss => "fast_ac_miss",
            Count::DecIdctPairs => "idct_PAIRS",
        }
    }
}

#[cfg(not(feature = "counters"))]
mod counters {
    use super::Count;
    #[inline(always)]
    pub fn bump(_c: Count, _n: u64) {}
    pub fn read() -> [u64; Count::COUNT] {
        [0; Count::COUNT]
    }
    pub fn reset_counts() {}
}

#[cfg(feature = "counters")]
mod counters {
    use super::Count;
    use core::sync::atomic::{AtomicU64, Ordering};
    #[allow(clippy::declare_interior_mutable_const)]
    const ZERO: AtomicU64 = AtomicU64::new(0);
    static COUNTS: [AtomicU64; Count::COUNT] = [ZERO; Count::COUNT];

    #[inline(always)]
    pub fn bump(c: Count, n: u64) {
        COUNTS[c as usize].fetch_add(n, Ordering::Relaxed);
    }
    pub fn read() -> [u64; Count::COUNT] {
        let mut out = [0; Count::COUNT];
        for i in 0..Count::COUNT {
            out[i] = COUNTS[i].load(Ordering::Relaxed);
        }
        out
    }
    pub fn reset_counts() {
        for c in COUNTS.iter() {
            c.store(0, Ordering::Relaxed);
        }
    }
}

pub use counters::{bump, read, reset_counts};

#[cfg(all(test, feature = "profile"))]
mod nesting_tests {
    use super::*;

    /// A parent scope must never measure fewer cycles than a scope nested
    /// inside it. When `DecRowDispatch` read 65.6 Mcycles against a nested
    /// `DecIdct` at 204.7, this test is what decided whether the profiler or
    /// the placement was at fault.
    #[test]
    fn parent_scope_is_never_smaller_than_its_child() {
        reset();
        for _ in 0..200 {
            let _outer = scope(Stage::DecRowDispatch);
            for _ in 0..50 {
                let _inner = scope(Stage::DecIdct);
                core::hint::black_box(0u64);
            }
        }
        let snap = snapshot();
        let outer = snap[Stage::DecRowDispatch as usize].0;
        let inner = snap[Stage::DecIdct as usize].0;
        assert!(
            outer >= inner,
            "parent {outer} < nested child {inner} - the profiler itself is unsound"
        );
    }
}
