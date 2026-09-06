# rusty_hevc — the plan

**One document.** Written 2026-09-05. HEVC / H.265 is the one top-10 codec
`remade_ffmpeg_rs` cannot read ([ffmpeg-parity.md](../ffmpeg-parity.md): "HEVC is
the lone gap"; [roadmap.md](../roadmap.md) parks it in Tier 3 as patent-gated;
[compatibility.md](../compatibility.md): "licensing posture undecided"). Every
iPhone, most cameras, every 4K broadcast and most surveillance products ship
HEVC, so an `rff` that cannot open them is not a drop-in. This plan says what a
pure-Rust HEVC **decoder** needs, what we already own, how it is gated, and in
what order it lands. The encoder is a later mission (§7).

Re-check §1 before use: it is a snapshot.

---

## 0. Mission and promise

**Remake HEVC decoding as a standalone, permissively licensed, pure-Rust crate
(`rusty_hevc`) whose output is bit-exact against the JCT-VC conformance suite,
and wire it into `rff` so that an HEVC product — an MP4 from a phone, an MKV
rip, a broadcast TS, an RTP camera stream, a HEIC still — decodes, remuxes and
transcodes through every path `rff` already has.**

The one-line test, as everywhere in the portfolio: *could this ship as-is to a
user who assumes their data is theirs alone, on a machine with no C toolchain,
and is every pixel provably the pixel the standard says?*

Scope for v1, in one line: **Main, Main 10 and Main Still Picture profiles
(4:2:0, 8 and 10 bit), every tool of HEVC version 1, decode only.** Range
extensions (4:2:2 / 4:4:4 / 12-bit / RDPCM), screen-content coding, scalable
and multiview layers, and any encoder are out (§7).

---

## 1. Where we are (2026-09-05)

**In this repo: nothing.** `grep -ri hevc crates/` finds only the three docs
above. No `CodecId::Hevc`, no `hvc1`/`hev1` sample entry, no TS stream type
`0x24`, no `V_MPEGH/ISO/HEVC`, no RFC 7798 depacketizer.

**What we own that transfers** (the decoder is ~half reuse by shape, not by
line):

| Piece | Where | Status for HEVC |
|---|---|---|
| CABAC arithmetic engine + `rangeTabLps`/`transIdx` tables | `rusty_h264-decoder/src/cabac.rs`, `-common/src/cabac_tables.rs` | **identical engine** — HEVC uses H.264's 9-bit range coder and the same 64×4 LPS table; only the binarizations and context tables differ |
| NAL splitting, emulation-prevention removal, Exp-Golomb bit reader | `rusty_h264-common/src/{nal,bit_reader}.rs`; `rff_format::avc::split_annexb` | reuse; NAL header widens to 2 bytes |
| Length-prefixed ↔ Annex-B conversion, config-record hoisting | `rff_format::avc::{avcc_to_annexb, build_avcc_record}` | generalize to a codec-agnostic `rff_format::nal` + a new `hvc.rs` (`hvcC` is an array of NAL arrays) |
| u16 plane pipeline for 10/12-bit, u8 fast path for 8-bit | `rusty_vp9` (profiles 2/3), `rusty_h264` planes | reuse the pattern; Main 10 needs it on day one |
| Feature-gated rdtsc stage profiler, `*_matches_scalar` SIMD discipline, fuzz never-panic harness | `rusty_h264-common/src/prof.rs`, `rusty_vp9` kernels, `rff-codec-vp9 fuzz_robustness` | port the shape |
| Codec adapter (Annex-B in, `Frame::Video` out, `catch_unwind` at the boundary, `extradata` prepend) | `crates/rff-codec-h264/src/lib.rs` | copy as `rff-codec-hevc` |
| Conformance-vector fetch script + cached CI job + MD5 harness | `scripts/fetch-vp9-vectors.sh`, `.github/workflows/ci.yml` `conformance`, `crates/rff/tests/vp9_conformance.rs` | clone for HEVC |
| Deblocking / intra / MC *structure* (edge loops, availability, reference clamping) | `rusty_h264-common/src/{deblock,predict,inter}.rs` | structure only — every formula differs (§3) |

**Oracles available on this box:** `ffmpeg 8.1.2` (`hevc` native decoder, LGPL,
black box); `clang 22`, `cmake`, `ninja`, `meson`, `python` — enough to build
the JCT-VC **HM** reference decoder, which is **BSD-3-Clause** and therefore the
one HEVC reference whose *source* we may transcribe from and instrument.
`libde265` and `openHEVC` are LGPL: black-box second oracles only, never read.

**The ecosystem, surveyed 2026-09-05** (`cargo search`/`cargo info`) — the
house rule is pure Rust **and** permissive, and "in-house when nothing mature
exists", the same test that produced `rusty_vp9`, `rusty_aac`, `rusty_mp3`:

| Crate | Ver. | License | What it is | Verdict |
|---|---|---|---|---|
| `rust_h265` | 0.1.0 | MIT/Apache | "Main and Main 10, 4:2:0" decoder, single author, 18 k lines, no SIMD | **measured 42/147 (28.6 %)** — fails every DBLK/WPP/RPS/WP/TILES class; not a fork base |
| `hpvcd` | 0.3.2 | BSD-3/Apache | "tiny HEVC decoder" is modest: 50 k lines, SSE/AVX/NEON, threadpool, tiles/WPP, SCC, 4:2:2/4:4:4, HEIF; MSRV 1.93 | **measured 147/147 (100 %)** — the fork base (§8 Q2) |
| `oxideav-h265` | 0.0.10 | MIT | parser + "decoder scaffold" | not a decoder |
| `media-codec-h265` | 0.1.1 | MIT/Apache | decoder for the media-codec framework | evaluate if the two above fail |
| `scuffle-h265`, `hevc_parser` | — | MIT | header/NAL parsers | reference for §3.A/B shapes only |
| `libde265-rs`, `*-sys` | — | LGPL + FFI | C bindings | **excluded** (FFI, copyleft) |
| `h265` | 0.0.0 | — | reserved name | — |

Crate names verified **free** 2026-09-05: `rusty_hevc`, `rusty_h265`,
`rff-codec-hevc`, `rff-format-hevc`. Claim `rusty_hevc` in wave 0 (the `rff`
name was lost by publishing it last — [publishing-plan.md](../publishing-plan.md) §0.3).

---

## 2. Strategy — what does not change

### 2.1 The decoder is a from-scratch inverse of the spec, gated by three oracles

1. **Symbol oracle: instrumented HM** (BSD-3). Env-gated `fprintf` of the
   CABAC state (`m_uiRange`, `m_uiValue`, `m_bitsNeeded`, the bin, the context
   index) at every `decodeBin`, plus the derived semantic state the
   `codec-bringup-decoder` skill says to dump (merge candidate lists with their
   sources, MV predictors, availability flags, QpY predictions). HM is
   single-threaded, so the trace is deterministic without the `--threads`
   scrambling that cost days on rav2d. Find the register conversion first: HM's
   `(range, value, bitsNeeded)` triple is not the spec's `(ivlCurrRange,
   ivlOffset)` pair, exactly as openh264's `uiOffset >> iBitsLeft` was not.
2. **Pixel oracle: the decoded-picture-hash SEI.** Every JCT-VC conformance
   bitstream carries an MD5 / CRC / checksum per picture (SEI payload 132).
   The decoder verifies *itself*, per picture, with no external tool — this is
   HEVC's gift to bring-up and it becomes a **standing gate**
   (`HEVC_VERIFY_SEI=1`, the shape of `VP9_RECON_CHECK`), on by default in
   every test that decodes a stream.
3. **Independent references: `ffmpeg -f framemd5` and libde265**, black-box.
   When ffmpeg and libde265 agree and we differ, it is our bug (the
   minimp3-plus-FFmpeg law); where the *symbol* oracle and the *pixel* oracle
   disagree, the parse follows HM and the recon follows the spec/ffmpeg.

### 2.2 One brick per commit, gated before measured

Every brick lands green: the crate suite, the SEI self-check on the streams it
unlocks, `cargo deny`. Tables are **sourced from HM or the spec and validated
on entry** (element counts, permutation checks, orthogonality of the transform
matrix, monotone β/tC tables) — never fabricated. Speed work waits for
conformance (§4 Phase 6) and then obeys `codec-measurement` (pinned CPU time,
ABBA, null arm, work-count parity, fresh binary).

### 2.3 Shape: standalone crate, thin adapter, the H.264 covenant

```text
crates/rusty_h265/                 the codec: #![forbid(unsafe_code)] core,
  src/{nal,ps,slice,dpb,cabac,cu,tu,residual,dequant,itx,intra,inter,mc,
       deblock,sao,tiles,wpp,sei,frame,prof}.rs
  accel/ (later)                   the one crate allowed `unsafe`: SIMD twins
