# rusty_icc

Pure-Rust ICC colour management: parse and write ICC v2/v4 profiles and
transform `u8`, `u16` and `f32` pixels between RGB, CMYK, grey and L\*a\*b\*.
No C, no `unsafe`, no dependencies. The pipeline follows LittleCMS 2 (tag
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
  Runs last at least 110 ms, so clock resolution does not matter. Thread
  CPU/wall was 1.000 in every sample.
- Input: one deterministic 4096x4096 gradient + sinusoid + noise image
  (16.78 Mpx), the same pixels for every arm. Output buffers are the same size
  for every arm. Intent relative.
- rusty_icc paths: the matrix-shaper path for sRGB to P3, the 33^3/17^4
  device-link grid for FOGRA39, the exact pipeline for f32.
- Null arm: medians agree to within 1.8% (0.982 to 1.009) in every case. The
  worst single pair was 0.910 to 1.287.

| case | rusty_icc Mpx/s (best / median) | lcms2 flags 0 | lcms2 alt flags | rusty_icc faster in (vs flags 0) | median time ratio lcms2/rusty_icc |
|---|---:|---:|---:|---:|---:|
| sRGB -> P3, u8 | 54.1 / 49.0 | 152.3 / 138.6 | 149.1 / 141.8 | 0/21 (z -4.58) | 0.353 |
| sRGB -> P3, u16 | 46.3 / 41.2 | 45.2 / 42.6 | 48.0 / 44.3 | 12/21 (z +0.65) | 1.025 |
| sRGB -> FOGRA39, u8 | 18.2 / 16.9 | 44.3 / 41.0 | 43.8 / 42.0 | 0/21 (z -4.58) | 0.417 |
| FOGRA39 -> sRGB, u16 | 12.7 / 12.0 | 25.6 / 23.8 | 27.5 / 25.4 | 0/21 (z -4.58) | 0.501 |
| sRGB -> Lab, f32 | 5.4 / 5.1 | 7.3 / 6.7 | 7.5 / 6.7 | 0/21 (z -4.58) | 0.763 |
| Lab -> sRGB, f32 | 7.6 / 6.8 | 9.0 / 8.3 | 8.8 / 8.3 | 0/21 (z -4.58) | 0.831 |

lcms2 is faster everywhere except 16-bit matrix-shaper, where the two tie
(lcms2 with `HIGHRESPRECALC` is ahead in 15 of 21 pairs, z -1.96, borderline):
2.8x for 8-bit matrix-shaper, 2.4x for RGB to CMYK, 2.0x for CMYK to RGB in
16 bits, and 1.2-1.3x for float. rusty_icc's integer loops are scalar f32/f64
per pixel with no SIMD, and there has been no speed work yet. No
speed fix was made here.

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
use of the `gamt` tag, and any SIMD. API notes against the issue's sketch:
`new_proofing` also takes the two layouts and `Options`, and gamut checking
is a separate `GamutCheck` returning a per-pixel mask rather than an alarm
colour.

Licence: Apache-2.0.
