# rusty_mp3 — development history

Release-by-release engineering notes, moved out of the README at 1.0.0 so the
README can describe the crate as it is. The changelog lists changes per
release; this file keeps the measurements and the reasoning behind them.

## 0.9.0

**Decoder conformance.** Five defects, found by running the official ISO
vectors. Our FFmpeg-matched corpus could not reach them because LAME never
emits the features involved:

- **Intensity stereo**, both forms: MPEG-1 (tan law, illegal position 7) and
  MPEG-2 (the 32-entry tables, illegal positions, per-window bounds on short
  blocks, M/S only below the bound). Fixes `l3-he_mode`, `l3-test45` and
  `l3-test46`.
- **Mixed blocks**, two defects: the long part used the wrong band limit on LSF
  streams, and the IMDCT applied the short window to the two long subbands.
- **count1 overread**: a final quad that runs past `part2_3_length` is now
  dropped, as FFmpeg does, unless it fills the granule to line 576.

ISO gate: **16/16** on x86_64 and on aarch64, on both the SIMD and the scalar
paths. Decode stays bit-exact with FFmpeg on all 678 corpus streams.

**MPEG-2 intensity stereo vs FFmpeg.** On positions that are illegal for their
band, or in the top band, the ISO reference PCM (`l3-test45/46`) and FFmpeg
disagree. We match the reference. This is the one place our output differs from
FFmpeg's.

**Short-block noise shaping (encoder).** Short blocks used to quantize every
(band, window) at one gain with flat scalefactors. They are now shaped against
the long-block thresholds mapped onto the short grid: **+0.0052 ODG mean, 31
better / 5 worse / 8 tied (sign z = +4.3)**, largest on mixed speech/music
(+0.028). The speed cost is below measurement resolution.
`MP3_SHORT_SHAPE=0` reproduces the pre-shaping encoder byte for byte.

**SIMD on every arch we ship.**

| change | effect |
| ------ | ------ |
| NEON twins of the three decode kernels (IMDCT-36, matrixing, windowing) | aarch64 was scalar; on x86 the same kernels are worth **1.60×** decode |
| one `#[target_feature]` entry per granule, so the decode kernels inline | 68 → 2 feature crossings per granule-channel |
| circular analysis FIFO + AVX/NEON analysis filterbank | encode **1.18–1.25×** |
| AVX/NEON quantizer inner loop | encode **1.08–1.10×** |

Every kernel is **bit-identical** to its scalar twin (separate mul+add, never
FMA). Each encoder change was byte-identical to its predecessor on 72/72 corpus
encodes, and decode hashes match across arms on 720/720 streams. NEON was
verified under qemu-aarch64; speed on real ARM hardware has not been measured.
`MP3_ISA=scalar` forces the scalar path.

## Encoder quality (0.8.0)

PEAQ ODG against LAME at matched bitrate, per clip and per rate, on three real
CC0/PD music clips (24 s, 44.1 kHz mono). Positive = LAME ahead. Per clip,
because a mean hides the thing that matters:

| gap to LAME | 96k | 128k | 160k | 192k |
| ----------- | --- | ---- | ---- | ---- |
| guitar | +0.054 | +0.020 | **−0.034** | +0.030 |
| piano | +0.564 | +0.322 | +0.134 | +0.096 |
| vocal | +0.295 | +0.334 | +0.174 | +0.125 |
| **mean** | **+0.304** | **+0.225** | **+0.091** | **+0.084** |

ODG runs 0 (imperceptible) to −4 (very annoying). The gap narrows with bitrate,
and on guitar at 160 kbps we are ahead. Regenerate with:

```sh
python tools/quality/ladder.py --arms slack,lame --rates 96,128,160,192 --dirs corpus --only corp_long
```

**What changed.** The per-band distortion loop had never shaped a single
band, on any content: it tested quantization noise (MDCT domain) against masking
thresholds (unnormalized 1024-point FFT power), two scales that differ by ~49 dB.
Every band therefore read as masked — 98.7% of them by more than six decades — so
the loop exited on its first iteration in 100% of granules and the encoder shipped
one global gain per granule where LAME shapes 71–77% of them. With the comparison
corrected the encoder now shapes 67–71% of granules, and it also stopped throwing
away the 63–89 bits per granule that a 1.5 dB gain step cannot place (LAME wastes
5–7). Measured across 8 content classes at 4 bitrates that is +0.045 ODG, 29/32
points better; on the real music above, +0.020.

*Earlier revisions quoted 0.72 ODG at 192k and 1.08 at 128k on a different
three-clip corpus that included a synthetic transient clip, and attributed the
loss to an inert distortion loop. The loop was not inert by design — it was
comparing two different units. Those figures are not comparable to the table above
because the corpus differs; the regeneration command is given so this one is.*

