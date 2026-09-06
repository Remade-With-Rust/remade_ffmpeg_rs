# In-house FLAC encoder — brick ledger

An **in-house, lossless** FLAC encoder for `rff-codec-flac` (mirrors the
decoder→encoder path we did for MP3). Built one brick at a time; each brick is
independently revertible, ships with its reference test, and must pass the gate
before the next starts.

## Architecture (decided)

- Lives in `crates/rff-codec-flac/src/encode.rs`; `register()` flips
  `encoder: None → Some(FlacEncoder factory)`.
- The container muxer (`rff-format-flac`) is a **passthrough** (a FLAC "packet"
  is the whole stream), so **the encoder emits the complete native stream**:
  `fLaC` + STREAMINFO metadata block + all FRAMEs. Self-contained + testable
  without the muxer.
- **Integer, lossless.** Engine audio is `S16` (native 16-bit → lossless) or
  `F32`/`F32Planar` (quantized to an integer grid at a chosen depth — lossless
  *relative to that grid*, and truly lossless when the float came from ints).
  Internally the encoder works on `i32` samples per channel at `bits_per_sample`.
- **Buffer-then-emit** (v1): accumulate all samples, encode on `flush()`, emit
  the finished `.flac` as packet(s). Lets STREAMINFO carry real total-samples +
  min/max frame size + MD5. (Streaming/incremental is a later option.)

## The gate (every brick)

1. **Lossless round-trip (primary, in-tree):** encode → decode with `claxon`
   (the crate our decoder already uses) → integer samples **exactly equal** the
   input integers. Integer-exact `assert_eq!`.
2. **Spec-valid cross-decoder (self-tolerated ≠ legal):** real `flac -d` /
   `ffmpeg` decodes our output and matches. Proves it's a legal FLAC stream, not
   just one *we* can read. (Skipped-with-note if the tool isn't on PATH.)
3. **Compression ratio** is the quality metric — must not regress brick-to-brick.

## Bricks

| # | Brick | Adds | Gate | Status |
|---|---|---|---|---|
| 1 | **Framing + CONSTANT/VERBATIM** | BitWriter, CRC-8/16, STREAMINFO, frame/subframe headers, raw+constant subframes, registration | lossless round-trip + `flac -d`; size ≈ raw | ☑ |
| 2 | **FIXED predictors + Rice** | fixed orders 0–4 (pick best), Rice residual coding (single partition) | lossless + compresses (~50–60%) | ☑ |
| 3 | **Partitioned Rice** | 2^p partitions, per-partition Rice param, partition-order search | lossless + smaller than #2 | ☑ |
| 4 | **LPC** | windowed autocorrelation → Levinson-Durbin → quantized coeffs, order search, best-subframe-type select | lossless + best ratio (≈ libFLAC) | ☑ |
| 5 | **Stereo decorrelation** | independent / left-side / right-side / mid-side; pick smallest | lossless + smaller on stereo | ☑ |
| 6 | **Presets + MD5 + apodization** | `-compression_level`, block-size choice, Tukey window, MD5 in STREAMINFO (`flac -t`) | lossless across presets; `flac -t` passes | ☑ |
| 7 | **E2E + CLI + depths/channels** | 24-bit (F32@24), mono/stereo/multichannel, `wav→flac` transcode, external-tool validation | full transcode + `flac -t` + ffmpeg read | ☑ |

**✅ ALL 7 BRICKS COMPLETE** — a working, in-house, lossless FLAC encoder: framing +
CONSTANT/FIXED/LPC/VERBATIM subframe selection + partitioned Rice + stereo
decorrelation + `-compression_level` presets + MD5 integrity + 16/24-bit + up to 8
channels, wired end-to-end through the engine. ~14 tests green; validated against
claxon (independent decoder) and ffmpeg (reference) at every step.

**Baseline vs the reference** (`crates/rff/tests/flac_baseline.rs`, `#[ignore]`d,
`FFMPEG_BIN=… --ignored --nocapture`): on an 8 s stereo music-like signal, ours =
**795 793 B (56.4% of raw)** vs ffmpeg `-compression_level 8` (padding-stripped) =
**795 557 B (56.4%)** — **parity within 0.03%** (236 B on ~796 KB). A from-scratch
encoder matching libavcodec's maximum-compression FLAC. (Ratio is signal-dependent;
this is one point, not a corpus average — but a strong one.)

## Brick log (append before/after per brick)

- **Brick 1 — Framing + CONSTANT/VERBATIM. DONE.** Emits a complete self-contained
  native stream (`fLaC` + STREAMINFO + frames); subframes are CONSTANT (flat block)
  or VERBATIM (raw 16-bit samples). Gates: claxon round-trip integer-exact ✅;
  ffmpeg decode byte-identical to source PCM ✅. Compression: **none yet** (VERBATIM
  ≈ raw). Files: `src/encode.rs` (BitWriter MSB-first, CRC-8/16, UTF-8 frame#,
  STREAMINFO, frame/subframe framing), `src/lib.rs` (registered the encoder factory).
  A manual `emit_for_external_check` (`#[ignore]`) writes files for the ffmpeg gate.
