# rusty_aac — SIMD kernel inventory and twin reachability

`codec-vectorize-kernel` campaign, 2026-10-04, branch `aac-complete`.
Every kernel of the encoder, the decoder and SBR/PS is itemised below with its
cost, what the **shipped** code actually is, and whether its SIMD twin is reached
from the shipping entry points.

## Instruments

| instrument | what it answers | how |
|---|---|---|
| stage profiler | where the CPU goes | `prof` module (feature `profile`, zero-cost off): one scope per call around whole loops; encoder stages sum over the worker threads |
| SIMD census | does production reach the twin? | `prof::count` per kernel arm, elements through SIMD vs scalar; `examples/aacprof` drives `AacEncoder` / `AacDecoder` on real files |
| post-LTO asm scan | did a scalar loop vectorise? | `cargo rustc --example … -- --emit asm` on a **binary** crate (post-ThinLTO), kernels temporarily `#[inline(never)]`, packed vs scalar ops per function |
| kernel price | what a twin can be worth | `dsp::kernel_price` (ignored test): ns per call × calls per frame |
| verdict | did the brick pay? | ABBA-interleaved best-of-3 pairs of the whole decode, with an A/A null arm; below the clock floor, instructions per output in the shipped loop |

**Trap found on the way:** the library is built with `-C linker-plugin-lto`, so
`--emit asm` on the rlib shows LLVM's ThinLTO *pre-link* pipeline — which defers
loop vectorisation to link time. That first scan reported `sbr::dec::apply` at
117 packed / 813 scalar float ops; the shipped (post-LTO) code is **1232 / 326**.
Read the assembly of a linked binary, never the rlib's.

## Workloads

LC = 80 s mono 44.1 kHz (`al04` ×10); HE v1 = 320 s stereo 48 kHz
(`al_sbr_cm_48_2` ×10); HE v2 = 79 s stereo 44.1 kHz (`sbr_i-ps_i` ×10); encode =
3 real + 3 synthetic corpus clips at 128 kb/s. Shares are of instrumented stage
CPU on a loaded host (absolute ms vary run to run; shares and counts do not).

## A. Kernels with a SIMD twin — all reached

| kernel | twins | production caller | census (SIMD elements / total) | oracle test | per-arch |
|---|---|---|---|---|---|
| `encode::quantize_band` | AVX2; AVX-512 (`simd-avx512`) | `code_core`, `code_frame_short`, `estimate_bits` | **15,290,368 / 15,290,368 (100.00%)** | `quantize_simd_matches_scalar` (bit-exact, incl. adversarial rounding points) | aarch64: scalar `round` auto-vectorises to `frinta v.2d` (bit-exact) — no hand twin needed. AVX-512 untested on this host (no AVX-512) |
| `encode::Xpow::new` (`xpow_avx2`) | AVX2 | every encode path | **1,425,408 / 1,425,408 (100.00%)** | `xpow_avx2_matches_scalar_with_tail` | aarch64: `sqrt` auto-vectorises to `fsqrt v.2d` |
| `dsp::Radix2Fft::run` | **AVX (new)**, **NEON (new)** | every IMDCT, SBR QMF analysis + synthesis | LC 1,817,600; HE v1 69,488,640; HE v2 11,248,640 — **100.00%** each | `fft_simd_matches_scalar` (bit-exact n = 2..1024, both signs) | NEON run under qemu-aarch64: pass |
| `decode::synth::fmul_window` | **SSE (new)**, **NEON (new)** | every long/short/LD/ELD synthesis | LC 1,813,120; HE v1 7,720,960; HE v2 851,840 — **100.00%** each | `fmul_window_matches_reference` (bit-exact, tails) | NEON run under qemu-aarch64: pass |

Defects found in the pre-existing twins (fixed, `ba1611a`):

- `xpow_avx2`'s scalar tail never advanced its index — an infinite loop for any
  length not a multiple of 4, masked because every caller passes 1024.
- `quantize_band`'s twins used `floor(v + 0.5)`, which is not `round(v)` at the
  double just below ½ (the sum rounds up to 1.0); they had no oracle test, so
  nothing caught it. Now `floor(v + (0.5 − 2⁻⁵⁴))`, exact for `v ≥ 0`.

## B. Every hot kernel / stage, with its Step-0 verdict

Shipped = post-LTO packed vs scalar float ops in the kernel (outlined for the scan).