**Still open:** the psychoacoustic model produces no short-block thresholds.
Since 0.9.0 short blocks are shaped, but against long-block thresholds mapped onto
the short grid rather than a real short-block masking model. Per-band shaping is
MPEG-1 only. Full evidence,
per-class tables and the measurement method are in
[`docs/plans/mp3-gate-ledger.md`](https://github.com/Remade-With-Rust/remade_ffmpeg_rs/blob/main/docs/plans/mp3-gate-ledger.md).

## Performance notes (0.4 – 0.5)

Measured on a real 6:53 stereo 44.1 kHz music track (412.9 s, 15,806 frames) at
CBR 192 kbps.

**0.4.0** caches the psychoacoustic model's FFT twiddle factors — they depend
only on the transform size, but were rebuilt on every call, which cost ~1.26 M
`cos`/`sin` evaluations across the track to produce ten distinct values — and
reuses the mid/side scratch across frames instead of reallocating it:

|                                     | before   | after        |
| ----------------------------------- | -------- | ------------ |
| encode CPU (median of 41 pairs)     | 7,047 ms | **6,766 ms** |
| allocations per frame               | 32.06    | **21.06**    |
| zero-filled allocations / 800 frames | 8,000    | **2**        |

**1.045× faster encode**, 33/41 paired wins, z = 3.90. The output is
byte-identical across the change (same md5 over the full track), so both arms
are provably doing the same work rather than one of them doing less. Decode is
untouched at 5.04 allocations per frame — its per-block path was already
allocation-free.

Method: pinned to one core at High priority, CPU time rather than wall,
arms ABBA-interleaved, 41 pairs, with a null arm (the same binary against
itself) reading 1.017 as the session's resolution floor. This is a
same-binary-family delta, not a cross-implementation ratio.

The allocation counts are reproducible with the bundled instrument, which
counts through whichever global allocator the binary sets:

```sh
cargo run -p rusty_mp3 --release --example allocaudit -- 800 192
```

### VBR correctness — 0.5.0

**If you use `-q:a` / `vbr_quality`, upgrade.** Every release up to and
including 0.4.1 produced VBR streams that FFmpeg rejects (`invalid new backstep
-1`) and that decode to noise. Three stacked defects:

- the masking thresholds (FFT power domain) were compared directly against
  quantization noise (MDCT domain), scales ~10⁴ apart — so the gain search
  saturated at the coarsest setting for 97.5% of granules and every quality
  setting produced the same ~39 kbps;
- the quality scale was inverted (NMR ≥ 1 means noise *at or above* the masking
  threshold, so even the best setting asked for audible noise);
- with those fixed, quality could demand more bits than the largest legal frame
  holds, and the overflow corrupted the bit-reservoir back-pointer.

Measured on 60 s of real guitar, ours at each `-q:a`:

| `-q:a` | kbps | SNR | FFmpeg decode |
| ------ | ---- | --- | ------------- |
| 0 | 315.1 | 54.47 dB | clean |
| 4 | 300.3 | 47.34 dB | clean |
| 9 | 143.8 | 9.01 dB | clean |

CBR is unaffected and byte-identical across the change.

**VBR quality — fixed in 0.6.0.** Through 0.5.1 the VBR path ran its own
noise-to-mask gain search, and PEAQ measured it **3.5 ODG behind LAME** at
matched bitrate — worse at 268 kbps than the CBR path managed at 192, because
the criterion was dimensionless but unanchored. `-q:a` is now a target average
bitrate that drives the **same two-loop quantizer CBR uses**.

PEAQ ODG at matched *actual* bitrate (matching `-q:a` between encoders does not
match the rate, so it proves nothing):

| point | ours | LAME | gap |
| ----- | ---- | ---- | --- |
| 192 kbps | −0.44 | +0.01 | 0.45 |
| 128 kbps | −1.36 | −0.90 | 0.46 |
| 80 kbps | −3.07 | −2.77 | 0.30 |
| *(0.5.1, 200 kbps)* | *−3.50* | *+0.01* | *3.51* |

So VBR now sits in the same 0.3–0.5 ODG band as CBR rather than three ODG
adrift, and `-q:a` lands where users expect (q=0 ≈ 245 kbps, q=5 ≈ 130,
q=9 ≈ 65). Closing the remaining ~0.4 ODG is ordinary encoder tuning and
applies to CBR and VBR alike.

Short blocks are covered by the same budget, so they no longer need a separate
path.

### Decode — vs FFmpeg, at matched CPU

Both sides decode the same 27.5-minute stream and **discard the output**, pinned
to the same physical cores, wall clock, arms alternated, 15 pairs:

| cores each | rusty_mp3 | FFmpeg | result |
| ---------- | --------- | ------ | ------ |
| 1 physical | 1,952 ms | 2,431 ms | **1.24× faster** (15/15, z = −3.87) |
| 2 physical | **1,017 ms** | 1,455 ms | **1.39× faster** (15/15, z = −3.87) |
| *2, our serial build (control)* | *1,807 ms* | *1,445 ms* | *1.30× slower* |

We also use **less total CPU**: 1,875 ms against FFmpeg's 2,125 ms on one core.
The win is not bought with extra work.

The control row is the one that makes the two-core number trustworthy: our
*serial* build on the same two cores reads `cpu/wall` 0.95 — it cannot use the
second core and loses. Only the pipelined build converts the budget, at
`cpu/wall` 1.78. So the gain is real concurrency, not scheduling luck.

Three asymmetries had to be removed before any of this was visible, and one was
ours: FFmpeg's CLI uses ~2 cores even with `-threads 1` (`cpu/wall` 1.96); our
CLI wrote 582 MB while `-f null -` writes nothing; and our own profiler hashed
every sample, 17% of its own runtime, work FFmpeg never did. A CLI-to-CLI
comparison with those in place read 1.20× *behind* — it was measuring the output
path, not the codec.

### Decode — 0.5.0 (SIMD)

Two AVX kernels in the synthesis filterbank, both **bit-identical** to their
scalar twins (each output owns a lane and accumulates in the original order;
separate mul+add, never FMA). Runtime-detected, with the scalar paths kept as
oracles and as the fallback.

| kernel | share of the win |
| ------ | ---------------- |
| matrixing (`matrixing_avx`) | **1.162×** whole decode, 31/31 pairs, z = 5.57 |
| windowing (`window_avx`) | 1.019×, 23/31 pairs, z = 2.69 |

The gap between those two is the useful part: same stage, same instruction set,
same effort, 16.2% versus 1.9%. Auto-vectorization had produced **0 packed ops
against 1698 scalar ones** in this kernel, but "the stage is hot" was still not
enough to aim a kernel — it took the split *within* the stage.

### Decode — 0.4.1

Three structural changes, measured on a real 27.5-minute stereo stream encoded
by LAME (a decoder benchmarked on its own encoder's output skips paths that
encoder never emits, so provenance matters):

| brick | change | effect |
| ----- | ------ | ------ |
| bit reader | `peek(n)` loaded eight bytes and shifted, instead of looping once per bit | huffman 954 → 706 ms |
| synthesis | V FIFO addressed circularly instead of shifted (a 960-float memmove per pass, ~17.5 GB per track), plus a transposed window loop so the operands are contiguous | synthesis 1008 → 857 ms |
| IMDCT | exact kernel symmetry — half the dot products are derived, 648 → 324 MACs per subband | imdct share 31.9% → 19.9% |

**1.185× faster decode overall**, 39/41 paired wins, z = 5.78, against a null
arm of 1.006. Measured directly rather than by chaining the per-brick ratios,
which would have overstated it as 1.233×.

All three are **bit-identical**, not merely close. The IMDCT one is the
surprise: halving the work costs no precision because the symmetries
(`cos36[17−n][k] == −cos36[n][k]`, `cos36[53−n][k] == +cos36[n][k]`, and the
same pair in the 12-point short-block kernel) hold *exactly* in the stored f32
tables, and IEEE multiplication and round-to-nearest are sign-symmetric — so a
mirrored sum is exactly the negation of the computed one.

Verified byte-identical over a 15-stream corpus spanning joint/true-L-R/mono,
MPEG-1 (44.1/48/32 kHz), MPEG-2 (22.05/24 kHz — the 576-sample granule path),
MPEG-2.5 (11.025 kHz), 128–320 kbps CBR plus VBR, and four content classes.
Short blocks are 15–37% of granules there; **mixed** blocks are 0% on every
stream because LAME never emits them, so that path is gated separately against
a dense reference implementation instead of being assumed covered.

```sh
cargo run -p rusty_mp3 --release --example decprof -- input.mp3
```

## WebAssembly

Both halves run on wasm, and **bit-exactly**: the same bitstream produces the same
samples and the same encoded bytes as a native build.

| target | decode | encode | notes |
| ------ | ------ | ------ | ----- |
| `wasm32-wasip1` | ✅ bit-exact | ✅ bit-exact | has a clock and threads; the stage profiler and the pipelined decoder both work |
| `wasm32-unknown-unknown` | ✅ bit-exact | ✅ bit-exact | the browser: no clock, no threads (see below) |

There is no `Instant` and no thread on `wasm32-unknown-unknown`. The stage
profiler compiles out there rather than trapping, and `decode_pipelined` degrades
to the serial path — it is a speed optimisation whose output is identical either
way, so a browser caller gets the same samples, just on one thread. Nothing needs
a feature flag and the crate has no wasm-specific dependencies; the module
requires **zero host imports**.

Verify it yourself — the check compares hashes across host, wasmtime and node:

```sh
bash tools/bench/wasm_check.sh
```

*Earlier releases compiled for `wasm32-unknown-unknown` and trapped on the first
decoded frame, because the profiler called `Instant::now()` on every stage. A
build check does not catch that; the gate above runs the codec on the target and
compares output.*
