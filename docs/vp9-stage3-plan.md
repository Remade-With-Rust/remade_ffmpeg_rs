# VP9 Stage 3 — Keyframe Intra Decode to Pixels: Exact Plan

Goal: turn a decoded VP9 **key frame** into pixels that are **bit-exact** against
FFmpeg/libvpx. This document enumerates every primitive that must be mirrored
*exactly*, classifies how each is obtained and verified, and orders the work.

Discipline (same as the AAC build): fixed tables are **sourced from a reference
and validated** (counts / ranges / structural invariants / cross-frame consume),
**never fabricated**. Algorithms are transcribed from the VP9 spec / libvpx and
verified by round-trip, float-reference, or end-to-end pixel comparison.

Reference frame for verification: the libvpx `keyframe.vp9` already in the crate
— **Profile 1, sRGB (4:4:4, `subsampling_x=subsampling_y=0`), 8-bit, 96×64,
all-keyframe**. 4:4:4 means U/V are full-resolution (no chroma subsampling — the
simplest reconstruction path), an ideal first target.

Classification key: **[GEN]** compute from a formula and assert vs known values ·
**[TBL]** transcribe a fixed table and validate · **[ALG]** transcribe an
algorithm and verify · **[done]** already implemented & tested.

---

## A. Inverse transforms (numerical core) — `transform.rs`

- A1 **[GEN]** `cospi_i_64 = round(cos(i·π/64)·2^14)`, i=1..31 (16 distinct used).
- A2 **[TBL]** `sinpi_i_9`, i=1..4 (ADST-4 constants); validate via ADST round-trip.
- A3 **[done]** `idct4`, `iwht4` (+ `fdct4` for round-trip).
- A4 **[ALG]** `idct8`, `idct16`, `idct32` (butterflies; `dct_const_round_shift`).
- A5 **[ALG]** `iadst4`, `iadst8`, `iadst16`.
- A6 **[ALG]** 2D inverse driver: row transform → intermediate round/shift →
  column transform → `ROUND_POWER_OF_TWO` final shift → add to prediction → clamp.
  Per-size shifts: 4×4 ⇒ 4, 8×8 ⇒ 5, 16×16 ⇒ 6, 32×32 ⇒ 6 (with the 32×32 ÷2).
- A7 **[TBL]** `tx_type` selection: `intra_mode_to_tx_type_lookup[10]`; tx_type
  applies only to 4×4/8×8/16×16 (32×32 ⇒ DCT_DCT); lossless ⇒ WHT.
  Verify: float-reference per transform + round-trip; end-to-end at A6 wiring.

## B. Dequantization — `quant.rs`

- B1 **[TBL]** `dc_qlookup[256]`, `ac_qlookup[256]` (8-bit). (10/12-bit variants
  deferred — Profile 0/1 are 8-bit.)
- B2 **[ALG]** q-index derivation: `base_q_idx` (+ segment delta, off here) +
  `delta_q_y_dc` / `delta_q_uv_dc` / `delta_q_uv_ac`, clamped 0..255.
- B3 **[ALG]** coefficient dequant: `coef[0]` uses DC quant, rest AC quant;
  32×32 has an extra `/2` (round). Validate: spot-check known q-index outputs.

## C. Probability model — `prob.rs`

- C1 **[TBL]** `default_coef_probs[4][2][2][6][6][3]` (band 0 → 3 ctx).
- C2 **[TBL]** `default_skip_prob[3]`.
- C3 **[TBL]** `kf_y_mode_probs[10][10][9]` (above-mode × left-mode → 9 tree probs).
- C4 **[TBL]** `kf_uv_mode_probs[10][9]` (Y-mode → 9 tree probs).
- C5 **[TBL]** `kf_partition_probs[16][3]`.
- C6 **[ALG]** `inv_remap_prob` + `inv_recenter_nonneg` + `inv_map_table[254]`
  ([TBL]) — applies the stage-2 sub-exp deltas to the defaults.
- C7 **[TBL]** `pareto8_full[255][8]` — expands the 3 coded "model" coef nodes to
  the full token-tree probabilities (`vp9_model_to_full_probs`).
  Verify: counts/ranges; that applying stage-2 updates leaves probs in 1..255;
  ultimately end-to-end pixels.

## D. Entropy / token decoding — `token.rs`

- D1 **[TBL]** trees: `intra_mode_tree`, `partition_tree`, `token_tree`.
- D2 **[ALG]** generic tree decoder `read_tree(tree, probs)` over the bool decoder.
- D3 **[TBL]** coefficient extra-bit data: `cat1..cat6_prob[]`, `cat_minval[]`,
  `extra_bits` counts; the token→base-value mapping.
