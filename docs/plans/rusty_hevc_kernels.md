# rusty_h265 — every scalar implementation, and what happens to it

**Measured 2026-09-05**, `in_to_tree_720p_8bit.hevc` (x265 medium crf24, 60 frames),
pinned core, High priority, CPU time, decode only (output discarded), ABBA over 3
rounds. Shares come from **stage ablation on the uninstrumented binary** — each arm
parses every bin (HEVC entropy decode does not depend on reconstructed samples, so
the work counts are identical) and skips one pixel stage:

| arm                                           | best ms | stage ms |                    share |
|-----------------------------------------------|--------:|---------:|-------------------------:|
| full decode                                   |   3,750 |        — |                        — |
| no motion compensation                        |   1,453 |    2,297 |               **61.3 %** |
| no residual (scale + inverse transform + add) |   2,969 |      781 |               **20.8 %** |
| no intra prediction                           |   3,063 |      688 |               **18.3 %** |
| no SAO                                        |   3,188 |      563 |               **15.0 %** |
| no deblocking (+SAO)                          |   3,094 |      656 | 17.5 % (deblock ≈ 2.5 %) |
| parse only (no pixels at all)                 |     266 |    3,484 |                   92.9 % |

Shares overlap — removing one stage relieves cache pressure on its neighbours — so
this is a **ranking**, not an additive budget. The one hard number: **7.1 % of decode
is entropy decoding and syntax**; everything else is pixels, and pixels are what
kernels are for.

## The list

`ceiling` = what the stage could give if its cost went to zero, from the ladder above.
`plan` = what the brick actually is: **R** = redundancy/structure (do first, no
`unsafe`), **K** = SIMD kernel, **A** = algorithmic, `—` = leave alone.

### Motion compensation — `src/mc.rs`, `src/ctu/inter.rs` (61.3 %)

| #   | scalar implementation                                                             | shape                                           | plan         |
|-----|-----------------------------------------------------------------------------------|-------------------------------------------------|--------------|
| 1   | `interpolate`, full-pel copy path                                                 | copy + shift, no filter                         | K            |
| 2   | `interpolate`, horizontal-only 8-tap luma / 4-tap chroma                          | FIR across a row                                | K            |
| 3   | `interpolate`, vertical-only                                                      | FIR down a column, unit stride per tap          | K            |
| 4   | `interpolate`, 2-D (H pass → `tmp`, then V pass)                                  | two FIRs + a scratch buffer                     | K            |
| 5   | `fetch()` — per-sample `clamp` of both coordinates                                | 2 branches **per tap per sample**               | **R**        |
| 6   | `vec![0i32; bw*bh]` per prediction unit **per plane**, plus `tmp` in the 2-D path | heap traffic in the hottest loop in the decoder | **R**        |
| 7   | `weighted_write`, uni-prediction default                                          | shift + clamp, a **second pass** over the block | **R** then K |
| 8   | `weighted_write`, bi-prediction default                                           | average of two 14-bit buffers                   | K            |
| 9   | `weighted_write`, explicit weights (uni and bi)                                   | multiply-add + clamp                            | K            |

The three **R** items are the ones Step 0 of `codec-vectorize-kernel` insists on
first: a per-sample clamp and a heap allocation per prediction unit are not things a
wider register fixes.

### Residual — `src/itx.rs`, `src/ctu/residual.rs` (20.8 %)

| #   | scalar implementation                    | shape                                                                             | plan                             |
|-----|------------------------------------------|-----------------------------------------------------------------------------------|----------------------------------|
| 10  | `dequant`                                | multiply, rounding shift, clamp per coefficient                                   | K                                |
| 11  | `idct_1d`                                | **naive N×N matrix multiply** — 32 multiplies per output for a 32-point transform | **A** (partial butterfly) then K |
| 12  | `idst_1d`                                | 4×4 matrix multiply                                                               | K                                |
| 13  | `inverse_transform`, transform-skip path | shift + round                                                                     | K                                |
| 14  | residual add-and-clamp into the plane    | elementwise add + clamp                                                           | K                                |

Item 11 is the one to fix before touching intrinsics: the spec's even-odd
decomposition is the reason every real decoder's inverse transform is a butterfly,
and it removes work rather than parallelising it.

### Intra prediction — `src/intra.rs`, `src/ctu/residual.rs` (18.3 %)

| #   | scalar implementation                                                           | shape                                                                | plan                                            |
|-----|---------------------------------------------------------------------------------|----------------------------------------------------------------------|-------------------------------------------------|
| 15  | reference gathering calls `PicState::available()` **once per reference sample** | z-scan lookup, CTB lookup, slice and tile compare — per sample       | **R** (availability is per 4×4, not per sample) |
| 16  | `substitute`                                                                    | backward fill of unavailable samples                                 | — (short, runs once per block)                  |
| 17  | `filter` — 3-tap smoothing, and the strong bilinear at 32×32                    | FIR over ≤ 128 samples                                               | K                                               |
| 18  | planar prediction                                                               | two linear ramps per row                                             | K                                               |
| 19  | DC prediction + the three edge filters                                          | reduction, then fill                                                 | K                                               |
| 20  | angular prediction                                                              | build the projected reference, then a 2-tap interpolation per sample | K                                               |

### Loop filters — `src/filters.rs` (17.5 %)

| #   | scalar implementation                                                           | shape                                             | plan             |
|-----|---------------------------------------------------------------------------------|---------------------------------------------------|------------------|
| 21  | boundary-strength derivation                                                    | per 4×4, branch-heavy, ~1/16 of samples           | —                |
| 22  | `filter_luma_edge`                                                              | 4-line segment, strong/normal decision            | K (low priority) |
| 23  | `filter_chroma_edge`                                                            | 2-line segment                                    | —                |
| 24  | SAO band offset                                                                 | per sample: band lookup, add, clamp               | K                |
| 25  | SAO edge offset                                                                 | per sample: two neighbour compares + availability | **R** then K     |
| 26  | **`let src = planes[c].clone()`** — a full plane copy per component per picture | pure memory traffic, allocated fresh each time    | **R**            |

### Parsing and bookkeeping (7.1 % total — mostly not kernel work)

| #   | scalar implementation                    | shape                                   | plan |
|-----|------------------------------------------|-----------------------------------------|------|
| 27  | `Cabac::decode` / `bypass` / `terminate` | inherently serial arithmetic coder      | —    |
| 28  | residual coefficient scan loops          | data-dependent, serial                  | —    |
| 29  | `PicState::fill4`                        | small rectangular fills of per-4×4 maps | —    |

### Output paths (outside the decode measurement, paid by every caller)

| #   | scalar implementation               | shape                         | plan |
|-----|-------------------------------------|-------------------------------|------|
| 30  | `Frame::write_yuv`                  | crop + narrow `u16` → `u8`    | K    |
| 31  | `frame_to_rff` in the adapter       | the same crop + narrow, again | K    |
| 32  | `narrow_to_8` in `rff-filter`       | rounding shift `u16` → `u8`   | K    |
| 33  | `sei::compute` MD5 / CRC / checksum | only under `verify_sei`       | —    |

## Order of execution

Arithmetic first (`codec-measurement`: prune before building). Expected whole-decode
gain = stage share × (1 − 1/speedup):

| brick                            |  share | needed speedup | whole-decode gain |
|----------------------------------|-------:|----------------|------------------:|
| MC structure (5, 6, 7)           | 61.3 % | 2×             |             1.44× |
| MC kernels (1–4, 8, 9)           | 61.3 % | 4× total       |       up to 1.72× |
| inverse transform butterfly (11) | 20.8 % | 3×             |             1.16× |
| intra availability (15)          | 18.3 % | 2×             |             1.10× |
| SAO (24, 25, 26)                 | 15.0 % | 3×             |             1.11× |

Everything below ~2 % of decode (deblocking, `fill4`, chroma edges) is **not worth a
kernel** and is recorded here so nobody re-derives that conclusion.

## Verdicts — what was built, and what it measured

Every brick below was gated on the **full 147-stream JCT-VC HEVC_v1 suite, bit-exact**
before its number was taken, then measured pinned to core 4 at High priority on CPU
time, ABBA-interleaved, decode only. `z = (wins − N/2) / (0.5·√N)`.

**Null arm on this box, 21 pairs: median 1.000×, z = −0.22, p25–p75 0.985–1.031.**
That ±3 % is the resolution floor every verdict below is judged against.

| brick                                               | items      |   N | wins |     z |      paired median | verdict                 |
|-----------------------------------------------------|------------|----:|-----:|------:|-------------------:|-------------------------|
| MC structure + kernels                              | 1–9        |  21 |   21 | +4.58 |         **1.306×** | keep                    |
| sparse inverse transform                            | 10, 11, 13 |  13 |   13 | +3.61 |         **1.133×** | keep                    |
| intra availability per 4×4                          | 15         |  15 |   11 | +1.81 | 1.069× (all-intra) | keep, content-dependent |
| SAO structure (interior/ring split, reused scratch) | 26         |  13 |   13 | +3.61 |         **1.282×** | keep                    |
| SAO band + edge kernels                             | 24, 25     |  21 |   19 | +3.71 |         **1.136×** | keep                    |
| intra kernels — stage isolated                      | 18, 19, 20 |  24 |   21 | +3.67 |         **1.068×** | keep                    |
| intra kernels — all-intra pipeline                  | 18, 19, 20 |  21 |   16 | +2.40 |         **1.031×** | keep, content-dependent |
| intra kernels — inter pipeline (control)            | 18, 19, 20 |  16 |    7 | −0.50 |             1.000× | no regression           |

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

| counter                             | kernel arm | scalar arm |
|-------------------------------------|-----------:|-----------:|
| `SAO_BAND_SIMD` / `SAO_BAND_SCALAR` |    524 / 0 |      0 / 0 |
| `SAO_EDGE_SIMD` / `SAO_EDGE_SCALAR` |  5,966 / 0 |      0 / 0 |
| `SAMPLES_SAO`                       | 20,384,632 |          0 |

And on `IPRED_A_docomo_2`, against `RH265_SCALAR_INTRA=1`:

| counter                          |  kernel arm | scalar arm |
|----------------------------------|------------:|-----------:|
| `INTRA_ANGULAR_SIMD` / `_SCALAR` | 166,038 / 0 |      0 / 0 |
| `INTRA_PLANAR_SIMD` / `_SCALAR`  |  67,952 / 0 |      0 / 0 |
| `INTRA_DC_SIMD` / `_SCALAR`      |  47,065 / 0 |      0 / 0 |
| `SAMPLES_INTRA`                  |  11,980,800 |          0 |

The scalar arm reaching **zero** kernel calls is the point: it proves the two arms of
the A/B are genuinely different code, which is exactly what the MC brick's first
reading failed to establish. Both arms are also the *same binary* under two env
settings, so no stale build or differing inline decision can masquerade as an effect.

### Where the time goes now, and why the list stops here

Re-measured on the final binary. Loop-filter shares from the ladder (valid: nothing
runs after them); the pixel shares from the **paired** harness, which is the only
instrument that resolves anything at this size.

| stage                               | share of decode | instrument                                |
|-------------------------------------|----------------:|-------------------------------------------|
| parse + CABAC + bookkeeping         |      **29.0 %** | ladder, `parse only`                      |
| SAO                                 |          11.3 % | ladder (was 19.7 % before its two bricks) |
| deblocking                          |           6.4 % | ladder, `no deblock+SAO` minus `no SAO`   |
| intra prediction, inter content     |          ~1.8 % | paired, z = 1.09 — inside noise           |
| intra prediction, all-intra content |      **27.3 %** | paired, 15/15, z = 3.87                   |

Everything still marked **K** in the tables above was priced against that, using
`expected gain = share × (1 − 1/speedup)` and the measured **±3 % null floor**:

| item                              |                                       share | speedup a kernel could give  |    expected gain | decision                     |
|-----------------------------------|--------------------------------------------:|------------------------------|-----------------:|------------------------------|
| 22 deblocking luma edge           |                                       6.4 % | 2× (branchy 4-line segments) |            3.2 % | **at the floor — not built** |
| 12 4×4 inverse DST                |                   inside residual, 4×4 only | 2×                           |            < 1 % | **not built**                |
| 17 reference-sample filter        |                ≤ 128 samples once per block | 3×                           |            < 1 % | **not built**                |
| 30–32 output narrowing `u16`→`u8` | outside decode; ~55 M samples per 60 frames | 4×                           | ~1 % of CLI wall | **not built**                |
| 27–29 CABAC, scan loops, `fill4`  |                               29 % combined | — serial by construction     |                — | **not a kernel job**         |

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

