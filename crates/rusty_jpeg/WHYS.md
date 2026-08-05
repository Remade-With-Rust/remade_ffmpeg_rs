# WHYS — why is Entropy 38.9% of JPEG encode?

Descent log. One entry per level. Never delete a level; a cause that turns out
small still explains why the fix aimed at it returned little.

Rule: **every "why" is answered by a measurement, never by a mechanism I can
explain.** A plausible explanation is the next hypothesis, not an answer.

---

## D6a — is the profile's content representative? **NO. Closes as a defect.**

- **ASKED:** before trusting "Entropy = 38.9%", does the benchmark content
  produce a realistic coefficient distribution?
- **MEASURED** (deterministic counters, immune to timing):
  - **39.1 non-zero AC coefficients per block, out of 63** — 62% of the AC
    spectrum is non-zero.
  - 40.6 Huffman symbols per block.
  - Whole-frame compression at q90: 12.4 MB raw → 5.01 MB = **2.5x**.
- **ANSWER:** the synthetic source (gradient + rings + per-pixel noise) is far
  denser than real imagery. A photograph at q90 compresses ~10-15x and leaves
  most of the AC spectrum zero. This content maximizes exactly the work Entropy
  does, so **38.9% is an upper bound peculiar to this clip, not the codec's
  profile.**
- **CONFIDENCE:** high — the count is exact and the compression ratio
  corroborates it independently.
- **CONSEQUENCE:** every share in the earlier breakdown is suspect in the same
  direction. Re-profile on content spanning realistic densities before choosing
  a target.
- **SPAWNED:** D6b (do the counters poison the cycles?), D3a (what is the share
  on realistic content?)
- **STATUS:** closed — instrument defect confirmed.

> This is the skill's own warning landing on me: the earlier guard test only
> asserted compression < 12x, which this content passes at 2.5x. A guard with a
> one-sided bound does not test for "representative", only for "not trivial".

## D6b — do the work counters poison the cycle measurement? **YES.**

- **ASKED:** the same profiled run reported `cycles/block 3289`, but the
  counter-free run reported 1074. Which is real?
- **MEASURED:** the counters fire ~558M times per run (138M symbols + 138M bit
  writes + 138M bit sums + 133M non-zero AC + 10.7M flushes), every one a
  relaxed `fetch_add` (`lock xadd`, ~20+ cycles), **all inside the Entropy
  scope**. 558M x ~20 = ~11 Gcycles, against a whole-run Total of ~9.2 Gcycles
  without them.
- **ANSWER:** with counters on, the Entropy bucket is measuring the instrument.
  The **counts are exact and usable; the cycles from that build are not.**
- **CONFIDENCE:** high — arithmetic reconciles with the observed 3x inflation.
- **ACTION:** counters moved behind their own `counters` feature, separate from
  `profile`, so cycles and counts are never read from the same binary.
- **STATUS:** closed.

---

## D3a — what is the share on REALISTIC content? **Entropy is 11.7%, not 38.9%.**

- **ASKED:** with a photographic fixture (1/f fractal detail, calibrated by
  sweep to 11.5x whole-frame compression), where does 4K encode actually go?
- **MEASURED** (profiler reset per density, so the buckets are not averaged
  across content classes — they were, initially, and the noise fixture swamped
  the photographic one):

  | stage | **photo (11.5x)** | noise (2.0x) |
  |---|---|---|
  | **Quantize** | **24.40%** | 21.86% |
  | FillBuffers | 17.31% | 10.68% |
  | residue | 15.14% | 10.29% |
  | **GetBlock** | **13.12%** | 7.80% |
  | **Entropy** | **11.69%** | **28.87%** |
  | HuffmanOptimize | 10.92% | 13.53% |
  | Fdct | 7.42% | 6.96% |

- **ANSWER:** Entropy's dominance was an artifact of the fixture. On content a
  user would actually encode it is **fifth**, at 11.7%. The real leader is
  **Quantize at 24.4%**, with FillBuffers + GetBlock (the block-assembly glue)
  together at **30.4%**.
- **CONFIDENCE:** high — the two profiles come from the same binary and differ
  only in input; the direction matches the counter evidence (50 vs 6.6 non-zero
  AC per block) exactly as predicted.
- **CONSEQUENCE:** **the stated goal of this session — "hammer Entropy" — was
  aimed by a broken instrument.** Optimizing Entropy to zero would buy 11.7% on
  realistic content, not 38.9%. Amdahl caps it there.
- **SPAWNED:** D4a (why is Quantize 24%?), D4b (why is block assembly 30%?)
- **STATUS:** closed.

## D4a — is Quantize's sign branch a mispredict? **NO. REFUTED.**

- **ASKED:** `quantize()` ends with `if value != abs_value { product *= -1 }`.
  DCT signs are essentially random, so that reads as a coin-flip branch run 64x
  per block — ~174M unpredictable branches for one 4K frame. Is it?
- **MEASURED:**
  - Hand-branchless rewrite (`(v ^ sign) - sign`), proven bit-identical
    **exhaustively** — all 65536 `i16` values x 64 positions x 6 qualities x
    luma/chroma.
  - Paired ABBA A/B, n=25, both content classes, arms differing *only* in the
    sign fixup:

    | arm | wins | z | median B/A |
    |---|---|---|---|
    | null [photo] | 14/25 | 0.60 | 1.0483 |
    | branchy [photo] | 10/25 | **-1.00** | 0.9456 |
    | null [noise] | 12/25 | -0.20 | 1.0000 |
    | branchy [noise] | 8/25 | **-1.80** | 0.9685 |

  - `--emit asm`: **156 `cmov` instructions** in the crate.
- **ANSWER:** LLVM already compiles that pattern to a conditional move. There
  was no branch to remove; the rewrite swapped one branchless form for another
  and measured *marginally worse* on both content classes.
- **CONFIDENCE:** high — two content classes agree in sign, the null arms are
  clean, and the `cmov` count gives the mechanism rather than just a null result.
- **ACTION:** shipped path **reverted** to the original. Both forms and the
  `set_branchy_quantize` knob retained so the re-test is free if the surrounding
  stages shrink or a target compiler stops emitting the move.
- **SCOPE OF THE REFUTATION:** this kills *"the sign fixup is a mispredicting
  branch worth removing by hand"*. It does **not** kill *"Quantize at ~24% has
  headroom"* — the ZIGZAG permutation gather is untested and still open.
- **STATUS:** closed, refuted.

## D4b — why is FillBuffers 17.3%? **Per-byte `push`. CONFIRMED, FIXED.**

- **ASKED:** for planar input, `fill_buffers` should be close to a memcpy. Why
  is it a sixth of encode?
- **MEASURED:** the chroma path was
  `for &c in src { for _ in 0..sh { dst.push(c) } }` — a capacity check and a
  length update **per byte**, ~16.6M of them for one 4K frame. (My own code,
  added earlier this session.)
- **FIX:** size the destination once (`resize`), then write through a slice with
  a constant-width chunk for the common `sh == 2` case. Byte-identical; the
  exact-replication tests, including the ragged-width one, still pass.
- **GATE** — paired ABBA, whole encode (the level *above* the change):

  | arm | wins | z | median B/A |
  |---|---|---|---|
  | null [4K] | 19/41 | -0.47 | 0.9898 |
  | push [4K] | **37/41** | **+5.15** | **1.2547** |

- **STAGE CONFIRMATION:** FillBuffers **17.31% -> 1.02%** of encode.
- **ANSWER:** confirmed and shipped. ~**1.25x whole-encode** at 4K.
- **ON THE MAGNITUDE:** at n=25 the median read 1.3226, which *exceeds* what
  Amdahl allows for a 17.3% stage (1.21x cap) — an impossible number, and the
  tell that the estimator was under-sampled. At n=41 it settled to 1.2547,
  consistent with the ~1.20x the share arithmetic predicts. **Win-rate was
  decisive at n=25; the magnitude was not.** Quote the range, not the
  flattering end.
- **STATUS:** closed, shipped.

## D4c — what is the residue? **Mostly the instrument. Instrument-limited.**

- **ASKED:** residue was the largest line (24-28%). Real work, or the probe?
- **MEASURED:** stopped assuming the probe cost and **measured** it — an empty
  `scope()` in a tight loop costs **~88 cycles**, not the ~40 the dump had been
  assuming. At 6.83M scope entries that is 605 Mcycles = **15.5% of Total**.
- **BISECTED:** an info-tier `BlockBody` scope (excluded from sums, so nesting
  cannot drive the residue negative — it did, to -165%, when the first version
  of these scopes overlapped their children) put ~509 Mcycles "inside the block
  loop". Of that, 3 nested probes x 1.36M x 88 = ~357 Mcycles is probe.
- **TWO BRICKS AIMED AT THE REMAINDER, BOTH REFUTED:**
  - in-place block write vs `push` of a stack temporary: **20/41, z = -0.16,
    median 0.9996** — LLVM already elides that copy.
  - (see D4a) the same story for the sign fixup.
  Both are explained by the same fact: there was far less glue than the raw
  residue implied.
- **ANSWER:** the residue is dominated by measurement cost at this scope
  granularity. Correcting per-stage makes `Fdct` read **11 cycles/block** for an
  AVX2 8x8 DCT — impossible, so the empty-loop figure is an UPPER bound (in a
  real loop the two `lock xadd`s partly overlap with surrounding work) and the
  true residue sits in a band this granularity cannot resolve.
- **CONFIDENCE:** high on the conclusion, low on any single number — which is
  the point.
- **ACTION:** `dump()` now measures the probe cost and prints a probe-corrected
  table plus the caveat. **The fix for the ambiguity is fewer scopes, not a
  better estimate**: coarsen the per-block scopes to per-block-row (1000x fewer
  entries) and the correction becomes negligible.