- D4 **[ALG]** `decode_coefs(plane, tx_size, scan)`: walk scan order, per position
  compute the coef context (D5), read `more_coefs` / token, read category extra
  bits + sign, dequant (B3), until EOB; track per-position token cache for context.
- D5 **[TBL]** scan + neighbor tables per (tx_size, tx_type):
  `default/col/row_scan_4x4` (+8×8,16×16), `default_scan_32x32`, and the matching
  `*_neighbors` tables for `get_coef_context`.
  Verify: scan tables are permutations of `0..N²`; neighbors in range; end-to-end.

## E. Block structure & mode info — `block.rs`

- E1 **[TBL]** geometry: `b_width_log2_lookup`, `b_height_log2_lookup`,
  `num_4x4_blocks_wide/high_lookup`, `size_group_lookup`, `partition_subsize`,
  `max_txsize_lookup`, `tx_mode_to_biggest_tx_size`.
- E2 **[ALG]** `decode_partition`: recursive 64→…→8(→4×4) with the above/left
  partition context (`kf_partition_probs` + ctx); sub-8×8 handling.
- E3 **[ALG]** `intra_frame_mode_info`: Y mode(s) via `kf_y_mode_probs` indexed by
  above/left modes (DC default at edges); for sub-8×8, per-4×4 Y modes; UV mode via
  `kf_uv_mode_probs[y_mode]`; the `skip` flag (C2). No ref/mv (intra).

## F. Intra prediction — `predict.rs`

- F1 **[ALG]** 10 modes: DC, V, H, TM, D45, D135, D117, D153, D207, D63.
- F2 **[ALG]** edge construction: above row, left col, above-left; availability
  (use 127 above / 129 left when absent); above-right extension for D45/D63;
  per tx block size (4/8/16/32). VP9 has **no** intra edge smoothing filter.
  Verify: per-mode unit cases (DC of known neighbors; V/H copy) + end-to-end.

## G. Reconstruction & tiles — `decode.rs`

- G1 **[ALG]** tile layout: 1+ tiles, per-tile bool decoder; non-last tiles have a
  4-byte big-endian size prefix; clear above/left contexts at tile start.
- G2 **[ALG]** per-superblock recursion → per coding block: mode info (E3) → for
  each plane (Y,U,V; subsampling from the header) → for each tx block: predict
  (F) → `decode_coefs` (D) → dequant (B) → inverse transform (A) → add+clamp into
  the frame buffer.
- G3 **[ALG]** frame buffer (Y,U,V planes, strides), output as a `Frame::Video`.

## H. End-to-end verification

Decode `keyframe.vp9` (and the other two frames) and compare every pixel to
FFmpeg's reference decode of `vp9.webm` — **bit-exact** on the deterministic
intra path, the same way AAC/EIGHT_SHORT/IS/TNS were proven.

---

## Execution order

1. **3b** A1–A7 — finish the transforms (this is mechanical + self-verifiable). ✅ done
2. **3c** B — dequant tables + derivation. ✅ done (per-block apply lands in D)
3. **3d** C — default-prob tables + `inv_remap` + Pareto expansion; apply updates. ✅ done
4. **3e** D — token decode (`decode_coefs`), scan/neighbor/band tables, generic
   tree reader, per-block dequant. ✅ done (intra-mode/partition *trees* land with E)
5. **3f** E — geometry tables, intra/partition trees, partition + mode-info
   *primitives* (contexts, neighbour modes, kf prob selection, reads). ✅ done
   (the recursion driver + mi-grid assembly wire up in G)
6. **3g** F — intra prediction: 10 predictors (bit-exact vs an independent
   reference) + the edge-assembly with 127/129 defaults & border extension. ✅ done
7. **3h** G — reconstruction loop + tiles → pixels. ✅ built (`decode.rs`): the
   compressed-header FrameContext, tile bool decoder, `decode_partition`
   recursion, per-block mode-info → predict → tokens → dequant → itx → add, and
   `Frame::Video` output. Wired into `receive_frame`.
