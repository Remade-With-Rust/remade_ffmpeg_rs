# rusty_h265 — every scalar implementation, and what happens to it

**Measured 2026-09-05**, `in_to_tree_720p_8bit.hevc` (x265 medium crf24, 60 frames),
pinned core, High priority, CPU time, decode only (output discarded), ABBA over 3
rounds. Shares come from **stage ablation on the uninstrumented binary** — each arm
parses every bin (HEVC entropy decode does not depend on reconstructed samples, so
the work counts are identical) and skips one pixel stage:

| arm | best ms | stage ms | share |
|---|---:|---:|---:|
| full decode | 3,750 | — | — |
| no motion compensation | 1,453 | 2,297 | **61.3 %** |
| no residual (scale + inverse transform + add) | 2,969 | 781 | **20.8 %** |
| no intra prediction | 3,063 | 688 | **18.3 %** |
| no SAO | 3,188 | 563 | **15.0 %** |
| no deblocking (+SAO) | 3,094 | 656 | 17.5 % (deblock ≈ 2.5 %) |
| parse only (no pixels at all) | 266 | 3,484 | 92.9 % |

Shares overlap — removing one stage relieves cache pressure on its neighbours — so
this is a **ranking**, not an additive budget. The one hard number: **7.1 % of decode
is entropy decoding and syntax**; everything else is pixels, and pixels are what
kernels are for.

## The list

`ceiling` = what the stage could give if its cost went to zero, from the ladder above.
`plan` = what the brick actually is: **R** = redundancy/structure (do first, no
`unsafe`), **K** = SIMD kernel, **A** = algorithmic, `—` = leave alone.

### Motion compensation — `src/mc.rs`, `src/ctu/inter.rs` (61.3 %)

| # | scalar implementation | shape | plan |
|---|---|---|---|
| 1 | `interpolate`, full-pel copy path | copy + shift, no filter | K |
| 2 | `interpolate`, horizontal-only 8-tap luma / 4-tap chroma | FIR across a row | K |
| 3 | `interpolate`, vertical-only | FIR down a column, unit stride per tap | K |
| 4 | `interpolate`, 2-D (H pass → `tmp`, then V pass) | two FIRs + a scratch buffer | K |
| 5 | `fetch()` — per-sample `clamp` of both coordinates | 2 branches **per tap per sample** | **R** |
| 6 | `vec![0i32; bw*bh]` per prediction unit **per plane**, plus `tmp` in the 2-D path | heap traffic in the hottest loop in the decoder | **R** |
| 7 | `weighted_write`, uni-prediction default | shift + clamp, a **second pass** over the block | **R** then K |
| 8 | `weighted_write`, bi-prediction default | average of two 14-bit buffers | K |
| 9 | `weighted_write`, explicit weights (uni and bi) | multiply-add + clamp | K |

The three **R** items are the ones Step 0 of `codec-vectorize-kernel` insists on
first: a per-sample clamp and a heap allocation per prediction unit are not things a
wider register fixes.

### Residual — `src/itx.rs`, `src/ctu/residual.rs` (20.8 %)

| # | scalar implementation | shape | plan |
|---|---|---|---|
| 10 | `dequant` | multiply, rounding shift, clamp per coefficient | K |
| 11 | `idct_1d` | **naive N×N matrix multiply** — 32 multiplies per output for a 32-point transform | **A** (partial butterfly) then K |
| 12 | `idst_1d` | 4×4 matrix multiply | K |
| 13 | `inverse_transform`, transform-skip path | shift + round | K |
| 14 | residual add-and-clamp into the plane | elementwise add + clamp | K |

Item 11 is the one to fix before touching intrinsics: the spec's even-odd
decomposition is the reason every real decoder's inverse transform is a butterfly,
and it removes work rather than parallelising it.

### Intra prediction — `src/intra.rs`, `src/ctu/residual.rs` (18.3 %)

| # | scalar implementation | shape | plan |
|---|---|---|---|
| 15 | reference gathering calls `PicState::available()` **once per reference sample** | z-scan lookup, CTB lookup, slice and tile compare — per sample | **R** (availability is per 4×4, not per sample) |
| 16 | `substitute` | backward fill of unavailable samples | — (short, runs once per block) |
| 17 | `filter` — 3-tap smoothing, and the strong bilinear at 32×32 | FIR over ≤ 128 samples | K |
| 18 | planar prediction | two linear ramps per row | K |
| 19 | DC prediction + the three edge filters | reduction, then fill | K |
| 20 | angular prediction | build the projected reference, then a 2-tap interpolation per sample | K |

### Loop filters — `src/filters.rs` (17.5 %)

| # | scalar implementation | shape | plan |
|---|---|---|---|
| 21 | boundary-strength derivation | per 4×4, branch-heavy, ~1/16 of samples | — |
| 22 | `filter_luma_edge` | 4-line segment, strong/normal decision | K (low priority) |
| 23 | `filter_chroma_edge` | 2-line segment | — |
| 24 | SAO band offset | per sample: band lookup, add, clamp | K |
| 25 | SAO edge offset | per sample: two neighbour compares + availability | **R** then K |
| 26 | **`let src = planes[c].clone()`** — a full plane copy per component per picture | pure memory traffic, allocated fresh each time | **R** |

### Parsing and bookkeeping (7.1 % total — mostly not kernel work)

| # | scalar implementation | shape | plan |
|---|---|---|---|
| 27 | `Cabac::decode` / `bypass` / `terminate` | inherently serial arithmetic coder | — |
| 28 | residual coefficient scan loops | data-dependent, serial | — |
| 29 | `PicState::fill4` | small rectangular fills of per-4×4 maps | — |

### Output paths (outside the decode measurement, paid by every caller)

| # | scalar implementation | shape | plan |
|---|---|---|---|
| 30 | `Frame::write_yuv` | crop + narrow `u16` → `u8` | K |
| 31 | `frame_to_rff` in the adapter | the same crop + narrow, again | K |
| 32 | `narrow_to_8` in `rff-filter` | rounding shift `u16` → `u8` | K |
| 33 | `sei::compute` MD5 / CRC / checksum | only under `verify_sei` | — |

## Order of execution

Arithmetic first (`codec-measurement`: prune before building). Expected whole-decode
gain = stage share × (1 − 1/speedup):

| brick | share | needed speedup | whole-decode gain |
|---|---:|---|---:|
| MC structure (5, 6, 7) | 61.3 % | 2× | 1.44× |
| MC kernels (1–4, 8, 9) | 61.3 % | 4× total | up to 1.72× |
| inverse transform butterfly (11) | 20.8 % | 3× | 1.16× |
| intra availability (15) | 18.3 % | 2× | 1.10× |
| SAO (24, 25, 26) | 15.0 % | 3× | 1.11× |

Everything below ~2 % of decode (deblocking, `fill4`, chroma edges) is **not worth a
kernel** and is recorded here so nobody re-derives that conclusion.

## Verdicts — what was built, and what it measured

Every brick below was gated on the **full 147-stream JCT-VC HEVC_v1 suite, bit-exact**
before its number was taken, then measured pinned to core 4 at High priority on CPU
time, ABBA-interleaved, decode only. `z = (wins − N/2) / (0.5·√N)`.

**Null arm on this box, 21 pairs: median 1.000×, z = −0.22, p25–p75 0.985–1.031.**
That ±3 % is the resolution floor every verdict below is judged against.