| #   | change                                                                                                                                                                   | kernel                                                                        | instructions / samples       | →            |
|-----|--------------------------------------------------------------------------------------------------------------------------------------------------------------------------|-------------------------------------------------------------------------------|------------------------------|--------------|
| 1   | SAO band offset: the four bands are **consecutive**, so the distance from `sao_band_position` *is* the table index — one `pshufb` replaces 4 compares, 4 masks and 3 ors | `band_avx2`                                                                   | 23 → 18 per 16               | −5           |
| 2   | SAO edge offset: `edgeIdx` is already 0..4, so it indexes the same shuffle table directly — no compares at all                                                           | `edge_avx2`                                                                   | 35 → 24 per 16               | −11          |
| 3   | Counted vector loop (one induction variable) — SAO AVX2                                                                                                                  | `band_avx2`, `edge_avx2`                                                      | 18 → 15, 24 → 21             | −3 each      |
| 4   | Counted vector loop — pixel kernels                                                                                                                                      | `put_uni` 15 → 9 / 10 → 8, `put_bi` 17 → 11 / 12 → 10, `add_residual` 12 → 10 |                              | −2..−6       |
| 5   | Counted vector loop — every FIR variant                                                                                                                                  | `fir_h`/`fir_v`, SSE2 and AVX2, 4- and 8-tap                                  | 8 kernels                    | **−6 each**  |
| 6   | Angular filter never leaves `i16`: `(32−f)·a + f·b = 32a + f·(b−a)`, and `32a` factors out of the shift exactly                                                          | `angular_sse2` 18 → 13, `angular_t_sse2` 178 → 127                            |                              | −5, **−51**  |
| 7   | Zero-shift horizontal FIR — `shift1 = BitDepth − 8` is **0 for 8-bit**                                                                                                   | `fir_h_avx2` 23 → 21, 15 → 13; `fir_h_sse2` 32 → 30, 20 → 18                  |                              | −2 each      |
| 8   | AVX2 angular (there was none)                                                                                                                                            | `angular`                                                                     | 1.625 → **0.688** /sample    | −58 %        |
| 9   | AVX2 planar (there was none)                                                                                                                                             | `planar`                                                                      | 4.750 → **1.500** /sample    | −68 %        |
| 10  | Two output rows per pass in the vertical FIR: windows overlap in `N − 1` rows, so **9 loads replace 16**                                                                 | `fir_v_avx2`                                                                  | 2.750 → 2.250 /sample        | −18 %        |
| 11  | **Full-pel copy had no kernel at all** — integer motion vectors went one sample at a time                                                                                | `copy_shift`                                                                  | scalar → **0.375** /sample   | new          |
| 12  | Two output rows per pass, SSE2                                                                                                                                           | `fir_v_sse2` 6.125 → 5.000, chroma 2.875 → 2.750                              |                              | −18 %        |
| 13  | Counted vector loop — SAO SSE2                                                                                                                                           | `band_sse2` 26 → 24, `edge_sse2` 40 → 38                                      |                              | −2 each      |
| 14  | AVX2 transposed angular: two 8×8 tiles per strip, so the rows go 16 wide                                                                                                 | `angular_t`                                                                   | 1.891 → **1.27** /sample     | −33 %        |
| 15  | **DC fill emitted zero vector instructions** — `slice::fill` did not lower across the call boundary                                                                      | `dc_fill`                                                                     | 1 → **16** samples per store | new          |
| 16  | Zero-shift vertical FIR (the vertical-only path also shifts by `shift1`)                                                                                                 | `fir_v_avx2` 72 → 68, 40 → 36; `fir_v_sse2` 80 → 76, 44 → 40                  |                              | −4 each      |
| 17  | Two vectors per trip — the loop bookkeeping was ~40 % of the shortest kernels                                                                                            | `put_uni_avx2` 0.500 → **0.406**, `put_bi_avx2` 0.625 → **0.531**             |                              | −19 %, −15 % |
| 18  | Two vectors per trip — residual add                                                                                                                                      | `add_residual_avx2`                                                           | 0.625 → **0.531**            | −15 %        |
| 19  | Two vectors per trip — SAO                                                                                                                                               | `band_avx2` 0.938 → 0.844, `edge_avx2` 1.312 → 1.219                          |                              | −10 %, −7 %  |
| 20  | Non-wrapping `sao_band_position` (28 of 32 values) needs no `mod 32`                                                                                                     | `band_avx2`                                                                   | 0.844 → **0.781**            | −7 %         |

### A second pass: 10 more (21–30)

Same instrument, same gate. **All ten gated together: 147/147 bit-exact, 14 kernel
oracle tests, 29 decoder tests.**

| #   | change                                                                                                                                                                                                                                         | kernel                                                                                                  | instructions / samples    | →            |
|-----|------------------------------------------------------------------------------------------------------------------------------------------------------------------------------------------------------------------------------------------------|---------------------------------------------------------------------------------------------------------|---------------------------|--------------|
| 21  | Two vectors per trip — horizontal FIR                                                                                                                                                                                                          | `fir_h_avx2` 1.438 → **1.344**, chroma 0.938 → **0.844**; zero-shift twins 1.312 → 1.219, 0.812 → 0.719 |                           | −7 %         |
| 22  | **Full-pel uni-prediction is the identity.** `copy_shift` writes `s << k` and `put_uni` computes `(v + 2^(k−1)) >> k` with the *same* `k`, so the pair is `s` exactly — an integer motion vector needs a rectangle copy, not two kernel passes | `copy_block`                                                                                            | 0.781 → **~0.1** /sample  | −87 %        |
| 23  | **Full-pel bi-prediction is `pavgw`.** The same composition one step on: `(s0·2^k + s1·2^k + 2^k) >> (k+1) = (s0+s1+1) >> 1`                                                                                                                   | `avg_block`                                                                                             | 1.281 → **~0.25** /sample | −80 %        |
| 24  | Two vectors per trip — full-pel copy                                                                                                                                                                                                           | `copy_shift_avx2` 0.375 → **0.281**, `copy_shift_sse2` 0.750 → **0.562**                                |                           | −25 %        |
| 25  | Two vectors per trip — SSE2 uni/bi write                                                                                                                                                                                                       | `put_uni_sse2` 1.125 → **0.938**, `put_bi_sse2` 1.375 → **1.188**                                       |                           | −17 %, −14 % |
| 26  | Two vectors per trip — SSE2 residual add                                                                                                                                                                                                       | `add_residual_sse2`                                                                                     | 1.375 → **1.188**         | −14 %        |
| 27  | Two vectors per trip — SSE2 SAO                                                                                                                                                                                                                | `band_sse2` 3.000 → **2.812**, `edge_sse2` 4.750 → **4.562**                                            |                           | −6 %, −4 %   |
| 28  | Two vectors per trip — angular rows, AVX2                                                                                                                                                                                                      | `angular_avx2`                                                                                          | 0.688 → **0.594**         | −14 %        |
| 29  | Two vectors per trip — angular rows, SSE2                                                                                                                                                                                                      | `angular_sse2`                                                                                          | 1.625 → **1.188**         | −27 %        |
| 30  | Planar narrows 16 lanes with one `packs` + one `permute` instead of half-filling a store twice                                                                                                                                                 | `planar_avx2`                                                                                           | 1.500 → **1.125**         | −25 %        |

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

| #   | composition                                                                                                                                                                                                       | what collapses                                    | scale                                                     |
|-----|-------------------------------------------------------------------------------------------------------------------------------------------------------------------------------------------------------------------|---------------------------------------------------|-----------------------------------------------------------|
| 31  | **Mixed bi-prediction folds its shift.** One list full-pel, one not: the full-pel list ran a whole `copy_shift` pass whose only work was `s << k`, which the bi write can do itself                               | `copy_shift` + `put_bi` → `put_bi_fp`             | 3,441 blocks; `MC_COPY` 8,552 → 5,111                     |
| 32  | **Intra reference samples reused across blocks.** `RefSamples::new()` per block zeroed ~388 bytes — 24 bytes of memset per predicted sample on a 4×4                                                              | allocation + use → one allocation                 | ~388 B → `4n+1` B per block                               |
| 33  | **`substitute` short-circuits.** The gather already derives availability per 4×4 to build the arrays; it now says whether anything was missing, and in a picture's interior nothing ever is                       | gather + substitute                               | skips `4n` scans + two fill loops                         |
| 34  | **The inverse transform's stage-1 scratch was a 4 KB local.** Zeroed per transform block — 256 bytes of memset per coefficient on a 4×4 — and never read before it was written                                    | allocation + use → one allocation                 | **197 MB of memset removed** (48,092 TBs × 4 KB)          |
| 35  | **Deblocking strength buffers reused across pictures** rather than allocated per picture                                                                                                                          | two `vec![0u8; w4*h4]` per picture → reused       | 115 KB/picture at 720p                                    |
| 36  | **`predict`'s own scratch carried in the reused struct** — the angular projected reference (`[0i16; 128]`) and the planar narrowing buffers                                                                       | allocation + use → one allocation                 | 256 + 132 B per block; `predict` now has **zero** memsets |
| 37  | **A partition mode yields at most four rectangles** — and allocated a `Vec` to iterate them                                                                                                                       | heap alloc + free per inter CU → a fixed array    | one malloc/free per inter CU                              |
| 38  | **The reference-smoothing double buffer** (`[0u16; 64]` twice) moved into the same reused struct                                                                                                                  | allocation + use → one allocation                 | 256 B per filtered block                                  |
| 39  | **Merge and MVP candidate lists are bounded by the spec** (five and three) and never leave the function that builds them                                                                                          | four `Vec`s per PU → fixed-capacity arrays        | **four** mallocs per prediction unit                      |
| 40  | **DC prediction + residual add fused.** DC writes one constant across the block and the residual add loads every one of them straight back; composed, `dst = clamp(dc + res)` needs neither the fill nor the load | `dc_fill` + `add_residual` → `add_residual_const` | 5,921 blocks on all-intra                                 |

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

|                                       |       ours | ffmpeg |     × ref |
|---------------------------------------|-----------:|-------:|----------:|
| after the six kernel bricks           |   1,016 ms | 297 ms |     3.42× |
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

| #     | division removed                    | why it was avoidable                                                                                   | site                                          |                                                                                                                                        |                            |
|-------|-------------------------------------|--------------------------------------------------------------------------------------------------------|-----------------------------------------------|----------------------------------------------------------------------------------------------------------------------------------------|----------------------------|
| 41    | `32 / n` in `idct_1d`               | `n` is always 4/8/16/32 — a power of two the compiler cannot see                                       | every column and row of every transform block |                                                                                                                                        |                            |
| 42–43 | `(yb + k) % run`, `(xb + k) % run`  | `run` is 4 or 2 — a mask, not a modulo                                                                 | intra reference gather, per run               |                                                                                                                                        |                            |
| 44    | `(16384 + \                         | td\                                                                                                    | /2) / td`                                     | §8.5.3.2.8 clamps `td` to −128..=127, so there are 256 possible answers — a table built at compile time from the spec's own expression | every scaled motion vector |
| 45–51 | seven `rs / ctb_w` and `rs % ctb_w` | `ctb_w` is constant for the whole picture — a per-picture reciprocal makes both a multiply and a shift | the coding-tree-block loop                    |                                                                                                                                        |                            |

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

| crate          | sites |                         |
|----------------|------:|-------------------------|
| `rusty_aac`    |    97 | encoder psychoacoustics |
| `rusty_mp3`    |    91 | encoder psychoacoustics |
| `rusty_vorbis` |    31 | encoder floor/residue   |
| `rusty_vp9`    |    14 |                         |
| `rusty_flac`   |    12 |                         |
| `rff-resample` |     7 |                         |

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

| table                                                          |          size | verdict                                                                              |       |                                                                                                                |
|----------------------------------------------------------------|--------------:|--------------------------------------------------------------------------------------|-------|----------------------------------------------------------------------------------------------------------------|
| `RANGE_LPS` / `STATE_TRANS` (CABAC)                            | 256 B + 128 B | already **fused** into one `FUSED[u32; 512]`, branchless — the symbolic work is done |       |                                                                                                                |
| `DCT32`                                                        |          2 KB | not a table problem — see the butterfly below                                        |       |                                                                                                                |
| `INV_ANGLE`                                                    |          60 B | **exact closed form found**: `round(8192 /                                           | angle | )` — verified against all 15 entries. Pruned: it needs a division, and a division is worse than a 60-byte load |
| `LEVEL_SCALE`                                                  |          24 B | ≈ `round(40 · 2^(k/6))` but not exactly; needs `pow`. Pruned                         |       |                                                                                                                |
| `CHROMA_QP_420`, `BETA`, `TC`, `INTRA_PRED_ANGLE`, scan tables | 52–140 B each | L1-resident, one load. Pruned per GAPS §1                                            |       |                                                                                                                |

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

| content          | effect                       |
|------------------|------------------------------|
| mainstream inter | **1.143×** (21/21, z = 4.58) |
| all-intra        | **1.068×** (14/15, z = 3.36) |

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

| kernel                                | loop instrs |   SIMD | per output |
|---------------------------------------|------------:|-------:|-----------:|
| `fir_v_avx2` (luma, general)          |          72 |     61 |      2.250 |
| **`fir_v_avx2_sym` (luma, folded)**   |      **57** | **43** |  **1.781** |
| `fir_v_avx2` (chroma, general)        |          40 |     33 |      1.250 |
| **`fir_v_avx2_sym` (chroma, folded)** |      **32** | **23** |  **1.000** |

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

| kernel           | memory-operand ops | explicit `vmovdqu` |
|------------------|-------------------:|-------------------:|
| `fir_h_avx2`     |                 48 |              **0** |
| `fir_v_avx2`     |                 16 |                 16 |
| `fir_v_avx2_sym` |                 17 |                 11 |

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

