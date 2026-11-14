# Changelog

All notable changes to this project will be documented in this file.

The format is based on [Keep a Changelog](https://keepachangelog.com/en/1.0.0/),
and this project adheres to [Semantic Versioning](https://semver.org/spec/v2.0.0.html).

## [Unreleased]

## [0.2.1](https://github.com/Remade-With-Rust/remade_ffmpeg_rs/compare/rusty_webp-v0.2.0...rusty_webp-v0.2.1) - 2026-11-14

### Other

- release

## [0.2.0](https://github.com/Remade-With-Rust/remade_ffmpeg_rs/compare/rusty_webp-v0.1.4...rusty_webp-v0.2.0) - 2026-11-14

### Fixed

- *(rusty_webp)* opaque pixels of blended animation frames were one too dark

### Other

- *(rusty_webp)* version 0.2.0; READMEs carry the 0.1 -> 0.2 numbers
- *(rusty_webp)* clear the clippy lints the campaign introduced
- *(rusty_webp)* RGBA to RGB in groups of 32 pixels
- *(rusty_webp)* read packed palette indices out of their pixels as words
- *(rusty_webp)* reuse the alpha plane buffer across animation frames
- *(rusty_webp)* reuse the frame buffer across animation frames
- *(rusty_webp)* blend animation frames a row at a time
- *(rusty_webp)* distance codes from a direct table, not a HashMap
- *(rusty_webp)* pack RGBA into ARGB words with a byte swap and a rotate
- *(rusty_webp)* extend LZ77 matches over zipped windows
- *(rusty_webp)* cache model inserts into a fixed-size table, unchecked
- *(rusty_webp)* one histogram pass for all colour-cache candidates
- *(rusty_webp)* encoder cache model keeps one word per slot
- *(rusty_webp)* write the predictor residuals a tile row at a time
- *(rusty_webp)* cost the predictors a tile row at a time
- *(rusty_webp)* inline the encoder's predictor, drop its dyn closure
- *(rusty_webp)* run each predictor over runs of blocks, not block by block
- *(rusty_webp)* top, top-right and top-left predictors as slice adds
- *(rusty_webp)* skip colour-transform blocks whose multipliers are zero
- *(rusty_webp)* convert RGBA to RGB eight pixels at a time
- *(rusty_webp)* one code-group lookup for single-group VP8L images
- *(rusty_webp)* colour-cache key as one byte-swapped load and a rotate
- *(rusty_webp)* no error value built per cache-coded pixel
- *(rusty_webp)* store token probability updates as they are read
- *(rusty_webp)* reverse Huffman codes through a byte table
- *(rusty_webp)* read the token probability updates in one decoder session
- *(rusty_webp)* unfilter ALPH straight from the decoded green channel
- *(rusty_webp)* take the alpha plane out of its decoded pixels as words
- *(rusty_webp)* unfilter the alpha plane a row at a time
- *(rusty_webp)* separable fancy chroma upsample; tidy oracle-only code
- *(rusty_webp)* simple chroma upsampling through the lane converter
- *(rusty_webp)* fancy-upsample YUV to RGB in two vectorizable stages
- *(rusty_webp)* size the macroblock list once per frame
- *(rusty_webp)* whole-block predictors take the block size at compile time
- *(rusty_webp)* store each predicted macroblock with row copies
- *(rusty_webp)* one residual buffer per frame instead of one per macroblock
- *(rusty_webp)* skip the residual add for macroblocks with no residual
- *(rusty_webp)* three-coefficient inverse transform
- *(rusty_webp)* inline the fast coefficient path, outline the file-tail path
- *(rusty_webp)* read coefficient signs with a branchless mask
- *(rusty_webp)* unroll the coefficient token tree in the fast path
- *(rusty_webp)* read each macroblock header in one decoder session
- *(rusty_webp)* boolean decoder keeps range - 1, as libwebp does
- *(rusty_webp)* boolean decoder renormalises through a shift table
- *(rusty_webp)* batch the simple loop filter across each edge
- *(rusty_webp)* loop filter reads and writes each row's 8 taps as one window
- *(rusty_webp)* loop filter loads and stores tap rows as slices

## [0.1.4](https://github.com/Remade-With-Rust/remade_ffmpeg_rs/compare/rusty_webp-v0.1.3...rusty_webp-v0.1.4) - 2026-11-13

### Other

- update Cargo.toml dependencies

## [0.1.3](https://github.com/Remade-With-Rust/remade_ffmpeg_rs/compare/rusty_webp-v0.1.2...rusty_webp-v0.1.3) - 2026-10-17

### Other

- update Cargo.toml dependencies
