# rusty_icc

Pure-Rust ICC colour management: parse and write ICC v2/v4 profiles and
transform `u8`, `u16` and `f32` pixels between RGB, CMYK, grey and L\*a\*b\*.
No C, no `unsafe`, no dependencies. The pipeline follows LittleCMS 2 (tag
selection, legacy `lut16` Lab encoding, absolute-colorimetric scaling,
black-point detection and compensation, proofing chains) so results are
comparable with it.

## What is verified

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
| sRGB -> Coated FOGRA39, relative (17^3) | f32 | 0.038 | 0.0012 | 0.125 | 1.35 |
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
  (ICC sRGB v4 perceptual rows: max dE00 2.4-2.8, printed by the test).
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

Not measured yet: speed, and any comparison against LittleCMS itself.

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
