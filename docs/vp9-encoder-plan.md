# VP9 Encoder — The Inverse House: Exact Brick Plan

Goal: turn raw YUV frames into a **legal, decodable VP9** bitstream, then into a
**well-compressed** one. This enumerates every primitive the encoder needs as a
numbered **brick**, classifies how each is obtained and verified, and orders the
work into layers so it can be built over months without losing the thread.

The decode side of `rff-codec-vp9` is **315/315 bit-exact vs the official libvpx
vectors**. That is the whole strategy: the encoder is largely the *inverse* of
code we already trust, and **our own decoder is the verification oracle** — almost
every mechanical brick is provable by a round-trip (`encode → decode → compare`)
with no external reference.

## The strong hand: VP9 *does* have a bit-exact target

MP3's honest caveat — "an encoder can't be bit-exact, psychoacoustic models
differ" — **does not apply to the structural core here.** VP9 reconstruction is
fully deterministic: given a bitstream, the decoder produces exact pixels. The
encoder must run the *same* reconstruction loop (predict → dequant → inverse
transform → loop filter — all **REUSE** of decoder code), so:

> **`decode(encode(frame))` == the encoder's own internal reconstruction, to the
> bit.** Not "within PSNR" — exactly.

So the bar splits, but higher than MP3's:
- **Structural conformance** (Foundation + Floors 1–4): the produced bitstream
  decodes — in *our* decoder and in *libvpx/ffmpeg* — to the encoder's exact
  reconstruction. Provable to the bit, held to the bit. The existing
  `crates/rff/tests/vp9_conformance.rs` gate runs in reverse here.
- **Quality** (Floor 5+): *which* modes/MVs/quantizers the RDO picks is where we
  legitimately differ from libvpx. Measured by PSNR/SSIM vs source at matched
  bitrate. No bit-exact claim on the *choices* — only on their faithful coding.

## Discipline (same as MP3 / VP9-decode)

- Fixed tables are **reused from the decoder** (already validated by 315/315),
  **never re-fabricated**. The encoder imports the same `const`s.
- Algorithms are **transcribed from the VP9 spec / libvpx (BSD, permissive)** and
  verified by round-trip through our decoder.
- Every brick lands green (suite + `cargo deny` + the conformance gate) with its
  own test. A forward brick that doesn't round-trip through its decode inverse
  fails the build.

## Classification key

**[done]** already exists on the decode side — reuse / invert ·
**[REUSE]** decoder code called verbatim by the encoder ·
**[INVERT]** the encoder needs the forward/writer twin of a decoder reader/parser ·
**[TABLE]** a fixed codebook/probability table imported as-is ·
**[GEN]** compute from a formula and assert vs known values ·
**[NEW]** brain with no decoder twin (search / decision / rate control) ·
**[GLUE]** wiring, no new math.

Scope for **v1**: **Profile 0, 8-bit, 4:2:0**, the corner the decoder is
strongest in. Intra-only first (Floor 3), then inter (Floor 4). 10/12-bit,
compound/alt-ref, and threaded tiles are Roof work.

All bricks live in a new `encode/` module tree inside `rff-codec-vp9`, plus a
`vp9lab` harness mirroring `mp3lab` (brick registry + round-trip metrics).

---

## Foundation — the writer core + shared tables (`encode/bitwriter.rs`, reuse)

Every floor writes through here. The tables are all **[done]**; the writer is new.

- **F1 [TABLE/done]** All codebooks reused verbatim from the decoder — nothing
  re-entered. Coefficient model `DEFAULT_COEF_PROBS` + `PARETO8_FULL`
  ([prob_tables.rs:9,837]); `KF_Y/UV_MODE_PROBS`, partition/skip/tx/ref/interp
  probs, `DEFAULT_NMV_CONTEXT`, `CAT*_PROB` ([prob_tables.rs:1095–1395]); scan
  orders + neighbors + `COEFBAND_*` ([scan_tables.rs]); `SUBPEL_FILTERS`
  ([inter.rs:19]); `DC/AC_QLOOKUP` ×3 bit depths ([quant.rs:10–120]); loop-filter
  threshold tables ([loopfilter.rs:29–43]). Validation: already 315/315.