- **STATUS:** closed — instrument-limited, not a code target.

## D4d — can Quantize be cut? *(three refutations, one lever left)*

- Pre-permuting `reciprocals`/`corrections` into zig-zag order, so two of the
  three permuted reads per coefficient become sequential (proven bit-identical
  exhaustively): **12/25 z = -0.20 [photo], 8/25 z = -1.80 [noise] — inside
  noise.** No win. The indices were compile-time constants in a fully unrolled
  loop, so there was no indexing cost to remove.
- With D4a (sign branch) and D4c (block copy), that is **three refuted
  hypotheses on this kernel**. The pattern is consistent: LLVM has already
  optimized this loop hard.
- **`--emit asm` confirms partial auto-vectorization already present:**
  `vpmulld` 25, `vpaddd` 52, `vpsrad` 12 (alongside 176 scalar `imul`).
- **REMAINING LEVER:** hand-AVX2 quantize (8 coefficients per iteration,
  `_mm256_sign_epi32` for the sign restore). It must beat an *already partly
  vectorized* loop, so it is not the easy win it looked like.
- **CEILING, priced before building:** Quantize is ~19.8% probe-corrected.
  Halving it buys ~10% of encode (~1.11x). Worth doing, not transformative.
- **PREREQUISITE:** `quantize(0, i) == 0` must hold at every position, or
  `_mm256_sign_epi32` (which zeroes when the sign operand is zero) is not a
  valid substitute for the scalar sign fixup. Check before writing intrinsics.
- **STATUS:** closed — see D5a.

## D5a — hand-AVX2 quantize. **CONFIRMED. Shipped. Stage cut ~58%.**

Five probes, per the deepened three-probe rule.

**Probe 1 — does it auto-vectorize?** Pulled the loop into an `#[inline(never)]`
`quantize_block_scalar` purely so it would have a symbol to disassemble, then
read the asm. A 64-iteration scalar loop, **zero packed instructions**. It also
settled D4a for good: `cmovsw`/`cmovnsl` — the sign fixup was branchless all
along — and confirmed D4d, since the two table reads index by `%r9` (`i`,
sequential) while only `movzwl (%rcx,%r10,2)` indexes by `%r10` (`ZIGZAG[i]`).

> **The named blocker: one gather.** LLVM will not synthesise a 64-element
> cross-lane permutation, so a single permuted load kept the entire loop scalar.

**Probe 2 — preconditions.** The kernel leans on two instructions whose
semantics differ from the scalar code, so both were checked, not assumed:
- `_mm256_sign_epi32(q, v)` returns **0** when `v == 0`, while the scalar path
  computes `((0 + corr) * recip) >> SHIFT`. Verified equal across **all 9 table
  types x 100 qualities x luma/chroma x 64 positions**.
- `_mm256_packs_epi32` **saturates**; `product as i16` **truncates**. Verified
  quantized magnitudes stay inside i16.
- Overflow bound by hand: `abs <= 32767`, `recip = 32768/divisor <= 4096`
  (divisors are stored pre-shifted, minimum 8), so the product tops out ~1.34e8,
  inside i32.

**The fix follows from the diagnosis**, rather than being a generic "add SIMD":
quantize in **natural order** — where block, reciprocals and corrections are all
sequential and the loop vectorizes — then apply the zig-zag permutation once
afterwards as pure data movement.

**Probe 3 — scalar oracle gate.** `assert_eq!` (integer kernel, no tolerance)
over 4 table types x 5 qualities x luma/chroma x 64 blocks, including the edge
cases that matter: all-zero, all-`i16::MAX`, all-`i16::MIN`, alternating
extremes. Bit-identical.

**Probe 4 — in-context paired A/B**, arms differing only in the quantizer
(AVX2 FDCT on both sides, so the comparison is single-variable):

| arm | wins | z | median B/A |
|---|---|---|---|
| null [photo] | 9/25 | -1.40 | 0.9755 |
| **scalar [photo]** | **25/25** | **+5.00** | **1.1513** |
| null [noise] | 16/25 | 1.40 | 1.0162 |
| scalar [noise] | 22/25 | +3.80 | 1.0361 |

**Probe 5 — end-to-end byte identity.** The kernel is bit-identical, so the
emitted file must be too: verified byte-for-byte at 64x64, **127x65** (odd
dimensions, exercising the ragged final block) and 1920x1080, on both content
classes. The first run of this gate failed — on the **test fixture**, which
sized chroma with `w/2` instead of `ceil(w/2)`, and `PlanarYcbcrImage::new`
correctly rejected the under-sized planes. Fixture bug, not kernel bug.

**RESULT:** Quantize **15.64% -> 7.48%** of encode; combined with the encode
itself getting 1.15x faster, the stage is now ~42% of its former cost — **cut by
~58%**. Whole encode **1.15x** on photographic content.

**End-to-end vs FFmpeg (4K y4m -> mjpeg, whole CLI, best-of-9, same I/O both
sides): 1.51x -> 1.06x.**

---

# Decoder descent

## D6e — do both arms decode the SAME BITSTREAM? **NO. The 2.20x was void.**

- **ASKED** (codec-measurement §4, work parity): both arms report a byte count.
  Do they match?
- **MEASURED:** they did not, and by a lot.
  - we decoded `photo.jpg` — **261,455 B**
  - ffmpeg decoded a frame of `photo300.avi` — **139,963 B**
  - I had built the reference clip with `-c:v mjpeg -q:v 3`, which **re-encoded**
    it. ffmpeg was decoding a stream with roughly **half the coefficients**.
- **FIX:** rebuilt with `-c:v copy` and asserted the extracted frame is
  byte-identical to `photo.jpg` before measuring anything.
- **RE-MEASURED** (300 frames, identical bitstream, pinned CPU time,
  single-thread both sides): ffmpeg **6.20 ms/frame**, ours **9.38** —
  **1.51x slower, not 2.20x.**
- **CONFIDENCE:** high — parity asserted by `cmp`, spreads 1.05-1.06x.
- **LESSON:** this is the third flattering-or-damning measurement in this
  campaign killed by a work-parity check, and the cheapest one to have run. The
  size of the two files differed by 1.87x and was visible the whole time.
- **STATUS:** closed.

## D3c — FUNCTION-LEVEL split by ABLATION, not by scopes. **Entropy is 41.9%.**

- **ASKED:** the scope-based profile said entropy ~11%, IDCT ~10%, plumbing
  ~78%. Those stages are entered 2.9M times each, and codec-measurement §6 says
  a share is only trustworthy when the CALL COUNT is small — high-call stages
  must be priced by **ablation on the uninstrumented binary**.
- **MEASURED** (300 x 1080p photo, pinned CPU time, medians; the ablations keep
  every surrounding access and delete only the work):

  | arm | cpu_med |
  |---|---|
  | baseline | 3093.8 ms |
  | `ABLATE=idct` | 2781.3 ms |
  | `ABLATE=entropy` | 1921.9 ms |
  | `ABLATE=idct,entropy` | 1484.4 ms |

  Solving (`E` = entropy, `I` = transform above the stores, `R` = everything else):
  - `R` = 1484.4
  - `E` = 2781.3 - 1484.4 = **1296.9 ms = 41.9%**
  - `I` = 3093.8 - 2781.3 = **312.5 ms = 10.1%**
  - identity checks: 1296.9 + 312.5 + 1484.4 = 3093.8 exactly.

- **ANSWER:** **entropy decode is 41.9% of decode, not 11%.** The scope-based
  figure was inflated away from it by the probe, in the direction §6 predicts:
  per-block scopes charge their own tax into the stage AND the residue, so the
  real hot stage looked small and the "plumbing residue" looked huge.
- **CONSEQUENCE — this reopens a prune.** I previously declined the bit-reader
  rewrite by computing its prize as ~8%, using the probe-distorted 25% share.
  On the ablated 41.9%, **making entropy free is worth 41.9%**, and the target
  needs 40.5% removed. The arithmetic that said "cannot close" was computed on a
  bad number. **codec-measurement §11: a refutation expires when its baseline
  moves — and mine moved because the baseline was wrong.**
- **CAVEAT, stated rather than buried:** the IDCT figure is NOT solid — the
  medians say 312.5 ms but the minima invert (baseline 2562.5 vs no-IDCT
  2578.1), i.e. the effect sits inside this run's spread (1.11-1.38x). Entropy's
  1296.9 ms is far outside it and is the number to act on.
- **STATUS:** closed for entropy; IDCT share open and under-resolved.

## D4e — what inside entropy? **260,265 `read_exact` calls per frame.**

- **MEASURED** (deterministic counters, 1080p photographic frame):

  | unit | per frame |
  |---|---|
  | **bytes read** | **260,265** |
  | refills | 38,378 |
  | symbols | 126,846 |
  | receive_extend | 67,491 |

- **ANSWER:** `bytes_read` is essentially the whole 261,455 B file. The Huffman
  decoder is already LUT-based with a 64-bit accumulator — the cost is not the
  symbol decode, it is that the accumulator refills **one byte at a time**
  through `read_u8` -> `Cursor::read_exact`, ~6.8 times per refill. Against the
  ablated entropy cost that is roughly **50 cycles per byte** for what should be
  a bounds check and an index.
- **STATUS:** closed.

## D5c — shared buffered reader. **CONFIRMED, SHIPPED. 1.14x whole-decode.**

- The buffer has to live at the **decoder** level, not inside the Huffman
  decoder: reading ahead would swallow bytes the marker and segment parsers need
  next, and a bare `Read` cannot push them back. Because `Buffered<R>` itself
  implements `Read`, every existing generic call site (`parse_sof`, `parse_dht`,
  `parse_app`, …) keeps working unchanged and shares the same buffer; only the
  entropy path takes the concrete type so it can use `next_byte`.
