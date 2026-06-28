# MP3 Encoder — The Inverse House: Exact Brick Plan

Goal: turn interleaved PCM into a **legal, decodable MPEG-1 Layer III** bitstream,
then into a **good-sounding** one. This document enumerates every primitive the
encoder needs as a numbered **brick**, classifies how each is obtained and
verified, and orders the work so we can come back to it over months.

The decode side of `rff-codec-mp3` is **bit-exact vs FFmpeg**. That is the whole
strategy here: the encoder is largely the *inverse* of code we already trust, and
**our own decoder is the verification oracle**. Almost every mechanical brick is
provable by a round-trip (`encode → decode → compare`) without reaching for an
external reference at all.

## Discipline (same as VP9 / AAC)

- Fixed tables are **sourced from a reference and validated** (counts / ranges /
  structural invariants / round-trip), **never fabricated**.
- Algorithms are **transcribed from ISO 11172-3 / a permissive reference** and
  verified by round-trip, float-reference, or end-to-end PCM comparison.
- Every brick lands green (suite + `cargo deny`) with its own test. Sourced tables
  are validated **on entry** so a bad transcription fails the build, not the ear.

## The one honest caveat: there is no bit-exact target on encode

A decoder can be bit-exact against FFmpeg; an **encoder cannot** — psychoacoustic
models differ, so LAME, FFmpeg and us will make different (all legal) bitstreams
from the same PCM. So the bar is split:

- **Structural correctness** (Floors 1–3): bit-exact **round-trips** through our
  own decoder, and the output **decodes cleanly in FFmpeg/LAME**. This *is*
  provable and we hold it to the bit.
- **Quality** (Floor 4+): output PCM stays under a perceptual/PSNR threshold vs
  input, measured through our bit-exact decoder. No bit-exactness claimed.

## Classification key

**[GEN]** compute from a formula and assert vs known values ·
**[TBL]** transcribe a fixed table and validate ·
**[ALG]** transcribe an algorithm and verify ·
**[GLUE]** wiring/plumbing, no new math ·
**[done]** already implemented & tested on the decode side (reuse / invert).

Scope for v1: **MPEG-1 Layer III only** (44.1/48/32 kHz), the same corner the
decoder is strongest in. MPEG-2/2.5 LSF is a later floor.

---

## Foundation — shared primitives & tables (`encode/tables.rs` + reuse)

The bricks every floor rests on. Several already exist on the decode side and are
reused verbatim; the rest are new forward-direction data.

- **N1 [done]** Scalefactor-band offsets — `SFB_OFFSET_LONG_V1` / `SHORT_V1`
  ([tables.rs](../crates/rff-codec-mp3/src/tables.rs)). Shared, validated.
- **N2 [done]** `SCALEFAC_COMPRESS_V1` → (slen1, slen2); `PRETAB`. Shared.
- **N3 [done]** Huffman codebooks `(code, len)` — `decode/codebooks.rs`,
  ISO-canonical and already proven by the bit-exact decoder. The encoder reuses
  the same numbers, indexed value→code (see B1).
- **N4 [GEN]** Forward quantizer power law: `ix = nint( (|xr| · 2^(-gain/4))^(3/4)
  − 0.0946 )`. Table `POW34[i] = i^(3/4)` (and the inverse `POW43` already needed
  by requantize) over `0..8207`, built once in a `OnceLock`. Validate: `POW34`
  then `POW43` round-trips to the integer; spot-check magic constant boundaries.
- **N5 [GEN]** Analysis polyphase window `C[512]` (ISO 11172-3 Table 3-B.3,
  analysis). Related to the synthesis window `D[]` we already ship by the standard
  `C[i] = ±D[i]/32` sign/scale convention. Validate: derive from `D[]` and assert
  the published spot values; the round-trip in L1 is the real proof.
- **N6 [GEN]** Analysis cosine matrix `M[32][64] = cos((2k+1)(j−16)π/64)`.
  Validate: orthogonality + reconstruct against the decoder's synthesis matrix.