- **F2 [NEW — the keystone]** `BoolEncoder` — the exact inverse of `BoolDecoder`
  ([bits.rs:59,133]). State `{value, range∈[128,255], count, out: Vec<u8>}`;
  `write_bool(bit, prob)` with `split = (range*prob + (256-prob)) >> 8`, carry
  propagation + byte emit on renorm; `flush()` drains the final bytes and the
  marker. Everything downstream writes through this one brick.
  **Verify:** a random `(bit, prob)` stream → `BoolEncoder` → `BoolDecoder` reads
  back the identical sequence (exhaustive over probs 1..255).
- **F3 [INVERT]** Bit/tree writers on top of F2: `write_literal(v, n)` (inverse of
  [bits.rs:157]); `write_tree(tree, probs, symbol)` (inverse of `read_tree`
  [token.rs:197]); and the MSB-first uncompressed-header `BitWriter` (inverse of
  the `BitReader` [bits.rs:10]). **Verify:** round-trip each through its reader.
- **F4 [INVERT]** Probability-delta coder: `forward_remap_prob` + `encode_term_subexp`
  — inverses of `inv_remap_prob` ([prob.rs:31], via `INV_MAP_TABLE`) and
  `decode_term_subexp` ([decode.rs:723]). Drives every prob update in the
  compressed header. **Verify:** round-trip through `diff_update` ([decode.rs:742]).
- **F5 [REUSE]** Forward probability adaptation — call `merge_probs` /
  `mode_mv_merge_probs` / `tree_merge_probs` ([adapt.rs:50–92]) **verbatim** with
  the encoder's own `FrameCounts` (the encoder accumulates the same counts as it
  *writes* that the decoder does as it *reads*). **Verify:** the encoder's
  post-frame adapted `FrameContext` equals the decoder's after decoding the same
  frame (both run identical merges on identical counts).

---

## Floor 1 — Pixels → Coefficients (the analysis path) · `encode/fdct.rs`, `encode/quantize.rs`

Pure DSP, **self-verifying** against the decoder's inverse transforms — no
external reference. Highest certainty; start here.

- **T1 [INVERT]** Forward DCT `fdct4/8/16/32` — exact inverses of `idct4/8/16/32`
  ([transform.rs:40–399]), reusing `COSPI`/`SINPI` ([transform.rs:16,402]) and the
  same `round_shift`/`round_pow2` rounding. `fdct4` already exists
  ([transform.rs:59], the seed). **Verify:** `fdct_n → idct_n` reconstructs within
  the transform's defined rounding, every size.
- **T2 [INVERT]** Forward ADST `fadst4/8/16` (inverse of `iadst*`
  [transform.rs:405–597]) + `fwht4` (inverse of `iwht4` [transform.rs:601], the
  lossless path). **Verify:** round-trip per size.
- **T3 [INVERT/GLUE]** 2D forward dispatch `forward_transform(residual, tx_size,
  tx_type)` — rows then cols with size-dependent shifts, `tx_type` from
  `INTRA_MODE_TO_TX_TYPE` ([transform.rs:659]) — mirroring
  `inverse_transform_add_rows` ([transform.rs:764]). **Verify:** `forward_transform
  → inverse_transform_add` reconstructs the residual exactly (the TDAC analogue).
- **Q1 [NEW]** Forward quantizer `quantize(coeffs, dq, bias) -> (levels, dqcoeff,
  eob)` — divide by the **same** `DC/AC_QLOOKUP` step ([quant.rs]) with a rounding
  bias; emit quantized levels, their dequantized reconstruction, and the EOB.
  **Verify:** `dequant(quantize(x))` matches the decoder's `Dequant` path
  ([quant.rs:149]); round-trip residual error ≤ the quant step.
- **T4 [E2E]** Pixel↔coeff core gate: `residual → forward_transform → quantize →
  dequant → inverse_transform_add` equals the encoder's reconstruction, **and**
  that reconstruction is what the decoder produces from the coded `levels`. This
  single test certifies the whole analysis core before a bit of syntax is written.

---

## Floor 2 — Coefficients/Modes → Bits (the coding path) · `encode/tokens.rs`, `encode/syntax.rs`

Mechanical inverses of decode stages, each **round-trip-verifiable** against the
matching decoder parser.

