# rusty_h265 — decoder anatomy

*Measured 2026-09-06 against `rusty_h265` 0.1.0 (`alloc=rusty isa=AVX2`).*

What the decoder spends its time on, what each stage's population actually is,
which kernel serves it, and what is still scalar. Every number here is
reproducible, and every one names the instrument that produced it — a figure
without its provenance is a figure nobody can audit later, including me.

Three instruments, in descending order of trust:

| instrument                     | what it answers                          | reproducible?           |
|--------------------------------|------------------------------------------|-------------------------|
| `RH265_CENSUS=1` (89 counters) | how often each path runs                 | exactly, every run      |
| `tools/hevc/kernel_icount.py`  | instructions in each kernel's inner loop | exactly, same toolchain |
| `hevc-vectors/.tmp/ab.ps1`     | what a change costs in time              | paired median + z       |

The counters come first on purpose. Below roughly 1% of the pipeline the clock
cannot resolve anything on this box, so a deterministic count of work removed is
the evidence and the clock is confirmation.

---

## 1. The stage map

Each stage measured as **with vs without**, paired, on one binary — both arms
are literally the same bytes, so a stale build or a different inlining decision
cannot masquerade as an effect.

Stream: `in_to_tree_720p_8bit.hevc`, 60 frames of 720p mainstream inter content.

| stage                                     | with/without | wins  |    z |    share of decode |
|-------------------------------------------|-------------:|-------|-----:|-------------------:|
| residual + inverse transform              |       1.371× | 15/15 | 3.87 |          **27.1%** |
| motion compensation                       |       1.342× | 15/15 | 3.87 |          **25.5%** |
| SAO                                       |       1.114× | 11/14 | 2.14 |          **10.2%** |
| deblocking                                |       1.071× | 15/15 | 3.87 |           **6.6%** |
| intra prediction                          |       1.000× | 6/12  | 0.00 | **not resolvable** |
| *remainder — parse, CABAC, orchestration* |              |       |      |             *~30%* |

**Superseded for the transform (2026-09-06).** The 27.1% row was measured before
the transform rewrite, which took **1.163×** whole-decode (17/17, z = 4.12) on
this stream — see `rusty_hevc_kernels.md`. The stage map above is the map that
*motivated* that work; re-measure before using it to pick the next target.

**These are ablations, so they are upper bounds and they do not sum to 100%.**
Turning a stage off removes its downstream effects too, and the stages overlap.
Read them as "this stage costs at most X", not as a partition.

**Intra reads 1.000× and that is the correct answer, not a broken measurement.**
The census says why: on this stream `SAMPLES_INTRA` is 1,269,104 against
`SAMPLES_MC` 134,759,232 — intra touches **0.9%** of the samples MC does. A
stage that small cannot be seen through a ±4% floor, and the counter settles it
without needing to be. On `ipred_x24` the same stage is 271,876,992 samples and
the entire decode is intra.

---

## 2. Content dispersion — why one stream is never enough

The same decoder, seven streams, seven completely different machines:

| counter                |  mainstream | txskip_x40 |      wp_x10 |   ipred_x24 |  dblk_heavy |    txbypass |
|------------------------|------------:|-----------:|------------:|------------:|------------:|------------:|
| `SAMPLES_MC`           | 134,759,232 | 18,074,880 | 486,944,640 |           0 | 645,809,568 |  47,355,872 |
| `SAMPLES_INTRA`        |   1,269,104 |  2,183,680 |  41,310,720 | 271,876,992 |   2,790,912 |   5,950,256 |
| `SAMPLES_SAO`          |  20,384,632 |  7,874,400 |   5,686,560 | 191,814,528 |           0 |           0 |
| `SAMPLES_ADD_RESIDUAL` |  11,415,488 | 10,590,080 |  53,070,400 | 178,781,952 |   4,921,744 |  33,883,680 |
| `CABAC_BYPASS_BINS`    |     504,582 | 10,015,520 |     223,660 |  14,978,208 |      36,718 |  10,604,855 |
| `RT_TX_BYPASS`         |           0 |          0 |           0 |           0 |           0 | **701,034** |