| stream               | rows elided | rows full |      saved |
|----------------------|------------:|----------:|-----------:|
| `RAP_A_docomo_6`     |   1,277,056 | 1,295,315 | **1.41 %** |
| `PICSIZE_A_Bossen_1` |   3,559,075 | 3,586,446 | **0.76 %** |
| `WP_A_Toshiba_3`     |     341,263 |   344,213 | **0.86 %** |

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

| kernel           | vector ops | folded | standalone loads |                     |
|------------------|-----------:|-------:|-----------------:|---------------------|
| `fir_h_avx2`     |         60 |     24 |            **0** | already optimal     |
| `planar_avx2`    |         30 |      6 |                0 | already optimal     |
| `angular_avx2`   |         48 |  **1** |           **11** | ← anomaly           |
| `angular_t_avx2` |        131 |      9 |               17 | ← anomaly           |
| `fir_v_avx2`     |         61 |      0 |                9 | folded, see above   |
| `edge_avx2`      |         61 |      3 |               12 | vertical class only |

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

| kernel           |    vector ops |     folded | standalone loads |
|------------------|--------------:|-----------:|-----------------:|
| `angular_avx2`   |   48 → **43** |  1 → **6** |       11 → **6** |
| `angular_t_avx2` | 131 → **123** | 9 → **17** |       17 → **9** |

−10.4% and −6.1% of the hot loop, five and eight loads promoted into operands.
Population is `RT_INTRA_ANG_ROW` = 529,447 calls on `PICSIZE_A_Bossen_1`, where
`SAMPLES_INTRA` (32.0M) is more than double `SAMPLES_SAO` (13.2M). Bit-exact,
147/147.

### A latent defect found next to it

`mullo_epi16` keeps the low 16 bits, so `f·(a − b)` must fit `i16`:

| bit depth | max &#124;a − b&#124; |     ×31 | fits?                     |
|-----------|----------------------:|--------:|---------------------------|
| 8         |                   255 |   7,905 | yes                       |
| 10        |                 1,023 |  31,713 | yes — 3% of headroom left |
| 12        |                 4,095 | 126,945 | **no, wraps silently**    |

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

| version                              | instructions | `imul` |
|--------------------------------------|-------------:|-------:|
| specification form, closure accessor |         1248 |     41 |
| + algebraic factoring                |         1232 |     41 |
| **+ hoisted addressing**             |      **790** |  **9** |

**−36.7% instructions, −78% multiplies**, and end to end **1.041x (16/18,
z = 3.30)** -- against a prediction of 4.9% from 36.7% of a 13.3% stage. Counter
and clock agree.

*The lesson is the playbook's own ordering, paid for again: redundancy before
symbolics before SIMD.* The elegant algebra was worth 1.3%; the boring address
arithmetic was worth 35%. A branchy accessor closure is the signature -- a plain
indexed loop would have been hoisted for free.

### Closing the symbolic list

| candidate                                       | verdict                                                                                                                                        |       |                                      |
|-------------------------------------------------|------------------------------------------------------------------------------------------------------------------------------------------------|-------|--------------------------------------|
| CABAC LPS/state tables                          | already fused branchless; `prom distill` returned rmse 36.9 on a table needing exactness -- PRUNED                                             |       |                                      |
| `DCT32` matrix                                  | **partial butterfly, 1024 -> 352 multiplies, 1.143x**                                                                                          |       |                                      |
| DC-only transform                               | **collapse to one scalar + fill, 1.074x on 71%-DC content**                                                                                    |       |                                      |
| `INV_ANGLE`                                     | exact closed form `round(8192/                                                                                                                 | angle | )` found -- PRUNED, needs a division |
| `LEVEL_SCALE`                                   | ~`round(40·2^(k/6))`, inexact, needs `pow` -- PRUNED                                                                                           |       |                                      |
| small tables (`CHROMA_QP`, `BETA`, `TC`, scans) | L1-resident, one load -- PRUNED per GAPS §1                                                                                                    |       |                                      |
| MC symmetric filter                             | **vertical fold, −21% instructions, −30% SIMD ops**                                                                                            |       |                                      |
| MC zero end-taps                                | **row elision, 0.76-1.41% of FIR rows**                                                                                                        |       |                                      |
| angular two-tap                                 | **sign flip frees a load, −10.4% / −6.1%**                                                                                                     |       |                                      |
| deblock strong filter                           | **factored, −1.3%**; addressing **−36.7%**                                                                                                     |       |                                      |
| planar incremental (`R(y+1) = R(y) − top[x]`)   | PRUNED -- the load lens shows planar already has 0 standalone loads; making the recurrence explicit costs back what the `mullo_epi32` saves    |       |                                      |
| chroma deblock                                  | already minimal (`(q0−p0)<<2`), no multiply to remove                                                                                          |       |                                      |
| `bypass_bits` as long division                  | PRUNED on population: **1.86 bins per call** (504,582 bins / 271,677 calls). One 64-bit division ~30 cycles against ~8 for two loop iterations |       |                                      |

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

| route                                         |   count |     share |
|-----------------------------------------------|--------:|----------:|
| `RT_DEBLOCK_SKIP` (rejected before filtering) | 170,582 | **39.0%** |
| `RT_DEBLOCK_WEAK`                             | 232,625 |     53.2% |
| `RT_DEBLOCK_STRONG`                           |  33,558 |      7.7% |

Two of every five segments never reach the kernel at all. Vectorising the
decisions would have paid the eight-tap gather on all of them. Only
`|delta| < 10·tc` is per line, and that is a lane mask.

Reachability confirmed by census rather than assumed: **`DEBLOCK_LUMA_SIMD` =
266,183 against `DEBLOCK_LUMA_SCALAR` = 761** — 99.7%. The 761 are picture-edge
segments where the conservative bounds check declines and the twin serves them.

#### Result

| change                           | effect                  | verdict                              |
|----------------------------------|-------------------------|--------------------------------------|
| address hoist (previous section) | **1.041x** whole decode | 16/18, z = 3.30                      |
| **the SIMD kernel**              | **1.032x** whole decode | 16/19, z = 2.98                      |
| strided merged scan              | 1.020x                  | 12/18, **z = 1.41 -- NOT a verdict** |

And the stage itself, re-priced the same way it was priced at the start:

|                     | deblocking as a share of decode     |
|---------------------|-------------------------------------|
| before the campaign | **13.3%** (1.154x, 19/20, z = 4.02) |
| after               | **6.5%** (1.070x, 20/21, z = 4.15)  |

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

### The inverse transform — ten wins, and the one that did not land

The transform was the largest stage in the anatomy at **27.1% of decode**, and it
was **entirely scalar** — no kernel, and an inner loop written the way the
specification reads rather than the way a machine executes it.

#### The enabling fact: `i64` was never needed

Every accumulation was `sum += c as i64 * T[k][j] as i64`. The bound says
otherwise. Coefficients are clipped to `+/-32768` before the first pass and again
between passes, so the worst output is `32768 * max_j sum_k |T[k][j]|`. Computed
from the table itself:

```text
  worst case  61,014,016      i32 ceiling  2,147,483,647      headroom  35x
```

`transform_accumulator_fits_i32` asserts this **from `DCT32` directly**, so
regenerating the table cannot silently invalidate it, and it fails if the margin
ever drops below 4x. The `i64` cost a widening per coefficient, double the
register pressure, and — the expensive part — half the lanes of any future
vector form.

#### The interchange

The odd part of the butterfly was written output-outer:

```rust
for j in 0..half { for k in (1..nz).step_by(2) { odd += src[k*s] * T[k*step][j] } }
```

Every one of those `N^2/4` inner steps reloads `src[k*s]`, recomputes the row
address `k*step`, re-tests `c != 0`, and walks the table DOWN a column — a stride
of 32 `i16`. None of that is the arithmetic. Interchanged to coefficient-outer,
each coefficient is loaded once, its row address is formed once, the zero test
skips a whole row instead of one product, and the inner loop is a contiguous scan.

#### The ten

| #   | win                                             | effect                               |
|-----|-------------------------------------------------|--------------------------------------|
| 1   | `i64` -> `i32` accumulators                     | every sign-extension gone: 31 -> 0   |
| 2   | loop interchange to coefficient-outer (DCT)     | contiguous table access              |
| 3   | table row pointer hoisted out of the inner loop | one address per `k`, not per `(j,k)` |
| 4   | zero test hoisted                               | skips a row, not a product           |
| 5   | source load hoisted                             | one load per coefficient             |
| 6   | `32 / n` -> `32 >> n.trailing_zeros()`          | a shift, not a divide                |
| 7   | `clip` runtime bool -> const generic            | was tested once per output sample    |
| 8   | `RH265_NAIVE_IDCT` read hoisted per-block       | was an atomic load per 1-D transform |
| 9   | `sums` array `i64` -> `i32`                     | 256 -> 128 bytes of stack per call   |
| 10  | the same interchange for the 4-point DST        | **-72.7%**                           |

Measured on the emitted assembly:

| function    | before                       | after         | delta      |
|-------------|------------------------------|---------------|------------|
| `idct_sums` | 243 instrs, 10 imul, 5 widen | 195, 7, **0** | **-19.8%** |
| `idct_1d`   | 195, 2, 1                    | 145, 1, **0** | **-25.6%** |
| `idst_1d`   | 395, 30, 25                  | 108, 4, **0** | **-72.7%** |

End to end, paired:

| content          | effect     | verdict         |
|------------------|------------|-----------------|
| mainstream inter | **1.163x** | 17/17, z = 4.12 |
| all-intra        | **1.079x** | 15/17, z = 3.15 |

147/147 bit-exact, and `butterfly_matches_naive` still pins the sums against the
naive `N^2` oracle at every size, `nz` and stride.

#### The one that did not land

The interchange was *also* meant to auto-vectorise: a contiguous
`out[0..half] += c * T[k][0..half]` is exactly the shape a vector unit wants.
It did not. The emitted `idct_sums` carries **7 vector instructions**, which is
incidental, not a vectorised loop.

Two reasons, both structural: the inner trip count is `half`, which is 2, 4, 8 or
16 and often below the threshold LLVM will widen for, and the table is `i16`
against an `i32` accumulator, so every lane needs a widening the compiler must
prove worthwhile. **The −19.8% came from the hoisting and the narrowing, not from
vectorisation** — and saying so matters, because the obvious next step is a
hand-written kernel and the case for it rests on this loop still being scalar.

### The transform kernel — what the compiler would not do

The scalar pass left the transform at **17.5% of decode** (1.212x, 14/15,
z = 3.36; 25.5% on all-intra), restructured into exactly the shape a vector unit
wants, and still scalar. `crates/rusty_h265-accel/src/itx.rs` is the kernel.

#### One primitive

Both of the butterfly's accumulation sites are the same operation:

```text
  out[j] = SUM over k of  src[k * s_in] * tab[k * tstep][j],   j < LEN
```

-- the base case with `(k0, kstep) = (0, 1)` and the odd part with `(1, 2)`.
`LEN` is 4, 8 or 16 (a butterfly half), so it is a const generic and the kernel
has three shapes rather than a runtime bound.

#### The three things the compiler could not get past

| obstacle                       | why LLVM declined                      | what the kernel does                         |
|--------------------------------|----------------------------------------|----------------------------------------------|
| trip count 2/4/8/16            | below its widening threshold           | `LEN` is const, so there is no threshold     |
| `i16` table, `i32` accumulator | a per-lane widening it must justify    | `vpmovsxwd`, one instruction, load folded in |
| 16 live `i32` accumulators     | spills them to stack every coefficient | two `ymm` registers, never leave the file    |

The third is the largest. The scalar form reloads and restores its whole
accumulator for **every coefficient**; the kernel holds it in registers across
the entire loop.

`i32` lanes are what the preceding scalar pass bought. With the `i64`
accumulators the code had that morning, a 256-bit register would hold four lanes
instead of eight -- the narrowing was the prerequisite, not a separate win.

#### Result

| content             | effect     | verdict         |
|---------------------|------------|-----------------|
| mainstream inter    | **1.036x** | 18/20, z = 3.58 |
| all-intra           | **1.052x** | 17/21, z = 2.84 |
| weighted prediction | 1.011x     | 13/17, z = 2.18 |

Reachability proven by census, not assumed: **`ITX_ACCUM_SIMD` = 2,469,361
against `ITX_ACCUM_SCALAR` = 0.** 147/147 bit-exact, and `accum_matches_scalar`
sweeps both call sites, all three lengths, every `nz`, four table strides and the
strides both transform passes use -- with a third of coefficients forced to zero
so the per-coefficient skip is exercised rather than assumed.

#### The dispatcher ate half the win

First measurement: **1.019x, z = 1.94 -- not a verdict.** The kernel was right;
the code around it was not. `accum` runs 2.47 million times a clip, and each call
was paying:

* an integer **division**, `(nz - 1 - k0) / kstep`, to bound the highest
  coefficient -- when `nz - 1` bounds it for either call site without dividing;
* **two** atomic loads, a `OnceLock` for the bring-up switch and `isa()` for the
  ISA -- when both are process-constant and collapse into one cached answer.
  `isa()`'s own documentation says callers hoist it above their loops.

|            | before           | after                |
|------------|------------------|----------------------|
| mainstream | 1.019x, z = 1.94 | **1.036x, z = 3.58** |
| all-intra  | 1.045x, z = 3.00 | **1.052x, z = 2.84** |

*A kernel is not the whole change; the dispatch is on the hot path too.* Half the
arithmetic win was being spent before the kernel was reached, and the symptom was
a result that read as noise.