- **B1 [INVERT]** Coefficient token encoder `encode_coefs` — inverse of
  `decode_coefs` ([token.rs:77]): walk `levels` in scan order (`get_scan`
  [token.rs:26], **REUSE**), derive context (`get_coef_context` [token.rs:52],
  **REUSE**), write EOB / zero-run / one / token-tree + `CAT*` extra bits + sign
  through F2, accumulating `coef`/`eob_branch` counts. **Verify:** `encode_coefs →
  decode_coefs` yields identical `dqcoeff` + `eob`.
- **B2 [NEW]** Token cost `coef_cost(levels, ctx, probs)` — bits **without
  emitting** (the RDO inner-loop oracle), from `−log2(prob)` cost tables built off
  the current `FrameContext`. **Verify:** equals B1's actual emitted bit count for
  random blocks.
- **B3 [INVERT]** Intra mode-info serializer — partition (`PARTITION_TREE`
  [block.rs:56]), Y/UV mode (`INTRA_MODE_TREE` [block.rs:43]; `kf_y/uv_mode_probs`
  [block.rs:160] for context — **REUSE**), `skip`, `tx_size`, `segment_id`
  (`SEGMENT_TREE` [decode.rs:953]) — inverses of `read_partition`/`read_intra_mode`/
  `read_tx_size`/segment reads. **Verify:** round-trip the `ModeInfo`
  ([block.rs:75]) through the decoder's parse, struct-equal.
- **B4 [INVERT]** MV serializer `encode_mv` — inverse of `read_mv`/`read_mv_component`
  ([mv.rs:70–151]): joint tree (`MV_JOINT_TREE`), per-component sign/class/bits/
  fp/hp through F2; predictor from `find_mv_refs` ([mv.rs:388], **REUSE**).
  **Verify:** `encode_mv(predictor, mv) → read_mv == mv`.
- **B5 [INVERT]** Inter mode-info serializer — inter mode (NEAREST/NEAR/ZERO/NEW),
  ref frame(s), `interp_filter`, compound flags — inverse of the decode.rs inter
  mode-info path; contexts via `get_mode_context` ([mv.rs:363], **REUSE**).
  **Verify:** round-trip through the decoder.
- **B6 [INVERT]** Compressed-header serializer — inverse of
  `parse_compressed_header` ([decode.rs:799]): `tx_mode`, coef-prob updates
  (`read_coef_probs` [decode.rs:778]), and skip/inter-mode/interp/ref/mode/
  partition/MV prob updates via F4. *Which* probs to update is a cost decision
  (R4, Floor 5); the **serialization** is the invert. **Verify:** `serialize →
  parse_compressed_header` yields an identical `FrameContext`.
- **B7 [INVERT]** Uncompressed-header serializer — inverse of the `FrameHeader`
  parse ([lib.rs:51]): profile, frame type, show/intra flags, size + render size,
  ref indices + sign bias, `base_q_idx` + deltas, loop-filter params, tile
  log2s, segmentation. **Verify:** `serialize → parse == FrameHeader`; byte
  accounting matches `header_size`.
- **B8 [INVERT/GLUE]** Tile + frame assembly — one `BoolEncoder` per tile, tile
  size prefixes, partition + concatenation, mirroring `decode_tiles`
  ([decode.rs:1406], **REUSE** structure); superframe index when needed.
  **Verify:** the decoder's `decode_tiles` accepts it; per-tile byte boundaries
  parse.

---

## Floor 3 — The dumb-but-valid controller (first decodable VP9)

The **simplest legal** intra-only encoder, end to end. Quality is mediocre on
purpose; correctness is bit-exact-provable. The milestone where
`ffmpeg -i in.y4m -c:v vp9 out.webm` first works **and libvpx plays it.**

- **C1 [NEW]** Trivial mode decision: fixed partition (one block size, e.g. all
  `BLOCK_32X32`), per-block cheapest of {DC,V,H,TM} by SAD, fixed `tx_size`, one
  frame-wide `base_q_idx`. No RDO, no segmentation, single tile.
- **C2 [REUSE+GLUE]** Intra reconstruct loop, per transform block:
  `build_intra_edges` + `predict` ([predict.rs:348,314], **REUSE**) → `residual =
  src − pred` → Floor-1 `forward_transform`/`quantize` → B1 `encode_coefs` →
  `dequant` + `inverse_transform_add` ([transform.rs], **REUSE**) → write into the
  **reconstruction buffer**; `set_ctx` ([decode.rs:2557], **REUSE**). This buffer
  is what the decoder must reproduce.