MC swings from **zero to 646 million samples**. Any conclusion drawn from one
stream is a conclusion about that stream.

### Two holes this table found, both now closed

**`txskip_x40` never exercised deblocking — it is named for a stage it skips.**
The census: 474,040 of its 479,480 segments (**98.9%**) rejected by `d >= beta`
before any filtering, with strong and weak filtering both **0**. It is a
*transform-skip* stream (`RT_TX_FLAT` 661,880 against 48,092 on mainstream), so
it has been **renamed `txskip_x40`** and a real deblocking stream added:

| stream                | segments |         skipped |            strong |    weak |
|-----------------------|---------:|----------------:|------------------:|--------:|
| `txskip_x40` (as was) |  479,480 | 474,040 (98.9%) |                 0 |       0 |
| **`dblk_heavy`**      |  868,778 | **33 (0.004%)** | **660,320 (76%)** | 206,854 |

The strong filter had **no population at all** in the old corpus. Every
deblocking number before 2026-09-06 was measured on content that mostly skipped.

**`RT_TX_BYPASS` read 0 on every stream.** Lossless coding
(`cu_transquant_bypass_flag`) is implemented and the conformance suite covers it,
but no bench stream contained it, so no performance number said anything about
that path. `txbypass` now puts **701,034** blocks through it.

Both streams are verified bit-exact against ffmpeg before adoption; the recipe
is in `hevc-vectors/bench/README.md`.

## 3. Kernel inventory

Instructions in the innermost vector loop, and per output sample. Deterministic:
same toolchain and source give the same numbers every run.

### Motion compensation — 25.5% of decode

| kernel              | instrs | SIMD | per output | |
| `fir_h_avx2`        |     43 |   40 |      1.344 | horizontal FIR; **every load folded into a `vpmaddwd`** 
| `fir_v_avx2`        |     72 |   61 |      2.250 | vertical FIR, 
| `fir_v_avx2_sym`    |     57 |   43 |  **1.781** | symmetric-tap fold, −21% instructions 
| `copy_shift_avx2`   |      9 |    6 |  **0.281** | full-pel copy — the integer-MV path
| `put_uni_avx2`      |     13 |   10 |      0.406 |   |
| `put_bi_avx2`       |     17 |   14 |      0.531 |      |
| `weighted_uni_avx2` |     18 |   15 |      1.125 | `pmaddwd`, weight and offset in one contraction  
| `weighted_bi_avx2`  |     17 |   14 |      1.062 |        |

Route populations (mainstream): 2-D **175,979** calls, horizontal 38,456,
vertical 36,956, full-pel 610. The 2-D path is 70% of MC, which is why the
zero-end-tap row elision and the two-row vertical structure both target it.

### Residual + inverse transform — 27.1% of decode

The largest stage, and mostly *not* kernel-bound. The wins here were algebraic:

| route                                          | mainstream | wp_x10 | ipred_x24 |
|------------------------------------------------|-----------:|-------:|----------:|
| `RT_TX_DC_ONLY` (collapses to a scalar + fill) |      3,170 | 54,130 |   697,104 |
| `RT_TX_GENERAL`                                |     44,922 | 21,600 | 2,315,064 |
| `RT_TX_FLAT`                                   |     48,092 | 75,730 | 3,039,168 |

`add_residual_avx2` is 17 instructions for 32 outputs (0.531/sample) — near the
floor. The stage's cost is the transform itself, addressed by the **partial
butterfly** (1024 → 352 multiplies at 32-point) and the **DC-only collapse**.

### Intra prediction

| kernel           | instrs | SIMD | per output |                                     |
|------------------|-------:|-----:|-----------:|-------------------------------------|
| `angular_avx2`   |     17 |   14 |  **0.531** |                                     |
| `angular_t_avx2` |    152 |  118 |      1.056 | transposed, two 8×8 tiles per strip |
| `planar_avx2`    |     18 |   15 |      1.125 |                                     |
| `transpose_sse2` |     67 |   52 |      1.047 |                                     |