- **GATE:** 55 tests pass, and decoded output is checksum-identical to the
  byte-at-a-time path on four images across both planar and RGB paths.
- **A/B** — paired ABBA over separate processes, pinned CPU time (sequential
  block-vs-block was useless here: the reference itself moved 5% between runs):

  | arm | A-wins | z | median B/A |
  |---|---|---|---|
  | NULL (buffered vs itself) | 8/21 | -1.09 | 0.9900 |
  | **buffered vs `read_u8`** | **21/21** | **+4.58** | **1.1376** |

- **A defect in my own instrument, caught and fixed:** the ablation knob was
  first placed *inside* the byte loop, putting a `OnceLock` atomic load on the
  hottest path in the decoder — 260k times per frame — to serve a switch that
  never changes. Hoisted to once per refill.
- **STATUS:** closed, shipped.

### Standing, and which number is which

- **Progress (single instrument, paired, trustworthy): 1.14x.**
- **Standing vs FFmpeg (cross-implementation ratio, unstable denominator):**
  ~1.5-1.65x slower. FFmpeg's own measured decode moved 1859 -> 1688 ms between
  two runs of the same command in the same session, which is larger than the
  improvement being measured — so per codec-measurement §12 this ratio is
  reported for standing only and never as progress.

## D6c — is the decoder comparison fair? **NO, twice.** Both errors flattered us.

- **ASKED:** unpinned wall said our 1080p decode was 3.65 ms/frame against
  ffmpeg's 4.28 (`-threads 1`) — 1.17x faster. Is that real?
- **MEASURED:** it is not.
  1. **Thread count.** With the `rayon` feature off, `select_worker` still picks
     a **std::thread mpsc worker** for anything above 128x128. So our arm was
     using several cores while ffmpeg was pinned to one. Different work, not
     less work.
  2. **Wall vs CPU on a migrating process.** Three identical in-process runs read
     3.59 / 4.85 / 3.92 ms — a **35% spread**. Pinning to one core at High
     priority and measuring **CPU time** dropped the spread to **1.03-1.08x**.
- **THE HONEST NUMBER** (300 x 1080p 4:2:0, pinned, CPU time, single-thread both
  sides): ffmpeg **3.70 ms/frame**, ours **7.86 ms/frame** — we are **~2.1x
  SLOWER in real CPU work.** Threaded and single-threaded are within 1% of each
  other when pinned, so the earlier "faster" reading was entirely extra cores.
- **STATUS:** closed. Two flattering numbers, both from unfair comparisons; the
  second was found only because the first was re-checked under pinning.

## D3b — where does decode time actually go? **Not the kernels.**

Probe-corrected (`DecEntropy` and `DecIdct` have identical call counts, so their
probe cost cancels and their ratio is unbiased):

| stage | real share |
|---|---|
| DecEntropy | ~11% |
| DecIdct | ~10% |
| **everything else** | **~78%** |

- **ANSWER:** entropy decode and the IDCT together are barely a fifth of our
  decode. The 2.1x gap is **not** in the kernels — SSSE3 IDCT is wired and does
  get called. It is in the plumbing around them.
- **FIRST NAMED CAUSE, FIXED:** `decode_scan` allocated and zeroed a fresh
  `Vec<i16>` for **every MCU row of every component** — ~6.3 MB per 1080p frame.
  Added `Worker::reclaim_buffer`, which the synchronous immediate worker
  implements by handing its finished buffer back; threaded workers keep the
  allocating fallback. **2562 -> 2359 ms (~1.09x).**
- **SECOND SUSPECT, REFUTED:** hoisting the per-block
  `is_x86_feature_detected!` + indirect IDCT call out to once-per-scan
  (~14.6M feature checks removed) measured **neutral to slightly worse**
  (median 2453 vs 2359 ms; identical minima). The macro already caches into a
  static, so the check was a predicted-taken branch on a hot atomic. Reverted —
  it added an `unsafe` block for no measured gain.

### Where the remaining ~2.2x actually is

Coarse scopes (low call count, so probe-free) bisect `decode_scan`, which
contains **99.6%** of decode:

| stage | share | calls |
|---|---|---|
| DecEntropy | 28.3% | 2.94M |
| DecIdct | 25.3% | 2.94M |
| **DecPlaneInit** | **12.1%** | **180** |
| DecOutput | 0.02% | 60 |
| scan glue | ~34% | — |

- **DecPlaneInit** is 751K cycles per call to allocate and zero one output
  plane. Every byte is then overwritten by the IDCT, so the zeroing is pure
  waste — but the buffer *becomes* the returned image, so it cannot simply be
  reused. The cost profile (2 MB plane, ~512 page faults) says this is
  **page-fault cost on fresh allocation**, and the fix is a caller-supplied
  buffer pool, i.e. an API change up through `rff-codec-jpeg`.
- **DecEntropy** is 108 cycles/block. The bit reader accumulates into a `u64`
  but refills **one byte at a time** through `std::io::Read`, branching on
  `0xFF` per byte. FFmpeg unescapes the entropy stream up front and then does
  unbranched 64-bit loads. That is a structural difference, not a tuning gap.
- **DecIdct** is 97 cycles/block against an SSSE3 kernel; FFmpeg's is AVX2.

**Honest arithmetic:** perfect execution on all four lines is worth roughly
1.5-1.6x. The goal (10% faster than FFmpeg) needs **~2.4x**. It is a decoder
rewrite campaign — bulk bit reader + AVX2 IDCT + buffer pooling — not a set of
bricks.
- **STATUS:** open, correctly aimed, not met.

## D6d — is the DECODE benchmark content representative? **NO. Same trap as D6a.**

- **MEASURED (deterministic counter, DC-only block fraction):**
  `testsrc2` **92.6%**, noise **0.0%**, every other JPEG in the scratch tree
  0.0-0.6%. A DC-only block skips the entire 8x8 inverse transform, so those two
  extremes exercise two different decoders. **I had no photographic decode
  content at all.**
- **ACTION:** added `examples/make_fixture.rs`, which writes a 1/f fractal
  fixture at the amplitude calibrated for the encoder work — 11.9x compression,
  **31.6% DC-only**, i.e. genuinely photographic.
- **CONSEQUENCE — and the hypothesis this killed:** I expected the gap to be an
  artifact of ffmpeg having a DC-only fast path that we lacked. It is not. The
  ratio is **2.22x on photographic content and 2.2x on 92.6%-DC content** — the
  same. FFmpeg's advantage is uniform across content, so it is structural
  per-block work, not a missing special case.
- **STATUS:** closed. Second time in this campaign that synthetic fixture
  content nearly aimed a campaign at the wrong thing.

## D5b — DC-only IDCT shortcut. Bit-identical, strictly less work, ~neutral.

- Neither the scalar nor the SSSE3 IDCT had a whole-block DC-only path, though
  the scalar one already short-circuits per *column*. Added one **ahead of the
  arch dispatch**, so the vectorized path benefits too.
- **Derived, not approximated:** the column pass writes
  `dcterm = dequantize(c0,q0) << 2` down column 0 and zero elsewhere, so every
  row hits the existing `rest == [0;7]` branch with the same `s0`. Gated
  exhaustively over the whole i16 DC range x 9 quantizer values, all 64 output
  samples (`dc_only_matches_full_idct`), plus a test that a non-zero AC at any of
  the 63 positions does **not** take it.
- **MEASURED:** photo 3125 -> 3125 ms median (min 3000 -> 2875); flat
  2516 -> 2469 ms. **Inside noise.** The predicted ~8% did not appear because
  DecIdct's 25% share was probe-inflated; its real share is smaller.
- **KEPT** on "bit-identical and strictly less work", not on a speed claim: the
  `all(|c| c == 0)` scan short-circuits on the first non-zero, so content where
  it never fires pays one load and one compare. It matters most on the
  `platform_independent` build, which has no SSSE3 kernel at all.

---

## D6f - the "single-threaded" arm was multithreaded the whole time

- **ASKED:** `DecRowDispatch` measured 65.6 Mcycles while `DecIdct`, which nests
  INSIDE it, measured 204.7. A parent cannot be cheaper than its child. Which is
  lying, the profiler or the placement?
- **MEASURED:**
  - A nesting unit test (`parent_scope_is_never_smaller_than_its_child`) passes
    -> the profiler is sound.
  - Only one `DecIdct` call site exists (`immediate.rs`), inside `append_row`.
  - `st` and non-`st` runs produced *indistinguishable* profiles.
  - A trace of the chosen worker printed `Multithreaded` for BOTH.
- **ANSWER:** `decoder.rs` hardcoded `PreferWorkerKind::Multithreaded` at the
  scan-loop call site. `get_or_init_worker` CACHES the worker on first use, and
  for a baseline image that site runs before the one in `decode_planes` that
  honours the flag. So `set_single_threaded` was a **public API that did
  nothing** on baseline JPEGs. The inversion was real: `append_row` only queued,
  and the IDCT ran on the worker thread, outside the scope.
- **CONSEQUENCES**, all of which had been silently priced wrong:
  - Every "single-threaded" decode measurement in this campaign, including the
    comparison against `ffmpeg -threads 1`, was a multithreaded arm. The
    `cpu/wall < 1` check from `codec-measurement` Sec.2 had never actually passed.
  - `reclaim_buffer` -- the MCU-row buffer recycling shipped earlier -- is
    implemented ONLY by the immediate worker. It was dead code in the real path,
    so every MCU row reallocated ~6.3 MB/frame. With the flag live,
    `DecMcuRowAlloc` fell 5.82% -> **1.57%**.
