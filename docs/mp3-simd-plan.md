# MP3 Hand-Written SIMD — Plan + Repeatable Skill

The remaining speed gap to FFmpeg (~2.2× encode, ~1.5× decode) is almost entirely
**hand-written SIMD**: our codec ships only what LLVM auto-vectorizes, FFmpeg/LAME
ship hand-placed AVX/NEON. This document is two things:

1. **The Skill** — a numbered, repeatable procedure for turning *any* hot scalar
   kernel into a verified SIMD kernel. Follow it identically for every brick.
2. **The Backlog** — the specific MP3 kernels to vectorize, prioritized by the
   profiler, each one brick.

It follows the conventions already proven in `rff-codec-vp9` (`inter.rs`,
`loopfilter.rs`, `transform.rs`): runtime feature detection → `#[target_feature]`
intrinsic kernel + scalar tail → aarch64 NEON mirror → a `*_matches_scalar` test.

---

## The one constraint that shapes everything: float, not integer

VP9's SIMD kernels are **integer** math, so the AVX2/NEON version does the *exact*
same operations and is **bit-identical** to scalar by construction. Most MP3 hot
kernels are **floating point** (FFT, MDCT, filterbank, the quantizer's `x^¾`).
Vectorizing them **reorders the float adds/muls and may use FMA**, so the result
differs from scalar at the **ULP level** — it is *not* bit-identical.

So the verification gate changes per kernel:

| Kernel kind | Examples | Gate |
|---|---|---|
| **Integer / exact** | Huffman LUT index math, bit packing | **bit-identical** to scalar (`assert_eq!`) |
| **Float** | FFT, filterbank, IMDCT, `xrpow`, quantize | **scalar-twin within tolerance**: max relative error < ~1e-5, AND end-to-end SNR/round-trip ≥ the scalar build, AND the FFmpeg match preserved (decode) |

This is the same discipline as Phase B's `fast_matrixing_matches_dense` (gated at
1.55e-7) — the scalar version stays in the tree as the **oracle**, forever.

---

## THE SKILL — "Vectorize a Kernel" (repeat verbatim per brick)

A kernel is a small, pure, hot function: inputs in, outputs out, no I/O, no state.
Every brick is one kernel taken through these seven steps.

### 1. Profile & isolate
- Confirm the kernel is actually hot with `encode::prof` / `decode::prof` (never
  vectorize on a hunch — every phase so far overturned the guess).
- Refactor the hot loop into a **pure scalar `fn kernel_scalar(...)`** with a flat
  slice/pointer signature (no closures, no generics). This is the reference forever.
- Note the data type (f32/f64/i32), the natural lane width, and the loop length
  (fixed sizes like 576/64/32 vectorize cleanest).

### 2. Pin the scalar as oracle
Before writing any SIMD, write the harness that will judge it:
```rust
#[cfg(test)]
fn assert_kernel_matches(scalar: impl Fn(&In)->Out, simd: impl Fn(&In)->Out) {
    for _ in 0..N {                      // many random + edge-case inputs
        let inp = random_input();
        let (a, b) = (scalar(&inp), simd(&inp));
        // float gate (relative); use assert_eq! for integer kernels
        assert!(max_rel_err(&a, &b) < 1e-5, "simd diverges: {:.2e}", ...);
    }
}
```
For now `simd` == `scalar` (trivially passes). The harness exists first so the SIMD
version is born under test.

### 3. Write the SIMD twin (x86-64 AVX2 first)
```rust
#[cfg(target_arch = "x86_64")]
#[inline]
fn has_avx2() -> bool { std::is_x86_feature_detected!("avx2") }

/// AVX2 twin of `kernel_scalar`. Float: equal within ULP reassociation.
/// Processes 8 lanes/iter; the `< 8` tail is scalar.
/// # Safety: caller guarantees `src`/`dst` in-bounds for the vector reads/writes.
#[cfg(target_arch = "x86_64")]
#[target_feature(enable = "avx2")]
unsafe fn kernel_avx2(/* raw ptrs + len */) {
    use std::arch::x86_64::*;
    // ... _mm256_loadu_ps / _mm256_fmadd_ps / _mm256_storeu_ps ...
    // then a scalar tail for the remainder
}
```
Rules: 8-wide for f32 (`__m256`), 4-wide for f64 (`__m256d`); always a scalar tail;
a `# Safety` doc stating the bounds the caller must guarantee.

### 4. Runtime dispatch + scalar fallback
The public function picks the implementation **once**, scalar is always reachable:
```rust
pub fn kernel(/* safe slices */) {
    #[cfg(target_arch = "x86_64")]
    if has_avx2() {
        // SAFETY: bounds checked here (lengths/strides) before the call.
        unsafe { kernel_avx2(/* ptrs */) }
        return;
    }
    kernel_scalar(/* slices */);   // default + the path on any other CPU
}
```
Dispatch on a cached flag if it's called in a tight loop (detect once at init).

### 5. Gate it
- Flip the harness from step 2 to compare `kernel_scalar` vs `kernel_avx2`
  (run the test on a machine/CI with AVX2). Float → tolerance; integer → `assert_eq!`.
- Re-run the **full** `cargo test -p rff-codec-mp3` (round-trips, conformance).
- Re-run the **byte-diff / FFmpeg-match** end-to-end check (decode must still match
  FFmpeg ≥ ~115 dB; encode round-trip SNR must not drop).
- Re-benchmark with the profiler **and** the CLI best-of-N. Record before/after.
  If it isn't faster (auto-vec was already good — see B3), **revert the brick.**