- **C3 [GLUE]** `Encoder` impl ([rff_codec::Encoder]): `configure(&Dictionary)`
  reads `qp`/`crf`/`b`; `send_frame` takes a YUV `VideoFrame`, encodes one key
  frame, queues the `Packet`; `flush` drains. (Bridge non-YUV input with
  `-vf format=yuv420p`, like the decoder's RGB note.)
- **C4 [E2E]** "The house stands": raw Y4M → our encoder → our decoder → pixels
  **== the encoder's reconstruction, bit-exact**, **and** the `.webm`/`.ivf`
  decodes cleanly in libvpx/ffmpeg. A real, valid VP9 key frame.

---

## Floor 4 — Inter prediction (the second house) · `encode/motion.rs`

The big lift: P-frames. Prediction, MV coding, and reconstruction are **REUSE**;
the *search* is **NEW**.

- **P1 [NEW]** Integer motion estimation: diamond/hex search per block, forming
  candidates with `predict_block` ([inter.rs:493], **REUSE**), SAD/SATD metric,
  predictor-centered from `find_mv_refs` (**REUSE**).
- **P2 [NEW]** Subpel refinement: half→quarter→eighth-pel around the integer best,
  through the 8-tap `SUBPEL_FILTERS` via `predict_block` (**REUSE**); honor
  `allow_high_precision_mv` + `lower_mv_precision` ([mv.rs:536], **REUSE**).
- **P3 [NEW]** Inter mode decision: NEAREST/NEAR/ZERO/NEWMV from the `find_mv_refs`
  predictors; single reference (LAST) first, compound later. RD-pick vs the intra
  cost from Floor 3.
- **P4 [REUSE+GLUE]** Inter reconstruct loop: `predict_block` → residual → the
  Floor-1/2 transform/quant/code path → reconstruct → `loop_filter_frame`
  ([loopfilter.rs:1569], **REUSE**) → store as a `RefFrame` ([decode.rs:1159],
  **REUSE**) in the 8-slot map.
- **P5 [NEW]** Reference & GOP: a simple GOP (key, then P referencing LAST);
  drive `refresh_frame_flags` / `ref_frame_idx` / `ref_sign_bias` ([lib.rs:93–97]).
- **P6 [E2E]** Inter gate: a moving clip → encode → our decoder bit-exact to the
  encoder's recon, **and** decodes in libvpx/ffmpeg.

---

## Floor 5 — The quality brain (RDO + rate control)

Replace Floor 3/4's stubs with real decisions. The only judgement-heavy work, now
isolated behind clean interfaces (B2 cost + a distortion metric).

- **R1 [NEW]** RDO mode/partition/tx search: `cost(B2) + λ·SSE`, λ from `qindex`;
  recursive partition 64→4, intra-mode search, tx-size search.
- **R2 [NEW]** Rate control: `qindex` per frame to hit CBR/CQ/VBR targets, wired to
  `-crf`/`-qp`/`-b:v`/`-pass`; a leaky-bucket model.
- **R3 [NEW]** Loop-filter-level search: pick `loop_filter_level` (0..63)
  minimizing reconstruction error vs source, applying `loop_filter_frame`
  (**REUSE**) at candidate levels.
- **R4 [NEW]** Forward prob-update decision: choose which compressed-header prob
  deltas to signal (delta cost vs coding savings), via F4/F5.
- **R5 [NEW]** Interp-filter selection; segmentation-driven adaptive `qindex`
  (`SEG_LVL_ALT_Q` [decode.rs:954]); trellis quantization (coefficient-level RD).

---

## Roof — conformance, tiles, advanced modes, tuning

- **Z1 [NEW]** Alt-ref + compound prediction (the bidirectional quality lever);
  `setup_compound_reference_mode` ([decode.rs:879], **REUSE**).
- **Z2 [NEW]** Two-pass (first-pass stats → second-pass allocation).
- **Z3 [REUSE]** Tiled / threaded encode — independent tile `BoolEncoder`s
  (the decoder's tile-column independence, in reverse).
- **Z4 [INVERT]** 10/12-bit (the `*_QLOOKUP_10/12` + high-bit `CAT6` tables are
  already present).
- **Z5 [NEW]** Speed presets (search-depth / partition / tx-search knobs).
- **Z6 [E2E]** Conformance corpus: a set of clips where (a) our decoder
  round-trips the encode **bit-exact**, (b) libvpx + ffmpeg decode it clean, (c)
  PSNR/SSIM vs libvpx at matched bitrate within a margin.

---

## Performance posture — where ASM earns its keep

Same rule as the decoder and `mp3lab`: **the scalar Rust path is always the
default and the correctness reference**; any SIMD lives behind the existing
`unsafe` accel boundary and is validated against its scalar twin through the
**same** round-trip gate. Build scalar-first; accelerate only where a profile says
so.

| Posture | Bricks | Why |
|---|---|---|
| **Reuse existing SIMD** | inter MC (`predict_block_avx2/neon`, [inter.rs]) · loop filter (`lf_core8`/`filter_edge8_avx2`, [loopfilter.rs]) | Already hand-vectorized + bit-exact in the decoder; the encoder calls them verbatim for prediction + reconstruction. Zero new `unsafe`. |
| **New SIMD candidates** | **P1/P2** ME SAD/SATD · **T1** forward DCT · **Q1** forward quant inner loop | The encoder's new hotspots (ME is the classic video-encoder cycle sink). Scalar-first; SIMD behind a feature once profiled, checked against the scalar oracle. |
| **Safe scalar Rust** | everything else (bool writer, token/syntax coding, headers, RDO control, rate control) | Cold or branchy + data-dependent. Asm would add `unsafe` for no measurable win. |

The decoder is **memory-bandwidth-bound** (see `docs/benchmarks.md`); the encoder
will be **compute-bound** (ME + RDO), so here SIMD on the new kernels can actually
pay — the opposite of the decode-side finding.

## Verification harness (the oracles) — strongest first

1. **Self round-trip (Foundation + Floors 1–2):** each forward brick → its decode
   inverse → compare. No reference. Catches range-coder carry bugs, transform
   sign/round errors, tree inversions, bit widths. The workhorse
   (`BoolEncoder↔BoolDecoder`, `fdct↔idct`, `encode_coefs↔decode_coefs`,
   serializers↔parsers).
2. **Reconstruction bit-exactness (Floor 3+):** the encoder's internal recon
   **==** our decoder's output for the produced bitstream, **to the bit** — VP9's
   determinism, the strong hand MP3 lacked. The `vp9_conformance.rs` machinery
   checks this in reverse.
3. **External decode (Floor 3+):** the bitstream decodes cleanly in libvpx +
   ffmpeg — proof we wrote a *legal* stream, not one only our decoder tolerates.
4. **Quality (Floor 5+):** PSNR/SSIM vs source at matched bitrate, within a margin
   of libvpx. No bit-exact claim on the RDO *choices*.

---

## Execution order

The house, bottom-up; each layer independently green (suite + `cargo deny` +
conformance) before the next.

1. **Foundation** F1→F2→F3→F4→F5. F2 `BoolEncoder` is the keystone — nothing
   writes without it; round-trip it exhaustively first.
2. **Floor 1** T1→T2→T3→Q1→T4 — the analysis core, self-verified by the
   forward→inverse round-trip. Zero entropy/judgement risk; pure DSP.
3. **Floor 2** B1→B2, then B3→B4→B5→B6→B7→B8 — coding + framing, each round-tripped
   through the matching decoder parser.
4. **Floor 3** C1→C2→C3→C4 — the dumb-but-valid intra controller. **The milestone:**
   first valid VP9 key frame, bit-exact through our decoder and accepted by libvpx.
5. **Floor 4** P1→…→P6 — inter prediction; the second large lift.
6. **Floor 5** R1→…→R5 — the quality brain, swapped in behind B2 + the distortion
   metric. The only research-grade work, cornered by itself.
7. **Roof** Z1→…→Z6 — alt-ref/compound, two-pass, tiles, high bit depth, presets,
   conformance corpus.

The payoff of this order: **~60–70% of the encoder (Foundation + Floors 1–3) is
mechanical and provable against our own bit-exact decoder — and uniquely for VP9,
provable *bit-exactly*.** It yields a working intra encoder early. The genuinely
hard part — ME + RDO + rate control quality — is cornered on Floors 4–5, never
tangled with a range-coder carry or transform sign bug.