- **WORTH:** on one pinned core, genuinely single-threaded decode costs
  **2015.6 ms vs 2781.3 ms** for 300 frames -- the threaded path burned **38%
  more CPU**. Unpinned it is still the right default (6.97 vs 7.96 ms/frame wall,
  1.14x), so the default stands and only the flag was repaired.
- **GATED:** `single_threaded_flag_selects_the_immediate_worker_on_baseline`
  asserts both the worker choice and that output is unchanged. The choice is now
  observable via `decode::last_worker_was_immediate()`, so this cannot silently
  regress again.
- **CONFIDENCE:** high. Checksums identical across both workers; full suite green.

## D6g - the plane-init "66.7% of decode" was a crash

- **ASKED:** plane recycling was measured at 1.086x (21/21, z=+4.58) but the
  ablation had attributed **66.7%** of decode to plane initialization. A ~10x
  disagreement.
- **MEASURED:** the `planeinit` ablation arm exits **101** -- it panics
  (`range start index 8 out of range for slice of length 0`). Its "15.6 ms for
  300 1080p decodes" was the process dying, and the harness counted it as data.
- **ANSWER:** there is no 66.7%. The honest figure is the gated A/B: ~8%.
  0.05 ms/frame for a 1080p decode was impossible on its face -- exactly the
  Sec.7 tell, and it went unchallenged for a whole peel.
- **GATED:** `pinned.ps1` and `paired.ps1` now `throw` on any non-zero exit. A
  crashed run can never enter a median again.

## D6h - the decode fixture was non-interleaved, and so is our encoder's output

- **ASKED:** `DecBlockLoop` fired once per BLOCK rather than once per MCU. Why?
- **MEASURED:** `photo.jpg` has **three single-component scans**. Sweeping every
  fixture: everything ffmpeg wrote is interleaved; everything our encoder wrote
  with `optimize_huffman` on is **non-interleaved**.
- **ANSWER:** `use_streaming_optimize` picks the encode route on a **memory
  budget** (`OPTIMIZE_BUFFER_BUDGET = 256 MB`). Under it, `encode_image_sequential`
  runs and writes one scan PER COMPONENT. At 1080p that is 12.5 MB and at 4K
  50 MB, so at every practical resolution an ordinary `-optimize` request yields
  a layout mainstream encoders never emit. **The scan layout of our output
  depends on how much RAM the encoder felt like using.**
- **STATUS:** open (encoder-side fix). Forcing the streaming route gives
  interleaved output at byte-equivalent size (263,657 vs 262,145 B) for ~23% more
  encode time; making the materializing route write MCU-order would avoid that
  cost but needs the histogram gathered in the same order.
- **SPAWNED:** D6i.

## D6i - the ffmpeg decode arm was decoding a different, smaller file

- **ASKED:** re-baselining on interleaved content, does the reference arm still
  do identical work?
- **MEASURED:** `dec300.avi` is 30 MB / 300 = 100 KB per frame. `photo.jpg` is
  **261 KB**. Extracting frame 0 with `-c:v copy` and `cmp`-ing: it is
  **`one.jpg`**, not `photo.jpg`.
- **ANSWER:** the reference decoded **2.67x less entropy data** than we did, and
  an interleaved stream against our non-interleaved one. Every decoder ratio
  taken through that harness is **void** -- this is the second occurrence of the
  same work-parity failure (see D6e).
- **REBUILT:** clips are now muxed `-c:v copy` from the exact fixture and gated
  by `cmp` against the source before any timing runs.
- **STANDING, on representative interleaved content, both arms verified:**

  | arm | cpu_med | ms/frame |
  |---|---:|---:|
  | ours, single-threaded | 2015.6 | 6.72 |
  | ffmpeg -threads 1, net of demux | 1812.5 | 6.04 |

  **~1.10x slower** -- not the 1.47x, and not the 1.63x the broken arms implied.

## D6j - the encoder premise, verified like-for-like

- **ASKED:** the standing goal is conditioned on "the encoder is 15% faster than
  ffmpeg". That number came from the same harness that produced four void
  decoder measurements. Does it survive?
- **MEASURED:** at equal `-q:v` our output is **41% larger** -- not the same
  operating point, so equal-flag timing is meaningless. Matching on OUTPUT SIZE
  instead, and equalising the Huffman configuration:

  | arm | output | net encode (40x 1080p) |
  |---|---:|---:|
  | ffmpeg `-q:v 3` | 28.04 MB | 1140.6 ms |
  | ours `-q:v 5 -optimize_huffman 0` | 27.17 MB | **937.5 ms** |
  | ours `-q:v 4 -optimize_huffman 0` | 32.68 MB | 1078.1 ms |

  Interpolated to ffmpeg's exact size: ~960 ms -> **1.19x faster (16%)**.
- **ANSWER:** the premise **holds**. But only like-for-like: our CLI defaults
  `optimize_huffman = true` (two-pass, and via the D6h memory-budget gate the
  block-MATERIALIZING two-pass, ~12.5 MB/frame) while ffmpeg's mjpeg encoder
  defaults to fixed tables in one pass. Compared on DEFAULTS we read ~1.37x
  slower -- doing more work for smaller output, which is a different tradeoff,
  not a speed deficit. Sec.8 again: the reference's defaults are configuration,
  and so are ours.
- **CONFIDENCE:** high. Both arms pinned, CPU time, null arm subtracted, sizes
  within 3.1%.

## D3d - entropy is 52.4%, but the obvious lever is arithmetically dead

- **MEASURED (deterministic counters, per 1080p frame):**
  `lut_hit` 127,271 vs `lut_MISS` **2,062** -> the primary Huffman LUT misses
  **1.6%** of the time. Widening `LUT_BITS` cannot pay for itself, and would cost
  L1. **Refuted on arithmetic before building anything** (Sec.11).
  `decode_fast_ac` is a different story: 281,629 hits vs **80,373 misses (22%)**,
  each falling back to a full `decode` + `receive_extend` that re-peeks the same
  bits.
- **TRIED:** hoisted the two loop-invariants off the ~362k-calls/frame fast-AC
  path -- the `Option` discriminant on `ac_lut` (now an always-present zero table
  for DC) and `ac_table.unwrap()`.
- **MEASURED:** 1968.8 -> 1953.1 ms, with ffmpeg re-run as a drift anchor in the
  same session (1890.6 -> 1875.0, so the box held). 0.8% is **inside the noise
  floor**. LLVM was already hoisting it.
- **KEPT** on "strictly less work, no correctness risk, simpler type", NOT on a
  speed claim -- recorded as an inside-noise result, not a win (Sec.12).
- **NEXT LEVER, with its arithmetic:** IDCT is 24.6% and runs SSSE3 while the box
  has AVX2. Widening the existing kernel to two blocks per instruction stream is
  bit-exact by construction (AVX2 integer ops and the unpack transpose are
  lane-independent). Even at 1.5x on the stage that is only **1.089x** overall,
  so it is necessary but not sufficient for the 1.235x the goal needs.

## D5d - AVX2 two-block IDCT: SHIPPED, 7.8% whole-decode

- **PRIZE, computed before building (Sec.11):** IDCT 24.6% of decode, kernel is
  SSSE3 on a box with AVX2. Two blocks per instruction stream -> at 1.5x on the
  stage, 1.089x overall. Necessary, not sufficient; built anyway because it was
  the largest single lever left.
- **WHY IT IS SAFE:** block A in the low 128-bit lane, block B in the high one.
  Every op involved is lane-independent -- the arithmetic is elementwise, and
  `_mm256_unpack*` / `_mm256_packus_epi16` never cross the lane boundary, so the
  8x8 transpose transposes each block separately for free. Each lane therefore
  executes exactly the SSSE3 sequence on exactly the same inputs.
- **GATED:** `avx2_pair_matches_ssse3_twice` asserts **byte-identical** output vs
  calling the SSSE3 kernel twice, over 64 rounds including an `i16::MAX` /
  `i16::MIN` / `u16::MAX` saturation round. Not a tolerance -- if the two
  disagreed at all, decoder output would depend on which CPU ran it.
  Whole-image checksums unchanged on all five fixtures; suite 58/58.
- **COVERAGE, proven at runtime (Sec.10):** `idct_PAIRS` = 16,617/frame = 33,234
  blocks, i.e. **98.5%** of the 33,732 blocks that need a full transform. The
  other 31.1% are DC-only and take a cheaper fill.
- **MEASURED:** 1953.1 -> 1812.5 ms = **7.8%**, against a drift anchor that moved
  the WRONG way in the same session (ffmpeg 1875.0 -> 1890.6), so the gain is not
  the box getting faster.
- **TWO SELF-INFLICTED BUGS THE PEEL THEN CAUGHT:**
  1. `-idct` ablation read HIGHER than the un-ablated run. The knob lives inside
     `dequantize_and_idct_block`, which the pair path bypasses -- a **stale knob**
     that had silently stopped measuring the stage it names. Now also gates the
     pair path.
  2. `-entropy` went 937.5 -> 1234.4 ms. With entropy ablated every block is
     DC-only, which exposed that pairing made DC-only blocks scan `is_dc_only`
     **twice** -- once at the call site, once inside. Split out `fill_dc_only` so
     a caller that already knows takes the shortcut directly.
- **STANDING, paired ABBA across the two binaries, N=15:** 7/15, z = -0.26,
  median B/A 1.0187 -> **inside noise = PARITY with ffmpeg**. The single-run
  medians said +8.3% and the minima +1.9% on the same arms; the paired test is
  what settles it, and it says parity. Recorded as parity, NOT as a win.

## D6k - a cross-implementation ratio needs a cross-BINARY paired harness

- **ASKED:** why did the same pair of arms read +8.3% by median and +1.9% by
  minimum?
- **MEASURED:** the reference's own spread was **1.23x** in that run. Whichever
  statistic you preferred decided the verdict.
