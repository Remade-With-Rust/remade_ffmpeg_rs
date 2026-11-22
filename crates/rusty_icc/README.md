# rusty_icc

Pure-Rust ICC colour management: parse and write ICC v2/v4 profiles and
transform `u8`, `u16` and `f32` pixels between RGB, CMYK, grey and L\*a\*b\*.
No C, no dependencies; `unsafe` lives only in one private module of x86-64
AVX2 kernels, each with a scalar twin that is its test oracle and the path on
every other CPU and target (the public API is safe, and the kernels match
the scalar code bit for bit). The pipeline follows LittleCMS 2 (tag
selection, legacy `lut16` Lab encoding, absolute-colorimetric scaling,
black-point detection and compensation, proofing chains) so results are
comparable with it.

## Against LittleCMS 2

The reference engine, linked through FFI in a separate tool
([`bench-lcms/`](https://github.com/Remade-With-Rust/remade_ffmpeg_rs/tree/main/crates/rusty_icc/bench-lcms), not part of the package). Every number below is
from `bench-lcms/run-accuracy.sh` and `run-speed.sh`; raw output is in
`bench-lcms/results/`.

### Accuracy

**Method:** lcms2 2.17 (Ubuntu `2.17-1ubuntu0.2`) and rusty_icc read the same
profile bytes (rusty_icc's built-ins encoded, the committed test profiles,
Coated FOGRA39, the synthetic camera profiles; lcms2's own
`cmsCreateLab4Profile` against `Profile::new_lab_identity`) and transform the
same grid: 33^3 RGB, 9^4 CMYK, 17^3 RGB into CMYK and for proofing, 256 grey
steps. Every case runs all four intents (two for Lab and grey) and, for FOGRA39
and the flare camera, with and without `cmsFLAGS_BLACKPOINTCOMPENSATION`;
`TYPE_*_8`, `TYPE_*_16`, `TYPE_*_FLT` (CMYK float scaled from lcms2's 0..100),
lcms2 at default flags. Proofing is `cmsCreateProofingTransform` +
`cmsFLAGS_SOFTPROOFING` against `Transform::new_proofing`, intents
perceptual/relative with proof intent relative/absolute. dE00 is CIEDE2000
between the two outputs, both converted to Lab by one shared evaluator
(lcms2, destination profile, relative colorimetric, `cmsFLAGS_NOOPTIMIZE`).
lcms2's float transforms are unbounded (out-of-gamut matrix-shaper output
leaves 0..1); rusty_icc clamps, so float outputs are compared after clamping.
The last two columns quantise lcms2's exact pipeline (`TYPE_*_FLT` +
`cmsFLAGS_NOOPTIMIZE`) to the sample type and give each engine's max
per-channel distance from it: this shows which engine an integer difference
comes from. Each row is the worst over its intents and BPC settings; the full
336-row table is `bench-lcms/results/accuracy.md`.

| case | colours | u8 mean / max dE00 | u16 mean / max dE00 | f32 mean / max dE00 | f32 max diff | u8 max codes: rusty_icc / lcms2 vs exact | u16 max codes: rusty_icc / lcms2 vs exact |
|---|---:|---|---|---|---:|---|---|
| sRGB->P3 | 35937 | 0.0075 / 1.077 | 0.0000 / 0.004 | 0.0000 / 0.000 | <0.0001 | 1 / 1 | 1 / 1 |
| P3->sRGB | 35937 | 0.0057 / 0.973 | 0.0002 / 0.005 | 0.0000 / 0.000 | <0.0001 | 6 / 6 | 7 / 10 |
| sRGB->Rec2020 | 35937 | 0.0081 / 1.972 | 0.0001 / 0.057 | 0.0000 / 0.000 | <0.0001 | 1 / 1 | 9 / 2 |
| Rec2020->sRGB | 35937 | 0.0044 / 0.895 | 0.0003 / 0.005 | 0.0000 / 0.000 | <0.0001 | 12 / 12 | 13 / 18 |
| sRGBv4pref(mAB)->P3 | 35937 | 0.0393 / 1.419 | 0.0023 / 0.055 | 0.0021 / 0.053 | 0.0003 | 2 / 7 | 17 / 15 |
| P3->sRGBv4pref(mBA) | 35937 | 0.0080 / 1.064 | 0.0064 / 0.308 | 0.0064 / 0.202 | 0.0032 | 3 / 3 | 209 / 274 |
| CGATS001(lut16)->sRGB | 6561 | 0.0035 / 0.826 | 0.0015 / 0.007 | 0.0013 / 0.005 | 0.0002 | 2 / 2 | 10 / 1 |
| camera(lut16 Lab)->sRGB | 35937 | 0.0425 / 4.270 | 0.0010 / 4.120 | 0.0007 / 0.005 | 0.0004 | 4 / 18 | 21 / 2016 |
| camera(mAB XYZ)->sRGB | 35937 | 0.0016 / 3.862 | 0.0011 / 3.818 | 0.0010 / 0.017 | 0.0003 | 4 / 7 | 18 / 1830 |
| flare camera->sRGBv2 | 4913 | 0.0346 / 4.271 | 0.0016 / 4.145 | 0.0006 / 0.021 | 0.0001 | 4 / 15 | 9 / 2029 |
| sRGB->FOGRA39 | 4913 | 0.0015 / 0.358 | 0.0014 / 0.268 | 0.0013 / 0.010 | 0.0001 | 2 / 2 | 7 / 218 |
| FOGRA39->sRGB | 6561 | 0.0036 / 1.063 | 0.0018 / 0.010 | 0.0016 / 0.008 | 0.0002 | 1 / 1 | 12 / 1 |
| FOGRA39->sRGBv2 | 6561 | 0.0060 / 1.077 | 0.0023 / 0.047 | 0.0024 / 0.047 | 0.0002 | 1 / 1 | 15 / 1 |
| proof sRGB->FOGRA39->sRGB (proof rel) | 4913 | 0.1977 / 4.278 | 0.0017 / 0.203 | 0.0015 / 0.009 | 0.0003 | 2 / 11 | 22 / 96 |
| proof sRGB->FOGRA39->sRGB (proof abs) | 4913 | 0.2321 / 2.359 | 0.0017 / 0.010 | 0.0015 / 0.008 | 0.0004 | 2 / 10 | 27 / 31 |
| sRGB->Lab | 35937 | 0.0012 / 0.837 | 0.0000 / 0.005 | 0.0000 / 0.000 | <0.0001 | 1 / 1 | 1 / 2 |
| Lab->sRGB | 35937 | 0.0007 / 0.706 | 0.0005 / 0.007 | 0.0000 / 0.000 | <0.0001 | 6 / 6 | 6 / 36 |
| sGrey v4->sRGB | 256 | 0.0000 / 0.000 | 0.0001 / 0.003 | 0.0000 / 0.000 | <0.0001 | 0 / 0 | 1 / 1 |
| sRGB->sGrey v2 | 4913 | 0.0010 / 0.397 | 0.0003 / 0.008 | 0.0003 / 0.008 | 0.0001 | 1 / 1 | 6 / 5 |

Reading it:

- **Float pipelines agree with lcms2's.** Over all 84 float rows, the largest
  per-channel difference is 0.0032 and the largest dE00 is 0.20, both for P3 to
  the ICC sRGB v4 `mBA` with the relative (and absolute) intent. Every other
  row is at or below 0.062 dE00. lcms2's float path at default flags gave the same outputs as
  with `cmsFLAGS_NOOPTIMIZE` in every case.
- **Integer differences come from lcms2's optimised paths.** The u16
  camera-LUT and RGB-to-FOGRA39 outliers (up to 2029 codes, max dE00 4.1) are
  lcms2's own distance from its exact pipeline; rusty_icc stays within 21
  codes of it there. The exceptions run the other way, for CMYK input
  in u16: FOGRA39 and CGATS001 are within 1 code of exact in lcms2 against 10
  to 15 codes for rusty_icc's 17^4 device-link grid. sRGB to Rec. 2020 in u16
  is 9 codes against 2, at the BT.709 curve's knee (see Self-consistency).
- **Black points match.** `Profile::detect_black_point` /
  `detect_destination_black_point` against `cmsDetectBlackPoint` /
  `cmsDetectDestinationBlackPoint`: 8 profiles (matrix v2 and v4, the ICC sRGB
  v4 LUT, CGATS001, three camera LUTs, FOGRA39), perceptual, relative and
  saturation, source and destination. The worst difference is 0.0001 dE00
  (`results/blackpoints.md`). In the float table every `+BPC` row agrees to
  0.047 dE00 or better. The absolute-colorimetric rows agree as closely as their
  relative counterparts, worst 0.20 for P3 to the `mBA`.
- **One real divergence was found and fixed.** lcms2 interpolates the CLUTs of
  output (`B2Ax`) and device-link LUTs trilinearly when the PCS is Lab
  (`ChangeInterpolationToTrilinear` in `cmsio1.c`); rusty_icc was using
  tetrahedral there. For P3 to the ICC sRGB v4 `mBA` with the perceptual intent
  in float, mean/max dE00 against lcms2 went from 0.108/2.66 to 0.0019/0.062,
  and sRGB to FOGRA39 improved as well. Regression test:
  `lab_pcs_output_luts_interpolate_trilinearly`.

### Speed

**Method:**
- Machine and builds: WSL2 (kernel 6.6.87.2) on an i7-14650HX. lcms2 2.17 was
  called through FFI from the same Rust binary; rustc 1.95.0, release,
  `codegen-units = 1`, with `rusty_alloc` as the global allocator.
- Arms: four, in one process per case, pinned with `taskset -c 4` and run at
  `nice -n -5`:
  - rusty_icc;
  - a second, identical rusty_icc transform (the null arm);
  - lcms2 at flags 0;
  - lcms2 with `HIGHRESPRECALC|NOCACHE` (0x440; `NOCACHE` alone, 0x40, for
    float).
- Order and rounds: each arm ran once to warm up, then 21 rounds whose arm
  order reverses every round.
- Timing: `std::time::Instant` around one whole-image call per run.
  Runs last at least 92 ms, so clock resolution does not matter. Thread
  CPU/wall was 0.999 to 1.000 in every sample.
- Input: one deterministic 4096x4096 gradient + sinusoid + noise image
  (16.78 Mpx), the same pixels for every arm. Output buffers are the same size
  for every arm. Intent relative.
- rusty_icc paths: the matrix-shaper path for sRGB to P3, the 33^3/17^4
  device-link grid for FOGRA39, the exact pipeline for f32.
- Null arm: medians agree to within 0.5% (0.998 to 1.005) in every case. The
  worst single pair was 0.924 to 1.280.
- These numbers are from after the performance pass below (binary
  2026-11-19 19:39). The 0.1.0 run is in the last column. Absolute rates drift
  between sessions: lcms2's 8-bit rate moved from 152 to 180 Mpx/s, and lcms2
  is the same system library in both runs. So compare the in-run ratios, not
  Mpx/s across runs.

| case | rusty_icc Mpx/s (best / median) | lcms2 flags 0 | lcms2 alt flags | rusty_icc faster in (vs flags 0) | median time ratio lcms2/rusty_icc | ratio in 0.1.0 |
|---|---:|---:|---:|---:|---:|---:|
| sRGB -> P3, u8 | 69.0 / 66.4 | 180.1 / 170.8 | 181.0 / 172.4 | 0/21 (z -4.58) | 0.383 | 0.353 |
| sRGB -> P3, u16 | 64.1 / 61.6 | 55.4 / 53.8 | 58.6 / 56.7 | **21/21 (z +4.58)** | **1.152** | 1.025 |
| sRGB -> FOGRA39, u8 | 35.8 / 34.7 | 51.9 / 49.5 | 54.2 / 52.3 | 0/21 (z -4.58) | 0.698 | 0.417 |
| FOGRA39 -> sRGB, u16 | 20.7 / 19.7 | 31.2 / 29.9 | 32.6 / 31.9 | 0/21 (z -4.58) | 0.666 | 0.501 |
| sRGB -> Lab, f32 | 7.9 / 7.6 | 8.5 / 8.3 | 8.5 / 8.2 | 3/21 (z -3.27) | 0.929 | 0.763 |
| Lab -> sRGB, f32 | 11.4 / 11.2 | 10.0 / 9.7 | 10.0 / 9.7 | **21/21 (z +4.58)** | **1.143** | 0.831 |

Where the gap now stands:
- **rusty_icc is faster** on 16-bit matrix-shaper (1.15x against lcms2 at
  flags 0, 1.09x against `HIGHRESPRECALC`, 20/21 pairs) and on Lab to sRGB in
  float (1.14x).
- **lcms2 is still faster** in four cases:
  - 8-bit matrix-shaper, by 2.6x. lcms2 has a dedicated 8-bit fixed-point
    path.
  - RGB to CMYK in 8 bits, by 1.4x.
  - CMYK to RGB in 16 bits, by 1.5x.
  - sRGB to Lab in float, by 1.08x.
- rusty_icc is scalar with no SIMD. Every integer output stays byte-identical
  to 0.1.0, so lcms2's fixed-point arithmetic is not an option without
  changing results.
- These lcms2 numbers predate the second performance pass (below) and have
  not been re-run since.

### Against craft-cms and moxcms (after the second pass)

Ratios are the other engine's time divided by rusty_icc's; above 1 means
rusty_icc is faster. Output hashes equal 0.1.1's in all 17 cases, and every
rusty_icc column of a 150-case dE00 table against LittleCMS's exact
pipeline is unchanged to 4 decimals.

| 1024x1024, one core | 0.1.1 | craft-cms (0.1.1 for reference) |
|---|---:|---:|
| sRGB -> FOGRA39 u8, perceptual / relative + BPC | 2.28 / 2.27 | 1.82 / 1.82 (0.66 / 0.65) |
| FOGRA39 -> sRGB u8 | 3.04 | 2.74 (0.84) |
| proof u8 | 2.17 | 2.53 (1.02) |
| sRGB -> FOGRA39 u16 / sRGB -> P3 u16 | 1.68 / 1.11 | 1.83 / 1.95 (1.12 / 1.83) |
| sRGB -> P3 f32 | 1.66 | 1.48 (1.01) |
| sRGB -> P3 u8 / P3 -> sRGB u8 / sRGB -> linear Rec.2020 f32 | 1.11 / 1.11 / 1.54 | 1.00 / 1.08 / 1.02 |
| **sRGB -> FOGRA39 f32, perceptual** | 1.24 | **0.82** (0.64) |
| **FOGRA39 -> sRGB f32** | 1.46 | **0.98, level** (0.81) |
| **proof f32** | 1.28 | **0.94** (0.77) |
| `Transform::new` sRGB -> P3 / sRGB -> FOGRA39 | 4.30 / 1.49 | 1.68 / 1.25 |
| **`Transform::new` FOGRA39 -> sRGB / proof** | 1.50 / 1.40 | **0.67 / 0.95** |

8-bit CMYK now clearly beats craft-cms on one core, while float CMYK and
float proofing are still behind it. craft-cms also splits rows over threads:
with 4 cores its time divided by single-threaded rusty_icc's is
**0.70 (sRGB -> FOGRA39 u8), 0.78 (sRGB -> P3 u16), 0.31 (sRGB -> FOGRA39 f32)
and 0.36 (FOGRA39 -> sRGB f32)**. **moxcms 0.8.1 is still faster everywhere
except proofing**: rusty_icc runs at 0.08-0.18x of it on matrix-shaper,
0.53-0.94x on CMYK u8, 0.55x on CMYK u16 and 0.11-0.14x on float CMYK; proof
u8 is 4.31x, because moxcms runs two transforms for a proof.

Method: an independent run by a downstream team
([PR #169 comment](https://github.com/Remade-With-Rust/remade_ffmpeg_rs/pull/169#issuecomment-7059019038)),
PR head `32582737` against rusty_icc 0.1.1, craft-cms and moxcms 0.8.1, one
pinned core, 20 alternating rounds, null arms 0.97-1.04.

### Performance pass (callgrind instruction counts)

**Instrument.**
- `examples/icc_ir.rs` runs fixed 256x256 workloads under `rusty_alloc`.
  Construction, parse and encode have their own workloads.
- `bench-lcms/cg.sh` counts only the `#[inline(never)] hot_*` functions
  (`--toggle-collect`), in WSL, invoked identically every time. Every run
  prints the pixel/byte count and an FNV of the output as work-parity
  anchors.
- Exactness ladder this session: a re-run of the same binary gave the same
  count to the instruction; a rebuild after an unrelated edit moved untouched
  workloads by 0 to 48 instructions.

**Gates on every brick:**
- all 9 anchors unchanged;
- at the end, the full test suite;
- the lcms2 accuracy and black-point tables, byte-identical to the committed
  `bench-lcms/results/*.md`.

Integer and float outputs are bit-identical to 0.1.0.

| workload | Ir 0.1.0 | Ir now | change | Ir/pixel now |
|---|---:|---:|---:|---:|
| sRGB -> P3, u8 (matrix-shaper) | 14,876,788 | 11,665,579 | -21.6% | 178 |
| sRGB -> P3, u16 (matrix-shaper) | 14,876,815 | 11,665,608 | -21.6% | 178 |
| sRGB -> FOGRA39, u8 (33^3 grid) | 52,183,953 | 29,383,878 | -43.7% | 448 |
| FOGRA39 -> sRGB, u16 (17^4 grid) | 76,743,592 | 56,188,182 | -26.8% | 857 |
| sRGB -> Lab, f32 (exact pipeline) | 127,793,649 | 100,058,095 | -21.7% | 1527 |
| Lab -> sRGB, f32 (exact pipeline) | 117,451,720 | 91,382,641 | -22.2% | 1394 |
| Transform::new sRGB -> FOGRA39 (33^3 build) | 124,796,781 | 110,023,896 | -11.8% | - |
| Transform::new FOGRA39 -> sRGB (17^4 build) | 279,354,676 | 236,660,195 | -15.3% | - |
| FOGRA39 parse + encode | 9,396,037 | 3,183,843 | -66.1% | - |

**What the census found.** The census used self cost, call counts, and the
`--tree=caller` view of memcpy/memset/realloc/allocator callers.
- The transform hot loops make **no allocations per pixel**.
- **Copies** were runtime-length `copy_from_slice` calls, each a real `memcpy`:
  - in `ClutEval::eval` (101,474 calls on the 8-bit grid workload);
  - in `Chain::eval`, both in and out (202,946 calls on the float workloads);
  - in `Stage::apply`.
- `Profile::encode` grew its buffer by doubling: 40 reallocs, each copying
  everything written so far.
- **Instruction sites:**
  - `f64::floor`, which is a libm call on baseline x86-64 (4.7% + 2.2% of the
    grid workloads);
  - checked loads of every CLUT corner value;
  - `Vec<Vec<f32>>` table indexing through runtime channel counts in the
    matrix-shaper loop (68% of p3u8 self cost, 8% of it in slice bounds
    checks);
  - libm `pow(x, 1.0)` on identity curves;
  - a per-element push in LUT encoding.

**Kept bricks**, one commit each, with Ir before and after in the commit body:

| brick | main effect |
|---|---|
| truncate instead of `floor()` in CLUT interpolation | fogu8 -13.0%, fog16 -11.6% |
| the same in sampled-curve evaluation | build3 -6.7%, build4 -5.7% |
| tetrahedral kernel reads corners as slices (choose the tetrahedron once, one bounds check per corner) | fogu8 -14.4%, fog16 -13.2% |
| monomorphised matrix-shaper loop over fixed-size table rows | p3u8 / p3u16 -21.6% |
| grid loop in its own function (undid that brick's +1.7-1.9% on the grid path) | fogu8 -3.3%, fog16 -2.6% |
| CLUT eval writes into the caller's array (no memcpy) | fogu8 -5.7%, fog16 -3.6% |
| `Chain::eval` on whole `[f64; 16]` arrays (no memcpy) | labf32 -1.5%, labinf32 -1.6% |
| skip libm `pow` for exponent 1.0 (identity curves, exact) | labf32 -19.5%, labinf32 -20.4% (build +0.2/+0.4%) |
| `Profile::encode` reserves its size up front | parse -11.1% |
| bulk big-endian writers for LUT data | parse -61.9% |
| 8-bit grid lookups read a per-value (cell, fraction) table | fogu8 -18.7% |

**Refuted and reverted:**
- Choosing the tetrahedron once but keeping the per-value checked loads:
  fogu8 +2.4%, fog16 +1.7%. The win needs both halves.
- Routing `ClutEval::eval` itself through the axis-table code (one body, two
  call sites): fogu8 -18.7% but fog16 +11.8% and build +1.5%. The body was
  split per call site instead.

### Second performance pass (39 workloads, 15 bricks)

**Instrument.**
- `examples/icc_ir.rs` now has 39 workloads (256x256 pixels each, or one
  construction, parse or encode): 8-bit, 16-bit and f32 transforms through
  every engine, 64-pixel calls (`small*`), and `Transform::new` on its own
  (`build*`, `first16` = construction plus the first 16-bit call).
- Counted under callgrind in WSL with only the `hot_*` functions collected;
  every line prints the pixel or byte count and an FNV of the output, and
  all 39 anchors were unchanged by every brick.
- Every brick passed `cargo fmt --check`, clippy with `-D warnings` and the
  full suite with FOGRA39 (unit, malformed, the moxcms oracle, profiles,
  doctest) before it was committed.
- Exactness ladder: a re-run of the same binary gives the same count, except
  `parse`/`parsev4` (HashMap seed, about +-150) and `buildbpc` (allocator
  addresses); a rebuild moves untouched workloads by 0 to ~50. A WSL restart
  can shift a workload by a fixed amount (smallp3f32 +12,288 for the same
  binary), so every comparison below is between arms counted in one session.

Every integer and float output is bit-identical to 0.1.1. No approximation
was used: no baked curve tables with an error bound, no fixed point, no
replacement for libm `pow` or `cbrt`.

| workload | Ir before | Ir after | change | Ir/pixel |
|---|---:|---:|---:|---:|
| sRGB -> P3, u8 (`p3u8`) | 10,223,776 | 9,502,859 | -7.1% | 145 |
| sRGB -> FOGRA39, u8 (`fogu8`) | 28,859,525 | 11,219,622 | -61.1% | 171 |
| FOGRA39 -> sRGB, u16 (`fog16`) | 56,188,182 | 21,180,350 | -62.3% | 323 |
| CGATS CMYK -> sRGB, u8 (`cgatsu8`) | 46,129,832 | 15,933,154 | -65.5% | 243 |
| Lab -> sRGB, u16 (`lab16in`) | 33,445,435 | 14,936,758 | -55.3% | 228 |
| grey -> sRGB, u8 (`grayu8`) | 9,626,541 | 593,104 | -93.8% | 9 |
| sRGB -> P3, f32 (`p3f32`) | 122,844,078 | 65,293,965 | -46.8% | 996 |
| ProPhoto -> sRGB, f32 (`prof32`) | 96,208,547 | 45,484,173 | -52.7% | 694 |
| sRGB -> Lab, f32 (`labf32`) | 100,058,095 | 88,065,099 | -12.0% | 1344 |
| Lab -> sRGB, f32 (`labinf32`) | 91,382,641 | 50,164,125 | -45.1% | 765 |
| sRGB -> FOGRA39, f32 (`fogf32`) | 200,066,031 | 151,110,706 | -24.5% | 2306 |
| proofing sRGB/FOGRA39/sRGB, f32 (`prooff32`) | 375,167,167 | 267,037,105 | -28.8% | 4075 |
| `Transform::new` sRGB -> P3 (`buildp3`) | 16,685,199 | 3,400,428 | -79.6% | - |
| `Transform::new` FOGRA39 -> sRGB (`build4`) | 236,660,196 | 134,553,592 | -43.1% | - |
| `Transform::new` with black-point compensation (`buildbpc`) | 208,558,566 | 65,187,054 | -68.7% | - |
| new + first 16-bit call (`first16`) | 55,936,485 | 15,507,473 | -72.3% | - |

No workload of the 39 is above its count before the pass (`parse` and
`parsev4` moved by seed noise only). Wall, before against after (`chrt -f`
and `taskset`, 16 ABBA pairs, in-process minimum; after/before floor, all
16/16 faster unless noted): p3u8 0.890 (14/16), fogu8 0.601, fog16 0.562,
labf32 0.774, labinf32 0.661, p3f32 0.616, fogf32 0.803, toprof32 0.805,
lab16in 0.752, grayu8 0.029, build4 0.621, first16 0.276.

**Bricks**, five veins, three each, one commit per brick with its Ir table
and wall check in the commit body:

| vein | brick | main effect (Ir) |
|---|---|---|
| curve evaluation | the inverse of sRGB-shaped curves keeps its break value instead of a `pow` per value | p3f32 -23.6%, labinf32 -31.8% |
| curve evaluation | sampled-curve segment index with one `cvttsd2si` | prof32 -3.3%, build4 -2.3% |
| curve evaluation | LUT stages of unit-gamma curves are one clamp op | labinf32 -14.5%, buildlab -13.4%, labf32 -8.7% |
| CLUT interpolation | grid cell with one `cvttsd2si` | lab16in -4.3%, fog16 -3.3% |
| CLUT interpolation | tetrahedral kernel reads four corners on fixed-width lanes | fogu8 -15.9%, fog16 -11.1% |
| CLUT interpolation | 3- and 4-input front ends straight into fixed-width kernels (trilinear, tetrahedral, 4-input) | fog16 -39.1%, lab16in -26.5%, build3 -25.7%, fogf32 -21.1% |
| f32 pipeline | fused loop for matrix/TRC pairs, compiled in the library | p3f32 -21.5%, lin2020f32 -30.2% |
| f32 pipeline | the general f32 loop compiled in the library, chain evaluated in place | labinf32 -5.9%, labf32 -3.4% |
| f32 pipeline | sRGB-shaped matrix/TRC pairs evaluate their curves in line | p3f32 -11.4%, smallp3f32 -10.7% |
| `Transform::new` | black-point search builds its round-trip sides once | buildbpc -46.6% |
| `Transform::new` | shaper tables built once per distinct curve | first16 -68.1%, buildp3 -63.9% |
| `Transform::new` | grid builds evaluate device curves once per axis value | buildlab -37.6%, build3 -19.4% |
| integer paths | device-link grid loops compiled in the library with fixed channel counts | cgatsu8 -59.2%, fogu8 -51.4%, fog16 -25.0% |
| integer paths | `from_unit` skips the clamp the saturating cast already does | togray16 -8.6%, p3u8 -7.1% |
| integer paths | grey 8-bit sources read a 256-entry output table filled by the loop itself | grayu8 -93.3% |

**Refuted, with numbers in the commit bodies:**
- seven-corner tetrahedral lanes with a range test per corner: fogu8 +5 Ir
  per pixel (21 tests materialised with `setcc` and spills);
- the unit-gamma op as a `Stage` variant: +0.21..0.25% on 11 workloads;
- the fused f32 form stored in `Chain` (builds +600..1,375 Ir), or boxed
  (+33..975 Ir);
- the 3-input front end with a by-value `tetra` shared with the recursion:
  fogu8 +6.75%, fog16 +8.4%;
- all CLUT kernels inlined into `ClutEval::eval` (3.3K instructions):
  lab16in 30% fewer instructions but 8-13% slower on the clock, in three
  probes; landed with each kernel out of line;
- the in-line sRGB curve loop sharing a frame with the general loop: +3 Ir
  per pixel on the other curve pairs;
- the 8-bit axis table cached in the engine, three shapes: cgatsu8 +2.36%,
  +1.95%, and fogu8/fog16 +2.5..3%;
- the grey table check in `Transform::transform`: +12 Ir per 8-bit
  matrix-shaper call;
- `from_unit` without the clamp before the grid loops moved: fog16/lab16in
  +2 Ir per pixel (it waited, then landed);
- the inverse of sampled curves through a bucket index: toprof32 -20.5% but
  +0.04..0.99% on 20 other workloads (a new op arm re-rolls the interpreter).
  Not landed.

**What exactness leaves on the table.**
- libm `pow` is ~112 Ir a call through glibc and six calls per pixel on the
  f32 matrix/TRC path: about 730 of p3f32's 996 Ir per pixel. That path is
  near its exact ceiling; going further needs an approximate or table-driven
  `pow`.
- `cbrt` (XYZ to Lab) is ~100 Ir a call, three per pixel: about 23% of
  labf32 now.
- None of this pass needed `unsafe`.

## Other checks

**Oracle: [`moxcms`](https://crates.io/crates/moxcms) 0.9.1** (dev-dependency
only). Both engines read the same profile bytes and transform the same dense
grid; moxcms in its most precise mode (tetrahedral, floating point, no CICP
shortcut). "diff" is the absolute per-channel difference in 8-bit codes
(`u8`), 16-bit codes (`u16`) or `0..1` units (`f32`); dE00 is CIEDE2000
between the two outputs read back through the destination profile.
Reproduce with `cargo test -p rusty_icc --release --test oracle -- --nocapture`.

| case (colours) | type | max diff | mean diff | mean dE00 | max dE00 |
|---|---|---:|---:|---:|---:|
| sRGB -> Display P3, all 4 intents (33^3) | u8 | 1 | 0.031 | 0.025 | 1.19 |
| | u16 | 7 | 0.43 | 0.0011 | 0.031 |
| | f32 | 0.0003 | <0.0001 | 0.0028 | 0.045 |
| Display P3 -> sRGB, all 4 intents (33^3) | u8 | 1 | 0.023 | 0.014 | 1.29 |
| | u16 | 7 | 0.35 | 0.0007 | 0.023 |
| | f32 | 0.0006 | <0.0001 | 0.0025 | 0.040 |
| sRGB -> Rec. 2020, relative (33^3) | u16 | 17 | 0.36 | 0.0012 | 0.058 |
| ProPhoto v2 `curv` -> sRGB, relative (33^3) | u16 | 7 | 0.22 | 0.0005 | 0.018 |
| sGrey v4 / v2 -> sRGB (256) | u8 | 0 | 0 | 0 | 0 |
| ICC sRGB v4 `mAB` (17^3 CLUT) -> P3, relative | f32 | 0.0021 | 0.0008 | 0.112 | 0.68 |
| P3 -> ICC sRGB v4 `mBA`, relative | f32 | 0.020 | 0.0012 | 0.120 | 0.81 |
| CGATS001 v2 `lut16` CMYK -> sRGB (9^4) | f32 | 0.027 | 0.0015 | 0.237 | 2.99 |
| Coated FOGRA39 -> sRGB, relative (9^4) | f32 | 0.0048 | 0.0006 | 0.090 | 0.37 |
| sRGB -> Coated FOGRA39, relative (17^3) | f32 | 0.0077 | 0.0009 | 0.106 | 0.58 |
| camera-style `lut16` Lab input -> sRGB (33^3) | f32 | 0.029 | 0.0012 | 0.122 | 0.80 |
| camera-style `mAB` XYZ input -> sRGB (33^3) | f32 | 0.015 | 0.0011 | 0.119 | 0.88 |

The test prints all 81 rows (every intent and sample type); the `u8` max dE00
in matrix-shaper rows is a single code step at dark saturated colours. Where
the engines differ, by design or by precision:

- **LUT paths.** sRGB re-encoded as an `mAB` (M curves + matrix) and run
  through each engine's LUT path, against its own matrix-shaper path (f32,
  33^3): rusty_icc max 4.0e-4 / mean 1.2e-5, moxcms max 2.1e-3 / mean 8.8e-4.
  The ~0.1 mean dE00 in the LUT rows above sits on the moxcms side.
- **4-D CLUTs.** moxcms interpolates CMYK quadlinearly; rusty_icc follows
  LittleCMS (linear in the first input over tetrahedral in the other three).
  CGATS001's 6-point grid magnifies the difference.
- **Perceptual into v4 profiles.** rusty_icc applies LittleCMS's implicit
  black-point compensation from the v4 perceptual black; moxcms does not
  (ICC sRGB v4 `mAB` -> P3 perceptual: max dE00 2.43 against moxcms, 0.053
  against lcms2).
- **Absolute colorimetric.** moxcms treats it as relative; rusty_icc scales
  by the media white points (LittleCMS rule: D50 for v2 display profiles).
  Compared above only where the two coincide (v4 display profiles).
- moxcms has no black-point compensation, so BPC is tested behaviourally
  (flare black and FOGRA39 400% black map to display black with BPC on and
  stay at 19 and 28 out of 255 without it).

Coated FOGRA39 is not committed (Adobe's licence); its rows run when the file
is found (`RUSTY_ICC_FOGRA39`, or `printpdf` in the local cargo registry).
The camera profiles are synthetic, written by this crate's encoder.

**Self-consistency.** Integer engines against the exact `f32` pipeline: the
matrix-shaper fast path is within 0.5 codes (`u8`) and 0.7 codes (`u16`) for
sRGB -> P3; the worst `u16` case is 10.3 codes at the BT.709 curve's knee,
where its `s15Fixed16`-rounded parameters leave a 0.0007 step. The 33^3
device-link grid used for LUT profiles is within 0.61 codes (`u8`) and 30
codes (`u16`) of the exact pipeline on the ICC sRGB v4 profile.

**Round trips.** Every committed test profile, Coated FOGRA39, the built-ins
and the synthetic LUT profiles satisfy `parse(encode(p)) == p` and
`encode(parse(encode(p))) == encode(p)` byte for byte.

**Malformed input.** Truncated headers (every length 0..131), declared sizes
past the data, tag offsets/sizes past the end or overflowing, 2^32-1 tag
counts, CLUT grids up to 255^15, absent CLUT data, zero/one-entry tables,
bad curve types and counts all return `Err`. A deterministic fuzz of 32,500
mutations (bit flips, truncation, corrupted tag-table fields, random words,
small-integer writes, splices) over 13 seed profiles (the 8 committed ones,
4 generated, Coated FOGRA39): no panic; 17,049
mutants still parsed and 4,798 transforms built from them ran in `u8`,
`u16` and `f32`. A counting allocator (wrapping `rusty_alloc`) bounds the
heap growth of every parse at 64 KiB + 16x the input; the worst measured on
inputs over 4 KiB was 3.23x. Tag decoding is capped at 4x the profile size
(1,000 overlapping tags over one `lut16` are rejected with 85 KB peak heap).

## Example

```rust
use rusty_icc::{Intent, Layout, Options, Profile, Transform};

let src = Profile::new_from_slice(&std::fs::read("camera.icc")?)?;
let dst = Profile::new_display_p3();
let t = Transform::new(
    &src, Layout::Rgba,
    &dst, Layout::Rgba,
    Intent::Perceptual,
    Options { black_point_compensation: true },
)?;
let pixels: Vec<u16> = vec![0; 4 * 64];
let mut out = vec![0u16; pixels.len()];
t.transform(&pixels, &mut out)?;

// Soft proof: sRGB -> press -> sRGB, showing the paper white.
let press = Profile::new_from_slice(&std::fs::read("CoatedFOGRA39.icc")?)?;
let srgb = Profile::new_srgb();
let proof = Transform::new_proofing(
    &srgb, Layout::Rgb, &press, &srgb, Layout::Rgb,
    Intent::RelativeColorimetric, Intent::AbsoluteColorimetric, Options::default(),
)?;

// Write a profile.
let bytes = Profile::new_rec2020().encode();
```

`rusty_icc::GamutCheck` flags colours a proof round trip moves by more than
a CIEDE2000 threshold.

### Fast `f32` precision

`f32` pixels go through the exact pipeline by default. Opt in to table-based
`f32` arithmetic, as fast colour engines use, with
`Transform::with_precision(Precision::Fast)` (integer pixels are exact either
way):

```rust
use rusty_icc::{Intent, Layout, Options, Precision, Profile, Transform};

let t = Transform::new(
    &Profile::new_srgb(), Layout::Rgb,
    &Profile::new_display_p3(), Layout::Rgb,
    Intent::RelativeColorimetric, Options::default(),
)?
.with_precision(Precision::Fast);
```

What it covers and how far it may be from the exact path (measured; see
`tests/fast_precision.rs` and the crate's unit tests):

- matrix/TRC RGB to matrix/TRC RGB: within 1e-5 per channel (worst measured
  2.3e-6 over all 2^24 8-bit colours and a 257^3 lattice), output clamped to
  `0..=1` as the exact path's is;
- matrix/TRC RGB into a Lab-PCS LUT output profile and a Lab-PCS LUT input
  profile into matrix/TRC RGB (float CMYK both ways): within CIEDE2000 0.001
  on Coated FOGRA39 (budget 1.0 worst, 0.1 mean);
- everything else keeps the exact path, as do matrix-shaper pairs whose
  output curve is a pure power law (Adobe RGB, ProPhoto).

The result is deterministic: the same `f32` operations in the same order with
or without SIMD and on every target (a matrix-shaper pixel whose linear value
lands within 4e-6 of an output curve's segment break takes the exact path).

## Scope

Implemented: v2/v4 parsing (header, tag table, `XYZ`, `curv`, `para` 0-4,
`mft1`, `mft2`, `mAB `, `mBA `, `text`, `desc`, `mluc`, `sf32`, `sig `; other
types kept verbatim), intent-selected `A2B0..2`/`B2A0..2` with `A2B0`
fallback, matrix/TRC and grey TRC, XYZ and Lab PCS, abstract Lab/XYZ
profiles, `wtpt`/`chad`; four intents; BPC; proofing; gamut check; writing
with v4 MD5 profile IDs; built-in sRGB, Display P3, Rec. 2020, primaries +
curve RGB, grey, Lab identity.

Not implemented: device-link and named-colour profiles, `D2Bx`/`B2Dx`
(`multiProcessElementType`) tags (an `A2Bx` of that type is an error),
iccMAX (v5), XYZ as a pixel layout, more than 4 device channels, BGR/planar
layouts, mixed input/output sample types, adaptation states other than full,
use of the `gamt` tag, and SIMD beyond the x86-64 AVX2 kernels (the 8- and
16-bit matrix-shaper loop, the 8- and 16-bit RGB to CMYK grid loop and the
fast `f32` paths; aarch64 and wasm32 run the scalar code). API notes against the issue's sketch:
`new_proofing` also takes the two layouts and `Options`, and gamut checking
is a separate `GamutCheck` returning a per-pixel mask rather than an alarm
colour.

Licence: Apache-2.0.