8. **3i** H — bit-exact vs FFmpeg. ◀ in progress. Decodes the real key frame
   end-to-end; the **whole tile is bit-in-sync** (consumes 13106/13112 bits) and
   most blocks match FFmpeg exactly. Bugs found + fixed by FFmpeg diff: (1) the
   bool-decoder init **marker bit** (`vpx_reader_init`); (2) sub-8×8 plane 4×4
   geometry (`n4_w` from the partition level, not `num_4x4[bsize]`); (3) the
   **ADST row/column orientation** in the 2D inverse transform; (4) the
   `read_coef_probs` per-tx-size update-present bit + `max_tx` bound. Current:
   ~94% chroma, ~82% luma.

   **What the remaining gap is *not* (each ruled out by a targeted experiment):**
   - *Not the loop filter.* `loop_filter_level=4`, but patching it to 0 in the
     bitstream and re-decoding with FFmpeg barely moves the match (→83/95/95%);
     the failing blocks are identical pre/post filter.
   - *Not `decode_coefs`.* An independent re-port of libvpx's `decode_coefs`
     (Python), replayed from my decoder's exact arithmetic state at a failing
     block, yields **byte-identical** coefficients to my Rust.
   - *Not the probabilities / state / neighbours.* The block immediately before a
     failure reconstructs FFmpeg's pixels **exactly** and its decoder state
     advances to **exactly** the failing block's entry state; the failing block's
     neighbours match the reference.

   **RESOLVED (intra bit-exact).** Root cause: the **d45 and d63 4×4 intra
   predictors**. libvpx ships *distinct* 4×4 predictors (`vpx_d45_predictor_4x4_c`
   / `vpx_d63_predictor_4x4_c`) that consume the **full above-right diagonal**,
   whereas the 8×8/16×16/32×32 `d45/d63_predictor` plateau the lower-right
   triangle at `above[bs-1]`. Our predictors only implemented the general
   (plateau) form, so every 4×4 D45/D63 block was wrong. Smooth/diagonal content
   selects these modes heavily; the noisy high-frequency test frame happened to
   pick only DC/TM/V/H (which ignore above-right), which is why it passed while
   smooth content failed. The other directional 4×4 specials (d117/d135/d153/d207)
   are provably identical to the general form via AVG2/AVG3 symmetry (verified).
   With the two 4×4 specials added, **all controlled frames are 100% bit-exact
   (Y/U/V, maxerr 0)** and the real `keyframe.vp9` is **100% bit-exact including
   the loop filter** (see B below).

   **B — loop filter (DONE, bit-exact).** `loopfilter.rs` implements the general
   (`non420`) per-plane deblocking path: sharpness→limit/mblim/hev thresholds,
   per-(segment,ref,mode) filter levels, the per-superblock 16/8/4 + internal-4×4
   edge masks from each block's transform size, and the verbatim `filter4/8/16`
   leaf kernels. Validated: `keyframe.vp9` (loop_filter_level=4) decodes **100%
   bit-exact on all three planes vs FFmpeg**; lf=0 frames are unaffected.

   ---

   **Breakthrough via FFmpeg *encoder*:** using FFmpeg to encode controlled
   keyframes (then decoding both ways) showed the decoder is **bit-exact**
   (100%, PSNR ∞) on high-frequency 64×64 and 96×64 content — i.e. the core
   (partition, modes, tokens, 4×4 transforms incl. ADST, sub-8×8, clipped
   superblock, chroma) is **correct**. The bug reproduces **minimally** on a
   *smooth gradient* (which forces large transforms): luma ~37%, chroma ~99%,
   `loop_filter_level=0`. So it is **isolated to the large-transform luma path**.
   Ruled out by direct check against the libvpx source: all scan + neighbour
   tables (4/8/16/32, default/row/col), `iadst8`/`iadst16` (verbatim),
   `read_selected_tx_size`, `tx_size_context`, `pareto8_full`. The remaining
   fault is a **desync somewhere in large-block luma decode** — the first
   wrong block decodes from a corrupted boolean-decoder state (its predecessors
   produce correct *pixels* but evidently consume wrong *bits*; smooth content
   masks this until a residual-bearing block). **Next step:** a full independent
   Python port of the first superblock (partition + mode-info + token decode)
   to compare per-block state against the Rust and pin the exact divergence —
   now tractable because the reproducer is a 64×64 single-superblock gradient.

### Verifying against FFmpeg (the H harness)

Wrap `keyframe.vp9` in an IVF and decode with a static ffmpeg, then diff planes:

```
# build IVF: DKIF hdr + (size u32le, ts u64) + frame; pix_fmt gbrp for sRGB 4:4:4
ffmpeg -i keyframe.ivf -f rawvideo -pix_fmt gbrp ref.raw
cargo test -p rff-codec-vp9 dump_keyframe_planes -- --ignored   # writes my_plane{0,1,2}.raw
# compare my_plane[i] to ref plane i (G,B,R); per-8×8 heatmap localises any mismatch
```

Each numbered step ends green (suite + `cargo deny`) with its own tests; the big
sourced tables (C1, C7, D5, B1) are validated on entry so a bad transcription
fails the build rather than silently corrupting pixels.
