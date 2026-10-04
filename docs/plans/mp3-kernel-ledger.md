# MP3 kernel deployment ledger

`codec-vectorize-kernel` Step −1 (REACHABILITY) over the whole of `rusty_mp3`, run
2026-10-03. One row per hand-written kernel: who reaches it, how much of the real
work goes through it, whether the scalar twin is bit-identical, and which arches
have it. Then the auto-vectorization state of every hot function that has NO
kernel, from the emitted assembly.

**Regenerate:**

```sh
cargo build --release -p rff-cli --bin rff           # the SHIPPING binary
MP3_CENSUS=1 target/release/rff -i x.mp3 -c:a pcm_s16le out.wav   # kernel-reach census on stderr
MP3_CENSUS=1 MP3_ISA=scalar target/release/rff -i x.mp3 -c:a pcm_s16le out_s.wav
cargo rustc -p rusty_mp3 --release --lib -- --emit=asm
python tools/bench/vec_census.py target/release/deps/rusty_mp3-*.s decode:: encode::
MP3_NOHASH=1 python tools/bench/pinab.py "MP3_ISA=scalar:<decprof.exe>" <decprof.exe> corpus/dl/mus_piano.mp3 1 20
```

`MP3_ISA=scalar` (read once) forces every kernel onto its scalar twin; the census
counts every arm, tallied once per granule.

---

## Hand-written kernels — 3, all decode, all AVX, all x86_64

| kernel | shipping caller | units / granule-ch | dispatch | oracle test | arches |
|---|---|---|---|---|---|
| `imdct::imdct36_avx` | `TransformState::granule_to_pcm` → `imdct::hybrid` | 32 long subbands (2 for mixed, 0 short) | `isa::use_avx()` once per granule | `imdct_simd_matches_scalar` (128 trials, `assert_eq!`) | x86_64 only |
| `synthesis::matrixing_avx` | `granule_to_pcm` → `synthesis::polyphase` | 18 passes | once per granule | `matrixing_simd_matches_scalar` (64 trials) | x86_64 only |
| `synthesis::window_avx` | `polyphase` | 18 passes | once per granule | `window_simd_matches_scalar` (every legal `head`) | x86_64 only |

**Encoder (added 2026-10-03, brick 2b):** `filterbank::fold_avx` / `fold_neon`
(window + fold 512 -> 64) and `filterbank::matrix_avx` / `matrix_neon` (64 -> 32,
over a transposed table), reached from `filterbank::analyze`, dispatched once
per granule, oracle tests `fold_simd_matches_scalar` (every legal `head`) and
`matrix_simd_matches_scalar`; census `encode::prof::FB_PASSES`. Filterbank stage
39.4 -> 5.8 ms, **whole encode 1.251x min / 1.178x median, 20/20, z=+4.47**;
72/72 encodes byte-identical; NEON twins pass under qemu, FMA poison fails both.

All three decode kernels are BIT-identical by construction (output index = lane, original
accumulation order, separate mul+add, no FMA).

### Reach — measured on the shipping binary

`rff -i <file> -c:a pcm_s16le` with `MP3_CENSUS=1`, four streams (LAME stereo
44.1k, ours mono VBR 22.05k, ours speech with 31.8% short blocks, shine stereo
48k):

- **default: 100.00% of IMDCT long subbands and 100.00% of synthesis passes take
  the AVX arm; scalar 0.**
- **`MP3_ISA=scalar`: 0.00% AVX** -- the census is printed by both arms, so "the
  fast arm runs" comes with "the slow arm does not".
- **Counts reconcile with geometry**: guitar 16,632 passes / 18 = 924
  granule-channels = 231 stereo frames = 6.03 s at 44.1 kHz; speech 105 of 154
  granules take the long IMDCT = the 31.8% short rate the encoder census reports;
  ISO l3-si_block 3,290 = 102 long x 32 + 13 mixed x 2.

### Bit-identity across arms

Decode hash (FNV-1a over the f32 PCM) identical under AVX and `MP3_ISA=scalar` on
**720/720 streams**: 678 from ours/LAME/shine (9 rates x mono/stereo x CBR/VBR) plus
the ISO/MPEG-2 conformance vectors (mixed blocks, both intensity-stereo forms).

### Value — the ISA-rung A/B

Same binary, `MP3_ISA=scalar` vs default, `pinab.py` (pinned, ABBA with the leading
arm alternated, 20 pairs, null floor 0.7%), 4.7 MB real LAME stereo:

**AVX is 1.601x min / 1.596x median faster on the decode stages, 20/20, z = +4.47.**

That number is the whole vector surface's worth, and therefore also the exact
price of the per-arch gap below.

### Per-arch