### SAO — 10.2% of decode

| kernel      | instrs | SIMD | per output |
|-------------|-------:|-----:|-----------:|
| `band_avx2` |     27 |   24 |      0.844 |
| `edge_avx2` |     39 |   36 |      1.219 |

Both use a `pshufb` table so the offset lookup is one shuffle rather than four
compares. Population is dominated by `RT_SAO_OFF` (36,710 of 43,200 CTB-plane
decisions on mainstream): **85% of SAO invocations do nothing at all.**

### Deblocking — 6.6% of decode

| kernel                        |  instrs |    SIMD |    per output |
|-------------------------------|--------:|--------:|--------------:|
| `luma_edge_sse2` (2-row body) |      53 |      31 |         3.312 |
| `luma_edge_sse2` (tails)      | 27 / 24 | 15 / 12 | 1.125 / 1.000 |

`i32` lanes, four lines per `__m128i`, one lane per line. `i16` would fit at 8
and 10 bits and double throughput — and was rejected deliberately, because at
12-bit the weak filter overflows (49,148 against a 32,767 ceiling) and that is
the exact defect class described in §5.

---

## 4. The load-folding lens

A kernel's loads may already be free — folded into an arithmetic instruction as
a memory operand — in which case no amount of algebra can remove them, and a
"clever" fold *adds* instructions because two memory operands cannot share one.
`tools/hevc/load_lens.py` measures which:

| kernel                  | vector ops | folded | standalone loads | reading                                |
|-------------------------|-----------:|-------:|-----------------:|----------------------------------------|
| `fir_h_avx2`            |         60 |     24 |            **0** | compiler already won; leave alone      |
| `planar_avx2`           |         30 |      6 |            **0** | same                                   |
| `fir_v_avx2`            |         61 |      0 |                9 | real loads — fold pays, and did (−21%) |
| `edge_avx2`             |         61 |      3 |               12 | real loads                             |
| `angular_avx2` (before) |         48 |      1 |               11 | anomaly — almost nothing folded        |
| `angular_avx2` (after)  |         43 |      6 |                6 | sign flip freed five loads             |

The `angular` row is the lens working: a two-tap interpolation with essentially
no load folding meant an operand was in the wrong position. Rewriting
`a + ((f·(b−a)+16)>>5)` as the identical `a + ((−f·(a−b)+16)>>5)` moved the
single-use value into the operand slot that *can* be memory, because AT&T
`vpsubw src2, src1, dst` allows memory only in `src2`.

---

## 5. What is still scalar, and why

| path                         | population       | why                         |
|------------------------------|------------------|-----------------------------|
| **CABAC**                    | every bin        | serial by construction      |
| deblocking **decisions**     | 437,526 segments | 39% early-out first         |
| `bypass_bits`                | 271,677 calls    | 1.86 bins per call          |
| chroma deblock               | 23,146 calls     | 5% of luma, already minimal |
| intra reference substitution | 193,971 calls    | branchy, per-sample         |

**CABAC** is the largest remaining scalar block and is not a vectorisation
target: each bin's range update feeds the next, so the dependency is the
algorithm. It is already branchless, with the LPS-range and state-transition
tables fused into one `FUSED[u32; 512]` step. Any win here is algorithmic.

**Deblocking decisions** stayed scalar deliberately. Two of every five segments
are rejected by `d >= beta` before any filtering, and vectorising the decision
would pay the eight-tap gather on all of them.

**`bypass_bits`** averages **1.86 bins per call** (504,582 bins over 271,677
calls). The loop *is* long division, and the closed form is a loss at that
length: one 64-bit divide costs ~30 cycles against ~8 for two iterations.

**Chroma deblock** is already minimal — `(q0−p0)<<2` has no multiply to remove —
and its population is 5% of luma's, so a kernel would return about a twentieth
of what the luma one did.

## 6. Instrument failure modes

Each of these produced a wrong number during this work. Recorded because they
will recur, and because a harness that can lie silently is worse than none.

