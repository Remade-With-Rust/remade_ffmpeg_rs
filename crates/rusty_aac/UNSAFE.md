# `unsafe` in rusty_aac

**Last audited**: 2026-10-04 (use-protection-please, H-16). **Policy**: `unsafe` exists
for exactly two purposes — explicit SIMD intrinsics, and filling freshly allocated
capacity without a redundant zero fill. No FFI, no `transmute`, no `static mut`, no
`Box::leak`, no manual `Send`/`Sync`. Every `unsafe` block carries a `// SAFETY:`
comment with its bounds argument; every `unsafe fn` carries a `# Safety` section
(both enforced in CI by `tools/hardening/aac_pattern_rules.py`, rules R1/R2).

Building with `--no-default-features` removes every SIMD kernel: the remaining
`unsafe` is class 2 only.

## Class 1 — SIMD kernels

Each kernel is a twin of a safe scalar function that stays in the tree as its
**oracle** (`*_matches_scalar` tests compare `to_bits()`; the twins perform the
scalar's exact products and sums in the same order, separate multiply and add,
never FMA, so they are bit-identical by construction). The scalar function is also
the fallback on every CPU without the ISA.

The two obligations, and how each is discharged:

1. **The ISA is present.** AVX / AVX2 / AVX-512 are detected at runtime once
   (`is_x86_feature_detected!`, cached in the plan or a `OnceLock`) and every call
   into a `#[target_feature]` function sits behind that flag. SSE2 and NEON are
   baseline on x86-64 and AArch64 respectively.
2. **Every access is in bounds.** Operands are fixed-size arrays (`[f32; 64]`,
   `[Cpx; 64]`, `[f32; 128]`) or slices the safe dispatcher re-bounds to the exact
   length the kernel reads immediately before the call. Pointer offsets are
   functions of loop counters and those lengths — never of stream data. Table
   indices come from internal tables (`bitrev`, twiddles) whose sizes are fixed at
   plan construction.

| File | Kernel(s) | Bound (set by the safe caller) | Oracle test |
|---|---|---|---|
| `src/dsp.rs` | `Radix2Fft::run_avx`, `run_neon` (FFT under every IMDCT and both SBR QMF banks) | `buf` re-sliced to `n`; twiddles `stw.len() == n - 2` by construction | `fft_simd_matches_scalar`, `run_bitrev_matches_run` |
| `src/decode/synth.rs` | `fmul_window_sse`, `fmul_window_neon` (IMDCT overlap window) | all four slices re-sliced to `len` / `2·len` | `fmul_window_matches_reference` |
| `src/sbr/qmf.rs` | `qmf_window_sse/_avx/_neon` (synthesis window), `ana_fold_avx` (analysis window + fold + pre-twiddle), `split_slot_avx`, `dct_post_avx` | `vb` / `win` re-sliced to `20·b` / `10·b`; `seg` to 320; fixed arrays elsewhere | `qmf_window_matches_scalar` (also the SSE twin directly), `ana_fold_matches_scalar`, `split_slot_matches_scalar`, `dct_post_matches_scalar` |
| `src/encode.rs` | `quantize_band_avx2`, `quantize_band_avx512` (opt-in feature), `xpow_avx2_raw` | `pow` / `sign` re-sliced to `out.len()` in `quantize_band`; `xpow_avx2_raw` writes `spec.len()` elements of capacity reserved for exactly that | `quantize_simd_matches_scalar`, `xpow_avx2_matches_scalar_with_tail` |

## Class 2 — filling fresh capacity

Hot paths that compute every element of a new buffer write into
`Vec::with_capacity` / `MaybeUninit` storage instead of zero-filling it first, then
publish the length. The obligation is that **every element is written before the
length is set or the slice is viewed as initialised**; each site's SAFETY comment
states why.

| File | Site | Why every element is written |
|---|---|---|
| `src/dsp.rs` | `mdct_fast` FFT buffers `re` / `im` | the pre-rotation loop writes index `p` for every `p < m` |
| `src/dsp.rs` | `mdct_fast` output | `2p` covers the even indices, `L-1-2p` the odd ones, `L = 2M` |
| `src/dsp.rs` | `Pow2Dct4::rotate_in_uninit` (IMDCT scratch) | `pre` and the pairs of `x` have `m` entries; the loop writes all `m` |
| `src/encode.rs` | `analyze_long` windowed input | both halves written for every `n < FRAME_LEN` |
| `src/encode.rs` | `Xpow::new` (AVX2 path) | `xpow_avx2_raw` writes all `n` elements of both buffers |

Measured value of the class (callgrind Ir, the memory-copies round): IMDCT scratch
LC −1.8%; encoder MDCT / Xpow / long window −0.4% to −1.2% per site.

## Inventory metrics

`cargo geiger --all-features` baseline (2026-10-04): unsafe functions **13/13**,
expressions **709/709** (each intrinsic call counts), impls 0/0, methods 2/2; zero
runtime dependencies. The count must not grow without a row above (H-11).

## Verification

- Oracle tests run on x86-64 natively and on AArch64 under qemu (NEON twins).
- The full decoder is checked end to end against FFmpeg on the ISO/IEC 14496-26
  conformance streams (81 EXACT + 2 documented FFmpeg deviations), and the
  instruction-count bench prints an FNV checksum of every output sample, which no
  kernel change in the history of this crate has moved.
- Miri, the sanitizers and `cargo careful` — see the hardening plan
  (`docs/plans/use-protection-please.md`, H-23..H-25).

## Changing this file

Adding an `unsafe` block requires: a scalar oracle and a `*_matches_scalar` test
(class 1) or a written argument that every element is initialised (class 2), a
`// SAFETY:` comment, a row above, and a bump of the audit date.
