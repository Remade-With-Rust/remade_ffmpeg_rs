# Changelog

All notable changes to this project will be documented in this file.

The format is based on [Keep a Changelog](https://keepachangelog.com/en/1.0.0/),
and this project adheres to [Semantic Versioning](https://semver.org/spec/v2.0.0.html).

## [Unreleased]

## [0.8.1](https://github.com/Remade-With-Rust/remade_ffmpeg_rs/compare/rff-format-v0.8.0...rff-format-v0.8.1) - 2026-11-22

### Added

- *(jpeg)* JPEG Multi-Picture Format (MPF) demux and mux

## [0.8.0](https://github.com/Remade-With-Rust/remade_ffmpeg_rs/compare/rff-format-v0.7.0...rff-format-v0.8.0) - 2026-11-21

### Added

- *(rff-format, rff-core)* codec IDs, FLAC/AAC/avcC helpers and a shareable SampleSource ([#114](https://github.com/Remade-With-Rust/remade_ffmpeg_rs/pull/114))

### Fixed

- *(mp4,mkv)* keep raw length-prefixed H.264/HEVC samples on mux

### Other

- Merge remote-tracking branch 'origin/main' into feat/mp4-mkv-114b

## [0.7.0](https://github.com/Remade-With-Rust/remade_ffmpeg_rs/compare/rff-format-v0.6.3...rff-format-v0.7.0) - 2026-11-21

### Added

- *(rff-format)* display matrix, track language/name/flags, metadata and stored-table APIs ([#114](https://github.com/Remade-With-Rust/remade_ffmpeg_rs/pull/114))

### Other

- Merge pull request #157 from Remade-With-Rust/fix/licenses-alac

## [0.6.1](https://github.com/Remade-With-Rust/remade_ffmpeg_rs/compare/rff-format-v0.6.0...rff-format-v0.6.1) - 2026-11-18

### Added

- *(mp4)* faststart and caller edit lists ([#114](https://github.com/Remade-With-Rust/remade_ffmpeg_rs/pull/114))
- *(format)* Demuxer::sample_index and set_raw_samples (random access, #114)

### Other

- *(format)* raw samples and avcC/hvcC extradata as the demuxers now give them ([#114](https://github.com/Remade-With-Rust/remade_ffmpeg_rs/pull/114))

## [0.6.0](https://github.com/Remade-With-Rust/remade_ffmpeg_rs/compare/rff-format-v0.5.0...rff-format-v0.6.0) - 2026-11-18

### Other

- *(format)* demuxers skip discarded streams at the source (Demuxer::set_discard)

## [0.5.0](https://github.com/Remade-With-Rust/remade_ffmpeg_rs/compare/rff-format-v0.4.8...rff-format-v0.5.0) - 2026-11-14

### Added

- *(core)* [**breaking**] video metadata, block additions, copy-only codecs and wider PCM for FilmCraft ([#110](https://github.com/Remade-With-Rust/remade_ffmpeg_rs/pull/110))

## [0.3.1](https://github.com/Remade-With-Rust/remade_ffmpeg_rs/compare/rff-format-v0.3.0...rff-format-v0.3.1) - 2026-10-23

### Added

- *(format)* seekable muxer output (SeekOutput / open_muxer_seekable), files open seekable
