# Changes since vendoring

Everything this fork does differently from upstream `jpeg-decoder` 0.3.2 and
`jpeg-encoder` 0.7.0. Keep this current — it is what makes re-syncing with
upstream possible.

## 0.1.7

The two items 0.1.6 deferred, both taken.

- **Per-coefficient magnitude lowering in the trellis.** Restricted to
  `|q| >= 2 -> |q| - 1`, which can never produce a zero — that restriction is
  what keeps every decision independent, since the set of non-zero positions is
  unchanged and no run-length moves. It matters most where EOB truncation was
  WORST: mean BD-rate **-1.88% -> -2.51%**, and the worst content (white noise)
  improves from **+3.83% to +1.40%**.

  | content   | EOB only | EOB+mag | mag alone |
  |-----------|---------:|--------:|----------:|
  | photo     |   -5.02% |  -5.70% |    -0.71% |
  | diagonal  |   -1.25% |  -0.58% |    +0.67% |
  | text      |   -0.49% |  -1.24% |    -0.75% |
  | noise     |   +3.83% |  +1.40% |    -2.39% |
  | gradient  |   -6.44% |  -6.41% |    +0.03% |

  A density-adaptive lambda was then built and **refuted in both directions**
  (tapering down: noise +1.40% -> +2.67%; tapering up: +12.07%). Flat lambda
  wins, so block density is not the axis that explains the noise loss. Removed
  rather than left switchable.

- **NEON forward DCT.** Not a transcription of the AVX2 kernel. The butterfly is
  written once over a five-method `Lanes` trait with two backends: a scalar
  model verified **bit-exactly on x86** against the reference DCT, and a NEON
  mapping of the same trait. The transform is verified on the machine it was
  written on; only fifteen one-line intrinsic bodies are certified by ARM CI.
  Ships with `RUSTY_JPEG_NEON_FDCT=0` as a runtime fallback.

The RD corpus grew from 3 content types to 6 (adds text, noise, gradient),
because a sign flip across two clips cannot support a dispatch rule.

## 0.1.6

Five codec improvements. The two bitstream-changing ones are gated on a corpus
BD-rate over 5 quality points and 3 content types, plus an ffmpeg round-trip.

- **Chroma is box-averaged when downsampling, not decimated.** `get_block` took
  only the top-left sample of each subsampling box and discarded the other
  three, aliasing every chroma frequency above the subsampled Nyquist into the
  baseband. The tell: on saturated chroma edges, PSNR sat FLAT at ~14.1 dB from
  quality 50 to 95 while the file grew 45 KB -> 83 KB. Error that does not
  respond to bitrate is not quantization error. **BD-rate -17.12% on chroma
  detail, -0.43% on photographic content, +2.30 dB on chroma edges** — better
  quality AND fewer bits, no content regressing.

- **Trellis quantization**, on by default. Chooses where each block's EOB falls
  using real Huffman code lengths and real squared error, instead of keeping
  every coefficient rounding produced. Lambda swept against the harness rather
  than asserted: **-3.14% BD-rate for +3.1% encode time**.

- **Interleaved scans with optimized Huffman tables.** Which encode route ran
  was decided purely on an internal memory budget, so `-optimize_huffman`
  emitted one scan PER COMPONENT at every practical resolution — a layout
  mainstream encoders never produce. Now always interleaved, without paying the
  streaming route's ~23% cost.

- **NEON quantize kernel for aarch64.** The encoder was AVX2-only, so ARM ran the
  scalar path and the crate's speed claim was silently x86-only. The forward DCT
  remains scalar on ARM — deliberately, rather than shipping a 500-line kernel
  that has never executed.

- **Fuzzing.** A `fuzz/` crate with three cargo-fuzz targets, plus deterministic
  robustness tests that run on stable in ordinary CI. Baseline: 80,000 malformed
  inputs, 38,192 decoded, 41,808 cleanly rejected, **zero panics**.

## 0.1.5

Housekeeping only, no behaviour change: drops the `Worker::fused_block` trait
method, which became dead when 0.1.4 started calling the concrete worker
directly. Internal — `Worker` is not public API. Kept as its own version so the
repository and the registry describe the same code.

## 0.1.4

- **The fused transform is reached through a concrete, inlinable call.** It was
  dispatched through `&mut dyn Worker` once per block — ~49k indirect calls per
  1080p frame, and an opaque boundary that stopped the DC-only test, the
  pair-holding logic and the IDCT dispatch from inlining into the decode loop.
  `Worker::as_immediate()` now resolves the synchronous worker **once per MCU**,
  so all six blocks of a 4:2:0 MCU reach the transform statically.

  Worth **5.4%** of whole-frame decode (18,296.9 → 17,312.5 ms over 3000 frames),
  the largest single decode win in this series. Output byte-identical.

Decode now measures **~1.04× faster than FFmpeg 8.1.2** (paired ABBA, N=15,
3000-frame arms, median 1.0390, byte-identical bitstream on both arms).

## 0.1.3

Decode plumbing. Output is byte-identical throughout — verified by whole-image
checksums on six fixtures (interleaved, non-interleaved, 4K, progressive, and
the multithreaded worker) plus the full suite.

- **Entropy decode is fused into the IDCT.** A baseline interleaved scan no
  longer accumulates a whole MCU row of coefficients before transforming it: each
  block is inverse-transformed the moment it is decoded, straight into the output
  plane, with one block held back so horizontally adjacent pairs still feed the
  two-block AVX2 kernel.

  The row buffer existed only so a row could be shipped to another *thread*. For
  a worker that consumes blocks synchronously it cost a full write → zero → read
  round trip of ~18.8 MB per 1080p frame, through a ~60 KB-per-row buffer too
  large for L1. The stage counters for that path now read **0 calls**.

  Progressive scans (which revisit blocks across scans) and non-interleaved
  streams (which index the buffer by position within a batch) keep the original
  path, and are verified by counter to fall back correctly.
