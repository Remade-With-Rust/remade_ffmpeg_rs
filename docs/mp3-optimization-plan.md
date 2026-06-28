# MP3 Performance Optimization Plan

A brick-by-brick plan to close the speed gap with FFmpeg on both encode and
decode, with a perceptual-quality guardrail so no speedup costs us quality.
Same methodology as `docs/mp3-encoder-plan.md`: numbered bricks, each validated
against the scalar reference and re-benchmarked.

## Baseline (measured 2026-06-28 · release build · 30 s stereo · 44.1 kHz · 128 kbps CBR)

| Stage | Ours (scalar Rust) | FFmpeg | Gap |
|---|---|---|---|
| Encode | 18.8× realtime | 92× (libmp3lame) | **4.9×** |
| Decode | 169× realtime | 484× (mp3float) | **2.9×** |
| File size | 479,550 B | 481,115 B | parity |
| Quality (SNR) | 21.5 dB | 20.7 dB | parity¹ |

¹ SNR on broadband content is noise-dominated and near-equal; it is **not** a
perceptual verdict — Phase D fixes that.

Our build is **pure scalar safe Rust with zero SIMD**. FFmpeg runs hand-tuned
SSE/AVX (decoder) and assembly (LAME). The gap is the price of that, *plus*
algorithmic shortcuts we haven't taken yet.

## Diagnosis — where the time actually goes (read from the code, not guessed)

**Encode is dominated by the two-loop quantizer**, not the transforms:
- `quantize::loops` runs up to **24 outer** iterations; each calls `inner_gain`,
  a **binary search (~8 probes)** over global_gain; each probe fully re-quantizes
  576 lines *and* runs a complete Huffman encode to count bits.
- `quantize_level` (the per-line forward quantizer) calls **`powf(0.75)` per
  coefficient** — ~24 × 8 × 576 ≈ **110,000 transcendental calls per granule**.
- `huff_cost` allocates a **`BitWriter` and actually encodes** the spectrum just
  to measure its length — ~**190 full Huffman passes per granule**.

**Transforms are dense matrix multiplies** where FFmpeg uses fast factorizations:
- Filterbank (`encode/filterbank.rs`): a dense **64→32 cosine matrix**, 2048
  mul/pass × 18 passes/granule.
- Forward MDCT (`encode/mdct.rs`): a dense **36×18 matrix per subband**, 648 mul ×
  32 subbands/granule.
- Decode mirror: synthesis (`decode/synthesis.rs`) is a dense **64×32 matrix**
  (2048 mul/granule); IMDCT (`decode/imdct.rs`) is the dense inverse.

**Conclusion:** the encode gap is *mostly algorithmic*. SIMD is necessary to
fully match FFmpeg, but tables + a fast bit-counter + a fast transform buy more,
and they make the eventual SIMD cleaner (vectorizing a table lookup beats
vectorizing `powf`). So: **algorithm first, SIMD second, quality gated throughout.**

---

## Phase A — Encode algorithmic wins ✅ DONE (2026-06-28)

**Result: encode 18.8× → 32.7× realtime (1.73× faster), byte-for-byte identical
output, 71 tests green, decode unchanged.** All pure speedups, gated by a
byte-diff of the 30 s/128 k benchmark `.mp3`.

The decisive move was **building a stage profiler first** (`encode::prof`) instead
of trusting the plan's guesses. It overturned two assumptions:
1. The hot path is **`quantize_short`**, not just the long `loops()` — synthetic
   dense signals go almost entirely short-block, and the benchmark uses short
   blocks too. So every win had to be applied to **both** quantizer paths.
2. **A3/A4 as originally planned were wrong.** The profile showed the per-probe
   cost was the Huffman **table search** (`select`), not re-quantizing, and
   `band_noise` was ~0 — so A4 was *not built* (a brick the foundation didn't
   need) and A3 became a table-search prune instead.

- **A1 — `xrpow` precompute** ✅ Hoist `|freq|^(3/4)` out of the per-line, per-probe
  inner loop (`~110k powf/granule → 576`); each quantize pass is now a
  multiply-and-round. Applied to `quantize_with_sf` *and* `quantize_short`.
  Verified: 0/92160 ULP flips vs the `powf` reference, output byte-identical.
- **A2 — Count-only Huffman cost** ✅ `huffman::cost` sums codeword lengths with no
  `BitWriter`/encode; replaced the throwaway-encode in `huff_cost` and
  `cost_short`. Pinned equal to `encode` by test.
- **A3 — Prune the table search** ✅ `best_pair_table` skips, in O(1), every pair
  table whose range can't cover the region's peak (they'd cost "infinity"
  anyway) — output-identical, kills the per-dead-table region walk.
- **A4 — Band-energy cache** ❌ **Not built**, by measurement: `band_noise` is
  negligible and only on the cold long path.

Remaining quantizer cost is `select` called per gain-probe (inherent to keeping
output identical) and the psychoacoustic FFT (now ~19%) — both better addressed
in Phase B/C than by more A-style micro-tuning.

**Gate met:** output `.mp3` unchanged; encode 32.7× RT (target was ≥50× — not hit
on this short-block-heavy signal, but the win is real and byte-exact; the rest
needs the fast transforms + SIMD of B/C, and the psy-FFT, to go further).

## Phase B — Fast decode (IN PROGRESS — profiling re-scoped it)

