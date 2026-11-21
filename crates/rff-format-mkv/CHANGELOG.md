# Changelog

All notable changes to this project will be documented in this file.

The format is based on [Keep a Changelog](https://keepachangelog.com/en/1.0.0/),
and this project adheres to [Semantic Versioning](https://semver.org/spec/v2.0.0.html).

## [Unreleased]

## [0.8.0](https://github.com/Remade-With-Rust/remade_ffmpeg_rs/compare/rff-format-mkv-v0.7.0...rff-format-mkv-v0.8.0) - 2026-11-21

### Added

- *(rff-format-mkv)* more CodecIDs, DNxHD/APV/MJPEG mux, HE-AAC rates, FLAC STREAMINFO rewrite, sample sources ([#114](https://github.com/Remade-With-Rust/remade_ffmpeg_rs/pull/114))

### Fixed

- *(mp4,mkv)* keep raw length-prefixed H.264/HEVC samples on mux

### Other

- Merge remote-tracking branch 'origin/main' into feat/mp4-mkv-114b

## [0.7.0](https://github.com/Remade-With-Rust/remade_ffmpeg_rs/compare/rff-format-mkv-v0.6.3...rff-format-mkv-v0.7.0) - 2026-11-21

### Added

- *(rff-format-mkv)* track names, languages, flags, interlacing, crop, display size, rotation, Info and Tags ([#114](https://github.com/Remade-With-Rust/remade_ffmpeg_rs/pull/114))

### Other

- Merge pull request #157 from Remade-With-Rust/fix/licenses-alac
- *(rff-format-mp4, rff-format-mkv)* list the #114 features in the crate READMEs

## [0.6.2](https://github.com/Remade-With-Rust/remade_ffmpeg_rs/compare/rff-format-mkv-v0.6.1...rff-format-mkv-v0.6.2) - 2026-11-19

### Added

- *(rff)* MP2 / MP1 codec ids, decoders and .mp2/.mp1/.mpa demuxing (#125 phase D)

## [0.6.1](https://github.com/Remade-With-Rust/remade_ffmpeg_rs/compare/rff-format-mkv-v0.6.0...rff-format-mkv-v0.6.1) - 2026-11-18

### Added

- *(mkv)* random-access sample index, raw samples, HEVC/ProRes/PCM muxing ([#114](https://github.com/Remade-With-Rust/remade_ffmpeg_rs/pull/114))

## [0.6.0](https://github.com/Remade-With-Rust/remade_ffmpeg_rs/compare/rff-format-mkv-v0.5.0...rff-format-mkv-v0.6.0) - 2026-11-18

### Other

- *(format)* demuxers skip discarded streams at the source (Demuxer::set_discard)

## [0.5.0](https://github.com/Remade-With-Rust/remade_ffmpeg_rs/compare/rff-format-mkv-v0.4.8...rff-format-mkv-v0.5.0) - 2026-11-14

### Added

- *(mkv)* colour/HDR, WebM alpha, codec delay, ContentEncodings and copy-only codecs

## [0.4.8](https://github.com/Remade-With-Rust/remade_ffmpeg_rs/compare/rff-format-mkv-v0.4.7...rff-format-mkv-v0.4.8) - 2026-11-14

### Fixed

- *(mkv)* take a track's CodecDelay off its timestamps
- *(mkv)* split laced blocks into their frames instead of dropping them

## [0.4.5](https://github.com/Remade-With-Rust/remade_ffmpeg_rs/compare/rff-format-mkv-v0.4.4...rff-format-mkv-v0.4.5) - 2026-10-29

### Fixed

- *(mkv)* DefaultDuration packet durations; drop a truncated final block

## [0.4.4](https://github.com/Remade-With-Rust/remade_ffmpeg_rs/compare/rff-format-mkv-v0.4.3...rff-format-mkv-v0.4.4) - 2026-10-28

### Fixed

- *(mp4,mkv)* write B-frame timing as FFmpeg does ([#60](https://github.com/Remade-With-Rust/remade_ffmpeg_rs/pull/60))

## [0.4.0](https://github.com/Remade-With-Rust/remade_ffmpeg_rs/compare/rff-format-mkv-v0.3.1...rff-format-mkv-v0.4.0) - 2026-10-24

### Fixed

- *(mkv,vorbis,opus)* trim the stream end by Matroska DiscardPadding

## [0.3.1](https://github.com/Remade-With-Rust/remade_ffmpeg_rs/compare/rff-format-mkv-v0.3.0...rff-format-mkv-v0.3.1) - 2026-10-23

### Added

- *(mkv)* stream Matroska/WebM to seekable outputs ([#28](https://github.com/Remade-With-Rust/remade_ffmpeg_rs/pull/28))