- **Brick 2 — FIXED predictors + Rice. DONE.** Each subframe picks the cheapest of
  CONSTANT / FIXED order 0–4 (min estimated bits) / VERBATIM; the residual is
  Rice-coded (method 0, single partition, best `k` searched over 0–14). **Escape
  (k=15) is deliberately never emitted** — claxon returns `Unsupported` for it — and
  the subframe-type search falls back to VERBATIM when Rice would lose, so there's no
  correctness gap. Gates: lossless on sine+constant *and* high-entropy noise ✅;
  ffmpeg byte-identical ✅; **40 000 → 4 342 B (10.9%)** on the synthetic signal (real
  music ~50–60%). A noise guard confirms the VERBATIM fallback + no pathological
  blow-up. Note: `write_zeros` avoids the `>= 64` shift-overflow on long unary runs.
- **Brick 3 — Partitioned Rice. DONE.** The residual now splits into 2^p partitions
  (p searched 0..8, capped by the block-size factorization + a non-empty partition 0),
  each with its own exact-best Rice param. The FIXED order is chosen cheaply
  (single-partition cost), then that order is partition-optimized — **order 0 is always
  in the search, so brick 3 ≤ brick 2 by construction** (the sine stays 4342 B — no
  regression). Gates: lossless on sine / noise / varying-dynamics ✅; ffmpeg
  byte-identical ✅; **direct measurement — a loud/quiet block: 36 045 vs 37 131 bits
  (2.9% smaller), partition order 6 auto-chosen.** PERF NOTE: the search is exact
  per-order (O(po·k·n)); a later brick can merge finest-order partition sums
  (libFLAC-style) to ~O(n) if encode speed matters.
- **Brick 4 — LPC. DONE (the big one).** Tukey(0.5)-windowed autocorrelation →
  Levinson-Durbin (orders 1..12) → libFLAC-style coefficient quantization (14-bit
  precision, **non-negative** shift, rounding error feedback) → residual via the exact
  i64 decoder arithmetic → partitioned Rice. Order chosen from the Levinson residual
  energy. Each subframe now picks the cheapest of CONSTANT / LPC / FIXED / VERBATIM.
  Gates: lossless on every signal ✅; ffmpeg byte-identical ✅; on a mid-band AR(2)
  resonance **LPC order 2 beats FIXED (which fell back to order 0) by 7.9%**, and it
  auto-selects the correct order. **⚠ KEY BUG (classic): Levinson solves the AR model,
  so the predictor coefficients are the NEGATION of the raw `lpc[]` (libFLAC's
  `lp_coeff = -lpc`). Without it the predictor anti-correlates — it still round-trips
  losslessly but the residual is ~2× the signal (worse than FIXED). LESSON: a lossless
  round-trip does NOT validate prediction quality — only a size/ratio check does.**
  Deferred to brick 6: precision adaptation, apodization search, exhaustive order search.
- **Brick 5 — Stereo decorrelation. DONE.** For 2-channel blocks, analyzes L / R /
  mid / side (side at bps+1) and picks the cheapest of the four FLAC modes:
  independent(1) / left-side(8) / right-side(9) / mid-side(10). `side = L−R`,
  `mid = (L+R)>>1` — inverses of claxon's reconstruction (`R = L−side`, `L = side+R`,
  and `mid<<1 | (side&1)` then `(mid±side)>>1`). The side channel's extra bit rides on
  the channel-assignment code; the frame-header sample size stays `bps`. Refactored
  `write_subframe` → `analyze_subframe` (returns a costed `SubframeChoice`) +
  `write_subframe_from`, so all four candidates are costed before committing. Gates:
  all 8 tests lossless ✅ (incl. correlated stereo); ffmpeg byte-identical on a
  decorrelated stream ✅; **direct measurement: mode 9 chosen, 18.4% smaller than
  independent**; correctly declines to independent when a channel is constant
  (sine+const stays 4342 B).
- **Brick 6 — Presets + MD5 + apodization. DONE.** (1) **MD5 signature** — in-house
  streaming MD5 (RFC 1321, `src/md5.rs`, no dependency, validated against the RFC
  vectors) over the interleaved LE samples, written into STREAMINFO. Verified
  definitively: the STREAMINFO MD5 **equals `md5sum` of the raw PCM**, so `flac -t`
  integrity passes. (2) **`-compression_level 0..8`** via `configure()` → max LPC
  order (4 / 8 / 12), threaded through analyze / decide / try_lpc. (3) **Apodization
  search** — `try_lpc` now tries two Tukey windows (α = 0.5, 0.2) per block and keeps
  the smaller candidate. Gates: 11 tests lossless ✅ (incl. level-0-vs-8 monotonic);
  MD5 matches raw audio ✅; ffmpeg byte-identical ✅. Deferred to a perf pass:
  partition-sum merging + streaming MD5 (both O(n)).
- **Brick 7 — End-to-end + depths + channels. DONE.** (1) **Engine E2E** — new
  `crates/rff/tests/flac_roundtrip.rs` drives WAV → flac → WAV through the real
  `rff::transcode::run` pipeline (demux → decode → encode → mux) and asserts
  **bit-exact** recovery, so `rff -i in.wav out.flac` works for real. (2) **24-bit** —
  `F32` input now maps to a 24-bit grid (S16 stays 16-bit); ffmpeg reports
  `flac, 96000 Hz, mono, s32 (24 bit)` and claxon round-trips lossless. (3)
  **Multichannel** — up to 8 independent channels (3-ch tested lossless); >8 rejected.
  Gates: 13 crate tests + the engine round-trip ✅; ffmpeg accepts 16- and 24-bit ✅;
  MD5 valid ✅.