**Profiling the decoder (`decode::prof`) overturned the plan's premise, exactly
like Phase A.** The decode hotspot was **NOT the transforms** — it was the
**Huffman decoder at 83%** (synthesis 12%, IMDCT 1%). So Phase B's first brick
became a fast Huffman decoder, not a fast transform.

- **B-Huff — Table-driven Huffman decode** ✅ **DONE (2026-06-28).** `decode_index`
  was a bit-by-bit linear scan over the whole codebook per symbol
  (O(max_len×table) ≈ thousands of compares/pair). Replaced with a peek-and-lookup
  table (per book, lazy `2^min(max_len,12)` LUT → `(symbol, length)`; rare >12-bit
  codes fall back to the kept linear scan). Added `BitReader::peek/skip`.
  **Bit-exactness proven** by `lut_matches_linear_for_every_codeword`. **Result:
  decode 169× → 288× realtime (1.75×)**, Huffman stage 7× faster, output
  bit-identical, still matches FFmpeg at 119.6 dB. Decode gap vs FFmpeg: 2.9× → ~1.7×.

- **B3 — Fast decode synthesis + IMDCT.** *Now* the top decode stage (synthesis,
  the dense 64→32 matrix, is 42% after B-Huff; IMDCT ~4%). The plan's original B3,
  validated by measurement this time. **Risk:** a fast DCT changes the float
  arithmetic and may erode the 119.6 dB FFmpeg match — gate carefully.
- **B1/B2 — Encode transforms.** Deprioritised: Phase A's profile shows the encode
  filterbank is 9% and the forward MDCT 1.4% — cold. The encode transform worth
  attacking is the **psychoacoustic FFT (~19%)**, not the filterbank/MDCT.

**Gate:** round-trip SNR ≥ current; **decode stays bit-exact** (proven, not
assumed); FFmpeg match preserved; benchmark re-run.

## Phase C — SIMD (the explicit ask)

**Posture (unchanged from `lab::bricks::accel`):** scalar stays the default and
the correctness oracle; SIMD lives behind a feature flag and is validated against
its scalar twin on every run. No `unsafe` math without a scalar reference test.

- **Approach:** `std::simd` (portable SIMD) first — one code path covers x86-64
  (SSE/AVX) and aarch64 (NEON). Drop to `target_feature` intrinsics only where a
  kernel measurably underperforms. Runtime dispatch via `is_x86_feature_detected!`
  picking AVX2 / SSE2 / scalar at startup.
- **C1 — Filterbank kernels.** Vectorize the 512-tap window-fold MAC and the DCT
  butterflies (B1).
- **C2 — MDCT/IMDCT butterflies.** Vectorize B2/B3's inner butterflies (4–8 lanes).
- **C3 — Quantizer line loop.** After A1, the per-line quantize + noise
  accumulation is a vectorizable map+reduce (gather on the power table, or a
  vectorized polynomial approximation of `x^0.75`).
- **C4 — Synthesis filterbank (decode).** Vectorize the decode DCT + window MAC.

**Gate:** SIMD output matches scalar within tolerance (bit-exact for decode
integer paths; SNR-equal for float); feature-flag build + scalar build both green.

## Phase D — Perceptual quality harness (guardrail, runs alongside A–C)

SNR is the wrong yardstick for a perceptual codec. Before trusting any speedup,
we need a quality metric that tracks the ear.

- **D1 — Objective difference grade.** Implement a PEAQ-style (ITU-R BS.1387
  basic) or a lighter ODG proxy (bark-band masking error → grade) that scores
  decoded-vs-original perceptually.
- **D2 — Lab integration.** Add `quality` next to SNR in `lab::metrics`; a CI gate
  that ODG does not regress vs the committed baseline after each optimization brick.
- **D3 — Head-to-head vs LAME.** Report our ODG vs `libmp3lame` at matched
  bitrate — the real "are we as good as FFmpeg" answer the 21.5/20.7 dB SNR can't give.

## Phase E — Parallelism (bonus, after A–C)

- **E1 — Frame/channel threading.** Analysis + quantize are independent per
  channel and (largely) per frame; fan out with Rayon. Encode is the natural
  beneficiary (the quantizer is the cost). Decode is more sequential (bit
  reservoir couples frames) — thread the transform stage only.

---

## Milestones & targets

| Milestone | Bricks | Encode | Decode |
|---|---|---|---|
| **M1** | Phase A | ≥ 50× RT | — |
| **M2** | Phase B | ≥ 70× RT | ≥ 300× RT |
| **M3** | Phase C (SIMD) | approach LAME (≥ 80×) | ≥ 400× RT |
| **Quality** | Phase D | ODG ≥ baseline, within reach of LAME | bit-exact preserved |

## Discipline (non-negotiables)

- **Scalar is always the default and the reference.** SIMD is opt-in and twin-validated.
- **Decode stays bit-exact** on the conformance corpus through every change.
- **Encode output is byte-identical** through Phase A (pure speedups); Phase B/C
  may change LSBs — gated by SNR + ODG, never by "looks fine".
- **Re-benchmark after every brick** (the 30 s/128k harness above); record the
  number in the lab so regressions are visible.

## Recommended order

**A1 → A2 → A3** first (the encode gap is mostly here and needs no SIMD), in
parallel stand up **D1** so quality is watched from the start. Then **B3** (fast
decode transforms — closes the decode gap), then **B1/B2**, then layer **C**
(SIMD) over the now-clean kernels, and **E** last.