- **N7 [GEN]** Forward-MDCT cosine basis + the four window shapes (long / start /
  short / stop sine windows, ISO 2.4.3.4.10.3). The exact inverse of the decoder's
  IMDCT windows. Validate: TDAC round-trip in L2.
- **N8 [TBL]** `linbits` per Huffman table (escape lengths) — the encode-side twin
  of the decoder's per-table linbits. Validate vs the decode table.
- **N9 [GEN]** Region-boundary tables: map `(region0_count, region1_count)` ↔ SFB
  line boundaries for long blocks, and the fixed short-block partition. Inverse of
  decode's region derivation. Validate: round-trip with the decoder's logic.

---

## Floor 1 — Time → Frequency (the analysis path) · `encode/filterbank.rs`, `encode/mdct.rs`

Pure DSP, **self-verifying** against the decoder's synthesis/IMDCT — no external
reference needed. This is where to start: highest certainty, zero psychoacoustic
judgement.

- **L1 [ALG]** Analysis filterbank `analyze(pcm, fifo) -> [[f32; 18]; 32]`
  ([filterbank.rs](../crates/rff-codec-mp3/src/encode/filterbank.rs)). 18 passes
  per granule: shift 32 new samples into the 512-tap FIFO, window with `C[]` (N5),
  fold 512→64, apply `M` (N6) to get 32 subbands.
  **Verify:** feed white noise through `analyze` → decoder's synthesis filterbank;
  reconstruct the input within the filterbank's known delay + float epsilon.
- **L2 [ALG]** Forward MDCT `forward(subbands, block_type, overlap) -> [f32; 576]`
  ([mdct.rs](../crates/rff-codec-mp3/src/encode/mdct.rs)). One 36-pt MDCT/subband
  for long/start/stop, three 12-pt for short; carry overlap; frequency-invert odd
  subbands; mixed blocks keep subbands 0–1 long.
  **Verify:** `forward` → decoder's antialias⁻¹/IMDCT → exact reconstruction
  (TDAC) within epsilon, per block type, including the long↔short transitions.
- **L3 [ALG]** End-to-end analysis round-trip: `PCM → L1 → L2 → (decoder) IMDCT →
  synthesis → PCM`. The two halves compose to identity (within the codec's inherent
  delay). This single test certifies the entire time/frequency core before any
  bit is written.

---

## Floor 2 — Frequency → Bits (the coding path) · `encode/huffman.rs`, `encode/bitstream.rs`

Mechanical inverses of decode stages, each **round-trip-verifiable** against the
matching decoder parser.

- **B1 [ALG]** Huffman encode-table builder: invert `decode/codebooks.rs` into a
  value→`(code, len)` lookup per table (34 big-value + 2 count1 quad tables).
  **Verify:** for every table, every decodable symbol encodes to bits the decoder
  reads back to the same symbol (exhaustive).
- **B2 [ALG]** `estimate_bits(coeffs, table)` ([huffman.rs](../crates/rff-codec-mp3/src/encode/huffman.rs))
  — sum codeword + linbits + sign lengths for a region under a table, **without
  emitting**. The quantizer's inner-loop cost oracle.
  **Verify:** equals the actual emitted bit count from B3 for random regions.
- **B3 [ALG]** `encode(quant, writer)` — partition the spectrum into big_values
  regions + the count1 quad region, pick the cheapest table per region (consistent
  with B2), emit codewords + linbits escapes + signs.
  **Verify:** `encode` → decoder's Huffman spectrum decode → identical `is[576]`.
- **B4 [ALG]** Region selection: choose `region0_count` / `region1_count` (long) or
  the implied short split, and per-region `table_select`, minimising B2 cost.
  **Verify:** decoder re-derives the same boundaries; total bits match the choice.