- **The unused row buffer is no longer allocated** on a fused scan.
- No `Arc` refcount traffic on the per-block path: the quantization table is
  reached by a split borrow rather than a clone, which would otherwise put an
  atomic RMW on a path that runs ~49k times per 1080p frame.

Measured against FFmpeg 8.1.2, one pinned core, CPU time, 3000-frame arms at
matched work: **decode ~1.5–3% faster** (single-instrument ratio 1.032; paired
N=15 median 1.0148, inside noise — the effect is real but small enough that it
sits near this machine's resolution). Encode is unchanged at **1.19× faster** at
matched output size.

## 0.1.2

Performance and one real API defect. All changes gated on **unchanged decoded
output** (whole-image checksums on six fixtures) and the full suite.

- **AVX2 two-block IDCT.** The SSSE3 kernel widened to 256 bits, transforming
  two 8×8 blocks per instruction stream — block A in the low 128-bit lane, block
  B in the high one. Every operation involved is lane-independent (the
  arithmetic is elementwise; `unpack`/`packus` never cross the lane boundary, so
  the 8×8 transpose transposes each block separately), which makes the output
  **byte-identical to running the SSSE3 kernel twice** rather than merely close.
  A 64-round oracle test asserts exactly that, including a saturation round at
  `i16::MAX`/`i16::MIN`/`u16::MAX`. Worth **7.8%** of whole-frame decode; covers
  98.5% of blocks needing a full transform (the rest are DC-only and take a
  cheaper fill).
- **`set_single_threaded` actually works now.** It was a public API that did
  **nothing** on baseline JPEGs: the scan-loop call site hardcoded the
  multithreaded worker, and because the worker is cached on first use that site
  won. Nothing caught it because output is identical either way — only cost
  differs, and it differed a lot. The synchronous worker uses **~38% less CPU**,
  and enabling it also made `reclaim_buffer` live, so MCU-row coefficient
  buffers are recycled instead of reallocating ~6.3 MB per 1080p frame
  (`DecMcuRowAlloc` 5.82% → 1.57%). Regression-gated by
  `single_threaded_flag_selects_the_immediate_worker_on_baseline`, with the
  choice observable via `decode::last_worker_was_immediate()`.
- **DC-only fill split out** (`fill_dc_only`) so a caller that has already
  established a block has no AC energy takes the shortcut without rescanning all
  63 coefficients.
- Loop-invariant hoists on the fast-AC path (the `ac_lut` `Option` discriminant
  and `ac_table.unwrap()`), kept for being strictly less work — the speed effect
  measured inside the noise floor.
- New `counters` feature, split from `profile`: deterministic event counters
  (symbols, refills, LUT hits/misses, IDCT pairs). Separate on purpose —
  enabling both at once perturbed cycle counts 3×.

Measured against FFmpeg 8.1.2, one pinned core, CPU time, matched output size:
**encode 1.19× faster**, **decode at parity** (paired ABBA, N=21, inside noise).

## Vendoring (brick 1)

Mechanical only. **Gate: byte-identical encode and decode output vs the
pre-vendor build, verified on 4K and 8K frames.**

- `jpeg-decoder/src/` → `src/decode/`, `jpeg-encoder/src/` → `src/encode/`;
  each upstream `lib.rs` became the module's `mod.rs`.
- Intra-crate paths re-rooted (`crate::x` → `crate::decode::x` / `crate::encode::x`).
- Crate-level attributes moved to `src/lib.rs`. The encoder's `#![no_std]` was
  dropped (the merged crate is `std`); its `forbid(unsafe_code)`-unless-`simd`
  and the decoder's `deny(unsafe_code)` are preserved as module-level
  attributes, so the unsafe surface is unchanged.
- Upstream crate-level doc comments replaced with module docs (their doctests
  referenced test fixtures that were not vendored).
- The encoder's round-trip tests used `jpeg-decoder` as a dev-dependency; they
  now use `crate::decode`, making the round-trip oracle in-crate and permanent.
- Features renamed into one namespace. **`simd` is now ON by default** — this
  is the one behavioural change, and it is why encode output is byte-identical
  to the *simd-enabled* pre-vendor build rather than the workspace's previous
  scalar default.

## Planar Y'CbCr encode input (brick 2)

Added [`encode::PlanarYcbcrImage`] — an `ImageBuffer` over planar `yuv420p` /
`yuv422p` / `yuv444p` planes with per-plane strides.

JPEG *is* a Y'CbCr codec, so a planar video frame already sits in the right
colour space at the right chroma resolution. Encoding it via RGB, as callers
previously had to, converted colour twice and resampled chroma up and then back
down. Measured on a 4K frame: **488 ms → 245 ms** (2.0x) and the chroma
round-trip loss gone.

It is exactly lossless, and the reason is worth writing down: the encoder
subsamples in `get_block`, which **point-samples** — it takes every Nth sample
and does not average. So replicating each chroma sample `h`x`v` times in
`fill_buffers` is precisely undone. The one condition is that the encoder's
`SamplingFactor` matches the source layout, which is what
`PlanarYcbcrImage::sampling_factor()` returns.

> Note for future quality work: that point-sampling is also a real defect when
> the encoder genuinely has to downsample (a 4:4:4 source asked to emit 4:2:0).
> Dropping 3 of every 4 chroma samples without a low-pass aliases; libjpeg
> averages. Fixing it is a candidate for the chroma RD gap, but it changes
> output, so it needs the corpus gate.

Decode still returns interleaved RGB: the decoder runs its chroma upsampler
before colour conversion, so a planar decode path needs a tap ahead of the
upsampler. Not yet done.