| failure                        | it produced                | the fix                   |
|--------------------------------|----------------------------|---------------------------|
| no work-parity check           | **"46× faster"**           | frame counts vs `ffprobe` |
| success exit after decoding 0  | a timed no-op              | exit 1 on `frames=0`      |
| asymmetric output paths        | our arm penalised          | both arms discard         |
| best-of-N on a noisy box       | 1.59× where truth was 2.1× | paired median + z         |
| reading the wrong emitted copy | a false "no change"        | compare the largest copy  |
| mtime-based rebuild skipping   | a false "guard SURVIVED"   | bump mtime on every write |
| ops counted as cycles          | a predicted win of ~4%     | measure, then believe     |
| allocator drift                | a campaign 1.133× off      | refuse non-`alloc=rusty`  |

The first two combined into the worst of them. The harness reported this decoder
**46× faster than ffmpeg** on a stream it cannot decode at all: the binary
rejected the Range-Extensions profile, produced `frames=0 errors=60` in 6 ms,
and **exited 0**. The clock was not wrong — it was timing nothing, and nothing
in the exit status said so. Divergent work counts VOID a comparison; they do not
weaken it.

"Reading the wrong emitted copy" is subtler. `filter_luma_edge` is emitted twice
— a 1,232-instruction hot body and a 186-instruction outlined cold path — and a
script taking whichever `.s` was newest landed on the small one twice, reporting
"no change" for a change that was in fact −36.7%.

"Ops counted as cycles" was mine: 17.3M scan iterations at "~5 ops" was quoted
as 86M ops as though ops were cycles. A predictable loop streaming from L1
retires ~4 ops/cycle, so the true share was nearer 0.8% than 4%, and the change
measured z = 1.41 — not a verdict.

## 7. Reproducing every number

```bash
# populations (89 counters)
RH265_CENSUS=1 ./target/release/rusty_h265.exe <stream> -

# kernel instruction counts, from the emitted assembly
python tools/hevc/kernel_icount.py

# which kernels have real loads vs folded ones
python tools/hevc/load_lens.py

# a stage's share: with vs without, paired, one binary
powershell -File hevc-vectors/.tmp/ab.ps1 -A <exe> -B <exe> \
    -EnvA "" -EnvB "RH265_ABLATE_MC=1" -Rounds 15 -Stream <stream>

# the public comparison against ffmpeg
powershell -File tools/bench/codec-bench.ps1 -Ours <exe> -Fmt hevc \
    -Streams "<a>,<b>" -Rounds 15 -NullArm -Markdown
```

Switches that isolate a path, all bit-exact against their default arm except the
three ablations: `RH265_SCALAR_GATE`, `RH265_SCALAR_SAO`, `RH265_SCALAR_INTRA`,
`RH265_SCALAR_DEBLOCK`, `RH265_NAIVE_IDCT`, `RH265_NO_DC_FAST`,
`RH265_NO_SYM_FOLD`, `RH265_NO_TAP_ELIDE`, `RH265_NO_SAO`, `RH265_NO_LF`, and
the ablations `RH265_ABLATE_MC` / `_INTRA` / `_RESIDUAL`.

---

## 8. Where the remaining time is

Against ffmpeg 8.1.2 we are **1.5–2.3× slower** (0/15, z = −3.87 on every
stream). The stage map says where that gap can still come from:

1. **Residual + transform, 27.1%** — the largest stage. The butterfly and
   DC-collapse are landed; what remains is the general N² path on non-sparse
   blocks.
2. **MC, 25.5%** — heavily vectorised already. `fir_h` is at the load-folding
   floor; `fir_v` is the two-row structure. Remaining ideas are narrow.
3. **Parse/CABAC, ~30%** — serial, and the single largest scalar block. Not a
   SIMD target; any win here is algorithmic.
4. **SAO, 10.2%** — 85% of invocations are `RT_SAO_OFF`, so the win is in
   dispatch, not arithmetic.

The honest reading: the pixel paths are close to their structural ceiling with
the current designs, and the remaining gap to ffmpeg is concentrated in entropy
decode and orchestration rather than in any kernel.
