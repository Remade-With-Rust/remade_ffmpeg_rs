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

## Phase A — Encode algorithmic wins (biggest ROI, no SIMD, fully portable)

The decisions the quantizer makes must not change — these are pure speedups,
gated by **identical encoder output** (byte-for-byte same `.mp3`) plus a re-run
of the benchmark.

- **A1 — Forward power-law table.** Replace `powf(0.75)` in `quantize_level` with
  a lookup (the inverse of the existing `requant_magnitude` `pow43` table —
  LAME's `int2idx`). Build once, index by a scaled magnitude. *Expected: the
  single biggest encode win.*
- **A2 — Count-only Huffman cost.** A `huff_bits(coeffs, table)` that sums
  codeword lengths with **no `BitWriter`, no allocation, no real encode**. Cache
  the table selection so the rate loop reuses it across probes.
- **A3 — Analytic / incremental gain search.** Changing `global_gain` scales the
  step uniformly; quantize once at a reference gain and derive the clip-free and
  budget-fitting gains by scaling, instead of fully re-quantizing per binary
  probe. Cuts the inner loop from ~8 full passes to ~1–2.
- **A4 — Reuse band energies.** Cache `|xr|` and per-band sums across outer
  iterations (only the amplified band changes), so `band_noise` isn't a full
  recompute each pass.

**Gate:** output `.mp3` is unchanged; encode ≥ ~50× RT target (≈2.5× faster).

## Phase B — Fast transforms (encode + decode, still portable)

- **B1 — Fast analysis filterbank.** Replace the dense 64→32 matrix with a fast
  32-point DCT (the standard MP3/AAC filterbank factorization). O(N log N) vs O(N²).
- **B2 — Fast forward MDCT.** Replace the dense 36/12-point matrices with the
  standard DCT-IV/FFT factorization used by decoders, run in reverse.
- **B3 — Fast decode synthesis + IMDCT.** The inverses of B1/B2 — the decode
  hotspots. This is where the 2.9× decode gap closes.

**Gate:** round-trip SNR ≥ current (80.5 dB tone); **decode stays bit-exact on
the conformance corpus**; benchmark re-run. decode ≥ ~300× RT target.

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