| brick | items | N | wins | z | paired median | verdict |
|---|---|---:|---:|---:|---:|---|
| MC structure + kernels | 1–9 | 21 | 21 | +4.58 | **1.306×** | keep |
| sparse inverse transform | 10, 11, 13 | 13 | 13 | +3.61 | **1.133×** | keep |
| intra availability per 4×4 | 15 | 15 | 11 | +1.81 | 1.069× (all-intra) | keep, content-dependent |
| SAO structure (interior/ring split, reused scratch) | 26 | 13 | 13 | +3.61 | **1.282×** | keep |
| SAO band + edge kernels | 24, 25 | 21 | 19 | +3.71 | **1.136×** | keep |
| intra kernels — stage isolated | 18, 19, 20 | 24 | 21 | +3.67 | **1.068×** | keep |
| intra kernels — all-intra pipeline | 18, 19, 20 | 21 | 16 | +2.40 | **1.031×** | keep, content-dependent |
| intra kernels — inter pipeline (control) | 18, 19, 20 | 16 | 7 | −0.50 | 1.000× | no regression |

**Cumulative: 3,750 ms → 828 ms best (≈4.5×)** on the same stream and instrument, with
all 147 streams still bit-exact and the SEI picture hashes still 100/100.

Two results are worth keeping for their method rather than their size:

- **The MC brick's first reading was 3/9 wins, z = −1.00 — a flat refutation of what
  turned out to be a 1.3× win.** The census showed the *scalar* arm reporting
  `MC_LUMA_SIMD = 87903`: `rusty_h265-accel` declares `default = ["simd"]` and the
  consumer inherited it. Adding `default-features = false` to
  `crates/rusty_h265/Cargo.toml` changed nothing, because **Cargo ignores
  `default-features = false` on an inherited workspace dependency unless the workspace
  entry declares it too.** A byte census, not an argument about the call graph, is
  what separates "the kernel doesn't help" from "neither arm reaches the kernel".
- **The intra brick is why the ablation ladder cannot be trusted across a pixel
  stage.** `no intra` measured a *negative* share (−15.6 %, later −21.6 %): the loop
  filters do data-dependent work, so ablating a pixel stage changes how much work the
  filters downstream of it do. `.tmp/stage.ps1` now measures the pixel stages against
  a **no-loop-filter baseline**, where there is no downstream stage left to
  contaminate. That fixed the *validity* — but see below, it did not fix the ladder.

### Three instrument faults, all found by chasing an impossible number

None of these changed a line of decoder code; all three would have changed a verdict.

1. **The unpaired min-of-N ladder does not resolve anything here.** Even restricted to
   valid arms, it still read `no intra` as −21.6 %, and read two *identical*
   configurations 5.7 % apart. The same question asked with the paired harness
   (`.tmp/ab.ps1`, ABBA, per-round ratios) answered **1.018×, z = 1.09** — noise, as
   it should be. **Treat the ladder as a ranking of large stages and nothing more; any
   share under ~20 % must come from the paired instrument.**
2. **Windows CPU time is quantised to ~15.6 ms, and the harness was scoring ties.** A
   null arm on a 300 ms stream read **z = −2.40 for identical code** — a "significant"
   result manufactured entirely out of timer resolution, because every exact tie was
   counted as a win for arm A. `ab.ps1` now excludes ties, runs the sign test over the
   pairs that actually differ, and warns when too few differ. The other half of the
   fix is the workload: 24 concatenated copies of `IPRED_A_docomo_2` (480 frames,
   `errors=0`, ~13 s) instead of one 20-frame copy.
3. **A `[u16; 32*32]` stack scratch is a 2 KB memset per call.** The first version of
   the intra brick predicted into a whole-block temp before transposing. For the 4×4
   blocks that dominate intra content that is **128 bytes of memset per predicted
   pixel**, and it ate the entire kernel win: the isolated stage measured **6/11,
   z = 0.30** — flat, and a refutation if recorded. Moving the transpose inside the
   kernel so the scratch is one 8×8 tile (128 bytes) turned the same brick into
   **10/11, z = 2.71**, and a 4×4 in-register transpose took it to 21/24, z = 3.67.
   This is the three-probe rule earning its keep: probes 1 and 2 both said "flat", and
   both were measuring a defect in the arm rather than the idea.

### Reachability

The census (`RH265_CENSUS=1`, or the `census` feature) counts calls down each path on
a real decode. On `in_to_tree_720p_8bit.hevc`, 60 frames:

| counter | kernel arm | scalar arm |
|---|---:|---:|
| `SAO_BAND_SIMD` / `SAO_BAND_SCALAR` | 524 / 0 | 0 / 0 |
| `SAO_EDGE_SIMD` / `SAO_EDGE_SCALAR` | 5,966 / 0 | 0 / 0 |
| `SAMPLES_SAO` | 20,384,632 | 0 |

And on `IPRED_A_docomo_2`, against `RH265_SCALAR_INTRA=1`:

| counter | kernel arm | scalar arm |
|---|---:|---:|
| `INTRA_ANGULAR_SIMD` / `_SCALAR` | 166,038 / 0 | 0 / 0 |
| `INTRA_PLANAR_SIMD` / `_SCALAR` | 67,952 / 0 | 0 / 0 |
| `INTRA_DC_SIMD` / `_SCALAR` | 47,065 / 0 | 0 / 0 |
| `SAMPLES_INTRA` | 11,980,800 | 0 |

The scalar arm reaching **zero** kernel calls is the point: it proves the two arms of
the A/B are genuinely different code, which is exactly what the MC brick's first
reading failed to establish. Both arms are also the *same binary* under two env
settings, so no stale build or differing inline decision can masquerade as an effect.

### Where the time goes now, and why the list stops here

Re-measured on the final binary. Loop-filter shares from the ladder (valid: nothing
runs after them); the pixel shares from the **paired** harness, which is the only
instrument that resolves anything at this size.

| stage | share of decode | instrument |
|---|---:|---|
| parse + CABAC + bookkeeping | **29.0 %** | ladder, `parse only` |
| SAO | 11.3 % | ladder (was 19.7 % before its two bricks) |
| deblocking | 6.4 % | ladder, `no deblock+SAO` minus `no SAO` |
| intra prediction, inter content | ~1.8 % | paired, z = 1.09 — inside noise |
| intra prediction, all-intra content | **27.3 %** | paired, 15/15, z = 3.87 |

Everything still marked **K** in the tables above was priced against that, using
`expected gain = share × (1 − 1/speedup)` and the measured **±3 % null floor**:

| item | share | speedup a kernel could give | expected gain | decision |
|---|---:|---|---:|---|
| 22 deblocking luma edge | 6.4 % | 2× (branchy 4-line segments) | 3.2 % | **at the floor — not built** |
| 12 4×4 inverse DST | inside residual, 4×4 only | 2× | < 1 % | **not built** |
| 17 reference-sample filter | ≤ 128 samples once per block | 3× | < 1 % | **not built** |
| 30–32 output narrowing `u16`→`u8` | outside decode; ~55 M samples per 60 frames | 4× | ~1 % of CLI wall | **not built** |
| 27–29 CABAC, scan loops, `fill4` | 29 % combined | — serial by construction | — | **not a kernel job** |