#### What this kernel does not cover

AVX2 only. SSE2 has no `pmulld` and no `pmovsxwd`, so a Baseline machine takes
the scalar twin -- correct, and slower. An SSE4.1 arm would close that, and the
`Isa` enum would need a rung between `Baseline` and `Avx2` to express it.

### Finishing the transform — ten more, and what is still not won

The accumulate kernel left the rest of the transform scalar. Counting per
32-point transform: **28 combine iterations** (16 + 8 + 4 across recursion
levels) and **32 shift/clip iterations** -- 60 scalar steps against a vectorised
accumulate.

| #   | win                                 | what it removed                                         |
|-----|-------------------------------------|---------------------------------------------------------|
| 1   | `shift_clip` kernel, 8-lane         | `n` scalar add/shift/clamp per 1-D transform            |
| 2   | `shift_clip` 4-lane path            | 16,836 calls a clip that fell below the 8-lane floor    |
| 3   | butterfly output stage vectorised   | `half` reversed scalar stores; `vpermd` reverses in one |
| 4   | **fused accumulate + butterfly**    | `half` stores + `half` loads, 1.66M times a clip        |
| 5   | superseded kernel deleted           | a counter that would have read 0 forever                |
| 6   | DST through the shared accumulate   | the 4-point DST's own scalar loop                       |
| 7   | DST shift/clip through the kernel   | its scalar output loop                                  |
| 8   | dispatcher division removed         | an integer divide on 2.47M calls                        |
| 9   | two atomic loads to one cached bool | `OnceLock` + `isa()` per call                           |
| 10  | `RH265_NAIVE_IDCT` branch deleted   | a test per 1-D transform for a finished A/B             |

| content             | kernels vs scalar | verdict         |
|---------------------|-------------------|-----------------|
| mainstream inter    | **1.078x**        | 20/20, z = 4.47 |
| all-intra           | **1.108x**        | 20/21, z = 4.15 |
| weighted prediction | **1.045x**        | 18/21, z = 3.27 |

Up from 1.036x / 1.052x / 1.011x with the accumulate alone. 147/147 bit-exact.

#### Number 4 is the one worth remembering

Two correct kernels either side of a scratch array is not the same as one kernel.
`accum` stored `half` sums and `butterfly` immediately reloaded them -- for
values that were already in registers, 1.66 million times a clip. Fusing them was
worth more than either kernel's own arithmetic refinement.

#### Number 5 is the one the census caught

Fusing left `butterfly` with tests, a dispatcher, a counter -- and **no caller**.
`ITX_BUTTERFLY_SIMD` and `ITX_BUTTERFLY_SCALAR` both read 0, which is
indistinguishable from a cold path and is exactly how a dead kernel survives.
Deleted rather than kept as a spare part.

#### Not won

* **AVX2 only.** SSE2 has no `pmulld` or `pmovsxwd`, so a Baseline machine takes
  the scalar twin throughout. An SSE4.1 rung in the `Isa` enum would fix it.
* **The column pass's output is still scalar.** Stage 1 writes with stride `n`,
  and AVX2 has no scatter, so vectorising the arithmetic there buys three vector
  ops against eight stores that have to happen anyway -- pruned on that
  arithmetic, not measured.
* **The recursion is still three calls deep** for a 32-point transform.
* `sums` is a `[i32; 32]` zeroed on every 1-D transform when only `n` entries are
  read. Whether LLVM elides it was not established, so it is not claimed.

### The SSE4.1 rung, and a static instruction count used wrongly

Two items the previous pass listed as *not won*.

#### The rung

`Isa` had `Scalar | Baseline | Avx2`, and the transform kernels gated on
`== Avx2`, so every pre-2013 machine ran the whole transform scalar. SSE4.1 is
exactly where the instructions this transform needs arrive -- `pmulld`,
`pmovsxwd`, `pminsd`/`pmaxsd` -- and none of them exist in SSE2. Adding the rung
between `Baseline` and `Avx2` costs nothing elsewhere: an existing
`match isa() { Isa::Avx2 => .., _ => sse2_kernel }` routes `Sse41` to the SSE2
arm, which is correct because SSE4.1 is a superset.

| arm    | what a pre-AVX2 machine gets              |
|--------|-------------------------------------------|
| before | the scalar twin, for the entire transform |
| after  | **1.077x** (15/17, z = 3.15)              |

**`RH265_ISA` caps the detected level** (`scalar` / `baseline` / `sse41`) so the
rung can be exercised on a machine that has AVX2 -- which is every machine here.
An arm no test can reach is an arm nobody has verified, which is the same reason
every kernel keeps a `RH265_SCALAR_*` switch. All 28 accel tests and the full
147-stream corpus pass on the SSE4.1 rung, not just on AVX2.

Deleting the now-redundant `shift_clip4_avx2` fell out of it: the SSE4.1 arm
handles any multiple of four, so the dedicated 4-lane AVX2 version lost its
caller. Second dead kernel this campaign, both found the same way.

#### The recursion, and the instrument that misjudged it

Flattening the three-deep butterfly recursion into a descend-and-unwind loop was
**reverted, then restored**, and the reversal is the useful part.

Measured by STATIC instruction count -- the size of the emitted body --
`idct_sums` went **94 -> 117**, and that read as a clear regression. The count
was right; the inference was wrong. **Turning a recursion into a loop grows the
body by construction**, because the callee's work moves inline. What executes
loses three call prologues and epilogues.

Static size is a good proxy for a kernel's inner loop and a poor one for
restructuring a call graph. At 31 pairs:

| content    | flattened vs recursive          | verdict                  |
|------------|---------------------------------|--------------------------|
| all-intra  | **0.996x**, 21/30 for flattened | z = -2.19                |
| mainstream | 1.000x, 14/21 for flattened     | z = -1.53, not a verdict |

Never slower, marginally faster where the transform is hottest -- so it stays.
The first measurement had run at 11 effective pairs (10 of 21 rounds landed as
exact ties at the timer quantum), which was under-powered for an effect this
size; §3's "pairing needs N" applies to ties as much as to spread.

### Motion compensation — fifteen wins, and the one the census found

MC is the largest stage: re-priced after the transform work at **1.263x**
(15/15, z = 3.87), **20.8% of decode**.

The first audit said it was finished -- every `*_SCALAR` counter 0, `fir_h_avx2`
at zero standalone loads, `fir_v_avx2` down to 9 loads from 16, accumulator
init already elided by LLVM, full-pel fast paths in the caller. Seven dispatch
wins came out of that and all measured neutral. **That audit asked the wrong
question.** It checked whether each kernel was vectorised. It did not check
whether every BLOCK reached the vector path.

#### The census question that mattered

Weighting samples by block width:

| stream        |                `w < 8` |           `w == 8..15` |   `w >= 16` |
|---------------|-----------------------:|-----------------------:|------------:|
| mainstream    |         850,928 (1.0%) |       4,290,944 (5.1%) |  78,203,392 |
| weighted-pred |        12,480 (0.009%) |              4,099,200 | 137,539,840 |
| **all-intra** | **29,742,720 (18.3%)** | **50,316,288 (31.0%)** |  83,060,736 |

The pixel kernels step 16 or 32 samples (`nvec = w / 16`). In 4:2:0 the chroma
block of an 8x8 luma PU is **4 wide**. So on intra-heavy content **18.3% of
pixel-kernel samples were running the scalar tail**, and another 31% were 8 wide
-- which `put_uni`/`put_bi`/`add_residual` served by calling their SSE2 kernel
ONCE PER ROW, rebuilding four broadcast constants each time, while
`weighted_uni`, `weighted_bi`, `avg_block`, `put_bi_fp` and
`add_residual_const` had no sub-16 vector path at all and went fully scalar.

#### The fifteen

| #   | win                                                                      |
|-----|--------------------------------------------------------------------------|
| 1   | one cached dispatch plan, replacing an `isa()` probe in four dispatchers |
| 2   | `LUMA_SYM`/`CHROMA_SYM` tables replacing a per-call `N/2` tap scan       |
| 3   | `sym_fold()`'s `OnceLock` folded into that plan                          |
| 4   | vertical fallback dispatches directly instead of re-probing              |
| 5   | one `census::enabled()` per `interp` -- it was four on the 2-D path      |
| 6   | dead `sym_fold()` helper removed                                         |
| 7   | edge-pad repeated clamped rows de-duplicated                             |
| 8   | 4-wide step in `put_uni_sse2`                                            |
| 9   | 4-wide step in `put_bi_sse2`                                             |
| 10  | 4-wide step in `add_residual_sse2`                                       |
| 11  | `w < 16` delegated as ONE call, not one per row (three AVX2 kernels)     |
| 12  | 8- and 4-wide steps in `put_bi_fp` -- was fully scalar under 16          |
| 13  | 8- and 4-wide steps in `add_residual_const` -- was fully scalar under 16 |
| 14  | 8- and 4-wide steps in `weighted_uni` -- was fully scalar under 16       |
| 15  | 4-wide step in `avg_block`                                               |

| stream        | effect     | verdict         | `w < 8` population |
|---------------|------------|-----------------|--------------------|
| all-intra     | **1.021x** | 17/21, z = 2.84 | 18.3%              |
| mainstream    | **1.024x** | 12/15, z = 2.32 | 1.0%               |
| weighted-pred | 1.000x     | 9/19, z = -0.23 | **0.009%**         |

**The third row is the evidence the first two are real.** The win appears
exactly where the census put the population and vanishes where it did not, which
is a much stronger claim than two isolated verdicts.

Wins 1-7 measured 1.000x on their own: 251,001 MC calls against the transform's
2,469,361, so the same dispatch cleanup worth half a kernel there is ~0.4% here.
Kept as strictly-less-work with proven parity, claimed as nothing.

#### The measurement that nearly recorded a 27% regression

The dispatch batch first read **0.788x, 0/25, z = -5.00**, and nothing about it
resembled noise. It was not noise. It was not a regression either.

`cargo build --features bench-alloc` had been followed by `cargo test`, which
relinks the same path WITHOUT the feature -- so one arm ran rusty_alloc and the
other the system allocator, measured 1.133x apart on this decoder. The bisect
exposed it as arithmetically impossible: `before -> newmc` read 1.024x and
`newmc -> current` read 1.000x, which cannot multiply to 0.788x.

**`ab.ps1` had no allocator guard**, though `codec-bench.ps1` and `pinbench.ps1`
both did. It has one now, verified to fire. Every A/B run through that harness
before today was unguarded.

### A second narrow-block sweep — ten more, and all of them neutral

Asked whether MC held another ten, the answer was yes: the block-width audit had
only been applied to the PIXEL kernels. Running the same question over the MC
FILTERS, intra and SAO found ten more paths that whole common widths reached
through a scalar tail.

| #   | win                             | what was scalar                                       |
|-----|---------------------------------|-------------------------------------------------------|
| 1   | `dc_fill_sse2` 4-wide store     | `nvec = n / 8`, so 4x4 wrote a row at a time          |
| 2   | `planar_avx2` delegates `n < 8` | its SSE2 twin is 4-wide; this one is not              |
| 3   | `fir_v` two-row body, 8-wide    | stepped 16, then scalar                               |
| 4   | `fir_v` two-row body, 4-wide    | as above                                              |
| 5   | `fir_v` odd-row tail, 8-wide    | the same gap in the second tail                       |
| 6   | `fir_v` odd-row tail, 4-wide    | as above                                              |
| 7   | **`fir_h_sse2_tail` 4-wide**    | **fully scalar**, and it serves every row under 8     |
| 8   | `copy_shift_sse2` 4-wide        | full-pel copy of a 4-wide block, one sample at a time |
| 9   | `weighted_bi` 8-wide            | the kernel the first sweep missed                     |
| 10  | `weighted_bi` 4-wide            | as above                                              |

**All ten measured 1.000x** (z = 0.00 on all-intra, z = 0.73 on mainstream), and
the census says why -- the same census that predicted the first sweep's win:

|            | first sweep (pixel kernels)                    | second sweep                                                                       |
|------------|------------------------------------------------|------------------------------------------------------------------------------------|
| population | `w < 8` **18.3%**, `w == 8` **31.0%** on intra | MC filters `w < 8` **0.6%**, `w == 8` 3.2%; and **zero** on intra, which has no MC |
| result     | **1.021x / 1.024x**, z = 2.84 / 2.32           | 1.000x, not a verdict                                                              |

Four percent of MC's samples, on a stage that is 20.8% of decode, is ~0.8% --
under the floor by construction. The intra half looked more promising at 28.4%
of samples in `n < 8` blocks, but `angular_row_sse2` **already had** a 4-wide
step, and angular is the bulk of intra: `dc_fill` and `planar` are the minority
of that 28.4%.

So: ten real reductions in work, bit-exact, 147/147 on both ISA rungs, kept --
and **claimed as nothing**, because the clock cannot see them and the population
says it never could. The value of the first sweep was not the technique; it was
that the technique met a large population.

#### An incident worth recording

`mc.rs` was reduced to **0 bytes** mid-campaign. A patch script did
`io.open(path, "w")`, which TRUNCATES before writing, and the write then failed
with a permission error -- leaving an empty file that still compiled the crate's
other modules, so the first symptom was a test count quietly dropping from 29 to
22. Restored from a scratch copy. Every patch script now writes to a sibling
temp and `os.replace`s it, so a failed write cannot destroy the original.