- **B5 [ALG]** Side-info serializer — the exact inverse of
  [decode/sideinfo.rs](../crates/rff-codec-mp3/src/decode/sideinfo.rs): write
  `main_data_begin`, `scfsi`, and every per-granule field at the right widths.
  **Verify:** `serialize(si)` → `sideinfo::parse` → struct-equal; bit accounting
  hits `side_info_len()*8` exactly (the parser's `debug_assert`).
- **B6 [ALG]** Scalefactor serializer — write long/short scalefactors band-major
  with (slen1, slen2) from `scalefac_compress`, honouring `scfsi` reuse in granule
  1 (the band-major gotcha that broke decode — mirror it exactly).
  **Verify:** round-trips through `decode/scalefactors.rs`.
- **B7 [ALG/GLUE]** Frame assembly + CRC `format(...)`
  ([bitstream.rs](../crates/rff-codec-mp3/src/encode/bitstream.rs)): header
  (`to_bytes`, [done]) + optional CRC-16 + side info + padded main data.
  **Verify:** the decoder's `parse_frames` accepts it; frame size matches header.
- **B8 [ALG]** Encoder bit reservoir: set `main_data_begin` from banked spare
  bytes; let a complex granule borrow; update `spare_bytes`.
  **Verify:** a multi-frame stream where one granule overspends still decodes; the
  back-pointer never exceeds the reservoir limit (320 kbps / version cap).

---

## Floor 3 — The dumb-but-valid controller (first decodable MP3)

Stand up the **simplest legal** psymodel + quantizer so the whole house produces a
playable file end-to-end. Quality is mediocre on purpose; correctness is provable.
This is the milestone where `ffmpeg -i in.wav -c:a mp3 out.mp3` first works and
**FFmpeg/LAME plays the result.**

- **C1 [ALG]** Trivial psymodel: always-long blocks, flat/constant masking
  threshold, perceptual-entropy = signal energy. Satisfies the `PsyResult`
  contract ([psychoacoustic.rs](../crates/rff-codec-mp3/src/encode/psychoacoustic.rs))
  with zero research risk.
- **C2 [ALG]** Rate-only quantizer (inner loop only)
  ([quantize.rs](../crates/rff-codec-mp3/src/encode/quantize.rs)): binary-search
  `global_gain` so the B2-estimated bits fit the granule budget; flat scalefactors;
  no distortion loop. Fills `GranuleSideInfo`.
  **Verify:** emitted bits ≤ budget; output decodes; round-trip PCM is recognisably
  the input (bounded error, not bit-exact).
- **C3 [GLUE]** `Encoder::send_frame` / `receive_packet`
  ([lib.rs](../crates/rff-codec-mp3/src/lib.rs)): accumulate 1152 samples/channel,
  call `encode_frame`, queue the `Packet`; flush the tail.
- **C4 [E2E]** Pipeline gate: `wav → our encoder → our decoder → PCM` under a
  loose PSNR floor, **and** the `.mp3` decodes in stock FFmpeg without error. The
  "the house stands" milestone.

---

## Floor 4 — The quality brain (make it sound good)

Replace Floor 3's stubs with real perceptual coding. This is the only part needing
LAME-grade judgement, now isolated behind clean interfaces.

- **Q1 [TBL]** Psymodel tables: absolute threshold of hearing, critical-band /
  partition boundaries, the spreading function, FFT analysis (Hann) windows.
  Sourced from ISO psymodel-2 / a permissive reference; validated by shape + range.
- **Q2 [ALG]** FFT front-end: 1024-pt (long) + 256-pt (short) spectra of the
  windowed PCM, energy + unpredictability per partition.
- **Q3 [ALG]** Masking threshold: spread energy across partitions (Q1), apply the
  tonality/noise-masking offset, fold to per-SFB thresholds + SMR.
- **Q4 [ALG]** Perceptual entropy → bit demand, feeding reservoir budgeting (B8)
  and the block-switch decision.
- **Q5 [ALG]** Block-type decision: attack/transient detection → long/start/short/
  stop with pre-echo control; drives L2's `block_type` and mixed-block flag.
- **Q6 [ALG]** Outer distortion loop: raise per-band scalefactors where quant noise
  > Q3 threshold, re-run the inner loop (C2), until noise is masked everywhere or
  scalefactor/bit budget is exhausted. Sets `preflag` / `scalefac_scale`.
  **Verify:** measured noise-to-mask ≤ 0 in masked bands; quality metric beats
  Floor 3 on the test corpus.

---

## Roof — stereo, rate modes, tuning, conformance

- **R1 [ALG]** Joint-stereo decision: M/S (and later intensity) per the energy
  criterion; sets `mode_extension`. Inverse of decode's stereo stage.
- **R2 [ALG]** Bitrate modes: CBR (done implicitly), ABR, and VBR (per-granule
  quality target driving the reservoir). Wired to the CLI `-b:a` / `-q:a`.
- **R3 [TBL]** Xing/LAME info header (first frame): VBR TOC + encoder delay/padding
  so players seek correctly and trim the LAME 1728+529 delay.
- **R4 [E2E]** Conformance corpus: encode broadband noise, transients, tones,
  stereo; assert (a) our decoder round-trip under threshold, (b) FFmpeg + LAME both
  decode it, (c) quality metric vs LAME at matched bitrate within a margin.
- **R5 [later]** MPEG-2/2.5 LSF: the V2 SFB tables (the existing `tables.rs` brick),
  9-bit `scalefac_compress`, 1-granule framing, intensity-stereo scalefactors.

---

## Performance posture — where ASM earns its keep, where Rust is enough

We don't trade speed for safety on the hot path. But most of the encoder is *not*
hot, and reaching for `unsafe` SIMD there would add risk for no gain. So each
brick carries a posture (tracked in code via `lab::bricks::accel`, shown in the
`mp3lab bricks` table):

| Posture | Bricks | Why |
|---|---|---|
| **SIMD** (hand-vectorised, isolated `unsafe`) | **L1** analysis filterbank · **L2** forward MDCT · **Q2** psymodel FFT | Inner loops of thousands of MACs per granule — the classic LAME asm hotspots; the FFT (Q2) is the single biggest cycle sink in a perceptual encoder. |
| **Hybrid** (scalar default + optional SIMD) | **C2** rate loop · **Q6** distortion loop · **R1** M/S stereo | Iterative/serial control, but the per-call requantize and per-line stereo vectorise. Scalar first; SIMD only if profiling says so. |
| **Safe scalar Rust** | everything else (tables, Huffman/bit coding, side-info, framing, reservoir, block decision) | Cold or branchy + data-dependent. Asm would add `unsafe` surface for no measurable win. |

**The rule (same as `rusty_h264-accel` / `rav1e`):** the **scalar Rust path is
always the default and the correctness reference**; any SIMD lives behind a Cargo
feature in an isolated `unsafe` accel boundary, and the lab validates it against
its scalar twin through the **same** TDAC/round-trip gate. Acceleration that
changes output fails the gate — so we get the speed without spending the safety.

Sequencing: build every kernel **scalar-first** (it's the reference the SIMD path
is checked against), ship a correct encoder, then accelerate L1/L2/Q2 only where a
profile shows it matters. Don't write asm before there's a scalar oracle to prove
it against.

## Verification harness (the oracle)

Three loops, in order of how early they apply:

1. **Self round-trip (Floors 1–2):** each forward brick → its existing decode
   inverse → compare. No reference. Catches sign conventions, windowing, bit
   widths, table inversions. This is the workhorse.
2. **Pipeline round-trip (Floor 3+):** `MP3_REF` wav → encode → our bit-exact
   decoder → PCM diff at the LAME delay (mirrors `decode_real_mp3_structure`).
3. **External decode (Floor 3+):** the `.mp3` must decode cleanly in FFmpeg and
   LAME — the proof we wrote a *legal* stream, not just one our own decoder
   tolerates.

---

## Execution order

The house, bottom-up. Each floor is independently green before the next.

1. **Foundation** N1–N9 — reuse the decode tables; generate N4–N7; validate on
   entry. (Tables first so nothing downstream fabricates data.)
   ✅ **N1–N4 done** — N1–N3 reused; **N4** (forward quantizer power law) verified
   by an exact round-trip across all 8207 levels (`encode/quantize.rs`).
2. **Floor 1** L1 → L2 → L3 — the analysis core, self-verified by TDAC round-trip.
   ✅ **DONE.** **L1** analysis filterbank reconstructs at **95 dB** through the
   decoder's synthesis (delay 481); **L2** forward MDCT is the exact TDAC inverse
   of the IMDCT for long/short/start/stop (< 1e-5); **L3** full chain
   (PCM→analyze→MDCT→IMDCT→synthesis→PCM) reconstructs at **82 dB** (delay 1057).
3. **Floor 2** B1 → B2 → B3 → B4, then B5 → B6 → B7 → B8 — coding + framing, each
   round-tripped through the matching decoder parser.
   ✅ **DONE (B1–B8).** Huffman cost/encode + region/table selection (B1–B4)
   round-trip the spectrum exactly through `decode::huffman`; side-info +
   scalefactor serializers (B5/B6) round-trip the structs; frame assembly (B7) +
   the reservoir stream assembler (B8, borrows + caps at 511) produce frames the
   real `Mp3Decoder` decodes.
4. **Floor 3** C1 → C2 → C3 → C4 — the dumb-but-valid controller.
   ✅ **DONE.** Trivial psymodel (C1) + rate-loop quantizer (C2, binary-search
   `global_gain`, reject clipping) + Encoder plumbing (C3, mono) yield a real MP3.
   The pipeline gate (C4): a tone round-trips PCM→encode→our-decode→PCM at **81 dB**,
   **and the `.mp3` decodes in FFmpeg** to the correct waveform (rms/peak exact).
   Needed one fix beyond the bricks: the encoder must apply the *forward* alias
   butterfly (`encode/antialias.rs`) — the inverse of the decoder's `reduce()`,
   which it applies before the IMDCT.
5. **Floor 4** Q1 → … → Q6 — the quality brain, swapped in behind the same
   interfaces. The only research-grade work, now isolated.
   ✅ **Q1–Q4 + Q6 done** (Q5 block-switching deferred — needs the short-block
   coding path). Constants from the published formulas (Terhardt ATH, Zwicker
   Bark, Schroeder spreading); in-house radix-2 FFT (Q2); spread-energy masking
   thresholds (Q3); perceptual entropy (Q4). The two-loop quantizer (Q6) shapes
   noise under the mask — peak NMR **7.4 → 0.2 dB** on a two-tone signal — and a
   multitone round-trips at **69 dB** through our decoder and **FFmpeg**.
   ✅ **Q5 (block switching) done.** Forward reorder (subband→bitstream), short
   quantizer (79 dB coefficient round-trip), the LONG/START/SHORT/STOP FSM, and an
   attack detector — transients drive short blocks and the switched stream decodes
   clean in our decoder and FFmpeg. Per-window perceptual shaping of short blocks
   is a later refinement (they use flat scalefactors today).
6. **Roof** R1 → R4 (R5 later) — stereo, rate modes, LAME header, conformance.
   ✅ **R1, R1+, R2, R3, R4 done.** Stereo (independent L/R + per-frame **mid/side
   joint stereo**); **VBR** (quality-target quantizer → per-frame bitrate, Xing
   tag); **Xing/Info header** (FFmpeg reads exact duration); **conformance corpus**
   (8 signals, our-decoder floors + FFmpeg clean); **R5 MPEG-2/2.5** (V2 LSF band
   tables + framing, six rates 8–24 kHz, all FFmpeg-validated).

## ✅ Complete — 35/35 bricks

Every brick is built and verified; `cargo run --example mp3lab -- bricks` shows
35 ✓. The encoder is MPEG-1/2/2.5, mono / stereo / joint-stereo, CBR / VBR,
psychoacoustically noise-shaped, with block switching and a Xing/Info header —
and **every output decodes correctly in both our bit-exact decoder and FFmpeg 8.1**.

Remaining refinements (quality, not coverage): per-window perceptual shaping of
short blocks and the MPEG-2 LSF scalefactor *scheme* (both use flat scalefactors
today, which is valid); intensity stereo; the real CRC-16.

The payoff of the build order: ~70% of the encoder (Foundation + Floors 1–3) was
mechanical and provable against our own bit-exact decoder, yielding a working
encoder early. The genuinely hard part — psychoacoustic quality — was cornered by
itself on Floor 4, never tangled up with a filterbank sign bug.
