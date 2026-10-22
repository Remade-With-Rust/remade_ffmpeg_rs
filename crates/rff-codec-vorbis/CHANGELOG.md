# Changelog

All notable changes to this project will be documented in this file.

The format is based on [Keep a Changelog](https://keepachangelog.com/en/1.0.0/),
and this project adheres to [Semantic Versioning](https://semver.org/spec/v2.0.0.html).

## [Unreleased]

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