| stage / kernel | share | shipped code | verdict | action |
|---|---|---|---|---|
| **enc codebook select** (`best_codebook_for_band`, `spectral_bits`) | **65%** of encode | integer LUT work, 39 packed-int, no float | **not a SIMD job** — a per-tuple table gather repeated for up to 11 codebooks per band; the cost is redundant re-evaluation | → `codec-eliminate-redundancy` (share tuple indices across book pairs, prune by LAV, cache per band across rate-loop probes). **Largest remaining encoder lever** |
| enc rate-loop estimate | 15% | quantize (twinned) + `coef_bits` LUT | twin already reached; rest is a LUT | redundancy, with the above |
| enc quantize | 8% | AVX2 twin | twinned ✓ | — |
| enc mdct (`mdct_fast`) | 6% | 13 packed / 34 scalar (f64) | partly vectorised; priced below the floor | none now |
| enc psy | 4% | 6 packed / 73 scalar | small share | none now |
| enc xpow | 2% | AVX2 twin | twinned ✓ | — |
| **dec Huffman + side info** (`HuffBook::decode`, `decode_tuple`, `BitReader`) | LC **~65%** (after the dequant brick), HE v1 ~18%, HE v2 ~8% | bit-serial, no vector ops | **not a SIMD job** — a serial bit-reader; each codeword's length gates the next read | → `codec-eliminate-redundancy` (wider LUT / multi-symbol decode). **Largest remaining LC lever** |
| **dec dequant** (`dsp::dequant`) | was ~40% of LC | f64 `powf(4/3)` per coefficient (libm, no SIMD form) | Step 0 → redundancy, not a twin | **done (`58ac5b7`)**: bit-exact `|q|^(4/3)` table, LC decode **1.43×** |
| dec imdct + window | LC ~17%, HE 4–7% | FFT + `fmul_window` twins; DCT-IV rotations auto-vectorise after the rewrite | ✓ | **done** (`bd172a9`, `68c33b2`, `51fb215`) |
| **dec SBR QMF synthesis** | HE **32–33%** | windowing 454 packed / 1 scalar; two DCT-IV(64) per slot | FFT twin + DCT-IV rewrite landed | done; the rest is already vector code |
| dec SBR QMF analysis | HE 11–21% | 31 packed / 5 scalar + FFT64 | FFT twin landed | done |
| dec SBR HF gen + adjust | HE 12–14% | `hf_gen` 36/6, `gain_calc` 57/40, `hf_inverse_filter` 76/8, `hf_assemble` 39/30 | **auto-vectorised** (Step 0: no twin) | none |
| **dec PS** | HE v2 **28%** | `hybrid_analysis` 114/0, `stereo_processing` 121/24, `decorrelation` 51/15, `hybrid_filter` 51/26 | **auto-vectorised** | none; the remaining scalar is the decorrelator's recursive all-pass (a loop-carried dependency) |
| dec tools (M/S, IS, TNS, prediction) | 1–2% | TNS is a recursive AR filter | serial by definition | none |

## C. Results of this campaign (decode, end to end)

| brick | commit | LC | HE-AAC v1 | HE-AAC v2 |
|---|---|---|---|---|
| FFT AVX + NEON twins | `bd172a9` | 1.060× (6/8) | **1.237× (8/8)** | 1.091× (7/8) |
| DCT-IV rotation rewrite + scratch | `68c33b2` | 1.020× (7/8) | 1.052× (7/8) | 1.083× (7/8) |
| `fmul_window` SSE + NEON twins | `51fb215` | flat (priced ≤3%); loop 9 → 2.75 instr/output | flat | — |
| dequant table | `58ac5b7` | **1.432× (8/8)** | **1.100× (8/8)** | — |

Each ratio is the median of 8 ABBA pairs against the previous brick, with an
A/A null arm whose band ran roughly 0.93–1.10. Every brick is bit-identical to
its scalar oracle, and the ISO conformance census stayed at 81 EXACT + 2 KNOWN
throughout.

## Standing gate (re-run when the kernel inventory changes)

- `cargo test -p rusty_aac` on default, `--no-default-features` and
  `--features profile,lab`; and on aarch64 (`qemu-aarch64`, see below).
- `aacprof enc … / dec …` — every census line must read 100.00% on the arch
  under test.
- `aacconf` — 81 EXACT + 2 KNOWN.

aarch64 recipe (WSL): `CARGO_TARGET_AARCH64_UNKNOWN_LINUX_GNU_LINKER=aarch64-linux-gnu-gcc
CARGO_TARGET_AARCH64_UNKNOWN_LINUX_GNU_RUNNER='qemu-aarch64 -L /usr/aarch64-linux-gnu'
cargo test --release -p rusty_aac --target aarch64-unknown-linux-gnu --lib`.
