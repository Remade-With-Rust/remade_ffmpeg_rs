# Changelog

All notable changes to this project will be documented in this file.

The format is based on [Keep a Changelog](https://keepachangelog.com/en/1.0.0/),
and this project adheres to [Semantic Versioning](https://semver.org/spec/v2.0.0.html).

## [Unreleased]

## [0.4.0](https://github.com/Remade-With-Rust/remade_ffmpeg_rs/compare/rff-codec-vorbis-v0.3.1...rff-codec-vorbis-v0.4.0) - 2026-10-24

### Fixed

- *(mkv,vorbis,opus)* trim the stream end by Matroska DiscardPadding

## [0.3.0](https://github.com/Remade-With-Rust/remade_ffmpeg_rs/compare/rff-codec-vorbis-v0.2.4...rff-codec-vorbis-v0.3.0) - 2026-10-22

### Added

- *(vorbis)* decode with the in-house rusty_vorbis decoder, not lewton

### Fixed

- *(ogg,vorbis)* trim the stream end to the final granule position
- *(vorbis)* emit 3-8 channel audio in FFmpeg/WAV channel order
- *(vorbis)* decode to f32 instead of quantising to s16

### Other

- *(rff-codec-vorbis)* state the measured end-to-end speed
- *(clippy)* clear the warnings in the crates this fix touches

### Changed

- Decode with the in-house `rusty_vorbis` decoder instead of `lewton`
  (`lewton` is now only a dev-dependency, the decode oracle for the encoder
  tests). Output is unchanged in kind — `f32`, FFmpeg channel order, end
  trimmed to the final granule — and matches FFmpeg 8.1 to ~2e-7.

### Fixed

- The end trim counts packets in granule terms (`prev_blocksize/4 +
  blocksize/4`), so it no longer misses the final granule when a page ends on
  a long block that a short one follows (the decoder outputs that block's
  `(long - short)/4` tail a packet early): such streams decoded 32 samples
  long at 48 kHz.
- Zero-length audio packets are skipped instead of failing the decode.