crates/rff-codec-hevc/             the adapter: Annex-B + extradata in, Frame out
crates/rff-format/src/hvc.rs       hvcC record, hvc1/hev1 ↔ Annex-B, SPS dims
```

Same covenant as `rusty_h264`: the codec core is safe Rust; acceleration is a
seam. `default-features = false` in the adapter so `rff`'s own
`#[global_allocator]` stays in charge (CLAUDE.md allocator rule). Published to
crates.io on its own cadence like `rusty_vp9`; the monorepo `crates/rusty_h265`
is the source of truth until it graduates to its own repo.

### 2.4 Patents: ship and document, isolated, gate-out-able

HEVC is the most patent-encumbered codec we will carry (Access Advance, Via LA,
Velos Media pools; no royalty-free grant). The posture already recorded for
H.264 and AAC in [compatibility.md](../compatibility.md#patents) applies
unchanged: an independent implementation clears *copyright*, not *patents*; we
grant no patent license; the obligation sits with whoever distributes a product.
The decoder lives in its own adapter crate behind a Cargo feature so a
patent-clean build can drop it. **Whether it is on by default in `rff` (as
H.264 is) is Tim's call — §8 Q1.**

### 2.5 Clean-room line

HM (BSD-3) and the ITU-T H.265 spec are the only texts transcribed from.
FFmpeg's `libavcodec/hevc*`, libde265 and openHEVC are never opened; they are
executed as oracles only. Record this in the crate README and NOTICE the way
`rusty_jpeg` records the IJG clause.

### 2.6 Speed posture: SIMD kernels are the primary path; the core is integer-only

- **Kernels ship default-on.** Once Phase 6 lands them, the runtime-dispatched
  SIMD twins (SSE2/AVX2 on x86-64, NEON on aarch64) in `rusty_hevc-accel` are
  the **primary** path — exactly as `rusty_h264` ≥ 0.10 ships its portable Rust
  SIMD default-on (no `nasm`, no C) and `rusty_vp9` dispatches on
  `is_x86_feature_detected!`. The scalar twin is the oracle and the fallback,
  reachable with `--no-default-features`, never the shipping path on a CPU that
  has the ISA. Prefer SSE2 wherever the kernel fits (baseline on x86-64: no
  detection, no second path nobody runs — the rusty_dds BCn precedent).
- **Every kernel proves it is reached.** `*_matches_scalar` test, a **byte
  census** on the conformance suite reading ~100 % down the kernel path
  (`codec-vectorize-kernel/REACHABILITY.md` — rusty_zstd shipped a documented
  1.14–1.26× kernel that decode called on 0 % of its bytes), and a NEON sibling
  or a written note per kernel. A flat arm-toggle is not a refutation until the
  arm is proven wired.
- **The decoder core is integer-only by construction.** HEVC's normative decode
  has no floating point anywhere — DCT/DST matrices, interpolation, deblocking,
  SAO and CABAC are all fixed-point. So the `rusty-fast-transcendentals` defect
  class (a libm `round()` / `floor()` / `powf` left inline in a hot loop, which
  silently keeps the whole loop scalar) is excluded by a lint, not by vigilance:
  `#![deny(clippy::float_arithmetic)]` in `rusty_hevc` and `rusty_hevc-accel`,
  plus a CI grep that fails on `f32`/`f64`/`libm` in `src/`. Float exists only in
  tests (PSNR) and the bench harness.
- **Kernel-writing rules inherited from that skill, for the accel crate:** the
  whole loop lives under one `#[target_feature]` function with its helpers
  `#[inline(always)]` inside it (a `#[target_feature]` helper called per block
  cannot inline and swung a result from +38.7 % to −47.8 %); check the
  **baseline ISA of every op** in a kernel, not just the headline one
  (`_mm_round_*`/`floor` are SSE4.1, above the portable baseline); `--emit asm`
  and count packed ops before believing auto-vectorization did anything.

---

## 3. What has to be built — the primitive inventory

Classification, as in the VP9 and MP3 plans: **[reuse]** exists in the
portfolio · **[TBL]** transcribe a fixed table and validate · **[GEN]** compute
from a formula and assert · **[ALG]** transcribe an algorithm and verify ·
**[GLUE]** wiring. The number in brackets is a rough size in lines of Rust,
from the H.264 decoder's 26k-line body scaled by the syntax difference.

### A. Bytestream and NAL layer (~400)
- A1 **[reuse]** Annex-B start-code split; emulation-prevention removal (`rusty_h264-common::nal`).
- A2 **[ALG]** 2-byte NAL header: `nal_unit_type` (6 bits, 64 types), `nuh_layer_id`, `nuh_temporal_id_plus1`. VCL types 0–31 (TRAIL/TSA/STSA/RADL/RASL N/R, BLA ×3, IDR ×2, CRA); non-VCL 32–40 (VPS/SPS/PPS/AUD/EOS/EOB/FD/prefix SEI/suffix SEI). `nuh_layer_id > 0` is **dropped** (SHVC/MV-HEVC out of scope).
- A3 **[ALG]** Access-unit boundary from `first_slice_segment_in_pic_flag` + AUD; the adapter's `split_access_units` mirrors the H.264 one.

### B. Parameter sets (~900)
- B1 **[ALG]** VPS (parse, keep for ids; nothing downstream needs it in v1).
- B2 **[ALG]** SPS: `profile_tier_level` (general + sub-layer, must be skipped exactly), `chroma_format_idc` (**refuse ≠ 1 in v1**), bit depths luma/chroma (8–10; note `TSUNEQBD` has *unequal* luma/chroma depth), `log2_max_pic_order_cnt_lsb`, `sps_max_{dec_pic_buffering,num_reorder_pics,latency_increase}`, CTB/min-CB/min-TB/max-TB sizes, max transform hierarchy depths, scaling-list enable + data, AMP, SAO, PCM (bit depths, sizes, loop-filter-disable), short-term RPS sets, long-term ref pics, `sps_temporal_mvp_enabled`, `strong_intra_smoothing`, VUI (aspect ratio, colour description, `field_seq`, timing, bitstream restriction) — VUI feeds `Stream`, never decode.
- B3 **[ALG]** PPS: `init_qp`, `cu_qp_delta_enabled` + `diff_cu_qp_delta_depth`, cb/cr QP offsets, slice-level chroma offsets present, weighted pred/bipred flags, `transquant_bypass`, **tiles** (`num_tile_columns/rows`, uniform spacing or explicit widths, `loop_filter_across_tiles`), **entropy_coding_sync** (WPP), `sign_data_hiding`, `cabac_init_present`, `num_ref_idx_default`, `constrained_intra_pred`, `transform_skip`, `lists_modification_present`, `log2_parallel_merge_level`, deblocking control + overrides, PPS scaling list, `slice_segment_header_extension`. **Range/SCC extension flags: parse, then refuse** with a named error.
- B4 **[TBL]** Default scaling lists (intra/inter 8×8; 16/32 by replication) and the scaling-factor derivation with DC terms **[ALG]**.
- B5 **[GEN]** Derived geometry: `PicWidthInCtbs`, `MinTbAddrZs` z-scan table, CTB raster↔tile scan (`CtbAddrRsToTs`, `TsToRs`, `TileId`, `colBd/rowBd`). Validate: permutations, tile coverage sums to the picture.

### C. Slice segment header (~500)
- C1 **[ALG]** `first_slice_segment_in_pic`, `no_output_of_prior_pics`, `dependent_slice_segment` (restores CABAC + header from the previous segment), `slice_segment_address`, slice type, `pic_output_flag`, POC lsb, **RPS** (inline short-term set or `short_term_ref_pic_set_idx`; long-term entries with `used_by_curr` and msb-present), `slice_temporal_mvp_enabled`, SAO luma/chroma flags, `num_ref_idx_active_override`, `ref_pic_lists_modification`, `mvd_l1_zero`, `cabac_init_flag` (swaps init types 1↔2), `collocated_from_l0` + `collocated_ref_idx`, `pred_weight_table`, `five_minus_max_num_merge_cand`, `slice_qp_delta`, cb/cr offsets, deblocking overrides, `slice_loop_filter_across_slices`, **entry points** (`offset_len_minus1`, substream byte offsets for tiles/WPP), header extension bytes, `byte_alignment()`.

### D. POC, RPS, DPB and output order (~700)
- D1 **[ALG]** `PicOrderCntVal` from lsb + msb wrap, reset at IRAP with `NoRaslOutputFlag`.
- D2 **[ALG]** RPS derivation into the five lists (`StCurrBefore/After`, `StFoll`, `LtCurr`, `LtFoll`); marking (unused-for-reference), **missing-reference generation** (grey pictures for BLA/CRA-with-leading-skipped, the `RAP_A/B`, `NoOutPrior`, `BUMPING` conformance streams live here).
- D3 **[ALG]** `RefPicList0/1` construction + `list_entry` modification; `num_ref_idx` ≤ 15.
- D4 **[ALG]** DPB "bumping" output: `sps_max_num_reorder_pics`, `max_latency`, `max_dec_pic_buffering`, `pic_output_flag`, RASL pictures dropped after CRA/BLA at stream start, `NoOutputOfPriorPics`. Output order gated against `ffmpeg -f framemd5` (which outputs in display order).
- D5 **[GLUE]** Frame pool + reference-count discipline (borrowed planes to the adapter, the `rusty_av2d 0.2.8` scratch-pool shape).

### E. CABAC (~1,200)
- E1 **[reuse]** Engine: 9-bit range, `rangeTabLps[64][4]`, `transIdxLps/Mps`, bypass, terminate — `rusty_h264`'s engine verbatim (HEVC §9.3.4.3 is H.264 §9.3.3.2). Gate: HM trace parity on the first symbols.
- E2 **[TBL]** Context initialization: ~150 context variables across three `initType` tables (`initValue` → slope/offset → state from `SliceQpY` **[GEN]**). Validate counts per syntax element against HM's `ContextTables.h`.
- E3 **[ALG]** Storage/sync: save contexts after the 2nd CTB of each row (WPP), restore at row start; re-init at tile starts; dependent-slice restore; `end_of_slice_segment_flag` / `end_of_subset_one_bit` terminates; PCM alignment and raw-byte reading.

### F. Coding quadtree and prediction units (~1,100)
- F1 **[ALG]** `split_cu_flag` (ctx from left/above depth), CU 8–64, `cu_transquant_bypass_flag`, `cu_skip_flag` (ctx from neighbours), `pred_mode_flag`, `part_mode` binarization (2N×2N, 2N×N, N×2N, N×N, four AMP shapes; ctx + bypass bins; N×N only at min CB size), `pcm_flag` + `pcm_sample` (bit depths), `rqt_root_cbf`.
- F2 **[ALG]** Intra mode coding: `prev_intra_luma_pred_flag`, `mpm_idx`, `rem_intra_luma_pred_mode`, the **MPM candidate derivation** from left/above (above in a different CTB row counts as DC), `intra_chroma_pred_mode` (4 + derived, mode-34 substitution).
- F3 **[ALG]** Inter PU syntax: `merge_flag`, `merge_idx` (TR bins with `MaxNumMergeCand`), `inter_pred_idc` (ctx by CU depth; bi-pred forbidden for 8×4/4×8), `ref_idx_l0/l1`, `mvd_coding` (`abs_mvd_greater0/1`, `abs_mvd_minus2` EG1 bypass, sign), `mvp_l0/l1_flag`.

### G. Transform tree and residual coding (~1,600)
- G1 **[ALG]** `split_transform_flag` (ctx `5 − log2TrafoSize`), inferred splits (max depth, intra N×N at depth 0, inter AMP/`max_transform_hierarchy_depth_inter == 0` rule), `cbf_cb/cr` (ctx = depth; chroma 4×4 carried by the parent for 4:2:0), `cbf_luma` (ctx `depth == 0`).
- G2 **[ALG]** `cu_qp_delta_abs/sign` at quantization-group granularity; **QpY prediction** from left/above within the CTB and the previous QG in decode order; chroma QP via the **[TBL]** `qPi → QpC` map for 4:2:0; `Qp′` with bit-depth offsets.
- G3 **[ALG]** `residual_coding`: `transform_skip_flag` (4×4 only), `last_sig_coeff_{x,y}_prefix` (ctx offset/shift by size and colour **[GEN]**) + suffix, **scan orders [GEN]** (diagonal / horizontal / vertical, chosen by intra mode for 4×4 and 8×8 luma, 4×4 chroma), `coded_sub_block_flag` (ctx from right/below), **`sig_coeff_flag` context derivation** (the 4×4 `ctxIdxMap[16]` **[TBL]**, the four neighbourhood patterns from `prevCsbf`, DC special case, luma/chroma/8×8 offsets), `coeff_abs_level_greater1_flag` (ctxSet from sub-block position and the previous sub-block's greater1 count, at most 8 per sub-block), `greater2` (one per sub-block), **sign data hiding** (parity over the sub-block when `lastSigScanPos − firstSigScanPos ≥ 4`), `coeff_abs_level_remaining` (Rice prefix/suffix with `cRiceParam` adaptation, escape beyond 3), sign bypass bins, level reconstruction with the hidden sign.
- G4 **[TBL]** `levelScale = {40,45,51,57,64,72}`; dequant with `bdShift`, scaling factor m (flat 16 or scaling list), clip to 16-bit **[ALG]**.

### H. Inverse transforms (~500)
- H1 **[TBL]** The 32×32 integer DCT matrix (4/8/16 are its sub-sampled rows). Validate: near-orthogonality, symmetry, `transMatrix[0][*] = 64`.
- H2 **[TBL]** 4×4 DST (`{29,55,74,84}`) for intra luma 4×4.
- H3 **[ALG]** Two-stage inverse: column pass, `>> 7` with 16-bit clip, row pass, `>> (20 − bitDepth)`; transform-skip (`<< 7` then the same shift); transquant bypass (residual is the level). Partial butterflies later (Phase 6); the matrix multiply is the oracle.

### I. Intra prediction (~800)
- I1 **[ALG]** Reference sample availability in z-scan order (`MinTbAddrZs`), `constrained_intra_pred` (inter neighbours unavailable), slice/tile boundaries; the **substitution process** (search up from bottom-left, fill forward).
- I2 **[ALG]** Reference filtering: `[1 2 1]` gated by size and mode distance (`intraHorVerDistThres = {7,1,0}` **[TBL]** for 8/16/32), **strong intra smoothing** (bilinear when the 32×32 edge is flat), never for chroma or 4×4.
- I3 **[ALG]** 35 modes: planar, DC (+ the DC edge filter for luma < 32), angular 2–34 with `intraPredAngle` / `invAngle` **[TBL]**, reference-array extension for negative angles, the mode-10/26 boundary smoothing (disabled by `disableIntraBoundaryFilter` rules), chroma mode derivation. Prediction is per **TU** inside the transform tree, not per PU — the ordering that trips every first HEVC decoder.

### J. Inter prediction (~1,800)
- J1 **[ALG]** **Merge**: spatial A1, B1, B0, A0, B2 with pairwise pruning and the parallel-merge-level (`log2_parallel_merge_level`) exclusions; **temporal** candidate (collocated bottom-right then centre, in the collocated picture from `collocated_from_l0/ref_idx`, long-term rules, **MV scaling** with `td/tb` distance scale factor **[GEN]**); **combined bi-predictive** candidates from the `l0CandIdx/l1CandIdx` table **[TBL]** (B slices); zero candidates; 8×4/4×8 converted to uni-pred.
- J2 **[ALG]** **AMVP**: spatial A (A0/A1, scaled and unscaled), B (B0/B1/B2, with the `isScaledFlag` rule), temporal, de-duplicate, pad with zero to two candidates.
- J3 **[ALG]** MV storage compressed to 16×16 for TMVP (`((x >> 4) << 4)` addressing); per-picture motion field kept in the DPB.
- J4 **[TBL]** Luma 8-tap quarter-pel filters `{−1,4,−10,58,17,−5,1,0}`, `{−1,4,−11,40,40,−11,4,−1}`, mirror; chroma 4-tap eighth-pel filters. **[ALG]** Separable MC with 14-bit intermediates (`shift1 = bitDepth − 8`, `shift2 = 6`, `shift3 = 14 − bitDepth`), bi-pred rounding, reference-picture edge clamping (equivalent to padding), 4:2:0 chroma MV = luma MV at 1/8 precision.
- J5 **[ALG]** Weighted prediction: explicit (`log2Wd`, `w0/w1`, offsets scaled by bit depth) and default; luma and chroma tables per reference.

### K. In-loop filters (~1,400)
- K1 **[ALG]** Deblocking on the 8×8 grid over TU **and** PU edges: Bs 2 (intra) / 1 (cbf, different refs or count, MV difference ≥ 4 quarter-pels) / 0; β and tC **[TBL]** indexed by `QpAvg` + slice offsets; luma strong/weak decisions (`d`, `dE`, `dEp`, `dEq`, the `tc` clip); chroma only at Bs 2; PCM and transquant-bypass samples untouched; `slice/tile_loop_filter_across` flags; **all vertical edges of the picture first, then horizontal** — the ordering is normative.
- K2 **[ALG]** **SAO** per CTB: `sao_merge_left/up`, type (band / edge / off) per component with chroma sharing, band position + 4 offsets (bit-depth-scaled), edge class (0°/90°/135°/45°) with 5 categories; applied to the **deblocked** picture while reading **unfiltered** neighbours — port as read-only input + separate output (the rav2d law: the reference's line buffers are an in-place artifact we do not need); disabled across slice/tile boundaries when the flags say so and on PCM/bypass CTBs with `pcm_loop_filter_disabled`.

### L. Parallel tools (~500)
- L1 **[ALG]** Tiles: CTB scan conversion (B5), availability limited to the tile, CABAC re-init and substream switch at tile boundaries (entry points), loop-filter-across-tiles.
- L2 **[ALG]** WPP: substream per CTB row, context sync from the row above (E3), availability unchanged.
- L3 **[ALG]** Slices and dependent slice segments: availability across slice boundaries (`SliceAddrRs`), filter-across-slices, dependent-segment CABAC continuation. v1 decodes substreams **sequentially**; threading is Phase 6.

### M. SEI, cropping, output (~400)
- M1 **[ALG]** SEI parsing: **decoded picture hash (132)** — MD5/CRC/checksum per plane, the standing self-gate; active parameter sets; recovery point; pic timing (`pic_struct`, for field flags surfaced as metadata, not decoded as fields); mastering display / content light level (137/144) parsed and carried on the `Stream` for HDR passthrough. Unknown payloads skipped by size.
- M2 **[GLUE]** Conformance-window cropping; `PixelFormat::Yuv420p` / `Yuv420p10`; `Stream` fields from SPS/VUI (dims, SAR, colour primaries/transfer/matrix, `nb_frames` unknown for raw streams).

### N. Container and transport plumbing (rff side, ~900)
- N1 **[GLUE]** `CodecId::Hevc` (`"hevc" | "h265" | "libx265"` names) in `rff-core`.
- N2 **[ALG]** `rff_format::hvc`: `hvcC` record (ISO 14496-15: configuration version, profile/tier/level, `lengthSizeMinusOne`, arrays of VPS/SPS/PPS NALs); `hvc1`/`hev1` sample entries → Annex-B (reuse the AVCC length-prefix walker, generalized); `build_hvcc_record` for the MP4/DASH muxer stream-copy path; **HEVC SPS dimension parse** (needs the exact `profile_tier_level` skip and conformance window) for the TS/RTP dims hooks that today call `avc::sps_dimensions`.
- N3 **[GLUE]** MP4/fMP4/DASH: demux `hvc1|hev1` (+ `hvcC`), mux stream-copy with `hvcC`; `codecs=` string `hvc1.1.6.L93.B0` for the DASH MPD; `MuxCaps` rows so `-show_targets` stays honest and `mux_caps.rs` drives the pair.
- N4 **[GLUE]** Matroska/WebM: `V_MPEGH/ISO/HEVC` (CodecPrivate = `hvcC`), demux + mux; WebM refuses it (restricted doctype).
- N5 **[GLUE]** MPEG-TS: stream type `0x24` in `codec_for` and `stream_type`; HEVC Annex-B PES; dims from the SPS; HLS inherits it.
- N6 **[ALG]** RTP **RFC 7798** depacketizer (2-byte NAL header, single NAL / AP / FU, DONL absent), `?pt=` pinned, in `rff-format-rtp` beside the RFC 6184 one; FLV/RTMP enhanced fourcc `hvc1` on input.
- N7 **[GLUE]** `rff-codec-hevc` adapter registered in `register_builtin_codecs` behind the `hevc` feature; `rffprobe` reports `hevc (Main 10), yuv420p10le, 3840x2160`.
- N8 **[GLUE, stretch]** HEIC: an `hvc1` item in the HEIF box tree `rff-format-avif` already parses → `rusty_hevc` intra decode → `PixelFormat` frame. iPhone photos are the single most common HEVC "product" a user will drop on `rff`.

Rough total: **~13k lines of codec + ~1k of plumbing**, against `rusty_h264`'s
26k-line decoder+common (which also carries CAVLC and an encoder's worth of
shared kernels). Expect the CABAC residual coding (G3), merge/AMVP (J1/J2) and
the DPB/RPS bookkeeping (D) to eat most of the bring-up calendar, in that
order — they are where every HEVC decoder's late bugs live.

---

## 4. The bricks — phases, each with its gate

Every phase is one or more sessions; every brick is one commit with its gate
in the message. Status: ☐ open · ☑ done · ✗ pruned (record why).

### Phase 0 — Foundation: measure before building (no decoder code)

| # | Brick | Adds | Gate | Status |
|---|---|---|---|---|
| H0.1 | **Corpus** | `scripts/fetch-hevc-vectors.sh` pulling the JCT-VC **HEVC_v1** conformance bitstreams (147 streams, bitstream + md5 only, not the yuv) from the ITU `jctvc-site/bitstream_exchange/draft_conformance/HEVC_v1/` tree into `hevc-vectors/`; class = the name prefix | script is idempotent, sha256 per file recorded | ☑ 2026-09-05 |
| H0.2 | **Harness** | `tools/hevc/conform.py`, decoder-agnostic (any `<in.bit> <out.yuv>` command): published-md5 + SEI picture-hash per output picture; `tools/hevc/verdict.py` folds runs into the class table; a **self-test** (a known-good stream must pass, a corrupted one must fail — the rusty_zstd harness law). The Rust `crates/rff/tests/hevc_conformance.rs` twin lands with H1.4 once `CodecId::Hevc` exists | self-test both directions | ☑ 2026-09-05 |
| H0.3 | **Oracle build** | HM built with clang/cmake in `../_ref_hm` (BSD-3, next to `_ref_libvpx`/`_ref_x264`); env-gated CABAC + semantic probes patched in and the **patch kept in-tree**; one deterministic trace per class committed as a fixture | trace reproducible run-to-run, line-for-line | ☐ (needed only on the build path, or for the fork's first desync) |
| H0.4 | **Ecosystem verdict** | Run `rust_h265` 0.1.0 and `hpvcd` 0.3.2 under H0.2 | a per-class pass table for each; **decision rule:** ≥ 90 % of the v1 suite bit-exact + permissive + reviewable → **fork it** (the rav1d → rusty_av2d precedent) and the plan's Phases 2–4 become gap-closing; else **build** from HM/spec. Record either verdict here with the numbers | ☑ 2026-09-05 — **hpvcd 147/147, rust_h265 42/147, ffmpeg 141/147**; rule said fork, **Tim chose BUILD** (§8 Q2) |
| H0.5 | **Skeleton** | `crates/rusty_h265` (workspace member, `forbid(unsafe_code)`, zero deps, `src/bin/rusty_h265.rs` = the harness front end `<in.bit> <out.yuv>`), README + `prof.rs` port, `crates/rff-codec-hevc` stub, `CodecId::Hevc`, `hevc` feature in `rff`; claim `rusty_h265` + `rusty_hevc` on crates.io (0.0.1, wave 0) | `cargo build --workspace`, `cargo deny`, names claimed | ☑ 2026-09-05 crate + bin + README + `rff-codec-hevc` + `CodecId::Hevc` landed; crates.io claim open |
| H0.6 | **Posture** | §8 Q1 answered and written into `compatibility.md` (replace "licensing posture undecided") | doc line landed | ☐ |

### Phase 1 — Parse everything, decode nothing

| # | Brick | Adds | Gate | Status |
|---|---|---|---|---|
| H1.1 | NAL + parameter sets | A1–A3, B1–B5 | every v1 stream parses VPS/SPS/PPS; dims/profile/bit-depth match `ffprobe`; PTL skip exact on the multi-sub-layer streams (`TSCL`, `NUT`) | ☑ 2026-09-05 (`nal.rs`, `ps.rs`; 147/147, `tests/conformance_parse.rs` vs `hevc-vectors/probe.json`) |
| H1.2 | Slice headers + entry points | C1 | slice count and `slice_segment_address` per picture equal HM's; entry offsets land on byte boundaries | ☑ 2026-09-05 (`slice.rs` incl. pred-weight table, RPL modification, dependent segments; every slice header of the suite parses to its `byte_alignment()`; entry-point→RBSP mapping via the recorded EPB positions is exercised in Phase 2/4) |
| H1.3 | POC / RPS / DPB dry run | D1–D4 without pixels (empty pictures) | output **order** and picture count equal `ffmpeg -f framemd5` on `POC_A`, `RPS_A–F`, `LTRPS`, `RAP_A/B`, `BUMPING`, `NoOutPrior`, `CONFWIN`; missing-ref generation exercised by `RAP_B` | ☑ 2026-09-05 (`decoder.rs`: §8.3.1 POC, §8.3.2 marking, §8.3.3 generation, §8.3.4 lists, §C.5.2 bumping; **picture count = reference on 147/147**, order is checked by the md5 from H2.4 on) |
| H1.4 | `rffprobe` | N1, N2 (parse half), N3–N5 demux | `rffprobe` shows the HEVC stream of an MP4, MKV and TS that ffmpeg made; `-c copy` remux MP4→MKV→TS→MP4 and ffmpeg decodes each hop | ☐ |

### Phase 2 — Intra pictures, bit-exact

| # | Brick | Adds | Gate | Status |
|---|---|---|---|---|
| H2.1 | CABAC engine + init | E1–E3 | the rusty_h264 engine + HEVC's 157-context init (Tables 9-5…9-37, transcribed from HM `ContextTables.h`) | ☑ 2026-09-05 (`cabac.rs`) |
| H2.2 | Quadtree + intra syntax | F1–F2 | whole-picture parity; the 4×4 z-scan availability map (`pic.rs`) is the neighbour oracle | ☑ 2026-09-05 (`ctu.rs`) |
| H2.3 | Transform tree + residual coding | G1–G4 | `TUSIZE_A`, `TSKIP_A`, `SDH_A`, `DELTAQP_A–C`, `SLIST_A–D` all bit-exact | ☑ 2026-09-05 (`ctu/residual.rs`) |
| H2.4 | Inverse transforms + intra pred → first pixels | H1–H3, I1–I3 | **SEI MD5 match** across the whole intra set; the 32×32 DCT is generated from the 33 unique magnitudes and validated for orthogonality | ☑ 2026-09-05 (`itx.rs`, `intra.rs`, `tables.rs`) |
| H2.5 | Deblocking | K1 | `DBLK_A–G` (incl. `DBLK_A_MAIN10`) bit-exact | ☑ 2026-09-05 (`filters.rs`) |
| H2.6 | SAO | K2 | `SAO_A–H` and `SAODBLK_A/B` bit-exact | ☑ 2026-09-05 (`filters.rs`) |

### Phase 3 — Inter pictures

| # | Brick | Adds | Gate | Status |
|---|---|---|---|---|
| H3.1 | Inter syntax + AMVP + MC | F3, J2, J4 | `AMVP_A–C`, `MVCLIP_A`, `MVEDGE_A`, `MVDL1ZERO_A` bit-exact | ☑ 2026-09-05 (`mc.rs`, `ctu/inter.rs`) |
| H3.2 | Merge (spatial + combined + zero) | J1 spatial/combined | `MERGE_A–G`, `PMERGE_A–E` bit-exact | ☑ 2026-09-05 |
| H3.3 | TMVP + motion compression | J1 temporal, J3 | `TMVP_A`, `POC_A` bit-exact; the motion field is compressed to 16×16 when the picture finishes | ☑ 2026-09-05 |
| H3.4 | Weighted prediction | J5 | `WP_A/B` and `WP_*_MAIN10` bit-exact, 256/256 pictures each | ☑ 2026-09-05 |
| H3.5 | DPB with real pixels | D2 missing refs, D4 | every `RAP`, `RPS`, `LTRPSPS`, `BUMPING`, `NOOUTPRIOR`, `OPFLAG`, `NUT`, `CIP`, `RPLM`, `STRUCT` and `EXT` stream bit-exact | ☑ 2026-09-05 |

### Phase 4 — Slices, tiles, WPP, the whole suite

| # | Brick | Adds | Gate | Status |
|---|---|---|---|---|
| H4.1 | Multiple slices + dependent segments | L3 | `SLICES_A`, `DSLICE_A–C`, `SLPPLP`, `VPSID_A`, `PPS_A`, `FILLER_A`, `HRD_A` bit-exact | ☑ 2026-09-05 |
| H4.2 | Tiles | L1 | `TILES_A/B` and `ENTP_A–C` bit-exact | ☑ 2026-09-05 |
| H4.3 | WPP | L2 | all twelve `WPP_*` streams bit-exact | ☑ 2026-09-05 |
| H4.4 | **Full suite** | — | **147/147 bit-exact (100 %), SEI picture hashes 100/100**, `TSUNEQBD_A_MAIN10` included. In-process gate: `crates/rusty_h265/tests/conformance_md5.rs`. The CI job is the one piece still open | ☑ 2026-09-05 |

### Phase 5 — The product paths

| # | Brick | Adds | Gate | Status |
|---|---|---|---|---|
| H5.1 | Adapter + registration | N7, extradata handling (`hvcC` → VPS/SPS/PPS prepend) | `crates/rff-codec-hevc` + `CodecId::Hevc` + `rff_format::hvc`; MP4/Matroska/TS demux and decode **pixel-identical to ffmpeg**, `-c:v h264` transcode works; Main 10 needed a new high-bit-depth→ 8-bit narrowing in `rff-filter` | ☑ 2026-09-05 |
| H5.2 | Muxing on stream copy | N2 `build_hvcc_record`, N3–N5 mux halves, `MuxCaps` | `mux_caps.rs` + `targets_end_to_end.rs` cover HEVC; ffmpeg 8.1.2 decodes our MP4/MKV/TS/HLS/DASH output | ☐ |
| H5.3 | RTP + FLV input | N6 | `ffmpeg -f rtp` → `rff` receives RFC 7798 (single/AP/FU) frame-exact; Janus `rtp_send --hevc` when a chip encoder exists | ☐ |
| H5.4 | HEIC still (stretch) | N8 | an iPhone `.heic` → `rff -i x.heic out.png`, pixels equal `ffmpeg`/libheif | ☐ |
| H5.5 | Docs truth-sync | `compatibility.md`, `ffmpeg-parity.md` (10/10 pure-Rust decoders), `readme.md` codec table, crate README with the hardening table (`use-protection-please`) | docs match `rff -codecs` | ☐ |

### Phase 6 — Robustness, then speed (only after Phase 4 is green)

| # | Brick | Adds | Gate | Status |
|---|---|---|---|---|
| H6.1 | Never-panic | mutation fuzzer with a **seed per NAL type and per decode path** (I/P/B, tiles, WPP, PCM, lossless, 10-bit), every entropy-coded loop bounded, DPB and picture-size allocations capped at the level-6.2 limits (8192×4320, 6 pictures), `catch_unwind` at the adapter | CI `hevc decode fuzz` job, 50 k malformed streams, 0 panics/hangs | ☐ |
| H6.2 | Instrument | rdtsc stage profiler (parse / CABAC / intra / MC / itx / deblock / SAO / DPB), a deterministic in-process bench on a committed 720p stream, the kernel-reach census | method line printed; profile-OFF wall reconciles with profile-ON minus tax | ☐ |
| H6.3 | Standing vs ffmpeg | pinned CPU time, `-threads 1`, ABBA, null arm, work counts (pictures, CTBs) equal both arms | an honest ratio in `benchmarks.md`, expected far behind — VP9's lesson says MC + deblock + SAO will be memory-bound | ☐ |
| H6.4 | Kernels | SIMD twins (8-tap MC, deblock, SAO, partial-butterfly IDCT), u8 plane domain for 8-bit content, in `rusty_hevc-accel` behind the seam, **default-on via runtime dispatch** (§2.6); each `*_matches_scalar`, byte-identical on the suite, NEON sibling or a written note | keep only what clears the noise floor; revert-if-flat; census reads ~100 % kernel reach on the suite | ☐ |
| H6.5 | Threads | WPP rows / tiles as the parallel unit (HEVC designed them for this), frame threading second | byte-identical to sequential on the suite; cores-busy printed | ☐ |

---

## 5. Standing gates (never removed once green)

- **SEI picture-hash self-check** on every decoded picture in tests (`HEVC_VERIFY_SEI=1`; the equivalent of `VP9_RECON_CHECK`).
- **HEVC_v1 conformance** — `cargo test -p rusty_h265 --release` runs all 147 streams in process; full suite locally before any release, cached subset per PR in CI (`hevc conformance (bit-exact)`, still to add).
- **ffmpeg framemd5 cross-check** on the container paths (MP4/MKV/TS) — catches output-order and cropping drift the SEI hash cannot see.
- **Never-panic fuzz** in CI.
- `cargo deny` (BSD-3 HM text transcribed, no LGPL source ever opened), `cargo test --workspace --exclude rff-ui`.
- **Float-free core lint** (`deny(clippy::float_arithmetic)` + the `f32`/`f64`/`libm` grep) and, from Phase 6 on, the **kernel-reach census** printed by the conformance run (§2.6).
- **The recon oracle for a future encoder** is this decoder: build it so `decode(bitstream) == recon` can be asserted by an encoder later (public per-picture reconstruction access, no hidden state).

---

## 6. Sizing and sequencing

`rusty_h264`'s decoder went from nothing to 35/35 conformant CAVLC in about
three weeks and to CABAC I/P/B pixel-exact in another five, alongside an
encoder (`../rs_h264`, 537 commits 2026-06-24 → 08-28). HEVC has no CAVLC, one
entropy coder and one intra/inter/filter design instead of H.264's profile
menu, but roughly twice the syntax surface (quadtrees, 35 intra modes, merge/
AMVP/TMVP, SAO, tiles/WPP, RPS). Working estimate, one engineer-session at a
time: **Phase 0 one session; Phases 1–2 two to three weeks; Phase 3 two
weeks; Phase 4 one week; Phase 5 one week; Phase 6 open-ended and gated.** H0.4 found a fork-worthy crate; Tim chose to build anyway (§8 Q2), so the
full calendar applies. Phase 1 took one session (2026-09-05).

Order of the phases is fixed. Inside Phase 2, do not touch deblocking until the
filter-less intra class is 100 % — the per-stage isolation oracle (feed each
filter the reference's previous-stage output) is how rav2d went 99 → 100.

---

## 7. Decisions that bind

1. **Pure safe Rust core, `#![forbid(unsafe_code)]`, acceleration behind a seam** — the `rusty_h264` shape. No C, no FFI, ever; hardware decode is a non-goal ([compatibility.md](../compatibility.md#hardware-acceleration--a-decision-not-an-omission)).
2. **Sources:** ITU-T H.265 and HM (BSD-3). FFmpeg, libde265, openHEVC are black-box oracles. Say so in the README.
3. **Standalone crate first** (`crates/rusty_h265`, later its own repo), thin `rff-codec-hevc` adapter, container code in `rff-format*`. Wave-0 name claim (`rusty_h265` + `rusty_hevc`).
4. **Conformance before speed.** No SIMD, no threads, no `unsafe` until Phase 4 is green; then `codec-measurement` rules every number — and once a kernel lands it is the **primary, default-on** path with the scalar twin as oracle (§2.6). The core never contains a float.
5. **v1 scope = HEVC version 1, Main / Main 10 / Main Still Picture, 4:2:0, ≤ 10 bit.** RExt/SCC/SHVC/MV-HEVC/3D are parsed-and-refused with a named error, never half-implemented.
6. **Decode only.** An HEVC encoder is a separate mission with a separate patent conversation (encode exposure is higher than decode; x265 is GPL and cannot be read).
7. **Allocator:** the adapter takes `rusty_hevc` with `default-features = false`; `#[global_allocator]` stays in `rff-cli` only.
8. **Patent posture:** ship-and-document, isolated, feature-gated (§2.4). Default-on is §8 Q1.

---

## 8. Open questions for Tim

- **Q1 — default-on?** H.264 and AAC are on by default with the ship-and-document posture. Same for HEVC (`default = ["h264-asm", "hevc"]`), or opt-in (`--features hevc`) so the default artifact stays out of the HEVC pools? Recommendation: **same as H.264** — a drop-in `ffmpeg` that cannot open a phone video is not a drop-in — with the feature there for anyone who needs the clean build.
- **Q2 — fork or build? DECIDED 2026-09-05: BUILD.** Tim: "build our rusty_h265 from scratch instead of forking theirs — I don't want to sit around fixing someone else's problems." The measurement stands (hpvcd 147/147 is the proof that 100 % is reachable in pure Rust, and it stays a black-box second oracle next to ffmpeg), but the code is ours from the spec + HM. Phases 1–4 are real bricks again; the crate is **`crates/rusty_h265`** (Tim's name, matching `rusty_h264`; `rusty_hevc` stays reserved as the alias to claim).
- **Q3 — HEIC in v1?** N8 is cheap once the intra decoder exists and is the most user-visible HEVC "product". Recommend in, as a stretch brick.
- **Q4 — repo split timing:** graduate `rusty_hevc` to `Remade-With-Rust/rusty_hevc` at Phase 4 (conformant) or at Phase 5 (usable)? `rusty_flac`'s lesson: portfolio repos ship fresh history, so it costs nothing to wait.

---

## 9. Ledger

| date | phase/brick | result | evidence |
|---|---|---|---|
| 2026-09-05 | plan | written; ecosystem surveyed; crate names verified free; ffmpeg 8.1.2 + clang/cmake present as oracle/toolchain | this document |
| 2026-09-05 | H0.1 corpus | **done** — all 147 JCT-VC HEVC_v1 streams fetched (129 MB zipped; bitstream + published yuv md5 only), `scripts/fetch-hevc-vectors.sh`, sha256 per file; three MainConcept zips ship the md5 as `_md5.txt` | `hevc-vectors/` (gitignored) |
| 2026-09-05 | H0.2 harness | **done** — `tools/hevc/conform.py` (published-md5 + SEI picture-hash MD5/CRC/checksum, decoder-agnostic, `--self-test` passes both directions). Self-test caught its own bugs: `-f hevc` needed for `.bit`, `-fps_mode` after `-i`, CRC = augmented form ≡ table CRC init 0x1D0F, checksum needed vectorising (bit-loop stalled 1080p for hours), `fsz==0` on TSUNEQBD | `tools/hevc/README.md`, `hevc-vectors/results/*.txt` |
| 2026-09-05 | north star | **ffmpeg native `hevc`, `-threads 1`, pinned CPU time**: 8-bit 720p **186 Mpx/s** (297 ms / 60 f), 10-bit **142 Mpx/s**; null arm = reference exactly. rust_h265 7.0× / 6.8× behind, hpvcd (1 thread, SIMD on) 5.95× / 5.5× behind | `tools/hevc/pinbench.ps1`, README standing table |
| 2026-09-05 | H0.4 verdict | **hpvcd 0.3.2: 147/147 bit-exact (100 %)**, SEI 100/100, incl. the 6 streams ffmpeg fails (NUT_A, RPS_D, SAODBLK_A/B, TSUNEQBD, VPSSPSPPS_A). **rust_h265 0.1.0: 42/147 (28.6 %)** — 25 no output, 48 decode errors, 32 silent mismatches; passes intra/RQT/MAXBINS, fails every DBLK/WPP/RPS/WP/PICSIZE/TILES class. **ffmpeg 141/147 (95.9 %)** — the north star is not a 100 % oracle. Decision rule (≥ 90 % + permissive + reviewable) selects **FORK hpvcd** (BSD-3 OR Apache-2.0, zero deps, 50 k lines, 151 in-src tests; 280 `unsafe` sites of which all but 4 files sit in `avx/ sse/ neon/`; no fuzz; MSRV 1.93; already carries SCC/4:2:2/4:4:4). Tim to confirm (§8 Q2) | `tools/hevc/verdict.py` over `hevc-vectors/results/*.json` |
| 2026-09-05 | H0.5 + Phase 1 | **Q2 decided: BUILD** (`crates/rusty_h265`, 0.0.1, zero deps, `forbid(unsafe_code)`). Same day: `bits.rs`/`nal.rs` (ported from rusty_h264, 2-byte header, EPB positions kept for entry points), `ps.rs` (VPS/SPS/PPS, PTL, scaling lists incl. Table 7-6 defaults, st_ref_pic_set inter-RPS prediction, VUI/HRD skip, tile scan tables), `slice.rs` (full header incl. LT pics, RPL modification, pred-weight table, entry points, dependent segments), `decoder.rs` (POC, RPS marking, missing-ref generation, RefPicList0/1, C.5.2 bumping), `frame.rs`, harness bin. **H1.1–H1.3 gates green on 147/147** (`cargo test -p rusty_h265 --release`). Bugs the gate caught: EOS must not output (C.5.2 gives it no output semantics — NoOutPrior_A dropped 3 pictures, RAP_B 3), level bound is MaxLumaPs not 8192×4320 (PICSIZE_A/B are 1056×8440), VPSSPSPPS_A is six resolutions in one stream | `crates/rusty_h265/tests/conformance_parse.rs`, `tools/hevc/probe_all.py` → `hevc-vectors/probe.json` |
| 2026-09-05 | H2–H4 **decoder complete** | **147/147 HEVC_v1 bit-exact (100 %), SEI picture hashes 100/100** — including the six streams ffmpeg itself fails. Written from the spec + HM in one session: CABAC (157 contexts), quadtree/CU/PU, residual coding, DST/DCT, 35 intra modes, merge/AMVP/TMVP, weighted prediction, PCM, lossless, transform skip, scaling lists, sign data hiding, deblocking, SAO, slices, dependent segments, tiles, WPP. HM (BSD-3) built at `../_ref_hm`; `tools/hevc/hm-oracle.patch` holds the boundary-strength / filter-decision / per-CU dumps that localised every desync | `hevc-vectors/results/rusty_h265.{json,txt}`, `crates/rusty_h265/tests/conformance_md5.rs` |
| 2026-09-05 | speed, first look | `pinbench.ps1` with our arm: 720p 8-bit, pinned CPU time — ffmpeg 344 ms (161 Mpx/s), **ours 4,172 ms (13 Mpx/s) = 12.1× behind**; rust_h265 6.7×, hpvcd 6.3×. Expected: `u16` planes for 8-bit content, a `Vec` allocation per PU per plane in MC, no SIMD. Phase 6 territory | `tools/hevc/README.md` standing table |
| 2026-09-05 | H7 Great Gate | **Full content-adaptive route inventory deployed** (`docs/plans/rusty_hevc_gates.md`): 24 routes across MC / residual / intra / loop filters / picture capability, each instrumented with a population counter and measured over an 11-stream corpus chosen to cover capability classes, not just the common one. Decoder gates are pure speed and bit-exact by law, so the finish line is **capability × population — a population served by a slow path is a missing kernel**. **Two missing arms found and built:** explicit weighted prediction (no kernel at all; serves 100 % of MC on `WP_A_Toshiba_3` — 33.9 M samples — and both uni and bi turn out to be one `pmaddwd` each: **1.300×, 18/20, z = 3.58**) and transform skip (65 % of transform blocks on `DBLK_A_MAIN10`, **0.500 instr/output vs ~5 scalar**, below the clock's resolution so judged by counter per §15). **★ Found a DEAD CENSUS COUNTER that had produced a wrong prune:** `PUT_WEIGHTED` was declared and never incremented, and reading it as 0 had earlier justified *not* building the weighted kernel — §10's "a flat arm is not evidence until the arm is wired", turned on the census itself. Also corrected: the `substitute` short-circuit's claim that picture interiors need no substitution — the population says **69–72 % of intra blocks do**. 147/147 bit-exact | `docs/plans/rusty_hevc_gates.md` |
| 2026-09-05 | H6.5 divides + allocator | **★ The whole campaign had been measured under the WRONG ALLOCATOR.** CLAUDE.md requires every performance measurement to run under `rusty_alloc` (what `rff-cli` ships); the standalone `rusty_h265.exe` bench binary had no `#[global_allocator]` and ran under the Windows system heap. Measured paired: **rusty_alloc is 1.133× faster here (13/15, z = 2.84)** — bigger than most wins being measured against it. Instruction counts are unaffected (static); clock numbers understated the product and overstated the allocation-removal compositions. Fixed with an optional `bench-alloc` feature (optional so the published lib stays dependency-free). Then a `rusty-fast-transcendentals` sweep: **the decoder contains zero floats and zero transcendentals** — integer by construction — but the integer twin of a libm call is `idiv`, which x86 has no SIMD form of. **18 hardware divides → 5**: `32 / n` in `idct_1d` (power of two), two `% run` masks, the §8.5.3.2.8 MV-scaling division (`td` is clamped to 256 values → compile-time table), and seven `rs / ctb_w` / `rs % ctb_w` (per-picture reciprocal, `debug_assert`ed per call and unit-tested across every conforming `ctb_w`). 147/147 bit-exact | `docs/plans/rusty_hevc_kernels.md` |
| 2026-09-05 | H6.4 compositions | **10 composition wins (31–40)**, all bit-exact. Two algebraic, each pinned by a test against the composition it replaces: mixed bi-prediction folds its `copy_shift` into the bi write (`put_bi_fp`, 3,441 blocks), and DC prediction fuses with the residual add. The other eight are one observation eight times: **a buffer whose lifetime is a call but whose size is a block** — the inverse transform's stage-1 scratch was 4 KB zeroed *per transform block* (197 MB per 60 frames), `RefSamples` 388 B per intra block, a `Vec` per inter CU to iterate ≤4 partitions, four `Vec`s per PU for spec-bounded candidate lists. Found by reading for allocations, not hot loops | `docs/plans/rusty_hevc_kernels.md` §"A third pass" |
| 2026-09-05 | H6.3 second instruction pass | **10 more deterministic wins (21–30)**, all bit-exact on 147/147. Two are compositions rather than kernels: **full-pel uni-prediction is the identity** (`copy_shift` writes `s << k`, `put_uni` computes `(v + 2^(k−1)) >> k` with the *same* `k`, so the pair is `s`) and **full-pel bi-prediction is `pavgw`** — an integer motion vector needs a rectangle copy or a rounding average, not two kernel passes (0.781 → ~0.1 and 1.281 → ~0.25 per sample; census: 616 uni + 1,575 bi blocks on the bench stream). The rest pair the vector loop so its three bookkeeping instructions are paid per two vectors: `angular_sse2` −27 %, `planar_avx2` −25 % (its narrowing half-filled a store), `copy_shift` −25 %, `put_uni_sse2` −17 %. **One probe refuted and recorded** (slice-copying the intra reference buffer: 980 → 981 instructions, no win) and **one kernel deliberately not written** (`PUT_WEIGHTED = 0` on the corpus — unreachable code is the defect this discipline exists to catch) | `docs/plans/rusty_hevc_kernels.md` §"A second pass" |
| 2026-09-05 | H6.2 instruction pass | **20 deterministic instruction-reducing wins across the kernels**, all bit-exact (147/147, SEI 100/100) and every one verified by `tools/hevc/kernel_icount.py` — a new standing instrument that counts instructions in each kernel's innermost vector loop straight out of the emitted assembly and divides by the samples that loop stores. Below a ±3 % null floor the clock cannot judge, so the counter is the evidence and the clock only confirms. Highlights: the angular filter never leaves `i16` (`(32−f)a + fb = 32a + f(b−a)`, `angular_t` 178 → 127); SAO band and edge offsets become one `pshufb` because the bands are consecutive and `edgeIdx` is already an index (−5, −11); counted loops leave LLVM one induction variable instead of three (−6 on every FIR variant); the vertical FIR emits two rows per pass so 9 loads replace 16; AVX2 angular/planar/transposed-angular where there were none (−58 %, −68 %, −33 %). **Two of the twenty were defects, not optimisations: the full-pel copy path — integer motion vectors — had no kernel at all, and `dc_fill` emitted zero vector instructions.** Both found by reading the assembly, neither by profiling | `docs/plans/rusty_hevc_kernels.md` §"Instruction-level pass" |
| 2026-09-05 | H6 kernels | **`crates/rusty_h265-accel` built from scratch** — scalar twin + SSE2/AVX2/NEON, runtime dispatch, a `*_matches_scalar` oracle per kernel, and a deterministic byte census — then deployed across a written census of all 33 scalar implementations. Six bricks, each conformance-gated *before* its number and measured pinned / ABBA / CPU-time against a **1.000× null arm (z = −0.22)**: MC structure+kernels **1.306×** (21/21, z 4.58), sparse inverse transform **1.133×** (13/13, z 3.61), intra availability 1.069× on all-intra (11/15, z 1.81), SAO structure **1.282×** (13/13, z 3.61), SAO band+edge kernels **1.136×** (19/21, z 3.71), intra prediction kernels **1.068×** isolated (21/24, z 3.67) and 1.031× on all-intra decode (16/21, z 2.40) with no inter regression (z −0.50). **3,750 → 828 ms best (≈4.5×)**; vs ffmpeg **7.00× → 3.42×** (8-bit), 6.37× → 3.84× (10-bit); **now 1.63× faster than `hpvcd`**, the C++ decoder the original decision rule said to fork. 147/147 still bit-exact, SEI 100/100. The list is now closed with arithmetic: everything still scalar is priced in the kernels doc against a ±3 % floor, and the largest remaining item is the 29 % parse stage, where no kernel applies | `docs/plans/rusty_hevc_kernels.md`, `tools/hevc/README.md` standing table |
| 2026-09-05 | H5.1 product path | `CodecId::Hevc`, `rff-codec-hevc`, `rff_format::hvc` (hvcC parse/build + SPS reader), MP4 `hvc1`/`hev1`, Matroska `V_MPEGH/ISO/HEVC`, TS stream type 0x24 with SPS-derived geometry. `rff -i x.{mp4,mkv,ts}` decodes **pixel-identical to ffmpeg**; Main 10 needed `narrow_to_8` in `rff-filter` (within 1 LSB of swscale's dithered output) | round-trips in `hevc-vectors/.tmp` |

## Brick log (append before/after per brick)

*(empty — starts with H0.1)*

- **2026-09-05 H0.5/H1.1–H1.3** — before: nothing in-tree. After: `crates/rusty_h265` parses the whole v1 suite and reproduces the reference output picture count on 147/147 with an empty pixel pipeline.
- **2026-09-05 H2–H5.1** — before: parse only. After: **147/147 bit-exact** and wired into the CLI. The four bugs that cost real time, every one found by diffing against HM rather than re-reading the spec:
  1. **An internal prediction-unit edge is not a transform edge.** Marking both from one flag let the deblocking cbf rule fire on prediction boundaries — a ±1 haze over every inter picture.
  2. **A dependent slice segment continues its slice's `qPY_PREV`.** Resetting it to `SliceQpY` at each segment start desynced the QP through the tiled `DBLK_D`.
  3. **The quantization group is the coding-tree node of `Log2MinCuQpDeltaSize`, not every ancestor.** Consuming the slice/tile/wavefront reset at the CTB node and then re-predicting from the previous CTB's last coding unit broke the first quantization group of every wavefront row (12 streams, 4–5 pictures each).
  4. **The B2 merge candidate is gated on how many candidates survived pruning**, not on raw neighbour availability.
  The method that found all four: the parse proves itself (entry-point offsets land exactly on the substream boundaries), so a byte-exact parse with wrong pixels localises the bug to derivation rather than entropy decoding.
- **2026-09-05 H6 (kernels)** — before: 3,750 ms, `forbid(unsafe_code)`, every pixel loop scalar. After: 828 ms, five kernel bricks landed, 147/147 still bit-exact. `unsafe` is confined to `rusty_h265-accel`'s `#[target_feature]` bodies; `rusty_h265` itself still carries no `unsafe`, and `--no-default-features` builds the whole decoder with `forbid(unsafe_code)` on the accel crate too. The three things that decided the outcome, none of them intrinsics:
  1. **Structure paid more than width, in that order every time.** Step 0 of `codec-vectorize-kernel` says eliminate redundancy first, and the ledger agrees: an edge-padded footprint (removing a per-tap-per-sample coordinate clamp), reusable scratch (removing a `Vec` per prediction unit per plane), the last-significant-coefficient rectangle bounding the inverse transform, per-4×4 availability instead of per-sample, and an interior/ring split in SAO. Each of those *then* left a flat rectangle for a kernel to take. The SAO restructure alone (1.282×) beat the SAO kernels it enabled (1.136×).
  2. **A byte census, not the call graph, is what proves an arm is wired.** The MC brick's first reading was 3/9 wins, z = −1.00 — a flat refutation — because the "scalar" arm was running SIMD: **Cargo ignores `default-features = false` on an inherited workspace dependency unless the workspace entry declares it too.** Recording that reading would have permanently deleted the campaign's largest win.
  3. **The ablation ladder is only valid for arms that cannot change an earlier stage's work — and even then it does not resolve.** `no intra` measured a *negative* share, which is the instrument asking for help: the loop filters do data-dependent work, so ablating a pixel stage changes how much filtering happens downstream of it. Fixing the validity (measure pixel stages against a no-loop-filter baseline) was not enough — the unpaired min-of-N ladder still read two *identical* configurations 5.7 % apart and still called `no intra` −21.6 %. The paired harness answered the same question at 1.018×, z = 1.09. **Any share under ~20 % has to come from the paired instrument.**
  4. **Two more instrument faults, both of which would have changed a verdict.** A null arm on a 300 ms stream read **z = −2.40 for identical code**, because Windows CPU time quantises to ~15.6 ms and the harness scored every exact tie as a win for arm A; ties are now excluded and the workload lengthened to ~13 s. And the first intra kernel measured flat twice (z = 0.30) because it predicted into a `[u16; 32*32]` stack temp — a 2 KB memset per call, which on a 4×4 block is 128 bytes of memset per predicted pixel. Moving the transpose inside the kernel (an 8×8 tile of scratch) turned the same brick into z = 3.67. Three probes, and the first two were measuring the arm rather than the idea.
