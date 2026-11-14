# Changelog

All notable changes to this project will be documented in this file.

The format is based on [Keep a Changelog](https://keepachangelog.com/en/1.0.0/),
and this project adheres to [Semantic Versioning](https://semver.org/spec/v2.0.0.html).

## [Unreleased]

## [0.2.1](https://github.com/Remade-With-Rust/remade_ffmpeg_rs/compare/rusty_vp9-v0.2.0...rusty_vp9-v0.2.1) - 2026-11-14

### Other

- release v0.3.14

## [0.2.0](https://github.com/Remade-With-Rust/remade_ffmpeg_rs/compare/rusty_vp9-v0.1.3...rusty_vp9-v0.2.0) - 2026-11-13

### Fixed

- *(rusty_vp9)* keep fdct8x8_avx2_matches_scalar x86_64-only

### Other

- *(rusty_vp9)* version 0.2.0
- *(rusty_vp9)* 0.2 performance in the READMEs and benchmarks page
- *(rusty_vp9)* quiet two non-x86 warnings from the campaign
- *(rusty_vp9)* rustfmt
- *(rusty_vp9)* decode_coefs cuts its scan tables to the block size
- *(rusty_vp9)* TM intra predictor works on row slices
- *(rusty_vp9)* V intra predictor copies a row at a time
- *(rusty_vp9)* DC and H intra predictors fill a row at a time
- *(rusty_vp9)* zero-pad the inverse-transform row scratch only where it is read
- *(rusty_vp9)* target_feature on the inverse-transform add kernels
- *(rusty_vp9)* read the VP9_GATEPROBE switch once, not per 16x16 block
- *(rusty_vp9)* always inline the bool decoder
- *(rusty_vp9)* decode_coefs indexes its caches without bounds checks
- *(rusty_vp9)* 4-wide vertical edges store back only the samples that change
- *(rusty_vp9)* per-plane transform size from a table, not a block-size search
- *(rusty_vp9)* loop-filter mask build takes one mode-info row slice per row
- *(rusty_vp9)* loop-filter selection loops jump between set mask bits
- *(rusty_vp9)* skip the filter16 sums when no lane of a wide edge is flat2
- *(rusty_vp9)* loop-filter kernel per (width, orientation), AVX2 decided per frame
- *(rusty_vp9)* gather only the edge-tile window the filter will read
- *(rusty_vp9)* gather the motion-compensation edge tile a row at a time
- *(rusty_vp9)* fixed-width copies for full-pel prediction
- *(rusty_vp9)* vertical 8-tap kernel keeps a ring of unpacked row pairs
- *(rusty_vp9)* block-level vertical 8-tap kernel, 16 samples per step
- *(rusty_vp9)* block-level horizontal 8-tap kernel, 16 samples per step
- *(rusty_vp9)* block snapshots hold their context spans inline
- *(rusty_vp9)* AVX2 narrowing of decoded 8-bit planes
- *(rusty_vp9)* build coded planes a row at a time
- *(rusty_vp9)* pred_sse hoists its per-call invariants
- *(rusty_vp9)* whole-window interior fast path for the diamond search SAD
- *(rusty_vp9)* separable clamp in the integer-search edge path
- *(rusty_vp9)* keep the winning level's filtered luma
- *(rusty_vp9)* build the luma loop-filter masks once per level search
- *(rusty_vp9)* whole-frame luma SSE through the SSE2 kernel
- *(rusty_vp9)* integer sub-block candidates use the u8 4x4 scorer too
- *(rusty_vp9)* fused u8 4x4 scorer for sub-pel sub-block candidates
- *(rusty_vp9)* sub-8x8 exhaustive search stops rescoring positions
- *(rusty_vp9)* inline clamp_mv_umv
- *(rusty_vp9)* rate tracker no longer pre-fills its token cache
- *(rusty_vp9)* zero-sized count sink for the coefficient costing walk
- *(rusty_vp9)* build the magnitude-cost table at compile time
- *(rusty_vp9)* literal bit-cost table instead of a OnceLock per cost_bit
- *(rusty_vp9)* find the last non-zero coefficient row bottom-up
- *(rusty_vp9)* SSE2 kernel for the tx-block reconstruction SSE
- *(rusty_vp9)* one pass for the tx-block residual and its energy
- *(rusty_vp9)* decoder clears only the coefficient rows a block wrote
- *(rusty_vp9)* AVX2 column pass for the ADST forward transform
- *(rusty_vp9)* AVX2 row pass for the ADST forward transform
- *(rusty_vp9)* flat tables and fixed-size loops for the ADST forward transform
- *(rusty_vp9)* tally emitted token counts straight into the frame counts
- *(rusty_vp9)* forward-transform intermediates use the tx scratch
- *(rusty_vp9)* stop allocating a 17 KB scratch per transform block

## [0.1.3](https://github.com/Remade-With-Rust/remade_ffmpeg_rs/compare/rusty_vp9-v0.1.2...rusty_vp9-v0.1.3) - 2026-10-17

### Fixed

- *(clippy)* the two hard errors that kept CI's clippy job red

### Other

- cargo fmt --all (CI's rustfmt job was red on main)