### CABAC — seventeen wins, and the counter that pointed at all of them

CABAC was the last large stage with no campaign. `decode` was already fully
branchless with a fused `lps | transMps<<8 | transLps<<16` LUT, so there was no
symbolic room left in the arithmetic itself. The wins had to come from the work
*around* each bin — which is the MC lesson restated: **ask whether every call
site reaches the efficient path, not just whether the core primitive is
efficient.**

The census settled where to look before anything was written:

| counter | mainstream 720p | `ipred_x24` | `txskip_x40` |
|---|---:|---:|---:|
| `CABAC_CTX_BINS` (context-coded) | 4,001,542 | 58,431,960 | 20,354,080 |
| `CABAC_BYPASS_BINS` | 654,921 | 19,934,640 | 17,434,400 |
| `CABAC_TERM_BINS` | 15,060 | 49,920 | 4,640 |

Context-coded bins are **89%** of the population, so the exchange rate on
`decode` is one instruction per four million. And two counters were flatly
embarrassing: the residual parser zeroed **11.4 M** `i32` per 720p stream when
the last-significant position said only 3.7 M were live (**178.8 M against
22.6 M** on intra content), and it spent 650 K tuple compares per stream doing a
*linear search* of the scan table for a value one load could give it.

#### The nine engine wins

1. **`FUSED` becomes state-major over the full byte domain.** Quartile-major put
   a context's four records 512 B apart — four cache lines per context, three of
   them cold. State-major puts them in one aligned 16-byte group, so the line a
   bin touches serves that context whatever `ivlCurrRange` is. Mirroring the
   table into 128..=255 also retires the `& 127`: a model byte is
   `pStateIdx * 2 + valMps` with `pStateIdx < 64`, so the mask never changed the
   value — it was there to prove the index in range, and the wider table proves
   it for free.
2. **The trace hook leaves the per-bin path.** `if self.trace` was a load, a
   test and a branch on every context bin, every bypass bin and every terminate
   bin, serving a debugging facility no shipping decode reads. Behind
   `cabac-trace` (default off) it is exactly as useful and costs nothing.
3. **The bypass compare is branchless.** A bypass bin is an equiprobable coin
   flip, so `if low >= scaled` mispredicted about half the times it ran — 0.65 M
   bins on a 720p stream, 19.9 M on intra content. `d = low - scaled` wraps
   negative exactly when the bin is 0 (both operands are below `2^51`, so bit 63
   is the sign); adding `scaled & m` back restores `low` in that case.
4. **`bypass_bits` hoists `scaled`.** `ivlCurrRange` is invariant across bypass
   bins, so the 64-bit shift the old per-bin `bypass()` recomputed every time is
   loop-invariant.
5. **…and keeps `low`/`cnt` in registers** for the run instead of round-tripping
   them through the struct once per bin.
6. **`bypass_bits(0)` returns immediately.** The census reads 1.86 bins per
   call: `rice` and the last-position suffix width are 0 often enough that
   skipping the run setup pays.
7. **`bypass_ones`** gives the unary prefix of `coeff_abs_level_remaining` the
   same loop-invariant treatment. That prefix plus EGk is most of the bypass
   population on coefficient-heavy content.
8. **`eg_k`'s prefix is closed form.** `sum(1<<(k0+j))` over the run is
   `((1<<m)-1)<<k0`, not an add and a bounds test per bin.
9. **The `bypass_bits` census check becomes a `const`.** `census::enabled()` is
   a `OnceLock` read — an atomic load plus a branch — which is fine per kernel
   call and is *itself* the cost being measured per bin. A new
   `census::ALWAYS = cfg!(feature = "census")` compiles the per-bin and
   per-coefficient instrumentation away entirely in the shipping build.

Measured as static instruction counts of one call's body, from the emitted `.s`:

| primitive | before | after | delta |
|---|---:|---:|---:|
| `decode` | 142 | 113 | **−29 (−20.4%)** |
| `bypass` | 106 | 78 | **−28 (−26.4%)** |
| `bypass_bits` | 144 | 100 | **−44 (−30.6%)** |
| `terminate` | 119 | 90 | **−29 (−24.4%)** |

#### The eight residual-parser wins

10. **The zero-fill is bounded by the coded sub-block box.** The parse can only
    write into sub-blocks at or before `last_sb` in the scan, so `scan_bbox`
    (a new prefix-cumulative-max table) bounds the written region exactly; and
    for a real transform the consumers — `dequant`, then stage 1 — read only
    inside `nz_w × nz_h`, which sits inside that same box. Transform-skip and
    transquant-bypass pass the block through untransformed and therefore read
    every sample, so those two kinds still clear all of it, and both are known
    before the coefficient loop starts.
11. **`last_sb` by table lookup**, not a linear search of the forward scan.
12. **`last_pos` likewise.** `last_x`/`last_y` are range-checked against `n`
    immediately above, so the "not found" arms both searches carried were
    unreachable.
13. **The significance context's sub-block term is hoisted.** The `+3` for a
    non-DC sub-block and the `+9/+15/+21` (luma) or `+9/+12` (chroma) size term
    depend only on the sub-block, the block size and the component — and were
    recomputed for every one of 2.3 M scanned positions (18.8 M on intra).
14. **Its neighbour term becomes a 192-byte constant.** The spec writes it as a
    decision on `prevCsbf` and then on `(xP, yP)`; as a nest of compares it ran
    up to four branches per scanned position. `SIG_NB[scanIdx][prevCsbf][n]` is
    one load.
15. **The significance loop stops recovering coordinates.** With the 4×4 map
    re-keyed by scan position (`SIG_CTX_4X4_BY_SCAN`), the loop needs nothing
    from `(xP, yP)` or `(xC, yC)` at all — two table loads and four
    adds/shifts per scanned position, gone. The `xC + yC == 0` DC rule becomes
    `dc_sb && n == 0`, and because the 4×4 map already reads 0 at (0,0), one
    test serves both block sizes.
16. **`csbf` is a `u64` bitmask**, not a 64-byte `[[bool; 8]; 8]` cleared on
    every one of 3.0 M transform blocks.
17. **`g1` is a `u16` bitmask**, not a 16-byte array cleared per sub-block.

Work counters, same three streams:

| counter | mainstream | `ipred_x24` | `txskip_x40` |
|---|---|---|---|
| `RES_FILL_STORES` | 11,415,488 → **7,346,208** | 178,781,952 → **67,291,776** | 10,590,080 → 10,590,080 |
| `RES_SCAN_SEARCH` | 650,291 → **96,184** | 15,297,504 → **6,078,336** | 10,233,320 → **1,323,760** |

`txskip_x40`'s fill is unchanged and that is the correct answer: every block on
it is transform-skip, which reads the whole block and must therefore clear the
whole block. A counter that had moved there would have been the bug report.

Work-count parity (`codec-measurement` §4) is exact on all three streams:
`CABAC_CTX_BINS`, `CABAC_TERM_BINS`, `RES_BLOCKS`, `RES_SIG_SCANNED` and
`RES_SIG_CTX` are **identical** before and after. The same bins are decoded from
the same bitstream positions; only the work per bin changed. (`CABAC_BYPASS_BINS`
rose from 504,582 to 654,921 — not new work, a fixed counter: the old one was
bumped only by `bypass_bits`, so every bin taken through `bypass()` directly —
signs, unary prefixes, EGk — went uncounted.)

The clock, confirming (pinned, High priority, CPU time, ABBA-interleaved, paired
per-round ratios, two separate binaries under `bench-alloc`):

| stream | median | win rate | z |
|---|---|---|---|
| `in_to_tree_720p_8bit` | **1.024×** | 25/35 | 2.54 |
| `ipred_x24` | **1.026×** | 21/25 | 3.40 |
| `txskip_x40` | **1.098×** | 22/25 | 3.80 |

`txskip_x40` moving most is the prediction coming true: it is the
bypass-heaviest stream in the set (17.4 M bypass bins against 20.4 M context
bins, where mainstream runs 0.65 M against 4.0 M), and the bypass path is where
the largest per-bin reduction landed.

#### Two instrument failures, both already in this ledger under other names

The first probe run reported `decode` at **2225 instructions** and **+0 for
every primitive in both arms**. Two `.s` files exist in `deps/`, and the *larger
one was three hours stale* — so "take the largest", which is the guard that
fixed the duplicate-`filter_luma_edge` problem during the deblocking campaign,
is precisely what broke this one. The probe now deletes every emitted `.s`
before each build. §7 caught it: 2225 instructions for a twenty-line branchless
function cannot be true.

Then `bypass_bits` read **+65, an apparent regression**. That was the
instrument, not the code: the probe pins `bypass` out of line, and in the
baseline arm `bypass_bits`'s per-bin body is *an inlined copy of `bypass`* — so
pinning the callee turned the loop body into a call and left only a 35-line
shell to count. Measured with only itself pinned, in a build where `bypass`
inlines as it does in production, it is **144 → 100**. `codec-measurement` §6:
the instrument is part of the system under test, and probes that call each other
must be measured in separate runs.

### CABAC, second sweep — ten more, and the asm listing that found them

The first sweep took the counters as far as reasoning about the source could.
The second started by *reading the emitted `decode`*, which immediately showed
three things no amount of source-reading had:

```
	cmpq	$156, %rdx          ; the "clamp" -- three instructions, on the
	movl	$156, %r10d         ; dependency chain that feeds the model load
	cmovbq	%rdx, %r10
	movzbl	40(%rcx,%r10), %eax
```

and, past the refill branch, ~55 instructions of byte-at-a-time bounds-checked
fallback for reading the last four bytes of a slice segment — inlined, because
`decode` is `#[inline(always)]`, into every one of its ~50 call sites.

28. **The context array is padded to 256 so the guard is one `and`.** Every
    call site is a constant base plus a bounded offset (checked by hand for
    `sig`, `gt1`, `gt2` and both last-significant prefixes), so
    `ctx_idx.min(NUM_CTX - 1)` has never clamped anything — it was proving the
    index in range. `CTX_PAD = 256` proves it with a single mask instead of
    `cmp`/`mov`/`cmov`, with no panic edge and no `unsafe`. The 99 pad bytes
    are never read or written.
29. **`refill`'s near-end fallback is `#[cold]` and out of line.** It runs a
    few times per slice segment and never touched the hot path's *speed* — only
    how far the hot path was spread across the instruction cache. This is the
    single largest item in the round: it takes ~23 instructions out of
    `decode`, `bypass` **and** `terminate` alike, because `refill` was inlined
    into all three.
30. **`sao_offset_abs` is truncated unary** (§9.3.3.2) — `bypass_ones`, not a
    hand-rolled `while … bypass() == 1`.
31. **`mpm_idx` likewise**, where the binarisation had been written out as
    nested `if self.cab.bypass() == 0` arms.
32. **`merge_idx`'s bypass suffix likewise.**
33. **The sign bits are pre-aligned.** They are consumed MSB-first, and the old
    form recovered bit `nsigns - 1 - k` per coefficient: two subtracts, a
    variable shift, a mask, and a `k < nsigns` guard. Left-aligned with
    `wrapping_shl(32 - nsigns)`, the sign is just the sign bit of an `i32` and
    the guard *disappears* — past `nsigns` the shifts have brought in zeros,
    which is exactly "not negative".
34. **`first_g2` is a sentinel, not an `Option`.** 16 is outside a sub-block's
    0..=15 scan positions, so the per-coefficient test is one compare instead
    of a discriminant plus a payload compare.
35. **The `gt1` context set is hoisted** out of its run — only `c1` moves.
36. **The last-significant prefix loops hoist their context base**, leaving only
    `px >> ctx_shift` in the loop.
37. **The per-block census routes become compile-time.** `route`/`arm` call
    `enabled()`, whose `OnceLock` read is an atomic load plus a branch, and
    those sites run per transform block and per intra block — 3.0 M each on
    intra-heavy content. Per-block and finer sites now test
    `census::ALWAYS`; per-picture ones keep the runtime switch.

Static instruction counts, cumulative over both sweeps:

| primitive | pre-campaign | after sweep 1 | after sweep 2 | total |
|---|---:|---:|---:|---:|
| `decode` | 142 | 113 | **90** | **−36.6%** |
| `bypass` | 106 | 78 | **55** | **−48.1%** |
| `terminate` | 119 | 90 | **66** | **−44.5%** |
| `bypass_bits` | 144 | 100 | — | **−30.6%** |

Work-count parity again exact: every counter — `CABAC_CTX_BINS`,
`CABAC_BYPASS_BINS`, `CABAC_TERM_BINS`, `RES_BLOCKS`, `RES_FILL_STORES`,
`RES_SCAN_SEARCH`, `RES_SIG_SCANNED`, `RES_SIG_CTX` — reads **identical** to
sweep 1 on all three streams. Sweep 2 removed no work at all; it made the same
work cheaper. (`CABAC_BYPASS_CALLS` moved 317,010 → 357,016, which is the
counter and not the decoder: bins that used to go through bare `bypass()`
now go through `bypass_ones`, which counts a call.)

The clock. Sweep 2 alone, then the campaign end to end:

| stream | sweep 2 | z | **cumulative** | z |
|---|---|---|---|---|
| `in_to_tree_720p_8bit` | 1.000× | 0.00 | **1.024×** | 3.80 |
| `ipred_x24` | 1.020× | 3.05 | **1.047×** | 4.13 |
| `txskip_x40` | 1.018× | 2.45 | **1.119×** | 5.21 |

Two of sweep 2's three are verdicts. The mainstream 1.000× is **inside the
noise floor, not a measured loss** (`codec-measurement` §12) — that stream runs
4.0 M bins in ~625 ms against a 15.6 ms timer quantum, so a ~1% per-bin effect
has no resolution there; the cumulative column, where the effect is four times
larger, resolves it at z = 3.80. And the three columns compose:
1.026 × 1.020 = 1.046 against 1.047 measured, 1.098 × 1.018 = 1.118 against
1.119. A cumulative number that did not equal the product of its rounds would
have meant one of them was measuring something else.

#### What reading the asm was worth

Every item in this sweep except the three `bypass_ones` rewrites came from the
listing rather than from the source. The clamp looked free in Rust (`.min()` on
a `usize`) and cost three instructions on the critical path; the refill fallback
looked cold and *was* cold, and still cost more than anything else here because
`#[inline(always)]` had replicated it fifty times. **Neither is visible without
looking at what was emitted** — which is the same lesson as the B7b nz-map entry
further up this file, arriving from the opposite direction: there the asm showed
a loop that had *not* vectorised, here it showed cold code that had been
copied everywhere.

### CABAC, third sweep — fifteen more, six refutations, and a better instrument

The second sweep ended with the residual inner loops at a local optimum. This
one started by building the instrument that should have existed all along, and
then followed it out of the entropy coder into the syntax layer around it.

#### The instrument, and why the probe was the wrong one

The `#[inline(never)]` probe used in sweeps 1 and 2 gives a primitive a symbol
to count — and changes inlining around it. Checked against the shipping build:
`decode` is fully inlined there (no symbol, no call sites) while `refill` is
not; under the probe, pinning `decode` made LLVM inline `refill` *into* it. So
the probe can move on a change the shipping build never sees.

The replacement counts what actually ships: `residual_block`, `transform_tree`,
`prediction_unit`, `coding_quadtree` and the CABAC symbols, plus the innermost
loops of the hot parser, all from a plain `--emit=asm` release build. It
immediately settled the sweep-2 `refill_tail` claim in the shipping build's own
terms — `coding_quadtree` **2842 with the split vs 3171 without**, −329, so the
cold fallback really had been duplicated and the claim stands.

#### Six candidates that REFUTED

Recorded because a wrong prune is permanent and these were all plausible:

| candidate | verdict |
|---|---|
| `np & 15` on the significance walk | 1931 → 1937, loops 392 → 399 — **worse both ways** |
| `np & 15` on the coefficient walk | function −7 but loops **+31** |
| `& (nn - 1)` on the coefficient index | function −6 but loops **+29** |
| branchless `threshold` | **exactly neutral** — LLVM already emitted it |
| `sig_pos[nsig & 15]` | function −6 but loops **+37** |
| table-driven `renorm` shift | `coding_quadtree` **+25**, loops −1 |

The pattern in four of them is the same and worth naming: **retiring a bounds
check by masking trades a never-taken branch and its out-of-line panic block for
a bigger loop body.** The function shrinks because the panic block goes; the
loop grows because LLVM if-converts differently once the early exit is gone. On
a path that runs per coefficient, the loop is what matters. All six are reverts
by measurement, not by noise (§12).

#### The fifteen that landed

**In the residual parser:**

1. **Five scan-table dispatches become one.** `residual_block` called
   `scan_order` twice, `scan_inverse` twice and `scan_bbox` once — five
   independent `match`es on the same two values, each emitting compares, `cmov`s
   and a `lea` per candidate table. *Three of the five were added by this very
   campaign*, so the per-block dispatch had been growing while the
   per-coefficient loops shrank. One `SCAN_SETS[log2sb][scanIdx]` lookup
   replaces all five: **`residual_block` 2118 → 1950, −168.**
2. **`Error::invalid` and `unsupported` are `#[cold] #[inline(never)]`.**
   `InvalidData` owns a `String`, so each conformance check inlined an
   allocation and a copy of its literal — four of them inside `residual_block`.
   Every `&str` literal shares one monomorphisation, so this is one out-of-line
   body for all of them, and the public enum is unchanged.
3. **The within-sub-block scan tables are `&[_; 16]`, not slices** — a sub-block
   is always 4×4, so the length belongs in the type.
4. **`self.coeffs` is reborrowed once per block.** Indexing it inside the
   coefficient loop reloaded the slice's pointer *and* length from the struct
   on every coefficient.

**In the syntax layer:**

5. **`wrap_qp` no longer divides.** It is `((v + 52 + 2·off) % (52 + off)) − off`
   — a signed division by a *runtime* divisor, tens of cycles — and it ran on
   **every coding unit**, not only those coding a `cu_qp_delta`. `qPY_PRED` is
   in `−off..=51` and `CuQpDeltaVal` is range-checked before the call, so the
   dividend lands in `0..2m` and the modulo can wrap at most twice. `idivl` is
   now absent from `coding_quadtree` and `transform_tree` alike.
6. **§6.4.1 availability is split** into `avail_at` (the current block) and
   `avail_n` (one neighbour). `available` recomputed the current block's z-scan,
   slice and tile indices on *every* neighbour query — `idx4` and `ctb_of` each
   scale by a runtime stride, so two multiplies and two loads — and every caller
   asks about two or more neighbours of the same block.
7. …**split_cu_flag**'s two neighbour tests share one `avail_at`.
8. …**cu_skip_flag**'s two share one.
9. …the **quantization group**'s qPY_A/qPY_B share one.
10. …**`mpm_candidates`**' two candidates share one.
11. …the **intra reference gather** shares one, and it asks hardest: up to
    `4n + 1` neighbours of a single block.
12. **`available` now inlines everywhere.** It was out of line and called 8
    times from `coding_quadtree` and 10 from `prediction_unit`; split, the
    per-neighbour half is small enough to inline at all of them.
13. **`avail_n_idx` hands back the neighbour's 4×4 index.** Every caller that
    finds a neighbour available then reads `ct_depth`, `pred_mode`, `qp_y` or
    `intra_mode` at it — and recomputed `idx4(xn, yn)`, a second multiply by the
    runtime stride that `avail_n` had already done and discarded.
14. **`pb_available` takes the hoisted `AvailAt`.** Merge candidate derivation
    asks it about five neighbours (A1, B1, B0, A0, B2) of one prediction block.
15. …and **`amvp`**'s five-position closure shares the same one.

`prediction_unit` went **4471 → 3884** on the availability split alone, with its
multiply count 36 → 26; `residual_block` ended at **1 multiply**, down from a
function that called an out-of-line `available` three times.

#### The clock

| stream | this sweep | z | **campaign, all 42 wins** | z |
|---|---|---|---|---|
| `in_to_tree_720p_8bit` | 1.000× | 1.41 | **1.051×** | 4.08 |
| `ipred_x24` | 1.026× | 2.33 | **1.068×** | 3.77 |
| `txskip_x40` | 1.017× | 1.63 | **1.127×** | 5.21 |

Only `ipred_x24` resolves within the sweep, which is the right stream for it to
resolve on — it is the intra-heavy one, and intra prediction is what queries
§6.4.1 availability hardest. The other two are positive but inside the noise
floor at this size; the cumulative column, four times larger, resolves all
three.

#### Two things this sweep is a warning about

**A static instruction count is void across an inlining boundary shift.** Batch
J read `prediction_unit` +544 against batch I — but `merge_motion` had stopped
being its own symbol and folded in, so the two numbers count different code.
This is the sweep-1 `bypass_bits` lesson (a probe that pins a callee changes its
caller) recurring one level up, at whole-function granularity. When inlining
moves, only the clock can adjudicate.

**A bench-corpus file can be a dud.** `in_to_tree_720p_intra.hevc` fails on
every arm: it is `general_profile_idc 4` (RExt), correctly refused as outside
Main / Main 10. It has been unusable since it was made and no measurement ever
noticed, because a harness that errors out looks like a harness that is busy.

### The surrounding components — eleven more

Sweep 3 ended with the finding that the entropy coder was done and the syntax
layer around it was not. This sweep stayed there, and it produced the largest
single-round effect on the mainstream stream of the whole campaign.

Three veins, all of them the same species of redundancy: **a value is computed,
used for one test, thrown away, and computed again by the next line.**

#### The inter path recomputed every neighbour index twice, and its motion twice

`pb_available` derived a neighbour's 4×4 index to read its `pred_mode`, returned
a `bool`, and then `motion_at(x, y)` derived the identical index again to read
its motion. Five candidates per prediction block in `merge_motion` (A1, B1, B0,
A0, B2), so five wasted multiplies by the runtime stride per PU.

`amvp` was worse. It ran two passes over the same candidate positions — an
unscaled `direct` pass, then a `scaled` one — and each pass called
`motion_at(p.0, p.1)` afresh. An available position therefore paid for `idx4`
**and** a whole `PuMv::from_motion` twice.

1. **`pb_avail` returns `Option<usize>`**, the neighbour's index, instead of a
   bool.
2. **`amvp` fetches each candidate's motion once** into `cand_a` / `cand_b` and
   both passes read that.

**`prediction_unit` 4428 → 3739 instructions, and its multiplies 40 → 15.**

#### `fill4` stored one element at a time

3. It is the map writer for `ct_depth`, `filter_bypass`, `pred_mode`,
   `intra_mode`, `qp_y`, `motion` and `nz` — **eight to ten calls per coding
   unit**, mostly over the same rectangle — and its inner loop indexed the map
   per element, so every 4×4 cell carried its own bounds check. Filling a row
   slice at a time checks once per row and becomes a `memset` for the
   byte-sized maps.

   This one is worth noting as an instrument lesson: it made the *static* count
   go **up** (`transform_tree` +56, `coding_quadtree` +34) because
   `slice::fill` emits more code at the call site than a naive loop. Dynamic
   work falls sharply and static work rises — the counter and the clock point in
   opposite directions, and here the clock is right, because the loop trip
   count is what changed.

4. **`qp_b` reuses the neighbour index** — the one site of sweep 3's
   `avail_n_idx` conversion that had been missed. Its twin `qp_a` was converted;
   `qp_b` two lines below still called `avail_n` and then `idx4`.

#### Seven census routes were still reading a `OnceLock` per block

`route` and `arm` call `census::enabled()`, whose `OnceLock` read is an atomic
load plus a branch. Sweeps 2 and 3 converted the per-transform-block sites to
the compile-time `census::ALWAYS`; these seven were missed, and they sit on
paths at least as hot:

5. `intra::predict`'s reference-filter route — **every luma intra block**.
6. …its DC deferred/filled route.
7. …its angular row/transposed route.
8. `deblock`'s luma arm — **per edge**.
9. …its chroma arm.
10. `sao`'s type-idx arm — per CTB.
11. …its interior/ring route.

**`intra::predict` 1058 → 960**, `apply_in_loop_filters` 3037 → 3002.

`apply_in_loop_filters`' two `std::env::var_os` calls were left alone
deliberately: they run once per picture, 60 times a stream, which is 0.03% —
pruned on arithmetic before building anything (§11).

#### The clock

| stream | this round | z | **campaign, all 53 wins** | z |
|---|---|---|---|---|
| `in_to_tree_720p_8bit` | **1.028×** | 4.20 | **1.063×** | 3.92 |
| `ipred_x24` | **1.026×** | 3.05 | **1.091×** | 4.13 |
| `txskip_x40` | 1.000× | −0.39 | **1.123×** | 4.75 |

Two verdicts, and the mainstream one is the strongest single-round result of the
campaign — which is what this round's content predicts, since merge/AMVP
derivation and `fill4` are exercised by inter content that `ipred_x24` and
`txskip_x40` barely have. `txskip_x40` reading flat is the honest outcome for a
stream that is 4×4 transform-skip almost throughout: it has neither the merge
candidates nor the intra prediction these wins touch, so there is nothing here
for it to gain, and it did not lose either.

### The MC entry path — ten wins inside `elide_taps()`'s neighbourhood

`elide_taps()` was a one-line `OnceLock` read guarding the tap-elision shortcut in
`interp`'s 2-D arm. Cracking it open meant reading every function the arm touches
on the way in, and the ten wins below all live in that entry path rather than in
any kernel. The instrument is the emitted `.s` for `interp_luma` / `interp_chroma`
— static instruction counts and, separately, the number of conditional branches
that exist only to reach a panic block.

| # | win | what it retires |
|---|---|---|
| 30–33 | `interp<const N, const NF>` takes fixed-size tables — `&[[i16; N]; NF]`, `&[(usize,usize); NF]`, `&[bool; NF]` — and masks its indices `fx & (NF - 1)`, `fy & (NF - 1)` | four length loads + four compare/branch-to-panic pairs per call |
| 34 | `shift3` moved inside the full-pel arm | two instructions on every *filtered* call, for a value only full-pel reads |
| 35–38 | `fir_h_scalar`, `fir_v_scalar`, `fir_v_u16_scalar`, `copy_shift_scalar` all `#[cold] #[inline(never)]` | four never-taken loop bodies off the hot path |
| 39 | `full_pel` extracted `#[cold] #[inline(never)]` | its prologue and `shift3` computation off the 2-D arm |
| 40 | `copy_shift` takes the threaded `McPlan` instead of calling `crate::isa()` | one dispatcher probe per full-pel block |