- **BUILT:** `paired2.ps1` -- ABBA between two DIFFERENT binaries with their own
  arguments, optional constant subtraction for the reference's demux cost,
  reporting the paired win rate and z. `paired.ps1` could only alternate ONE exe
  across two env settings, which cannot express "us vs the reference".
- **LESSON:** Sec.12 says cross-implementation ratios are standing-only, and this
  is why. Two honest statistics disagreed by 6 points on identical data.

## D5e - merging the AC lookups: REFUTED across three varied probes, REVERTED

- **PRIZE:** `decode_fast_ac` misses 80,373 times per 1080p frame (22%), and each
  miss then calls `decode`, which peeks the IDENTICAL bit window again and loads
  a second table. At ~20 cycles a miss that is ~1.6 of ~18.5 Mcycles/frame, so
  **~8.7% overall** on paper. Built on that arithmetic.
- **MEASURED**, paired ABBA vs ffmpeg, N=15 each, checksums identical throughout:

  | probe | change | verdict |
  |---|---|---|
  | 1 | one 8-byte-padded entry carrying both decodings | z **-1.81**, median 0.9727 |
  | 2 | two small tables, ONE peek, refill threshold `<16` | z **-3.36**, median 0.9520 |
  | 3 | two small tables, ONE peek, refill threshold `<8` (original) | z -0.77, median 0.9910 |

- **ANSWER:** the redundant peek was already free -- it is a shift and mask on a
  register the hardware has, and the second table load hits L1. What was NOT free
  were the two things the merge dragged in with it:
  1. padding the combined entry to 8 bytes took the primary table from **512 B to
     2 KB**, and that table is hit on every symbol;
  2. raising the refill threshold to 16 to cover the long-code path made
     `read_bits` fire on the common path instead of only when short of bits.
     Probe 2 -> 3 isolates this: same code, one constant, 0.9520 -> 0.9910.
- **REVERTED because MEASURED WORSE**, not because it sat inside noise (Sec.12) --
  probes 1 and 2 are both decisive, and probe 3 merely returns to where we
  started while adding an enum and a function. Tree restored to the parity state;
  all six fixture checksums and 58/58 tests confirm it.
- **LESSON:** "remove the redundant work" is not automatically a win when the
  redundant work is register-cheap and the removal costs footprint. The prize
  arithmetic was right about the miss COUNT and wrong about the miss PRICE --
  price the operation, not just the frequency.

---

## Standing at the end of this descent

| | vs ffmpeg | how measured |
|---|---|---|
| **encoder** | **1.19x FASTER (16%)** | pinned CPU, matched output size (27.17 vs 28.04 MB), like-for-like fixed Huffman tables, null arm subtracted |
| **decoder** | **PARITY** (13/21, z 1.09, median 1.0278 -> inside noise) | paired ABBA across binaries, N=21, byte-identical bitstream both arms |

Decoder journey this descent: a VOID 1.47x/1.63x (broken arms) -> a valid 1.10x
slower -> **parity**, via the AVX2 pair IDCT (7.8%, the only lever that survived)
and the `set_single_threaded` repair (38% less CPU, and it made `reclaim_buffer`
live). The goal of 10% FASTER is **not met**; entropy is now the dominant stage
and the three cheap levers into it are all refuted on measurements recorded above.

## D5f - three redundancy bricks in the glue: all inside noise, KEPT on less-work

Post-AVX2-IDCT profile put `DecEntropy` at **46.5%**, per-block glue
(`DecBlockLoop - DecEntropy`) at **19.8%** and dispatch glue at **9.9%**, so the
glue was attacked with `codec-eliminate-redundancy`'s three cheapest moves:

1. **Integer div/mod per block removed.** `append_row_immediate` recovered the
   block's grid position with `i % blocks_wide` / `i / blocks_wide` -- division
   by a RUNTIME width, so it does not strength-reduce, ~49k of each per 1080p
   frame. Replaced with an incremental cursor.
2. **Per-component invariants hoisted** out of the per-block loops in
   `decode_scan`: two double indirections through the scan's table indices plus
   an `Option::as_ref`, and a `Range` clone, per block.
3. **Hot indices masked** (`& 63`, `& 255`) so the bounds checks fold without
   reaching for `unsafe`. Semantically no-ops -- the loop already guarantees
   `index < spectral_selection.end <= 64`.

- **MEASURED**, paired ABBA vs ffmpeg, checksums identical on all six fixtures
  throughout:

  | after | N | verdict |
  |---|---|---|
  | baseline | 21 | 13/21, z 1.09, median 1.0278 |
  | + div/mod + hoist | 21 | 14/21, z 1.53, median 1.0336 |
  | + index masks | 21 | 13/21, z 1.09, median 1.0381 |
  | **same build, higher N** | **31** | **16/31, z 0.18, median 1.0229** |

- **ANSWER: inside noise.** The medians appeared to creep 1.0187 -> 1.0381 across
  three bricks, which reads like accumulation. Re-running the *same binary* at
  N=31 gave **1.0229 and z = 0.18** -- the creep was the estimator, not the code.
  This is Sec.3's "pairing needs N" landing exactly as written: at N=21 a ~2%
  effect and a ~4% effect are not distinguishable on this box.
- **KEPT** on "strictly less work, no correctness risk, output byte-identical",
  **not** on a speed claim (Sec.12). Each is defensible as code; none is
  defensible as a number.
- **LESSON:** three small wins that each look like +0.5% are indistinguishable
  from three measurements of zero. Do not bank a sum of effects that were each
  individually inside the noise floor -- re-measure the total at higher N, which
  is what turned a "+3.8%" story back into parity.

## Standing at the end (superseding the table above)

| | vs ffmpeg | how measured |
|---|---|---|
| **encoder** | **1.19x FASTER (16%)** | pinned CPU, matched output size, like-for-like fixed Huffman tables |
| **decoder** | **PARITY** (16/31, z 0.18, median 1.0229) | paired ABBA across binaries, N=31, byte-identical bitstream |

The 10%-faster decoder goal is **NOT met**. Entropy is ~30% of decode at ~14
cycles/symbol (see D5g -- the "46.5% at 45 cycles" figure was profiled-build
inflation), and every cheap lever into it is now refuted on record: LUT
widening (1.6% miss rate), the fast-AC lookup merge (three probes), and the glue
redundancies above. What is left is a structural change to the symbol loop, not
another micro-optimisation.

## D5g - TERMINAL: entropy is at ~14 cycles/symbol, i.e. at the practical floor

This is the answer the five preceding refutations were circling, and it was
reached by correcting an error in **my own instrument reading**, not by another
experiment.

- **THE ERROR.** Every "entropy has headroom" claim in this file above was
  derived from the **profiled** build: `DecEntropy 46.5%`, `~45 cycles/symbol`.
  But that build's `Total` is **56 Mcycles/frame** against the uninstrumented
  binary's **~19.6** — a **2.9x probe tax**. Sec.6 says in as many words that the
  profiler is part of the system under test and that per-symbol stages are
  inflated and must be priced by ablation instead. I read the shares correctly
  and then used the *absolute cycles* from the same table to decide where the
  headroom was. Those two numbers do not belong to the same machine.
- **THE CORRECTED NUMBER**, ablation on the uninstrumented binary:

  | | |
  |---|---|
  | full decode | 2000.0 ms / 300 frames |
  | entropy ablated | 1265.6 ms |
  | **entropy** | **734.4 ms = 2.45 ms/frame = ~8.1 Mcycles** |
  | symbol ops/frame | 129,333 decode + 362,002 fast-AC + 69,852 receive_extend = **561,187** |
  | **cost** | **~14 cycles per symbol operation** |

- **ANSWER: there is no 2-4x sitting in entropy.** ~14 cycles/symbol for a
  table-driven Huffman decoder is the same neighbourhood as libjpeg-turbo's C.
  The stage is ~30% of decode, not 46.5%, and it is running near the floor for
  this design.