### 6. Mirror to aarch64 NEON (optional, do after the AVX2 brick lands)
```rust
#[cfg(target_arch = "aarch64")]
#[target_feature(enable = "neon")]
unsafe fn kernel_neon(/* … */) { use std::arch::aarch64::*; /* float32x4_t … */ }
```
Verify by cross-build + the same `*_matches_scalar` test under
`cargo test --target aarch64-unknown-linux-gnu` (or QEMU).

### 7. Commit = one brick
One kernel, one commit: the scalar refactor + the SIMD twin + the dispatch + the
test + the measured before/after in the message. Never bundle two kernels — each
must be independently revertible (some won't pay off).

---

## Architecture & conventions

- **Location.** SIMD lives next to its scalar kernel in the same module (VP9 keeps
  `conv8_avx2`/`conv8_neon` beside the scalar path in `inter.rs`). Don't create a
  separate "simd crate"; locality keeps the twin honest.
- **Feature flags.** SIMD is compiled in by default and chosen at **runtime**
  (`is_x86_feature_detected!`), so one binary runs everywhere. No `unsafe` math
  without a `#[target_feature]` gate and a scalar twin test. (If a global opt-out
  is wanted, a `no-simd` Cargo feature can force the scalar path — mirrors the lab
  `accel` posture.)
- **`unsafe` is confined** to the `#[target_feature]` kernels; the dispatcher does
  the bounds proof in safe code and documents it at the call site.
- **The scalar twin never leaves.** It is the oracle for the test and the fallback
  for non-AVX2 / non-aarch64 CPUs.

## Tooling to build alongside (one-time)
- **`lab` SIMD bench mode** — extend the existing harness so each kernel reports
  scalar-vs-SIMD ns/call and max error in one command (so step 5 is push-button).
- **Reuse `encode::prof` / `decode::prof`** for stage attribution (already built).
- **CI matrix** — add an `aarch64` cross-build + test job so NEON bricks are gated,
  and keep a `RUSTFLAGS=-Ctarget-feature=-avx2` run so the scalar path stays tested.

---

## The Backlog — kernels to vectorize, by profiler priority

Numbers from the 30 s/128 k benchmark (release). Encode total ~115 ms, decode ~10 ms.

### Encode (the bigger prize — we're ~2.2× off LAME)
| Brick | Kernel | Share | Type | Gate | Notes |
|---|---|---|---|---|---|
| **S1** | **psychoacoustic FFT** (`encode/fft.rs`) | **27%** | f32 | tolerance | Biggest single encode cost. Radix-2 butterflies → AVX2 complex mul/add. High value, well-understood. |
| **S2** | **`xrpow`** — `|freq|^¾`, 576/granule | ~11% | f64 | tolerance | `powf` is slow; replace with a vectorized polynomial/`exp2(¾·log2)` approx. **Changes output** → gate on round-trip SNR, not byte-identical. |
| **S3** | **quantize line loop** (`quantize_uniform`/`with_sf`) | ~17% | f64→i32 | tolerance | Already branchless (Phase C) so partly auto-vec'd; explicit `__m256d` mul + round + clamp may still win. Measure first. |
| **S4** | **analysis filterbank** (`encode/filterbank.rs`) | ~9% | f32 | tolerance | Dense 64→32 + 512-tap window MAC. Dense matvec may already auto-vec (B3 lesson) — **measure before committing.** |
| **S5** | forward MDCT (`encode/mdct.rs`) | ~1% | f32 | — | Cold. Skip unless S1–S4 done. |

### Decode (already ~1.5× off FFmpeg; smaller absolute wins)
| Brick | Kernel | Share | Type | Gate | Notes |
|---|---|---|---|---|---|
| **S6** | **synthesis windowing** (`decode/synthesis.rs`) | ~12% | f32 | float-exact* | 512-tap D-window dot products. *Loop-order can keep it bit-identical; explicit FMA will not — gate on FFmpeg match. (Auto-vec reorder already tried & reverted — explicit intrinsics are the next lever.) |
| **S7** | requantize (`decode/requantize.rs`) | ~18% | f32 | tolerance | `pow43` table gather × scale per line. Gather is awkward in SIMD; the scale-multiply vectorizes. Measure the split first. |
| **S8** | synthesis matrixing (`matrixing_fast`) | ~10% | f32 | tolerance | Already fast (B3). Low priority. |

**Decision rule (from B3):** a dense matrix-vector loop often *already* auto-vec'd —
always confirm the scalar baseline isn't already SIMD before hand-writing. Several
backlog items may turn out to be reverts; that's expected and fine.

---

## Milestones & targets

| Milestone | Bricks | Target |
|---|---|---|
| **M1** — encode FFT | S1 | encode psy-stage ~2× → encode ~50× RT |
| **M2** — encode hot loops | S2, S3 | encode → ~60× RT (within ~1.5× of LAME) |
| **M3** — decode kernels | S6, S7 | decode → ~400× RT (near FFmpeg) |
| **M4** — portability | NEON mirrors of the landed bricks | aarch64 parity, CI-gated |

## Non-negotiables (the rules that keep it safe)
1. **Scalar stays the default and the oracle** — every kernel has a scalar twin and
   a `*_matches_scalar` test; the scalar path builds and runs on every CPU.
2. **No kernel without its test** — the harness is written *before* the SIMD (step 2).
3. **Profile before, benchmark after** — and **revert any brick that isn't faster**
   (auto-vectorization may already win; the B3 lesson is law here).
4. **Float gate = tolerance + end-to-end** — never claim correctness from the unit
   tolerance alone; the round-trip SNR and the FFmpeg decode match are the real gate.
5. **One kernel per commit** — independently revertible, with measured before/after.