The honest statement about the parse stage: at 29 % it is now the largest single item
in the decoder, and it is the one place where no kernel applies. An arithmetic coder
is a serial dependency chain; the lever there is algorithmic (fewer bins, better
context handling), not wider registers.

## Instruction-level pass: 20 deterministic wins

The bricks above were judged by a clock. Below the ~3 % null floor a clock cannot
judge anything, and every one of these changes is smaller than that. So the
instrument changes: **`tools/hevc/kernel_icount.py` counts the instructions in each
kernel's innermost vector loop, straight out of the emitted assembly, and divides by
the samples that loop stores.** Same toolchain and same source give the same number
every run, on any machine, under any load. A win is that number going down while the
`*_matches_scalar` oracles and the 147-stream conformance gate stay green — which
they did, for all twenty, at every step.

**All 20 were gated together at the end: 147/147 bit-exact, SEI hashes 100/100, 13
kernel oracle tests, 29 decoder unit tests.** The census confirms every kernel call
on a real decode takes a vector path and none falls back to scalar.

| # | change | kernel | instructions / samples | → |
|---|---|---|---|---|
| 1 | SAO band offset: the four bands are **consecutive**, so the distance from `sao_band_position` *is* the table index — one `pshufb` replaces 4 compares, 4 masks and 3 ors | `band_avx2` | 23 → 18 per 16 | −5 |
| 2 | SAO edge offset: `edgeIdx` is already 0..4, so it indexes the same shuffle table directly — no compares at all | `edge_avx2` | 35 → 24 per 16 | −11 |
| 3 | Counted vector loop (one induction variable) — SAO AVX2 | `band_avx2`, `edge_avx2` | 18 → 15, 24 → 21 | −3 each |
| 4 | Counted vector loop — pixel kernels | `put_uni` 15 → 9 / 10 → 8, `put_bi` 17 → 11 / 12 → 10, `add_residual` 12 → 10 | | −2..−6 |
| 5 | Counted vector loop — every FIR variant | `fir_h`/`fir_v`, SSE2 and AVX2, 4- and 8-tap | 8 kernels | **−6 each** |
| 6 | Angular filter never leaves `i16`: `(32−f)·a + f·b = 32a + f·(b−a)`, and `32a` factors out of the shift exactly | `angular_sse2` 18 → 13, `angular_t_sse2` 178 → 127 | | −5, **−51** |
| 7 | Zero-shift horizontal FIR — `shift1 = BitDepth − 8` is **0 for 8-bit** | `fir_h_avx2` 23 → 21, 15 → 13; `fir_h_sse2` 32 → 30, 20 → 18 | | −2 each |
| 8 | AVX2 angular (there was none) | `angular` | 1.625 → **0.688** /sample | −58 % |
| 9 | AVX2 planar (there was none) | `planar` | 4.750 → **1.500** /sample | −68 % |
| 10 | Two output rows per pass in the vertical FIR: windows overlap in `N − 1` rows, so **9 loads replace 16** | `fir_v_avx2` | 2.750 → 2.250 /sample | −18 % |
| 11 | **Full-pel copy had no kernel at all** — integer motion vectors went one sample at a time | `copy_shift` | scalar → **0.375** /sample | new |
| 12 | Two output rows per pass, SSE2 | `fir_v_sse2` 6.125 → 5.000, chroma 2.875 → 2.750 | | −18 % |
| 13 | Counted vector loop — SAO SSE2 | `band_sse2` 26 → 24, `edge_sse2` 40 → 38 | | −2 each |
| 14 | AVX2 transposed angular: two 8×8 tiles per strip, so the rows go 16 wide | `angular_t` | 1.891 → **1.27** /sample | −33 % |
| 15 | **DC fill emitted zero vector instructions** — `slice::fill` did not lower across the call boundary | `dc_fill` | 1 → **16** samples per store | new |
| 16 | Zero-shift vertical FIR (the vertical-only path also shifts by `shift1`) | `fir_v_avx2` 72 → 68, 40 → 36; `fir_v_sse2` 80 → 76, 44 → 40 | | −4 each |
| 17 | Two vectors per trip — the loop bookkeeping was ~40 % of the shortest kernels | `put_uni_avx2` 0.500 → **0.406**, `put_bi_avx2` 0.625 → **0.531** | | −19 %, −15 % |
| 18 | Two vectors per trip — residual add | `add_residual_avx2` | 0.625 → **0.531** | −15 % |
| 19 | Two vectors per trip — SAO | `band_avx2` 0.938 → 0.844, `edge_avx2` 1.312 → 1.219 | | −10 %, −7 % |
| 20 | Non-wrapping `sao_band_position` (28 of 32 values) needs no `mod 32` | `band_avx2` | 0.844 → **0.781** | −7 % |

### A second pass: 10 more (21–30)

Same instrument, same gate. **All ten gated together: 147/147 bit-exact, 14 kernel
oracle tests, 29 decoder tests.**

| # | change | kernel | instructions / samples | → |
|---|---|---|---|---|
| 21 | Two vectors per trip — horizontal FIR | `fir_h_avx2` 1.438 → **1.344**, chroma 0.938 → **0.844**; zero-shift twins 1.312 → 1.219, 0.812 → 0.719 | | −7 % |
| 22 | **Full-pel uni-prediction is the identity.** `copy_shift` writes `s << k` and `put_uni` computes `(v + 2^(k−1)) >> k` with the *same* `k`, so the pair is `s` exactly — an integer motion vector needs a rectangle copy, not two kernel passes | `copy_block` | 0.781 → **~0.1** /sample | −87 % |
| 23 | **Full-pel bi-prediction is `pavgw`.** The same composition one step on: `(s0·2^k + s1·2^k + 2^k) >> (k+1) = (s0+s1+1) >> 1` | `avg_block` | 1.281 → **~0.25** /sample | −80 % |
| 24 | Two vectors per trip — full-pel copy | `copy_shift_avx2` 0.375 → **0.281**, `copy_shift_sse2` 0.750 → **0.562** | | −25 % |
| 25 | Two vectors per trip — SSE2 uni/bi write | `put_uni_sse2` 1.125 → **0.938**, `put_bi_sse2` 1.375 → **1.188** | | −17 %, −14 % |
| 26 | Two vectors per trip — SSE2 residual add | `add_residual_sse2` | 1.375 → **1.188** | −14 % |
| 27 | Two vectors per trip — SSE2 SAO | `band_sse2` 3.000 → **2.812**, `edge_sse2` 4.750 → **4.562** | | −6 %, −4 % |
| 28 | Two vectors per trip — angular rows, AVX2 | `angular_avx2` | 0.688 → **0.594** | −14 % |
| 29 | Two vectors per trip — angular rows, SSE2 | `angular_sse2` | 1.625 → **1.188** | −27 % |
| 30 | Planar narrows 16 lanes with one `packs` + one `permute` instead of half-filling a store twice | `planar_avx2` | 1.500 → **1.125** | −25 % |

Wins 22 and 23 are the interesting pair. Both come from noticing that two kernels
compose to something far cheaper than either — the interpolation filter's full-pel
"filter" and the prediction write cancel out. `full_pel_composition_is_identity` pins
that against the general path for both bit depths and six block sizes, because a fast
path justified by algebra is exactly the kind that rots silently. On the 720p bench
stream the census shows it taking 616 uni and 1,575 bi blocks off the filter path.