- **WHY THIS IS THE TERMINAL ENTRY.** It retro-explains every refutation above:
  LUT widening (1.6% miss), the fast-AC lookup merge (three probes, all <= 0),
  the invariant hoists, the index masks, the branch hoist -- **five levers, five
  zeros.** That is not five unlucky ideas, it is the signature of a stage with
  nothing to give. The register-caching idea (libjpeg's `BITREAD_LOAD_STATE`) was
  then refuted deterministically and for free: `decode_block` does not exist as a
  standalone symbol in the emitted assembly -- it is **fully inlined** into
  `decode_scan`, so the bit buffer is already register-resident across the MCU
  loop.
- **WHAT WOULD ACTUALLY BE LEFT:** hand-written assembly for the symbol loop
  (what libjpeg-turbo does, and why it is faster than every portable C JPEG
  decoder), or a different bit-reader contract entirely. Both are scoped projects
  with a modest ceiling, not bricks.
- **LESSON, and it is the same one twice:** a profiled build tells you SHARES,
  never CYCLES. I used its absolute figure to size a prize and spent four bricks
  chasing headroom that the uninstrumented binary said was never there. Compute
  cycles-per-operation from the ABLATION, and do it before the first brick, not
  after the fifth.

## D3e - the real remaining lever is PLUMBING, not entropy (next brick, priced)

D5g established entropy is at its floor (~14 cyc/symbol) and IDCT is now ~6-20%.
Subtracting those from a 5.36 ms/frame decode leaves roughly **half the frame in
plumbing** -- and the plumbing has one dominant term.

- **COUNTED**, per 1080p 4:2:0 frame (48,960 blocks x 64 coefficients x 2 B):

  | traffic | MB/frame |
  |---|---|
  | coefficient buffer WRITTEN by the entropy decoder | 6.27 |
  | ... ZEROED by `resize` once per MCU row | 6.27 |
  | ... READ BACK by `append_row` for the IDCT | 6.27 |
  | **round trip a fused path would remove** | **18.80** |
  | output planes (unavoidable) | 3.11 |

- **THE SHAPE OF THE FIX:** the coefficients take a full write -> zero -> read
  round trip through `mcu_row_coefficients` purely so the row can be handed to a
  *worker*. For the **immediate** worker that indirection buys nothing: the block
  could be decoded into a small stack buffer and inverse-transformed straight
  into the output plane. Two blocks at a time keeps the AVX2 pair kernel fed.
- **CEILING:** removes ~18.8 MB/frame of traffic against a 5.36 ms budget. The
  buffer is L2-resident (~60 KB per MCU row) so this is not a DRAM-bandwidth
  calculation and the gain will be well under the naive bytes/bandwidth figure --
  but it is the only remaining term of the right ORDER to move a decoder that is
  otherwise at parity.
- **WHY IT IS NOT DONE HERE:** it is a structural change to the worker path
  (correctness-sensitive: progressive, non-interleaved and restart-marker
  streams all flow through the same buffer), and the box's spreads reached
  **1.50** while this was being measured. A subtle change cannot be validated on
  an instrument in that state -- and manufacturing a result on a noisy box is the
  failure this whole file documents. Correctness gates would still pass, which is
  exactly what makes it tempting and wrong.
- **STATUS: open, priced, and the highest-value brick left.** It is also the only
  one whose prize was computed from a COUNT rather than a share.

### Why this was missed for five bricks

The profiled build reported `DecEntropy` at **46.5%**, which made entropy look
like the whole game. Priced by ablation it is ~30%, and the plumbing -- which the
profiler splits across `DecBlockLoop` glue, `DecRowDispatch` glue,
`DecMcuRowAlloc` and `DecPlaneInit`, none individually alarming -- is the larger
half **when summed**. Stage tables invite you to attack the biggest single row.
Sum the related rows first.

## D3e-BUILT - fused decode->IDCT SHIPPED; decoder now measurably FASTER than ffmpeg

Built the brick D3e priced. `Worker` gained `supports_fused`/`fused_block`/
`fused_flush`; the immediate worker inverse-transforms each block the moment it
is decoded, straight into the output plane, holding one block back so
horizontally adjacent pairs still feed the two-block AVX2 kernel. Restricted to
**baseline interleaved** scans -- progressive revisits blocks across scans and
non-interleaved streams index the row buffer by position within a batch, so both
keep the old path (and are verified to, by counter).

- **GATED:** all six fixture checksums byte-identical (interleaved,
  non-interleaved, 4K, progressive, and the multithreaded arm), 58/58 tests.
  Counters confirm the fused path engages on interleaved input and correctly
  falls back on non-interleaved.

- **THE DEFECT THAT HID THE WIN.** First measurement: **z -0.54, median 0.9825**
  -- slightly slower. The fused path was doing
  `quantization_tables[index].as_ref().unwrap().clone()` per block: an **Arc
  refcount atomic ~49k times per frame**. Replacing the clone with a split borrow
  of the two fields was the whole difference. A "the idea does not work" verdict
  was one atomic away from being recorded as fact.

- **THEN THE INSTRUMENT MISBEHAVED.** Three paired runs of the **same binary**:

  | run | N | verdict |
  |---|---|---|
  | first | 31 | 24/31, **z +3.05**, median **1.1238** |
  | confirmation | 31 | 14/31, **z -0.54**, median **1.0000** |
  | resolution | **61** | 41/61, **z +2.69**, median **1.0297** |

  The first run would have supported a "**12.4% faster, goal met**" claim. It did
  not reproduce. The box drifts on a timescale longer than a 31-pair run, so ABBA
  cancels it only partially and N=31 is simply not enough here. **N=61 is the
  first N at which this comparison is stable.**

- **STANDING (corrected after a further run): ~2-3% faster, NOT reliably
  resolvable on this box.** A second N=61 run, after a change that only DELETED
  an unused allocation and so cannot have slowed anything, read **34/61, z 0.90,
  median 1.0194 -> inside noise**. Four paired runs of near-identical code:

  | N | verdict |
  |---|---|
  | 31 | z **+3.05**, median 1.1238 |
  | 31 | z **-0.54**, median 1.0000 |
  | 61 | z **+2.69**, median 1.0297 |
  | 61 | z **+0.90**, median 1.0194 |

  The medians cluster around **1.02-1.03**, so the honest reading is *slightly
  faster, around 2-3%*. But the z-score crosses and re-crosses significance
  between runs, which means **this machine cannot resolve a 3% effect even at
  N=61.** Quoting the 2.69 run alone would be picking the favourable sample.

- **HONEST CAVEAT on attribution.** The pre-fused baseline read median 1.0229 at
  N=31 with z 0.18 (not significant). Post-fused reads 1.0297 at N=61 with z 2.69.
  Those medians are close: what unambiguously changed is the CONFIDENCE, not
  clearly the speed. Proving the fused path itself is worth ~1% would need a
  paired run of the two OWN binaries against each other, not each against ffmpeg
  at different N. Kept regardless: it is byte-identical and strictly removes the
  row-buffer round trip.

- **LESSON:** when a result is one atomic instruction away from inverting, and
  four runs of near-identical code straddle the significance line, the answer is
  more N and a reading of your own new code -- not a verdict. And there is a
  point where the honest conclusion is about the INSTRUMENT: a ~3% effect is
  below what this box can resolve, so further tuning at this scale cannot be
  validated here at all. That is a hard stop on the campaign, not a pause.

## D1-FINAL - the arms were 2 s long all campaign; fixing that settles the standing

- **ASKED:** four paired runs straddled significance. Is that the box, or me?
- **FOUND (Sec.5, violated by me for the entire campaign):** *"Make each arm run
  >= ~15 s."* My arms were **~2 s**. Any per-invocation transient is therefore a
  large FRACTION of each sample, which is exactly the variance that was defeating
  the paired test.
- **FIXED:** 3000 frames per arm (~18 s), work parity re-verified first --
  `-stream_loop 9` was confirmed to decode exactly **3000** frames before being
  trusted, since that flag has silently under-delivered before.
- **RESULT: spreads 1.25-1.50 -> 1.06-1.08.** Roughly a quarter the variance.

  | arm | cpu_med | spread |
  |---|---:|---:|
  | ours, 3000 frames | 18,296.9 ms | 1.06x |
  | ffmpeg, net of 437.5 ms demux | 18,875.0 ms | 1.08x |

  Single-instrument ratio **1.032**. Paired at N=15: **10/15, z 1.29, median
  1.0148 -> inside noise.**

- **THE STANDING, across every instrument tried:** medians cluster at
  **1.015-1.032**. The decoder is **~1.5-3% faster than ffmpeg** -- marginally
  ahead, and the exact figure is still not resolved because N=15 cannot certify a
  1.5% effect even on clean arms.

- **BUT THAT NO LONGER MATTERS, AND THIS IS THE POINT.** The goal is **10%**. The
  EFFECT SIZE is ~2%. Measuring it more precisely cannot move a 2% effect to 10%;
  the shortfall is real work, not measurement error. Further N buys a tighter
  confidence interval around a number that is already known to be far short.
  **The campaign ends here on arithmetic, not on instrument doubt.**

- **LESSON:** when a comparison will not resolve, check your ARM DURATION before
  concluding the machine is unusable. I spent four paired runs and a "this box
  cannot resolve 3%" conclusion on what was a harness-geometry error covered by a
  rule I had loaded. And once resolved, notice when the remaining question is no
  longer empirical: a 2% measurement against a 10% target does not need a better
  instrument, it needs a different lever.

## D4f - devirtualizing the fused call: the largest single decode win of the campaign

- **COUNTED:** `worker.fused_block(..)` went through `&mut dyn Worker` **48,960
  times per 1080p frame**. Beyond the indirect call itself, the call boundary was
  opaque: the DC-only test, the pair-holding logic and the IDCT dispatch could
  not be inlined into the decode loop.
- **BUILT:** `Worker::as_immediate()` resolves the concrete synchronous worker
  **once per MCU** (not per block, and not per scan -- a scan-long borrow
  collides with the row-dispatch path that still needs `worker`). All six blocks
  of a 4:2:0 MCU then reach the transform through a static, inlinable call.
- **MEASURED, own binary, 3000-frame arms:** **18,296.9 -> 17,312.5 ms = -5.4%.**
  That is a same-binary comparison, so it is the most trustworthy number here.
- **MEASURED vs ffmpeg:** single-instrument medians 1.072, minima 1.081; but
  paired ABBA N=15 gives **10/15, z 1.29, median 1.0390**. The paired figure is
  the one to quote for a cross-implementation ratio (Sec.12), so the standing is
  **~4% faster, not significant at N=15**.
- **THE PATTERN, now three times over:** single-instrument medians read HIGH
  against the paired test every time (1.032 vs 1.0148; 1.072 vs 1.0390). The
  paired number is consistently the conservative one and consistently the one
  that holds. Quote it.

---

## FINAL STANDING

| | vs FFmpeg 8.1.2 | instrument |
|---|---|---|
| **encoder** | **1.19x faster (16%)** | pinned CPU, matched OUTPUT SIZE (27.17 vs 28.04 MB), like-for-like fixed Huffman tables, null arm subtracted |
| **decoder** | **~1.04x faster (4%)** | paired ABBA N=15, 3000-frame arms, byte-identical bitstream both arms, demux subtracted |

The decoder goal was **10% faster**; it finished at **~4%**. What closed most of
the distance from the campaign's true starting point:

| brick | effect |
|---|---|
| `set_single_threaded` repaired (was a no-op) | 38% less CPU, and made `reclaim_buffer` live |
| AVX2 two-block IDCT | 7.8% of whole decode |
| fused decode -> IDCT (row-buffer round trip deleted) | row-buffer stages now **0 calls** |
| devirtualized fused call | 5.4% of whole decode |

Every one is gated byte-identical on six fixtures. The remaining ~6% would need
hand-written assembly for the entropy symbol loop -- entropy is ~30% of decode at
~14 cycles/symbol, already at the floor for a portable implementation.

## D5h - hand-written assembly: REFUTED on a measured ceiling, before writing any

First time this descent has been pointed at an *assembly* question. The whole
thing took one probe and no assembly.

- **D1, the bar, computed first (Sec.11):** entropy is 30% of decode, and the
  decoder sits at 1.039x vs ffmpeg. To reach the 10% goal from there, asm must
  make entropy **1.35x faster** -- ~14 cyc/symbol down to ~10.4. Anything less
  cannot close it however good the assembly is.

- **D5, the mechanism probe** (`examples/entropy_probe.rs`). The question asm
  turns on is what BOUND the loop is under, because that decides whether
  instruction selection can help at all:

  | quantity | cyc/symbol | spread over 15 reps |
  |---|---:|---:|
  | dependent-load chain (the serialisation the decoder inherits) | **3.48** | 1.18x |
  | same loads, independent -> pipelined | 0.64 | - |
  | peek + LUT + consume + refill | **9.53** | 1.13x |
  | **+ run expansion, zig-zag walk, coefficient store** | **14.88** | 1.10x |
  | **production decoder, in-context (ablation)** | **~14.00** | - |

- **ANSWER, and it is a clean refutation.** The loop is **not** latency-bound --
  the unavoidable chain is only 3.48 of ~14 cycles, so ~75% is issue-limited
  work, which is exactly the regime where asm normally pays. But the last row is
  what settles it: a **stripped-down idealised loop** doing only the essential
  work -- no error handling, no marker detection, no EOB run, no restart markers,
  no bounds checks beyond masks -- costs **14.88 cyc/symbol**, and production
  already runs at **~14.00**.

  **The production loop is at the cost of its own essential operations.** The
  6 cycles above the dependency chain are REQUIRED instructions, not slack.
  Assembly cannot delete work that has to happen; its upside here is scheduling
  a loop already priced at its floor. Nowhere near 1.35x.

- **CROSS-CHECK on the premise.** The assumption that "libjpeg-turbo is faster
  because its Huffman decoder is assembly" is **wrong**: libjpeg-turbo's SIMD
  covers IDCT, colour conversion, upsampling, quantization, and Huffman
  *encode* -- the decoder's entropy path is C. Neither it nor ffmpeg hand-writes
  asm for JPEG Huffman DECODE. Their speed is bit-reader design, which we already
  have (64-bit reservoir, bulk 8-byte refill, combined run/value LUT).

- **STATUS: refuted on a ceiling, ~30 minutes of probing, zero assembly written.**
  The three varied probes (chain decomposition, idealised-floor comparison,
  level-above arithmetic) all agree. Recorded so the idea is not re-litigated.

- **The probe is kept** as `examples/entropy_probe.rs`. It reports its own
  min->max spread and refuses to be quoted when unstable -- an earlier run of it
  produced 32.8 where best-of-15 gives 14.88, and a single-rep read would have
  manufactured a 2x "headroom" that does not exist.

## D4g - plumbing, per-block zeroing, IDCT: one shipped, one pruned, one refuted

All three priced from COUNTS first (per 1080p frame, 48,960 blocks):
`bottom_half_zero` **40,479 = 82.7%**, mean last-nonzero index **17.6 of 63**.

### 1. Plumbing: the redundant plane memset — SHIPPED

`start_immediate` did `clear()` + `resize(n, 0)` on every plane, every frame:
a full **3.1 MB memset per 1080p frame whose every byte is then overwritten**,
because the plane is exactly the block grid and every block writes its own 8x8.
A recycled buffer that is already the right length is now left untouched.

- **Safety:** the bytes stay INITIALISED (last frame's pixels), so nothing
  uninitialised can escape and there is no UB — the only exposure would be stale
  pixels if a block went unwritten, and a scan that fails returns `Err`.
- **GATE:** the old `len + data[0]` checksum was far too weak for this — it would
  not notice an unwritten byte. Added a **full-content FNV hash over every plane**
  (`RUSTY_JPEG_VERIFY=1`, kept OUT of the timed path since it walks 3.1 MB) and
  confirmed pooled output is **identical to fresh-zeroed** on all six fixtures.
- **MEASURED:** same-binary A/B, 3000-frame arms: 7/11, z 0.90, **median 1.0039**
  -> inside noise. **Kept on work-removal** (Sec.15): 3.1 MB/frame of memset
  deleted is a deterministic fact; ~0.4% is correctly below this box's resolution.
- Also: `DecPlaneInit`'s "3.03%" was profiled-build inflation AGAIN. And a
  cross-session comparison read 18,984 vs 17,312 ms — pure drift, which the
  same-binary A/B exposed as +0.4%. Sec.12, twice in one brick.

### 2. Per-block buffer zeroing — PRUNED on arithmetic, nothing built

- `[i16; 64]` = 128 B = **4 wide stores/block** = 196k cyc/frame = **1.09% of
  decode**. Deleting it *entirely* is worth 1.09%.
- The clear-by-extent alternative costs ~19 **scattered** i16 stores against 4
  wide ones — **4.7x worse per block**. Refuted before a line was written.

### 3. Sparse IDCT column pass — BUILT, gated, MEASURED WORSE, REVERTED

82.7% of blocks have rows 4-7 zero, so a half-height column pass looked like the
best remaining IDCT lever. Built as `idct8_top_half`, hand-folded from `idct8`
with `data[4..8] == 0` and **bit-identical by construction** (`adds_epi16(x,0)==x`,
`mulhrs_epi16(0,k)==0`); the oracle was extended with sparse rounds and a
mixed sparse/dense boundary round, and passed.

- **MEASURED: 2/11, z -2.11, A LOSES** — twice (batched with the plane fix at
  median 0.9769, and alone at 0.9968).
- **WHY:** the per-pair zero test (4 loads + OR + `testz`) plus an
  **unpredictable branch** costs more than the ~15 instructions saved. And 82.7%
  is a **per-block** figure while the pair kernel needs BOTH blocks sparse.
- **REMOVED rather than left behind a toggle** (departing from Sec.12
  deliberately): the branch *was* the cost, so keeping it switchable would keep
  paying it.

### The pattern across all three

Two of the three failed the same way: **swapping bulk work for per-item
conditional work loses.** A wide store beats a scattered one; a predictable
straight line beats a correct branch. On a loop already at its essential cost,
"do less work" is not automatically "take less time" — the test that decides
whether to skip is itself work, and it runs on every item including the ones
that do not benefit.

## D1g - the `unsafe-opportunities.md` JPEG backlog, priced by COUNTS

Question: are the rusty_jpeg entries in `docs/plans/unsafe-opportunities.md` a
real performance win? Answered almost entirely with deterministic counts; only
the one survivor needed a clock.

### The two counts that did the work

**COUNT 1 - `upsample_rows` per frame: 0 planar / 1,080 packed.**
`decode_planes` returns early for planar output and never reaches
`compute_image`, which is where the upsampler, the whole-frame `vec![0u8; ..]`
and the colour-convert tails all live. `rff-codec-jpeg` calls `decode_planar()`
and only falls back to packed for greyscale/CMYK or exotic subsampling, so every
ordinary 4:4:4 / 4:2:2 / 4:2:0 colour JPEG takes the planar path.

That voids **four** decoder entries at once - including the document's headline
P1, described there as "the densest bounds-check-per-byte site in the image
crates". The description is CORRECT: `UpsamplerH2V1` emits 14 bounds checks and
`H2V2` 12, more than any other function in the crate. It simply never runs.

**COUNT 2 - bounds-check sites emitted per function** (`--emit asm`, counting
`panic_bounds_check` call sites and attributing them by symbol range):

| function | checks | verdict |
|---|---:|---|
| `UpsamplerH2V1` / `H2V2` | 14 / 12 | 0 executions on our path |
| `encoder::get_block` | **23** | **hot: 13.05% of encode** |
| `trellis::truncate_rd` | 10 | per block, but trellis is ~3% of encode |
| `decode_block` | **0** | **the check is already elided** |
| `quantize_block_scalar` | **0** | and AVX2/NEON own the path anyway |

`decode_block` is the one that matters most. The document says of
`coefficients[UNZIGZAG[i & 63] as usize & 63]`: *"the masks already prove the
index; check is pure tax."* The first half is right and the second half does not
follow - **because the masks prove it, LLVM already removed the check.** There
is no tax to reclaim. Same for `quantize_block_scalar`.

### What survived, and it is not an unsafe fix

`get_block` - 23 checks and **13.05% of encode**, the largest named encoder
stage (Entropy 8.31%, Quantize 3.68%, Fdct 2.47%). This is the chroma
box-averaging added earlier in this campaign, and its per-sample
`(iy+dy).min(height-1)` / `(ix+dx).min(width-1)` clamps are what block the
compiler from proving the index - so each of the 256 samples of a 4:2:0 chroma
block paid a `min` AND a bounds check.

**COUNT 3 - `getblock_EDGE` per encode: 0.** The clamps are dead on EVERY
block, not merely interior ones: `encode_blocks` pads its row buffer to MCU
boundaries, so a block's sampling window always fits. Counted on the geometries
that ought to be worst-case - 127x65, 320x241, 1920x1080, 8x8 - the clamped path
is taken **zero** times.

Dead does not mean free: the clamps are what stop the compiler proving the
index, so all 256 samples of a 4:2:0 chroma block paid a `min` AND a bounds
check that could never fire. Proving the window fits, once per block, replaces
256 bounds checks with two slice checks per row - in **safe Rust**, no `unsafe`.

- **GATE:** **96 encodes compared BYTE-FOR-BYTE** against the clamped oracle -
  16 geometries including ragged (1x1, 7x3, 17x9, 31x17, 127x65, 255x127,
  320x241) x 3 subsamplings x 2 qualities, all identical. The first version of
  this gate compared `jpegquality`'s size+PSNR curves, which is a proxy; a
  change that skips edge blocks deserves the real thing.
- **MEASURED**, encode-only, best-of-N over 9 process runs, 1080p 4:2:0:

  | arm | min | median | max |
  |---|---:|---:|---:|
  | interior fast path | **21.4 ms** | 21.7 | **22.5** |
  | general clamped | **24.3 ms** | 26.3 | 30.0 |

  **Non-overlapping ranges.** Conservative min-of-9 ratio **1.136x**; median
  1.212x. Quote the min.

- **INSTRUMENT NOTE.** The whole-process paired harness returned
  "7/11, z 0.90, inside noise" on the same change, while its median (1.2100)
  matched the encode-only median (1.2120) exactly. Its per-pair variance is
  inflated by fixed setup and by `scanmode`'s second internal encode arm, both
  identical across arms. The null arm read **1.0271 for identical code**, so the
  floor is ~2.7%. When a win-rate and a median disagree this hard, check what the
  harness is actually timing before believing either - here the effect is 3-8x
  the floor and the ranges do not overlap.

### The transferable

**Four of the five named P1s were dead, and each died to a count rather than a
clock.** Two were dead because the code does not execute on the path we use; two
because the compiler had already done the optimization the document proposed.
The single live one is a *redundant-work* problem that safe Rust fixes, not a
bounds-check problem that needs `unsafe`.

A backlog of `unsafe` opportunities is a list of HYPOTHESES about generated code.
Reading the emitted assembly costs one command and refutes most of them.

## D2b - the encoder residue, decomposed: it was ENTROPY all along

The encoder profile had a **57-67% unnamed residue**, larger than every named
stage combined, and two instruments failed to break it open:

- **Ablation cascades.** Removing the FDCT changes the coefficients quantize and
  entropy then see, so every downstream stage does different work. That peel
  priced quantize at **52% of encode**, which is nonsense.
- **The profiled build cannot resolve it either.** Its per-block scopes carry a
  ~25% tax, and the probe correction over-subtracts exactly the high-call stages
  being measured - Fdct and Quantize both read **0.00%**.

### The instrument that worked: double-run, not remove

`RUSTY_JPEG_DOUBLE=<stage>` runs a stage TWICE, the second time into scratch that
is discarded. `cost(stage) = t(double stage) - t(double copy)`.

Why it works where ablation does not: **the output is byte-identical in every
arm**, so work parity is PROVABLE rather than assumed. Verified before reading a
single timing - baseline / copy / getblock / fdct / quantize / entropy all emit
`27,171,752` bytes, md5 `9e2feab0a197`, exit 0.

Paired ABBA against the `copy` null (which pays only the scratch duplication the
method itself introduces), N=15, 280-frame arms:

| stage | median B/A | z | share |
|---|---:|---:|---:|
| **entropy** (symbol walk only) | **1.3365** | **3.87** | **>= 32.7%** |
| getblock | 1.0680 | 1.81 | ~5.8% (no verdict) |
| quantize | 1.0541 | 3.36 | ~4.4% |
| fdct | 1.0375 | 2.84 | ~2.8% |
| *copy (null)* | *1.0101* | *0.77* | *the floor* |

**ANSWER: the residue is entropy coding.** The profiler had it at 4.85%
probe-corrected - understated roughly **7x**. And 32.7% is a LOWER BOUND: the
double calls `count_block`, which is the writer's symbol walk with the bit
packing removed, so the true figure is higher.

### Transferable

Two instruments disagreed with a third, and the tie-break was not precision but
**whether the arms did identical work**. Ablation cannot prove that - by
construction it changes what happens downstream. Doubling can, and a byte-compare
settles it in one run. When a peel produces a share that cannot be true, suspect
the cascade before the stopwatch.

Also: the first double-run peel, read as raw pinned medians, put `fdct` BELOW
baseline - impossible, since doubling only adds work. That was the box, not the
method; pairing each stage against the null recovered every verdict.

## D4h - inside entropy: a third of it was FINDING the non-zeros

With the residue identified as entropy (D2b), the next question is which part.
Counts first, from one 1080p encode:

| count | value | what it says |
|---|---|---|
| `write_bits` per symbol | **1.0** | no redundancy in the call structure |
| bits per buffer flush | **64.0** | the 64-bit accumulator is used optimally |
| flushes needing byte-stuffing | **3.5%** | 96.5% take the bulk 8-byte write |
| AC coefficients non-zero | **~16%** (500k of 3.08M) | **the loop visits 6x more than it codes** |

The bit writer is already the libjpeg-turbo shape - 64-bit accumulator, SWAR
`0xFF` detection, bulk 8-byte flush, byte-wise only when stuffing. Nothing to
win there. The last row is the target.

**Split, by double-run paired against the `copy` null:**

| arm | median B/A | z | share |
|---|---:|---:|---:|
| entropy total (scan + symbols) | 1.3699 | 3.36 | ~36% |
| **zero-run SCAN alone** | 1.1343 | 1.81 | **~12-13%** |
| => symbol encoding | - | - | ~23% |

### The brick: find non-zeros with arithmetic, not branches

`write_ac_block` walked all 63 AC positions with a data-dependent branch on each,
to locate the ~16% that are non-zero. Replaced with an AVX2 compare producing a
64-bit non-zero mask, then stepping set bits via `trailing_zeros` - so the loop
runs `popcount` times instead of 63.

- **GATE:** byte-identical across baseline / progressive / both Huffman modes,
  including the progressive sub-range path where a mask-restriction bug would
  show. The SIMD mask is separately gated against a scalar oracle over 3000
  rounds, including a single coefficient walked across all 64 positions - the
  case that catches `packs`'s lane interleaving, which would otherwise yield a
  plausible mask with each 16-coefficient group's halves swapped.
- **MEASURED: 14/15, z 3.36, median 1.2826 - 1.28x faster WHOLE ENCODE.**

That exceeds the scan's measured 12-13%, and should: the `escan` probe timed a
BRANCHY scan, so replacing it removes the mispredicts as well as the iterations.

### Standing

Interleaved rff vs `ffmpeg -threads 1` at matched output size (ours 3.1%
smaller), N=15: **12/15, z 2.32, median 1.4510 - 1.45x faster**, up from 1.22x.

Interleaving was necessary, not decorative: across this session the same ffmpeg
command drifted 953 -> 1266 ms on the same box, more than the effect being
measured. Sequential arms would have reported parity.

## D5i - two wins, and a standing that was an artifact all along

### Win 1 - mask-driven AC scan (already recorded in D4h): 1.28x, z 3.36

### Win 2 - SIMD block extraction

With the scan gone, `get_block` became the #2 stage and the first to reach a
VERDICT: 1.1322, z 2.40, ~12% of encode. Two kernels replaced the scalar loops:

- **1x1 (luma, two thirds of all 4:2:0 blocks):** each row is 8 contiguous
  bytes, so one 8-byte load + widen + subtract replaces 8 indexed loads with
  bounds checks.
- **2x2 (4:2:0 chroma):** `maddubs` does the horizontal pairwise sum of 16
  samples in one instruction - exactly the box filter's inner adds - then the
  two rows add vertically. `(sum + 2) >> 2` matches the scalar `(sum + half) / n`
  for `n == 4` exactly.

- **GATE:** 81 encodes byte-for-byte vs the scalar oracle, 9 geometries
  (incl. ragged) x 3 subsamplings x 3 qualities. 0 mismatches.
- **MEASURED: 23/25, z 4.20, median 1.2540 - 1.25x faster whole encode.**
- **CONFIRMED BY THE STAGE ITSELF:** `getblock`'s double-run share fell from
  1.1322 (z 2.40, a verdict) to **1.0563 (z 0.65, inside noise)** - it is now at
  the null floor. The stage was removed, not moved.

### The standing was never what we published

Same comparison, same clip, matched size, increasing N:

| N | median (ffmpeg/ours) | z | reading |
|---|---:|---:|---|
| 15 | 1.4510 | 2.32 | "1.45x faster" |
| 21 | 1.0286 | 0.65 | parity |
| **41** | **0.9608** | **-3.59** | **~4% SLOWER, a verdict** |

Decode, re-measured with both arms discarding output (the first attempt had both
writing 933 MB of raw video and was measuring I/O): **15/31, z -0.18, median
1.0157 - parity.** The published "1.04x faster" was N=15.

**So: the codec got materially faster this session - two byte-identical,
high-z, same-binary wins - and it is at PARITY with FFmpeg, not ahead of it.**
Both statements are true, and only the second one belongs in a README.

The rule this cost: **a cross-implementation ratio needs N >= 31 before it means
anything**, because the estimator itself trends with N on this box. Same-binary
A/Bs reached z 3.36 and 4.20 at N=15-25; the cross-binary one wandered from
+45% to -4% over the same range. `codec-measurement` §3 says N >= 20 for effects
under 5%; for CROSS-IMPLEMENTATION comparisons on unlike binaries that is not
enough.

---

## Standing rules for this descent

- Counts come from `--features counters`. Cycles come from `--features profile`.
  **Never both.**
- Wall measurements are pinned (single core, High priority) and prefer CPU time;
  this box drifts ~15% and migrates across cores.
- No lever is refuted on one measurement. Three *varied* probes, and at least
  one at the level above the change.