Measured on the emitted assembly:

| MC entry | before | after | guard branches |
|---|---:|---:|---:|
| `interp_luma` | 500 | **346** (−154) | 20 → **5** |
| `interp_chroma` | 423 | **254** (−169) | 13 → **2** |

Three things worth keeping from this round.

**`NF` is a power of two by the standard, so the mask is free and it is what carries
the proof.** Luma has four fractional positions, chroma eight, and the caller already
derives `fx`/`fy` as `mv & (NF - 1)` — so `fx & (NF - 1)` never changes a value. What
it does is let LLVM see that the index cannot exceed the array, which is the only
reason the four table lookups stop emitting a compare and a jump to a panic block
apiece. Sizing the tables (`[T; NF]` rather than `&[T]`) is the other half: a slice
carries a runtime length that has to be loaded and tested even when it is a constant
at every call site.

**Marking one scalar fallback `#[cold]` made the callers BIGGER.** `copy_shift_scalar`
alone cost `interp_luma` +35 and `interp_chroma` +40 instructions — the outlined call
needs its arguments marshalled where the inlined body had them in registers already.
All four together measured −66 / −19. The lesson is that `#[cold]` is a *layout*
decision about a whole path, not a per-function annotation you can price one at a
time.

**The census priced `full_pel` before it was extracted.** 610 of 252,001 `interp_luma`
calls take the (0,0) arm — 0.24%. That is what justified moving it out of line
together with the `shift3` it is the sole consumer of; without the count it would have
been a guess, and the arm is cheap enough that guessing wrong is a regression.

Wins 30–40 are gated 147/147 + SEI 100/100 on both the AVX2 and the SSE4.1 rung, and
`chroma_never_elides` pins the premise the shortcut rests on: every fractional chroma
filter has nonzero taps at both ends, so `N == 8` in the elision condition folds the
whole shortcut away for `interp::<4, 8>` at compile time.

### The guard-branch census — fifteen more, and three refutations

A new instrument opened this round, and it is the reason the round exists. Every
panic call in the emitted assembly passes a `&core::panic::Location`, which rustc
emits as an `anon.*` rodata object holding `{file, line, col}`. Resolving the
`leaq anon.N(%rip)` inside each panic block therefore names **the exact line of our
code that failed to prove its index** — where a debug line-table only ever says
`core/src/slice/index.rs`, which names the check rather than the caller. It needs
no source edit and no debug info, so it cannot perturb what it measures.

Run over the shipping build, it produced a ranked worklist in one pass:

| site | guards | what it is |
|---|---:|---|
| `pic.rs:184–186` | 19 | `avail_at` reading `zs`, `slice_addr`, `tile_id` |
| `inter.rs:383` | 10 | `motion_at_idx` |
| `ctu.rs:642/645` | 9 | `mark_edges` |
| `deblock.rs:60` | 20 | inside `luma_edge_scalar` (already out of line) |
| `pixel.rs:174–210` | 19 | five `*_scalar` bodies inlined into their dispatchers |

**Guard count is a better instrument than instruction count for this class of
change.** It isolates the effect — proving an index in range moves it and nothing
else — and, unlike a static instruction count, it does not move when an inlining
boundary shifts. Both were recorded for every probe below; where they disagreed,
the disagreement was the finding.

#### Fifteen wins

**Parse layer** (−211 instructions, −14 guards):

1–2. `ScanSet` carries the significance-context rows. `SIG_CTX_4X4_BY_SCAN[scan_idx]`
and `SIG_NB[scan_idx][prev_csbf]` were two more `[..][scan_idx]` dispatches in the
per-sub-block loop, on the same `scan_idx` the `ScanSet` selection had already
resolved. Folding them in makes seven tables from one lookup instead of five, and
`prev_csbf & 3` proves the remaining index against a `[_; 4]`.

3–7. Five masked indices in `residual_block`: `sig_row[np & 15]`,
`sig_pos[nsig & 15]`, `sig_pos[(nsig - 1) & 15]`, `sig_pos[k & 15]`,
`pos_scan[np & 15]`. `np` walks 15..=0 and `sig_pos` is a `[u8; 16]` holding at
most sixteen positions, so every mask is a no-op on the value — it exists to carry
the proof. `residual_block` 1931 → 1893, guards 11 → 7.

8–9. `mark_edges` takes each of its two walks as ONE subslice — the left column a
strided walk of `edges[base..]`, the top row a contiguous run of it — where the
indexed form re-proved the bound on every element and in every one of the five
copies inlined into `coding_quadtree`. That function went 2973 → 2863, guards
34 → 26.

**Pixel kernels** (−362 instructions, −18 guards): 10–15. Six `*_scalar` fallbacks
marked `#[cold] #[inline(never)]` **as one set**, on the precedent of the three in
the same file that already were.

| entry point | instrs | guards |
|---|---:|---:|
| `put_bi_fp` | 297 → **226** | 3 → **0** |
| `add_residual_const` | 250 → **193** | 4 → **1** |
| `weighted_uni` | 205 → **112** | 5 → **0** |
| `weighted_bi` | 182 → **117** | 4 → **0** |
| `avg_block` | 209 → **149** | 3 → **0** |
| `transform_skip` | 104 → **88** | 0 → 0 |

#### Three refutations, all measured

**The fused per-CTB key — built, measured worse, reverted.** §6.4.1 asks exactly one
question of a neighbouring CTB (same slice AND same tile), so packing `slice_addr`
and `tile_id` into one `u64` makes it one load, one bounds check and one compare
where there were two, two and three — and the "is it decoded" test is subsumed,
because `-1` in the high half can never equal a decoded block's key. The arithmetic
was right and the measurement was not: `coding_quadtree` grew **+164 instructions
and +8 guards**. The cause is that it was a THIRD array rather than a replacement —
the loop filters still ask about `slice_addr` and `tile_id` separately — so the
inlined availability path had one more base pointer to keep live. Reverting it
alone restored the baseline exactly, which is what identified it: `mark_edges` was
suspected first and cleared by reverting that instead.

**`L0_CAND`/`L1_CAND` paired into one table of tuples — +22 instructions, no guard
change.** The two lookups are at the same index on the same line, so pairing them
should have made one bounds check of two. The guard count did not move, which says
LLVM had already merged the two identical checks; the tuple table only made the
load wider. **When a fusion's premise is "these two checks are the same check", the
guard count tells you whether the compiler already knew.**

**`copy_block_scalar` marked cold — wrong, and the instrument said so.** It came in
with the other six and took `copy_block` from 56 instructions to **1**. A block
copy cannot be one instruction: the function had become a bare jump, because
`copy_block` has no vector path at all — a per-row `copy_from_slice` is already the
best primitive, so the "fallback" is the ONLY path and runs on every full-pel
uni-predicted block. §7 caught it. The other six in that set have real SIMD
dispatchers above them and stayed.

### The loop filter — eleven more, and a broken instrument caught first

This round opened by discovering that the previous round's instrument was wrong.

#### The guard detector was measuring layout, not control flow

The first guard census asked "does a panic symbol appear within N lines after the
branch target". For one function that rule returned **16 guards at N = nxt+3 and 59
at N = nxt+4** — the entire difference being one line of window. Panic blocks share
tails and sit next to one another, so a line window either stops short of the shared
`callq` or falls through into an unrelated neighbour. Sweeping the window looked for
a plateau and found none:

| budget (instrs) | 4 | 8 | 12 | 16 | 24 | 32 | 48 | 64 |
|---|---:|---:|---:|---:|---:|---:|---:|---:|
| guards | 95 | 115 | 119 | 120 | 125 | 128 | 132 | 143 |

**No plateau is the tell that a threshold is measuring the wrong thing.** The rule
with no free parameter: walk the target block to its FIRST control transfer, and it
is a panic block if that transfer is a call to a panic symbol (following
unconditional `jmp`s). That gives **123** for the same function, and it cannot drift
with layout. `tools/hevc/panic_census.py` was rebuilt on it.

The correction matters for one function and not the others: `coding_quadtree`,
`prediction_unit`, `residual_block` and the pixel kernels read within one or two of
their old counts under both rules, so **the previous round's reported deltas stand**.
`apply_in_loop_filters` is where the window collapsed — it has by far the largest
shared-tail panic structure in the crate — and its true count was 123, not 16. Which
is how the biggest guard population in the decoder came to be invisible.

#### Eleven wins

`filter_chroma_edge` alone held **67 of those 123**: twelve indexed accesses through
an `idx(k, i)` closure, in a two-line loop that unrolls, in a function that inlines
once per chroma plane.

1–4. **The segment's footprint as ONE window, per direction, with the end written as
`lo + len`.** A vertical edge is four contiguous samples on each of two rows
(`stride + 4`); a horizontal edge is two samples on each of four rows
(`3 * stride + 2`). Writing the end as `lo + len` rather than an equal expression in
`origin` matters on its own — it makes `start <= end` provable too, and was worth 16
of the guards by itself.

5. **`edge_allowed`'s cross-CTB arm `#[cold] #[inline(never)]`.** An edge crosses a
CTB boundary once every 16 samples along the scan, so almost every edge answers
`cp == cq` and never enters it — but inlined, its four indexed reads put twelve
guards in the caller.

6–7. **The boundary-strength scan by row slice.** It walks every 4x4 block in the
picture — 57,600 a frame at 720p — and each iteration indexed five separate `Vec`s
at the same computed offset, so each carried its own check against its own length.
One row of each per `y4` proves them all at once; the above-neighbour is the previous
row's slice, which is where the `y4 > 0` test already put it.

8. **`pcm_samples` `#[cold] #[inline(never)]`** — `coding_quadtree` −203.

9–11. **The q block's CTB, filter parameters and `deblock_disabled` test shared by
both directions.** The two edges of one 4x4 block have the same q, and `edge_allowed`
derived it separately for each: two `ctb_of`, two `ctb_filter` lookups, two tests
where one of each suffices.

| function | instrs | guards |
|---|---:|---:|
| `apply_in_loop_filters` | 2943 → **2420** | 123 → **66** |
| `coding_quadtree` | 2863 → **2660** | 26 → **23** |

**On win 9–11 the two counters disagree, and the static one is the wrong
instrument.** Code size went **+33** while executed work went strictly down — one
`ctb_of`, one bounds-checked load and one bool test per edge-bearing block instead of
two of each. A static instruction count is a good proxy for executed work when a
change is straight-line, because removing a bounds check removes executed
instructions; it is a bad proxy when a change moves work BETWEEN BRANCHES, which is
what sharing a subexpression across two conditional arms does. Kept on the dynamic
count, with the static delta recorded here so the trade is visible.

#### Five refutations

- **The fused per-CTB key, second attempt.** Round one's failure was blamed on it
  being a THIRD array beside `slice_addr` and `tile_id`. Rebuilt as a REPLACEMENT,
  with accessors unpacking the halves for the loop filters — strictly less state than
  today — it cost `coding_quadtree` **+259 instructions**, worse than the +164 of the
  first attempt. The fusion itself is what does not pay, not the extra array. Both
  numbers are recorded on `slice_addr`'s doc comment so the idea is not re-litigated.
- **Row-slicing the STRIDED filter scan** — the same rewrite that won on the strength
  scan measured **+70 instructions for −4 guards** here. The difference is density:
  the strength scan visits every block and amortises one row slice over `w4`
  iterations; this one is strided (`xstep` 2, every other row), so the same setup
  serves half as many reads, against a body large enough that five more live slice
  pointers cost real registers. **The same technique has opposite signs on two loops
  in the same function.**
- **The chroma window split into exact-length rows.** Guards 77 → 51, and reverted:
  three `split_at_mut` calls and their reslices cost 58 instructions more than the 26
  guards they retired.
- **`n.min(32)` in `intra::predict`** to hand LLVM the block-size range fact —
  **exactly neutral**, 960 instructions and 28 guards before and after.
- **Masking the left-reference gather** (`refs.left[j & 63]`, both arrays `[_; 64]`)
  — **+2 instructions, no guard retired**: LLVM already proves that pair from the
  loop bound. A mask is only a win where the proof is actually missing.

### The harness was measuring with a 15.625 ms ruler

The 0.3.0 benchmark opened with `dblk_heavy` apparently 8% SLOWER than the 0.2.0
release note, on exactly the stream the loop-filter round had touched. It was not
a regression; it was two defects in our own harness, and finding them cost more
than the round that triggered the look.

**1. `Process.TotalProcessorTime` is kernel TICK ACCOUNTING, not a clock.** Every
reading in the 0.2.0 published table was an exact multiple of **15.625 ms**:

| reading | 609 | 641 | 1125 | 1391 | 1422 | 6125 | 6547 |
|---|---:|---:|---:|---:|---:|---:|---:|
| ticks | 39 | 41 | 72 | 89 | 91 | 392 | 419 |