**One probe refuted, recorded as such:** rewriting the intra reference-buffer build
from indexed stores to slice copies, expecting LLVM to emit a vector copy, moved
`intra::predict` from 980 instructions to 981. Kept because it is equivalent and
clearer, but it is not a win and is not counted as one.

**And one kernel deliberately not written:** explicit weighted prediction. The census
reads `PUT_WEIGHTED = 0` on the corpus — a kernel there would be unreachable code,
which is the defect this whole discipline exists to catch, not a win.

Four techniques account for all of it, and only one is "write more SIMD":

- **An algebraic identity that shrinks the data type** (6). Halving the width of the
  intermediate is worth more than doubling the register, because it removes the
  widening and narrowing either side of it as well.
- **A property of the bitstream syntax the kernel can exploit** (1, 2, 7, 16, 20):
  bands are consecutive, `edgeIdx` is already an index, `shift1` is zero at 8-bit,
  `sao_band_position` rarely wraps. Each turns a general computation into a lookup or
  removes it outright.
- **Loop shape** (3, 4, 5, 13, 17, 18, 19): a counted loop leaves LLVM one induction
  variable instead of three, and pairing iterations halves what is left. Nothing about
  the arithmetic changes.
- **Reuse and reach** (10, 12 share loads; 8, 9, 14 widen; 11, 15 are kernels that
  did not exist). Note that two of the twenty are not optimisations at all but
  **defects** — a hot path with no kernel, and a fill the compiler declined to
  vectorise. Both were found by reading the emitted assembly, neither by profiling.

### What the clock says, and why it is not the evidence

Confirmatory only, and it neither contradicts nor resolves: the all-intra paired A/B
reads 1.019× (11/15, z = 1.81) against a ±3 % null floor. That is what §15 predicts —
at this size the clock cannot promote itself to the verdict however many pairs it
runs. In the same session the decoder measured **2.24× faster than `hpvcd`** (was
1.63×), but ffmpeg's own throughput on this box moved 297 → 188 ms between sessions,
so no cross-session ratio is quoted: a denominator that swings 37 % cannot measure a
7 % change.

### Three instrument faults, again

Building the counter cost three broken readings before it produced a trustworthy one,
each caught by a number that could not be true:

1. **Every vector kernel reported 0 SIMD instructions.** A `\b` written through a
   shell heredoc became a literal backspace in the regex.
2. **`angular_t` reported a 527-instruction "inner loop", and `band_avx2` a
   51-instruction one that was the function epilogue.** LLVM lays exit blocks out
   early, so a backward jump is not necessarily a loop — no loop body contains a
   `ret`. Then "innermost by line containment" broke the other way when a scalar loop
   landed inside a vector loop's line range, and `add_residual` reported a
   5-instruction bounds check with no vector instructions in it.
3. **A real 18 % win displayed as a 64 % regression.** The outputs-per-iteration
   denominator was a hand-maintained constant, and the two-row FIR started storing 32
   samples per trip where the table still said 16. It is now derived from the loop's
   own stores and cannot go stale.

### A third pass: 10 compositions (31–40)

The first two passes made each kernel cheaper. This one asks a different
question — **where does one stage's output feed straight into another's input,
and what do the two collapse to?** The answers were mostly not in the kernels at
all.

**All ten gated together: 147/147 bit-exact, 16 kernel oracle tests, 29 decoder
tests.**

| # | composition | what collapses | scale |
|---|---|---|---|
| 31 | **Mixed bi-prediction folds its shift.** One list full-pel, one not: the full-pel list ran a whole `copy_shift` pass whose only work was `s << k`, which the bi write can do itself | `copy_shift` + `put_bi` → `put_bi_fp` | 3,441 blocks; `MC_COPY` 8,552 → 5,111 |
| 32 | **Intra reference samples reused across blocks.** `RefSamples::new()` per block zeroed ~388 bytes — 24 bytes of memset per predicted sample on a 4×4 | allocation + use → one allocation | ~388 B → `4n+1` B per block |
| 33 | **`substitute` short-circuits.** The gather already derives availability per 4×4 to build the arrays; it now says whether anything was missing, and in a picture's interior nothing ever is | gather + substitute | skips `4n` scans + two fill loops |
| 34 | **The inverse transform's stage-1 scratch was a 4 KB local.** Zeroed per transform block — 256 bytes of memset per coefficient on a 4×4 — and never read before it was written | allocation + use → one allocation | **197 MB of memset removed** (48,092 TBs × 4 KB) |
| 35 | **Deblocking strength buffers reused across pictures** rather than allocated per picture | two `vec![0u8; w4*h4]` per picture → reused | 115 KB/picture at 720p |
| 36 | **`predict`'s own scratch carried in the reused struct** — the angular projected reference (`[0i16; 128]`) and the planar narrowing buffers | allocation + use → one allocation | 256 + 132 B per block; `predict` now has **zero** memsets |
| 37 | **A partition mode yields at most four rectangles** — and allocated a `Vec` to iterate them | heap alloc + free per inter CU → a fixed array | one malloc/free per inter CU |
| 38 | **The reference-smoothing double buffer** (`[0u16; 64]` twice) moved into the same reused struct | allocation + use → one allocation | 256 B per filtered block |
| 39 | **Merge and MVP candidate lists are bounded by the spec** (five and three) and never leave the function that builds them | four `Vec`s per PU → fixed-capacity arrays | **four** mallocs per prediction unit |
| 40 | **DC prediction + residual add fused.** DC writes one constant across the block and the residual add loads every one of them straight back; composed, `dst = clamp(dc + res)` needs neither the fill nor the load | `dc_fill` + `add_residual` → `add_residual_const` | 5,921 blocks on all-intra |

Two of these are algebraic (31, 40) and both carry a test that pins the fused
form against the composition it replaces — `put_bi_fp_matches_the_composition`
and `add_residual_const_matches_fill_then_add` — because a fast path justified
by algebra is exactly the kind that rots silently.

The other eight are one observation wearing eight hats: **a buffer whose
lifetime is a call, but whose size is a block.** Every one was a `let mut x =
[0; N]` or a `Vec` that the profiler would never name, because the cost is
spread across every block rather than concentrated in a function. They were
found by reading for *allocations* rather than for hot loops, and together they
removed a few hundred megabytes of `memset` per sixty frames.

Fusion 40 is deliberately narrow: the §8.4.4.2.5 DC edge fix-ups make luma
blocks below 32×32 non-constant, so only chroma DC and 32×32 luma DC can defer.
Claiming it more widely would have been wrong.

### What it added up to

Measured after this pass, pinned CPU time, best of 9, with **ffmpeg reading
exactly 297 ms in this run and in the pre-campaign one** and a null arm of
1.000× — so the two are comparable:

| | ours | ffmpeg | × ref |
|---|---:|---:|---:|
| after the six kernel bricks | 1,016 ms | 297 ms | 3.42× |
| after 40 instruction/composition wins | **734 ms** | 297 ms | **2.47×** |

That is **1.38× on the whole decoder** from work that individually sat below the
clock's resolution, and it is why the counter had to be the instrument. Against
`hpvcd` — the mature C++ decoder with its own SSE/AVX kernels that the original
decision rule said to fork — we are now **2.24× faster**.