| target | kernels | status |
|---|---|---|
| x86_64 with AVX | all three | measured above |
| x86_64 without AVX | scalar twins | same code path as `MP3_ISA=scalar` |
| aarch64 (Apple Silicon, Graviton, ARM phones) | **NEON twins of all three** (2026-10-03, brick 1) | run under qemu-aarch64 from WSL (`tools/bench/arm_qemu_test.sh`): 97/97 lib tests, the three oracle tests EXECUTE NEON (poison: fused `vfmaq` fails all three), ISO gate 16/16 on NEON and on `MP3_ISA=scalar`. Inline into their callers (NEON is baseline: no `#[target_feature]` boundary). Speed on real ARM hardware NOT measured |
| wasm32-unknown-unknown / wasip1 | scalar twins | `tools/bench/wasm_check.sh` PASSES -- bit-exact with the host |

---

## Findings

1. **REACHABILITY form 3 (one-arch twin) -- priced, then FIXED by brick 1** (NEON
   twins; see the per-arch table). Found on the way: the test module did not
   COMPILE on aarch64 (`b` bound only under `cfg(x86_64)`), so every ARM CI leg
   failed before running a test -- "test the fallback path".
   Original entry: No kernel has an aarch64
   sibling. The ISA-rung A/B says what that costs: every ARM build decodes ~1.6x
   slower than it would with NEON twins of the same three loops. All three are
   lane-per-output f32 dot products, so a `float32x4_t` mirror is mechanical;
   bit-identity holds for the same reason (no FMA, same order).
2. **REACHABILITY form 1 (dead call graph) -- not a kernel, a pipeline.**
   `decode_pipelined` (the two-stage threaded decode behind the README's "1.39x
   on 2 cores") is called only by `examples/decprof.rs` and one test. The rff
   adapter decodes through the serial `Mp3Decoder`; its API is a byte slice and
   the adapter streams packets, so wiring it is a design change.
3. **Dispatch site.** All three kernels are `#[target_feature]` functions called
   from non-AVX callers, so none can inline: `imdct36_avx` is entered once per long
   subband (25,536 times on a 6 s clip) for a 36-instruction body, and the two
   synthesis kernels 18 times per granule each. The skill's law is to put the LOOP
   inside the `#[target_feature]` function. Each kernel also pays a `OnceLock`
   state check per call for its table, and `matrixing_avx` carries one
   `panic_bounds_check`. Unpriced.

---

## Auto-vectorization census (no kernel) — `tools/bench/vec_census.py`

Whole-function counts from the emitted asm. Verdicts belong to inner loops, but a
function with ~0 packed ops has no vectorized inner loop either.

| function | stage share | insns | packed f | scalar f | verdict |
|---|---|---:|---:|---:|---|
| `encode::quantize::quantize_into` (rate loop, ~8 probes / granule) | quantize **50.0%** of encode | 124 | 2 | 12 | scalar -- mixed f64 in / i32 out / sign, the cost model declines |
| `encode::quantize::band_noise` | in quantize | 164 | 7 | 12 | mostly scalar |
| `encode::filterbank::analyze` | **19.4%** of encode | 185 | 3 | 18 | scalar |
| `encode::psychoacoustic::analyze` (+ `fft::fft` 2 / 20) | **18.5%** of encode | 665 | 17 | 128 | scalar |
| `encode::mdct::forward` | 4.3% | 509 | 10 | 146 | scalar |
| `decode::imdct::hybrid` (short path + overlap; long path calls the kernel) | imdct 15.6% of decode | 637 | 3 | 153 | short-block IMDCT scalar |
| `decode::requantize::apply` | 10.8% of decode | 491 | 16 | 49 | partly vectorized |
| `decode::antialias::reduce` / `encode::antialias::expand` | small | 191 | 0 | 48 | scalar |

(encode shares: `corp_long_mus_piano` @128k; decode shares: LAME stereo guitar.)

### Ranked candidates (Step 0 still applies -- name the reason, price the stage)

1. ~~**NEON twins of the three decode kernels**~~ DONE (brick 1).
2. ~~**Encoder filterbank (19.4% of encode).**~~ DONE: brick 2a (circular FIFO,
   below resolution, byte-identical) + brick 2b (AVX/NEON fold + matrix, encode
   1.18-1.25x).
3. ~~**Encoder quantize (50% of encode).**~~ DONE (brick 3): `quantize_lines_avx` /
   `quantize_lines_neon`, the 22-band loop inside one `#[target_feature]` call;
   whole encode **1.08-1.10x** (stereo @192k 1.097x min / 1.083x med, floor 0.3%;
   mono @128k 1.102x med), 72/72 byte-identical, oracle incl. rounding seams /
   saturation / `-0.0` passes on x86 and aarch64, poison (drop the +0.5) fails both.
   Original entry:  `rusty_aac` already ships an AVX2
   quantize/xpow kernel (`codec-vectorize-kernel` 2026-07-03): a cross-crate form-2
   case -- port before writing. Named reason: the f64->i32 round+clamp with a sign
   is above what the cost model accepts at the SSE2 baseline.
4. **Dispatch-site hoist** (finding 3) -- one `#[target_feature]` boundary per
   granule instead of 32-68 calls.