So the "31 ms" differences we published were **two ticks**, and anything under one
was invisible. Two compounding consequences: the harness's tie-exclusion
(`if ($ta[$i] -eq $tb[$i]) { continue }`) discarded exactly the pairs that landed
on the same tick -- the closest, most informative ones, which is why `n` kept
coming in at 18-20 of 21; and on that lattice the median-of-paired-ratios and the
per-arm medians could disagree in **sign**. One stream read "1.028x faster, 16/20,
z = 2.68" while its per-arm medians said 6,125 ms against 6,547 ms, i.e. slower.
That contradiction is what exposed the whole thing.

**The free tell: divide a few readings by their smallest pairwise gap and see
whether they are all integers.**

**2. It charged process startup to each arm, and the arms are not comparable
there.** `ffmpeg.exe` is **242 MB**; our decoder is **696 KB**. ffmpeg was paying
roughly 170 ms of image loading on a ~280 ms decode, every run.

Both are fixed. Timing is now each arm's own internal decode time -- our binary
already printed `decode_ms`, ffmpeg has `-benchmark rtime` -- at 1 ms resolution
and excluding process launch on both sides. CPU time is still collected, for the
job it is actually good at: `cpu/wall` per sample detects a descheduled (< 1) or
multi-threaded (> 1) run. And the harness now measures its own quantum and refuses
to report a difference smaller than three of them.

**What it cost us in public: the ffmpeg ratio moves from 1.3x-1.8x to
1.4x-2.0x.** The decoder did not get slower -- measured directly against the 0.2.0
binary on the corrected harness it is 1.059x / 1.031x / 1.034x faster with z of
3.71 / 4.15 / 4.15. The published figure was too generous because ffmpeg was being
charged for loading a 242 MB binary. When two methods disagree, publish the one
that flatters you less.

**And a correction inside the correction.** My first estimate of the size of this
error was 2.5x-3x, from ffmpeg's self-reported 198 ms against our pinned 539 ms.
That was the same mistake one level down: 198 ms was an UNPINNED reading. Pinned,
ffmpeg is 277 ms and the real answer is 1.4x-2.0x. Pin both arms or neither, in
the throwaway probe as much as in the harness.

### Where the remaining 2x lives — measured, 2026-09-07

Four measurements, taken together, and they change the shape of the problem.

**1. The gap is 2.0x, and it is the same on every kind of content.** Pinned,
decode-only, self-reported time on both sides:

| stream | ffmpeg -threads 1 | ours | ratio |
|---|---:|---:|---:|
| x265-encoded 20s clip (real-world tools) | 1,590 ms | 3,338 ms | **0.495x** |
| JCT-VC conformance | 180 ms | 359 ms | **0.499x** |

That REFUTES a worry worth recording: an unpinned probe had read 2.82x on the
x265 clip against 1.4x-2.0x on the corpus, which looked like "our bench corpus
flatters us" (§9 -- bitstream provenance IS content, and the h264 project was
bitten by exactly that). Pinned, the two are identical. The corpus is fine. The
2.82x was an unpinned reading compared against a pinned one -- the same error,
one level down, that inflated the ffmpeg-startup estimate earlier the same day.

**2. Against ffmpeg's DEFAULT configuration we are 6.6x slower**, because we have
no threading at all: 4,697 ms against 712 ms with 24 cores. Note what ffmpeg gets
for those 24 cores though -- only **2.3x** (1,667 -> 712 ms). Frame-level
parallelism on this reference structure is weak, which means threading is a lever
where we could plausibly match or beat it rather than merely catch up.

**3. ★ The glue is compiled for 2003. `-C target-cpu=x86-64-v3` is worth 1.066x,
free, bit-exact.**

| stream | baseline (SSE2 glue) | v3 (AVX2 glue) | ratio | verdict |
|---|---:|---:|---:|---|
| x265 20s clip | 3,442 ms | **3,008 ms** | 1.066x | 12/15, z = 2.32 |
| JCT-VC conformance | 367 ms | **344 ms** | 1.066x | 14/15, z = 3.36 |

The campaign's own finding predicted this and nobody drew the conclusion: the
vector kernels are only 13% of MC, 5% of SAO, 2.2% of deblocking, so ~87% of the
time is in caller and glue code -- and ALL of that compiles for the portable
x86-64 baseline, because only the hand-written kernels carry `target_feature`.
That leaves VEX encoding, three-operand forms and sixteen ymm registers unused
across the majority of the decoder. One flag recovers more than the last two
optimisation rounds combined (1.031x-1.059x).

It is a distribution change rather than a code change -- a v3 binary needs an
AVX2 machine -- but we already runtime-dispatch AVX2 kernels, so shipping a
baseline and a v3 artifact costs nothing conceptually.

**4. We have no profiler.** Roughly ninety wins across the last rounds were
guided by static instruction and guard-branch counts. Those are good proxies for
work removed, and they are not a map of where the time is. The playbook's first
law is profile-first and re-profile after every win, and the bottleneck has moved
many times since anyone looked.

### The profiler, and what it found in one run

After roughly ninety wins taken on static instruction and guard-branch counts, a
stage profiler finally went in (`prof` feature, `RH265_PROF=1`). Those counters
are honest measures of *work removed*; they are not a map of *where the time is*,
and nothing had looked since the kernels landed. Two findings, both on its first
run, both bigger than anything the counters had produced.

**1. The sequence-invariant tables were rebuilt for every picture.** `MinTbAddrZs`
and the raster-order tile map are functions of the SPS and the tile layout alone,
so they are identical for every picture of a coded video sequence -- and building
`MinTbAddrZs` walks every 4x4 block in the frame, 57,600 of them at 720p, 600
times for a 600-picture clip. Per-picture setup: **13.7% of decode -> 4.7%**.

**2. The CLI serialised every frame into a buffer it then discarded.** With `-` as
the output, `drain` still called `write_yuv` -- 1.38 MB per frame, 830 MB over the
clip -- and wrote it to `io::sink()`. That is not decoding, `ffmpeg -f null -`
does not do it, and it sat on the path every published timing measures. Untimed
residue: **8.5% -> 1.8%**.

Measured, both arms under the shipping allocator, 15 pairs: **1.155x** on the
x265 clip, **1.307x** deblocking-heavy, **1.111x** conformance, 15/15 z = 3.87.

**The profile now**, on the 20-second clip: motion compensation 32%, entropy and
syntax 17%, inverse transform 14%, deblocking 12%, SAO 9%, per-picture setup 5%.
MC splits into ~32% of decode for the compensation itself against ~5% for
merge/AMVP derivation, and the campaign's own kernel-share figure puts only ~13%
of that inside the vector loops. The next structural item is the intermediate
`pred` buffer: interpolation writes `i16`, then `put_uni`/`put_bi` reads it back
to write the picture -- a whole pass that could fuse into the vertical filter.

#### Two instrument failures, one of them verdict-strength

**★★ The allocator guard checked one arm.** The harness enforces "must be built
with the shipping allocator", because a system-allocator build is not comparable
to what ships -- and it enforced it on `-Ours` only. A release gate's
`cargo test --release` had relinked the 0.3.0 reference without `bench-alloc`, so
the comparison ran rusty_alloc against the system allocator on a decoder that
allocated several MB per picture. It manufactured **1.61x-2.20x at 15/15,
z = 3.87** across three streams.

It was caught by arithmetic alone: the profiler had just predicted ~1.13x from
the work actually removed, and two numbers that disagree by 50% cannot both be
right. **A result far LARGER than your own prediction deserves the same suspicion
as one far smaller** -- the instinct is to bank it, and it is the same instrument
failure either way. Second time this exact trap has fired here.

**A cache key built from two inputs was keyed on one.** `SeqTables` derives from
`rs_to_ts` AND `tile_id`; the key held only `rs_to_ts`, on the reasoning that a
different tile grid must reorder the scan. `PPS_A_qualcomm_7` switches to a
layout with the same scan order and different tile numbering, the cache hit, the
stale map made `first_in_tile` wrong, and the parse died on
`end_of_subset_one_bit`. Conformance caught it at 146/147. **If a table is built
from two inputs, key it on two inputs** -- and the diagnostic that found it in one
run (rebuild on a cache hit and compare, behind an env var) is still in the tree.

### Cracking open the `pred` buffer -- and the measurement it exposed

The profiler put motion compensation at 32% of decode and the obvious structural
item inside it is the intermediate prediction buffer: `interp_*` computes a
14-bit value, stores it as `i16` into `scratch.pred`, and `put_uni` then reads it
back to shift, clamp and write the picture. A whole pass that could fold into the
interpolation's final vertical filter.

**Priced first (§11).** The census gives the populations:

| combine path | calls | share |
|---|---:|---:|
| `put_uni` (uni, interpolated) | 1,216,727 | **62%** |
| `put_bi` | 593,885 | 30% |
| `put_bi_fp` | 28,609 | 1.5% |
| `copy_block` / `avg_block` (full-pel) | 117,094 | 6% |

and the interpolation arms: **2-D 70.5%**, horizontal 14.8%, vertical 14.6%,
full-pel 0.2%. A microbenchmark puts `put_uni` at **0.26 ns/sample** (268 ns for
a 32x32), so at 1.22 M calls it is **roughly 127 ms of a 2,850 ms decode, 4.5%**.
Fusion does not remove all of that -- the fused kernel still stores to the
picture and still shifts and clamps -- so the prize is the `pred` load, the
separate loop and the call: **~3% of decode**, for three kernel variants
(`fir_h`, `fir_v`, `fir_v_u16_sym`) across three ISA rungs plus their oracles.

Recorded as priced-and-not-built. It is a real win, above the 1.017x null arm,
and it is a poor trade against what the same effort buys elsewhere.

#### ★★ The measurement that was steering the campaign

Pricing it meant asking how much the kernels are worth at all, and that turned up
a defect in an arm this campaign has leaned on for weeks.

**`RH265_ISA=scalar` did not select the scalar kernels.** Level 0 is the SSE2
rung, and `scalar` was accepted as a synonym for `baseline`, so every "scalar"
arm ever run executed vector kernels while announcing that it did not. Measured
that way the whole vector path looked worth **3%**.

With the `simd` feature genuinely off (`--no-default-features`):

| build | 20 s 720p30 clip |
|---|---:|
| scalar (no `simd`) | 7,554 ms |
| SSE4.1 | ~2,807 ms |
| AVX2 | 2,685 ms |

**The kernels are worth 2.81x.** And the ladder splits cleanly: scalar -> SSE4.1
is ~2.7x, SSE4.1 -> AVX2 is **1.045x** (14/15 z = 3.36 and 15/15 z = 3.87 on two
streams, against a 1.017x null arm).

This does not overturn the earlier measurement -- AVX2 vs SSE4.1 really is nearly
flat -- but it overturns the CONCLUSION drawn from it. "The vector loops are not
where the time is" was wrong; the correct reading is **"width above 128 bits buys
us almost nothing"**, which is a different statement with different consequences.
The kernels are essential and they are not width-limited, so wider AVX2 is worth
a few percent at most and the 2x gap to ffmpeg is not a vector-width gap.

Two fixes so neither can recur:

* `RH265_ISA=scalar` now **panics** with the reason and points at
  `--no-default-features`. An arm that silently does not do what its name says is
  worse than no arm: every conclusion drawn from it is wrong in the confident
  direction.
* A `--isa` flag, because an environment variable **cannot vary per arm** -- both
  arms of a paired A/B share an environment, so setting `RH265_ISA` turns the
  comparison into a null arm without saying so. The first attempt at the
  AVX2-vs-SSE4.1 measurement did exactly that and read 1.017x, z = 0.30.

### Would `unsafe` buy anything? Measured: no.

The decoder core is `#![forbid(unsafe_code)]` and 282 panic guards remain in its
hot functions, so the question is fair. Three probes, all null.

| probe | result |
|---|---|
| `panic = "abort"` (removes unwind tables and landing pads) | 0.990x / 1.006x, z = −1.29 / 0.26 |
| `get_unchecked` on the hottest per-4x4 map reads | 0.994x / 0.992x, 10/21, z = −0.22 |

The second is the real test: it converted the §6.4.1 availability path and the
neighbour-motion fetch -- `avail_at`, `avail_n`, `avail_n_idx`, `motion_at_idx`,
`pb_avail` -- which run several times per prediction unit across 652 K prediction
units. Output stayed bit-exact. It measured nothing, and the experiment was
reverted rather than left behind a feature flag: relaxing the crate's core
guarantee for a null is a bad trade, and a flag that exists will eventually be
switched on.

**Why both are null, structurally.** The per-SAMPLE loops are already unsafe:
`rusty_h265-accel` carries 149 `unsafe` in `mc.rs`, 216 in `pixel.rs`, 36 in
`deblock.rs`. Every hot vector kernel is raw-pointer code whose bounds are proven
once at the dispatcher and never again. What `forbid(unsafe_code)` still covers
is per-BLOCK glue, entered on the order of 1.2 M times a clip against the
kernels' ~700 M samples -- three orders of magnitude less often. **The safety
boundary is already drawn where the cost is not**, which is the design working
rather than a compromise.

**Where `unsafe` might still pay, and it is not bounds checks.** Per-picture
setup is ~5% of decode and most of it is zeroing memory that is then fully
overwritten; `MaybeUninit` would skip it. Two things make it only partly
applicable: the per-4x4 maps need non-zero initial values (`PRED_NONE`,
`slice_addr = -1`), and a picture's planes are not guaranteed fully written on a
stream with lost slices, which is exactly when reading uninitialised memory stops
being a performance question. Worth pricing; not worth assuming.

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