### The measurement was under the wrong allocator

Found by Tim asking whether the campaign complied with `building-the-new-internet`.
CLAUDE.md is explicit:

> Performance measurements (codec-* skill campaigns, best-of-N benches, A/B arms)
> must run under rusty_alloc, since it is what ships — an arm measured under the
> system allocator is not comparable.

**`rusty_h265.exe`, the binary every timing number in this campaign came from, had
no `#[global_allocator]`.** The shipping path (`rff-cli`) has run under rusty_alloc
since 2026-08-07; the standalone bench binary never did. Measured, paired, 15
rounds: **rusty_alloc is 1.133× faster than the system allocator on this decoder
(13/15, z = 2.84)** — larger than most of the wins that were being measured against
it.

What this does and does not invalidate:

- **The instruction counts stand.** They are static counts from the emitted
  assembly and do not know what an allocator is.
- **The clock numbers were all taken under the slower, non-shipping allocator**, so
  they understated the product — and, more importantly, they *overstated* the
  allocation-removal compositions (37, 39), because a `malloc` under the Windows
  heap costs more than one under a mimalloc remake. Those wins were justified by
  allocation *counts* rather than clocks, so the deterministic claim survives; the
  aggregate clock attribution shifts toward the kernels.

Fixed by an optional `bench-alloc` feature on `rusty_h265` — optional so the
published library stays dependency-free, since a library must never hijack a
downstream binary's allocator choice. `tools/hevc/pinbench.ps1` now documents that
the binary must be built with it.

### Transcendentals: the decoder has none, and that is the finding

Searched under `rusty-fast-transcendentals`. The skill targets operations with **no
SIMD instruction** — `exp`, `ln`, `powf`, `tanh` — because a libm call per element
keeps a loop scalar however many lanes are idle.

**`rusty_h265` and `rusty_h265-accel` contain zero `f32`, zero `f64` and zero
transcendental calls.** HEVC reconstruction is integer by construction; that is what
makes bit-exactness possible. There is nothing to fix and nothing was invented.

But the skill's *principle* transfers, and this is where it paid: the integer twin
of a libm call is **`idiv`**. x86 has no integer divide in SSE or AVX at all, and it
is 20–40 cycles, unpipelined. Counting `idiv`/`div` in the emitted assembly is the
same instrument as counting packed ops:

| # | division removed | why it was avoidable | site |
|---|---|---|---|
| 41 | `32 / n` in `idct_1d` | `n` is always 4/8/16/32 — a power of two the compiler cannot see | every column and row of every transform block |
| 42–43 | `(yb + k) % run`, `(xb + k) % run` | `run` is 4 or 2 — a mask, not a modulo | intra reference gather, per run |
| 44 | `(16384 + \|td\|/2) / td` | §8.5.3.2.8 clamps `td` to −128..=127, so there are 256 possible answers — a table built at compile time from the spec's own expression | every scaled motion vector |
| 45–51 | seven `rs / ctb_w` and `rs % ctb_w` | `ctb_w` is constant for the whole picture — a per-picture reciprocal makes both a multiply and a shift | the coding-tree-block loop |

**18 hardware divides → 5**, all 147 streams still bit-exact. The five that remain
are deliberate: three in `TileLayout::new` (once per picture parameter set, cold)
and two in the QP wrap, where the input bound needed to replace `%` with conditional
subtraction is not one I could prove from the spec — so it stays a division rather
than a maybe.

`CtbDiv` carries a `debug_assert_eq!` against the real division on every call, and a
unit test sweeps **every `ctb_w` a conforming stream can have** (1..=1056, the level
6.2 cap at the 16×16 minimum CTB) against `n / d` and `n % d`. A reciprocal that is
wrong for one picture width would otherwise be invisible.

### Where the transcendentals actually are

Since the decoder had none, the search widened to the workspace. 259 call sites with
no SIMD form, and none of them in a decoder:

| crate | sites | |
|---|---:|---|
| `rusty_aac` | 97 | encoder psychoacoustics |
| `rusty_mp3` | 91 | encoder psychoacoustics |
| `rusty_vorbis` | 31 | encoder floor/residue |
| `rusty_vp9` | 14 | |
| `rusty_flac` | 12 | |
| `rff-resample` | 7 | |

Two things worth flagging for whoever picks this up:

- **31 `round`/`floor`/`ceil`/`trunc` sites** sit in those same three audio encoders.
  That is the skill's §4 trap exactly: removing `exp` and leaving `round` removes
  nothing, because the second libm call keeps the loop scalar. Any campaign there
  has to take both.
- **No `fastmath` module exists anywhere in the workspace.** The skill says grep
  before writing one; there is nothing to reuse and nothing has drifted, so a single
  consolidated module with the scalar oracle tests is the right first brick rather
  than a fourth independent copy.

This is a report, not a claim: none of it is measured, and the audio encoders are
outside this mission.

## Symbolic optimization — the Prometheus pass

`Prometheus/` is the codec R&D refinery; its deployment doctrine is
`_greatgate/great-gate.md` §4, "Symbolic leaves". The governing constraint for a
decoder is GAPS §1, the **LUT reality check**: a formula that loses to an
L1-resident table is an expected ledger verdict, not a failure. Every table in
this decoder totals ~5–6 KB, so most table→formula candidates were expected to
prune, and did.

### The LUT inventory, and why almost all of it pruned

| table | size | verdict |
|---|---:|---|
| `RANGE_LPS` / `STATE_TRANS` (CABAC) | 256 B + 128 B | already **fused** into one `FUSED[u32; 512]`, branchless — the symbolic work is done |
| `DCT32` | 2 KB | not a table problem — see the butterfly below |
| `INV_ANGLE` | 60 B | **exact closed form found**: `round(8192 / |angle|)` — verified against all 15 entries. Pruned: it needs a division, and a division is worse than a 60-byte load |
| `LEVEL_SCALE` | 24 B | ≈ `round(40 · 2^(k/6))` but not exactly; needs `pow`. Pruned |
| `CHROMA_QP_420`, `BETA`, `TC`, `INTRA_PRED_ANGLE`, scan tables | 52–140 B each | L1-resident, one load. Pruned per GAPS §1 |

`prom distill --target cabac-lps --thorough` was run against the 256-point LPS
table (the doctrine's own named target, shared with rs_h264). It returned
`(3.156833 − x0)` at **rmse 36.9** on a table ranging 2–240 — and the experiment's
own note says "anything inexact is a ledger loss". A clean prune, recorded. The
search is also weak for this shape: 147 candidates at depth 3 over two inputs,
and the true construction involves a geometric progression in the state index
that the operator set does not span.

### The two wins were ALGEBRAIC, not table→formula

Symbolic optimization paid where the *matrix* had structure the code ignored.

**1. Partial butterfly for the inverse DCT.** `idct_1d` was a naive `N²` matrix
product — 1024 multiplies for a 32-point transform. HEVC's transform matrix has
even rows symmetric and odd rows antisymmetric about the midpoint (asserted by
`transform_matrix_has_the_butterfly_symmetry`), so

```text
  out[j] = E[j] + O[j]        out[N−1−j] = E[j] − O[j]
```

and `E` is itself the (N/2)-point transform of the even coefficients, so the even
half recurses: `T(N) = T(N/2) + (N/2)²`, i.e. **352 multiplies instead of 1024**.
Exact integer reassociation, not approximation — `butterfly_matches_naive` sweeps
every size, every `nz` and three strides and asserts bit-identity.

| content | effect |
|---|---|
| mainstream inter | **1.143×** (21/21, z = 4.58) |
| all-intra | **1.068×** (14/15, z = 3.36) |

The gap between them is explained by the gate ledger: all-intra content is 64 %
4×4 blocks, where the butterfly saves least.

**2. The DC-only collapse.** The same symmetry taken to its limit. Row 0 of the
DCT matrix is the constant 64, so a block with one non-zero coefficient has both
passes degenerate to a single scalar and a fill — `N²` multiplies become two.
Excluded for the DST, whose row 0 is `[29, 55, 74, 84]`.

Population is strongly content-dependent and cross-checks the gate ledger's
sparsity column exactly: **71 % of blocks on `WP_A_Toshiba_3`** (which the ledger
independently measured at 0.42 % live coefficient area), 23 % all-intra, 6.6 %
mainstream, 0.2 % on transform-skip-heavy content.

**1.074×** (13/14, z = 3.21) on the content where it is 71 % of blocks. Default-on.

### The symmetric filter — built, and the reason it pays in ONE kernel only

This was written up as "pruned on arithmetic, ~1.5 % of decode". That prune was
wrong, and the instruction counter says so.

Two HEVC filters are palindromes — luma `f2 = [-1,4,-11,40,40,-11,4,-1]` and
chroma `fC4 = [-4,36,36,-4]` — so `Σ t[k]·x[k]` collapses to
`Σ_{k<N/2} t[k]·(x[k] + x[N−1−k])`. The first estimate said this trades four
madds for four adds and nets ~nothing. **It misses that `pmaddwd` consumes an
unpacked PAIR, so halving the values halves the UNPACKS too.** Measured on the
emitted assembly, per two output rows:

| kernel | loop instrs | SIMD | per output |
|---|---:|---:|---:|
| `fir_v_avx2` (luma, general) | 72 | 61 | 2.250 |
| **`fir_v_avx2_sym` (luma, folded)** | **57** | **43** | **1.781** |
| `fir_v_avx2` (chroma, general) | 40 | 33 | 1.250 |
| **`fir_v_avx2_sym` (chroma, folded)** | **32** | **23** | **1.000** |

**−21 % instructions, −30 % SIMD ops**, bit-exact, 147/147 green. Population is
**25 % of pure-vertical calls** (`RT_MC_VSYM` 5,322 / 20,802 on `RAP_A_docomo_6`;
8,195 / 32,935 on `PICSIZE_A`), which is the one filter of the three that is
symmetric. Too small for any clock — §15 says the counter is the verdict here and
the clock is not admissible, so no time is claimed.

#### Why the identical fold does NOT pay on the horizontal kernel

The fold needs `x[k] + x[N−1−k]` in `i16`, which fails on the 2-D path's
intermediates: an 8-bit intermediate reaches `255·(4+40+40+4) = 22440`, and two
of those sum to **44880**, past `i16`. HEVC sized that intermediate to fill the
type. So the 2-D vertical pass keeps the general form and only the pure-vertical
arm — where the source is pixels, bounded by `2·(2^14−1)` — takes the folded one.

But the deeper reason is about LOADS, and it came out of the `.s`:

| kernel | memory-operand ops | explicit `vmovdqu` |
|---|---:|---:|
| `fir_h_avx2` | 48 | **0** |
| `fir_v_avx2` | 16 | 16 |
| `fir_v_avx2_sym` | 17 | 11 |

**`fir_h` has no load instructions at all** — every one is folded into a
`vpmaddwd` as a memory operand, because each sample is touched once per output.
Symmetry cannot save a load that was never an instruction, and it would *cost*
one: two memory operands cannot share an instruction, so the mirrored operand
would have to be loaded explicitly. `fir_v` is the opposite — the two-row
structure holds nine rows live and reuses each across taps and both outputs, so
its loads are real `vmovdqu`, and folding removes unpacks, madds AND five loads.

*The same algebraic identity is a win in one kernel and a loss in its neighbour,
and what decides it is not the algebra but whether the compiler had already
folded the loads.* Read the `.s` before pricing a symmetry.

### Zero end taps — built, and the bookkeeping nearly ate it

`f1[7] = 0` and `f3[0] = 0`, so on those the 2-D path filters one row that is
multiplied by nothing. Skipping it is bit-exact by construction: the arithmetic
removed is `x · 0`.

Measured directly as a work count (`MC_FIR_H_ROWS`, both arms via
`RH265_NO_TAP_ELIDE`):

| stream | rows elided | rows full | saved |
|---|---:|---:|---:|
| `RAP_A_docomo_6` | 1,277,056 | 1,295,315 | **1.41 %** |
| `PICSIZE_A_Bossen_1` | 3,559,075 | 3,586,446 | **0.76 %** |
| `WP_A_Toshiba_3` | 341,263 | 344,213 | **0.86 %** |

Only ~19 % of 2-D calls elide, because **no chroma filter has a zero end tap** —
`CHROMA_SPAN` is `(0,3)` for all seven fractional positions.

The first implementation scanned the tap array per call to find the nonzero
span. That scan runs on *every* 2-D call while the saving lands on a fifth of
them, so the bookkeeping was the same order as the win — a change that measures
as an improvement in the thing it optimises and a wash overall. It is now a
precomputed `LUMA_SPAN`/`CHROMA_SPAN` table (one indexed load), the tap rotation
is built only in the `lo != 0` case, and `tap_spans_match_the_filters` pins the
table against the filters so an edit to one cannot silently desync the other.

### Pruned on arithmetic, before building
- **The CABAC per-bin bounds clamp and trace hook.** One `min` and one
  well-predicted not-taken branch per bin. The clamp is a robustness guard
  against malformed streams; the branch is near-free.

### An instrument correction

The stage ladder reported SAO at **40.4 %** of decode. The paired instrument says
**8.5 %** (1.093×, 19/21, z = 3.71). The recorded lesson said "any share under
~20 % must come from the paired instrument" — this extends it: **the unpaired
min-of-N ladder is unreliable at any share.** Treat it as a hint about which
stage to interrogate, never as a number.

A hypothesis was refuted along the way, cleanly: the route census added
`census::arm()` calls per deblock edge (437,526 per run) and per angular row, and
those were the obvious suspect for the apparent regression. Measured against a
build with them removed: **1.000×, z = 1.60** — the instrumentation costs nothing.

### The load-folding lens, run across every kernel

The vertical-FIR result was not really about symmetry. It was about which loads
the compiler had already made free. That is a property any kernel has or lacks,
so it is worth measuring for all of them rather than rediscovering per kernel --
`tools/hevc/load_lens.py` counts, inside each kernel's hot loop, ALU ops whose
source is memory (the load is folded, costs no instruction) against standalone
vector loads.

The reading is:

* **many folded, ~no standalone loads** -- the compiler already won. Each value
  is touched once, so there is no load to remove and a fold would ADD one: two
  memory operands cannot share an instruction.
* **many standalone loads** -- values are held and reused, the loads are real
  instructions, and an identity that needs fewer values will pay.

| kernel | vector ops | folded | standalone loads | |
|---|---:|---:|---:|---|
| `fir_h_avx2` | 60 | 24 | **0** | already optimal |
| `planar_avx2` | 30 | 6 | 0 | already optimal |
| `angular_avx2` | 48 | **1** | **11** | ← anomaly |
| `angular_t_avx2` | 131 | 9 | 17 | ← anomaly |
| `fir_v_avx2` | 61 | 0 | 9 | folded, see above |
| `edge_avx2` | 61 | 3 | 12 | vertical class only |

`angular_avx2` stood out: a two-tap interpolation with essentially NO load
folding. That is the shape of a kernel where an operand is in the wrong position,
and it was.

### Angular intra — a sign flip that costs nothing and frees a load

HEVC 8.4.4.2.6 is `((32 − f)·a + f·b + 16) >> 5`. The kernel already used

```text
  a + ((f·(b − a) + 16) >> 5)          (i)   -- one multiply instead of two
```

The fix is to write the algebraically identical

```text
  a + ((−f·(a − b) + 16) >> 5)         (ii)
```

`a` is used twice (the difference and the final add) so it must live in a
register; `b` is used once. AT&T `vpsubw src2, src1, dst` computes `src1 − src2`
and **only `src2` may be memory** — so in form (i) the single-use value `b` sits
in `src1` and cannot fold, and in form (ii) it sits in `src2` and does. Negating
a broadcast constant that was being built anyway costs zero instructions.

| kernel | vector ops | folded | standalone loads |
|---|---:|---:|---:|
| `angular_avx2` | 48 → **43** | 1 → **6** | 11 → **6** |
| `angular_t_avx2` | 131 → **123** | 9 → **17** | 17 → **9** |

−10.4% and −6.1% of the hot loop, five and eight loads promoted into operands.
Population is `RT_INTRA_ANG_ROW` = 529,447 calls on `PICSIZE_A_Bossen_1`, where
`SAMPLES_INTRA` (32.0M) is more than double `SAMPLES_SAO` (13.2M). Bit-exact,
147/147.

### A latent defect found next to it

`mullo_epi16` keeps the low 16 bits, so `f·(a − b)` must fit `i16`:

| bit depth | max &#124;a − b&#124; | ×31 | fits? |
|---|---:|---:|---|
| 8 | 255 | 7,905 | yes |
| 10 | 1,023 | 31,713 | yes — 3% of headroom left |
| 12 | 4,095 | 126,945 | **no, wraps silently** |

Every angular SIMD kernel is sound **only** because `decoder.rs` rejects
`bit_depth > 10` (Main / Main 10). Nothing in `intra.rs` said so. The scalar twin
computes in `i32` and stays correct, so if RExt support were ever added the twins
would disagree only on 12-bit content — which no HEVC_v1 vector contains, so
`angular_matches_scalar` and the 147-stream gate would both stay green while the
output was wrong. `angular_i16_headroom_is_exhausted_at_10_bit` now pins the
bound and asserts 11-bit does NOT fit, so relaxing the SPS check fails a test
instead of shipping wrong pixels.

### Deblocking — the symbolic win was 1.3%, the redundancy win was 35%

Deblocking was the largest population still served entirely by scalar code:
`RT_DEBLOCK_LUMA` = 1,139,117 edge calls, `RT_DEBLOCK_CHROMA` = 554,174, and no
kernel of any kind. Priced with the paired instrument (`RH265_NO_SAO` against
`RH265_NO_LF`, which differ by exactly deblocking): **1.154x, 19/20, z = 4.02** --
**13.3% of decode**, against SAO's 8.5% which is fully vectorised.

**The symbolic part.** §8.7.2.5.7 presents the strong filter as six independent
weighted sums. They are not independent -- every one contains `p0 + q0`, and the
p-side and q-side are the same three expressions with `p1+p2` and `q1+q2`
exchanged:

```text
  S = p0+q0   U = p1+p2   V = q1+q2   W = 2S+p1+q1+4

  out[2] = ((S+2) + U) >> 2            out[5] = ((S+2) + V) >> 2
  out[3] = ( W    + U) >> 3            out[4] = ( W    + V) >> 3
  out[1] = ((S+4) + U + 2(p2+p3))>>3   out[6] = ((S+4) + V + 2(q2+q3))>>3
```

Every multiply disappears, including the `3*p2` and `3*q2` that were the filter's
only non-power-of-two coefficients. The weak filter's `9A - 3B` likewise factors
to `3(3A - B)` with `3x = x + (x<<1)`. Both verified exhaustively against the
specification's own form.

Measured on the emitted assembly: **1248 -> 1232 instructions, −1.3%.** LLVM had
already found most of the common subexpressions on its own.

**The redundancy part, which was 27x larger.** The function's sample accessor was

```rust
let idx = |k, i| if dir == 0 { (y+k)*stride + (x+i) } else { (y+i)*stride + x+k };
```

-- and it was called on every one of the ~40 sample reads, in a filter whose
arithmetic is otherwise entirely adds and shifts. That is why the emitted body
carried **41 integer multiplies**. The `dir` branch INSIDE the closure is what
made it expensive: it blocks loop-invariant code motion, so LLVM could not hoist
`(y+k)*stride` the way it routinely does for a plain `out[y*stride + x]` in a
nested loop. A segment is a regular lattice, so eight tap offsets computed once
turn every access into a base-plus-constant add and resolve `dir` once per call.

| version | instructions | `imul` |
|---|---:|---:|
| specification form, closure accessor | 1248 | 41 |
| + algebraic factoring | 1232 | 41 |
| **+ hoisted addressing** | **790** | **9** |

**−36.7% instructions, −78% multiplies**, and end to end **1.041x (16/18,
z = 3.30)** -- against a prediction of 4.9% from 36.7% of a 13.3% stage. Counter
and clock agree.

*The lesson is the playbook's own ordering, paid for again: redundancy before
symbolics before SIMD.* The elegant algebra was worth 1.3%; the boring address
arithmetic was worth 35%. A branchy accessor closure is the signature -- a plain
indexed loop would have been hoisted for free.

### Closing the symbolic list

| candidate | verdict |
|---|---|
| CABAC LPS/state tables | already fused branchless; `prom distill` returned rmse 36.9 on a table needing exactness -- PRUNED |
| `DCT32` matrix | **partial butterfly, 1024 -> 352 multiplies, 1.143x** |
| DC-only transform | **collapse to one scalar + fill, 1.074x on 71%-DC content** |
| `INV_ANGLE` | exact closed form `round(8192/|angle|)` found -- PRUNED, needs a division |
| `LEVEL_SCALE` | ~`round(40·2^(k/6))`, inexact, needs `pow` -- PRUNED |
| small tables (`CHROMA_QP`, `BETA`, `TC`, scans) | L1-resident, one load -- PRUNED per GAPS §1 |
| MC symmetric filter | **vertical fold, −21% instructions, −30% SIMD ops** |
| MC zero end-taps | **row elision, 0.76-1.41% of FIR rows** |
| angular two-tap | **sign flip frees a load, −10.4% / −6.1%** |
| deblock strong filter | **factored, −1.3%**; addressing **−36.7%** |
| planar incremental (`R(y+1) = R(y) − top[x]`) | PRUNED -- the load lens shows planar already has 0 standalone loads; making the recurrence explicit costs back what the `mullo_epi32` saves |
| chroma deblock | already minimal (`(q0−p0)<<2`), no multiply to remove |
| `bypass_bits` as long division | PRUNED on population: **1.86 bins per call** (504,582 bins / 271,677 calls). One 64-bit division ~30 cycles against ~8 for two loop iterations |

### The largest opportunity that is NOT symbolic

Deblocking is now 4.1% faster and still **100% scalar** at ~12.8% of decode. Every
other stage of comparable weight has a kernel. Horizontal edges (`dir == 1`) would
vectorise directly -- the four lines of a segment are contiguous, so each tap
position is one load -- while vertical edges need an 8x4 transpose first. That is
the next brick, and it is a `codec-vectorize-kernel` job, not a Prometheus one.

### The deblocking kernel — the last large stage that had none

Every other stage of comparable weight had a kernel; deblocking had nothing, at
**13.3% of decode**. `codec-vectorize-kernel` Step 0 was satisfied properly
before writing a line: the redundancy pass had already run (the address hoist
above, −36.7%), the emitted body carried **11 vector instructions in 790** — all
scalar `movd`/`movq`, so no auto-vectorisation to preserve — and all three
blockers were nameable:

1. **Data-dependent conditional writes.** `write[k][i]` gates every store, and
   the weak path's `|delta| < 10·tc` is decided per LINE.
2. **A cross-lane gather.** On vertical edges the four lines are `stride` apart
   while each line's eight taps are contiguous — the lane-per-line layout needs
   a transpose LLVM will not synthesise.
3. **Per-lane asymmetric clamps**, `clamp(p0 − 2tc, p0 + 2tc)`, with bounds that
   differ per lane.

#### Layout: one lane per line, and `i32` on purpose

A segment is four lines of eight samples, and the filter treats the lines
independently — so the vector layout is one lane per line: eight registers, one
per tap, each holding that tap for lines 0..3. Four `i32` lanes is exactly one
`__m128i`.

`i16` would fit — every sample and intermediate does at 8 and 10 bits — and would
process eight lines at once. **It was rejected deliberately.** The campaign that
preceded this one found two kernels that were correct only because a bound was
asserted in a distant SPS check, invisible to every gate including the 147-stream
corpus. Narrowing here would buy throughput in exchange for exactly that class of
latent defect. `i32` is bit-identical to the scalar twin by construction, at any
bit depth, with nothing to prove. The faster version stays available if the
population ever justifies re-earning the proof.

#### The two directions are not symmetric

* **Horizontal edges**: tap `t` is a whole ROW and the four lines are four
  adjacent columns, so one 8-byte load per tap already IS the layout — no
  transpose in either direction.
* **Vertical edges**: line `k` is eight contiguous samples, so one 16-byte load
  holds all eight taps of ONE line — the transpose of what is wanted. Four loads
  are transposed 4x8 -> 8x4 going in and back going out, three rounds of unpack
  each way.

#### The decisions stay scalar, and the census says why

`dd >= beta` rejects a whole segment before any filtering, and strong/weak,
`dep` and `deq` are per SEGMENT — twelve samples from lines 0 and 3. Keeping them
scalar preserves the early-out, and the population shows how much that is worth:

| route | count | share |
|---|---:|---:|
| `RT_DEBLOCK_SKIP` (rejected before filtering) | 170,582 | **39.0%** |
| `RT_DEBLOCK_WEAK` | 232,625 | 53.2% |
| `RT_DEBLOCK_STRONG` | 33,558 | 7.7% |

Two of every five segments never reach the kernel at all. Vectorising the
decisions would have paid the eight-tap gather on all of them. Only
`|delta| < 10·tc` is per line, and that is a lane mask.

Reachability confirmed by census rather than assumed: **`DEBLOCK_LUMA_SIMD` =
266,183 against `DEBLOCK_LUMA_SCALAR` = 761** — 99.7%. The 761 are picture-edge
segments where the conservative bounds check declines and the twin serves them.

#### Result

| change | effect | verdict |
|---|---|---|
| address hoist (previous section) | **1.041x** whole decode | 16/18, z = 3.30 |
| **the SIMD kernel** | **1.032x** whole decode | 16/19, z = 2.98 |
| strided merged scan | 1.020x | 12/18, **z = 1.41 -- NOT a verdict** |

And the stage itself, re-priced the same way it was priced at the start:

| | deblocking as a share of decode |
|---|---|
| before the campaign | **13.3%** (1.154x, 19/20, z = 4.02) |
| after | **6.5%** (1.070x, 20/21, z = 4.15) |

**The stage is halved.** The two verdicts sum to 7.3% against a 6.8-point drop,
which is the consistency check.

#### The scan restructure, and an arithmetic error worth recording

The driver walked every 4x4 block FOUR times a frame -- a luma and a chroma pass
per direction -- to reach 460,672 filter calls: **37.5 blocks scanned per call**.
`bs_v` is only ever written where `x4 % 2 == 0` and `bs_h` where `y4 % 2 == 0`,
and chroma's `% 4` positions are a SUBSET of luma's, so the four scans become one
half-density strided scan per direction -- 17.3M iterations down to 5.2M, with
the filter call counts IDENTICAL (437,526 luma / 23,146 chroma) proving parity.

It measured **z = 1.41**. Not a verdict, and the prediction that motivated it was
wrong: 17.3M iterations x "~5 ops" was quoted as 86M ops as though ops were
cycles. A sequential scan that loads `bs[i]` and branches on zero is perfectly
predicted and streams from L1, retiring ~4 ops/cycle -- so it is nearer 0.8% of
decode than the 4% implied. Kept, because it is strictly less work with proven
parity, but recorded as **counter-verified and clock-inconclusive**, not a win.

That result also pruned its own sibling: the same restructure applied to the bS
BUILD loop (skipping blocks with both coordinates odd) was written and then not
landed -- it is the same class of change, four times smaller, and its sibling had
just failed to resolve.

147/147 bit-exact. `luma_edge_matches_scalar` sweeps both directions, both bit
depths, four `beta` and four `tc` values, `no_p`/`no_q`, and four sample patterns
chosen so the strong, weak and early-out arms are each reached — and asserts that
they were, because random samples almost never trigger the strong filter and a
test that silently covers one arm is how the last two defects survived.

### What is deliberately still scalar

- **The SAO ring** (CTB edges, slice and tile boundaries) — every sample there needs
  a §6.4.1 availability derivation on both neighbours. Splitting it out is what made
  the interior a flat rectangle the kernel could take.
- **Pictures containing transquant-bypass or PCM-loop-filter-disabled samples.** The
  kernels take a whole rectangle unconditionally, so `has_bypass` routes the picture
  to the scalar loops. It is a picture-level flag, computed once per picture.
- **Explicit weighted prediction** (item 9) — rare enough that the census counter
  exists mainly to confirm how rare.
- **The DC edge fix-ups** (item 19's three boundary rows) — the flat fill is a kernel,
  the fix-ups are `3n` samples with their own `c_idx` and size conditions and stay
  with the caller that owns those conditions.
- Items 12, 16, 17, 21, 22, 23, 27–33, priced in the table above.

Every one of those has its scalar twin in the tree as the oracle, and both intra and
SAO carry an env switch (`RH265_SCALAR_INTRA`, `RH265_SCALAR_SAO`) so the A/B can be
re-run inside one binary whenever the surrounding shares move.
