# Changelog

All notable changes to this project will be documented in this file.

The format is based on [Keep a Changelog](https://keepachangelog.com/en/1.0.0/),
and this project adheres to [Semantic Versioning](https://semver.org/spec/v2.0.0.html).

## [Unreleased]

## [0.9.0](https://github.com/Remade-With-Rust/remade_ffmpeg_rs/compare/rff-core-v0.8.4...rff-core-v0.9.0) - 2026-11-24

### Fixed

- *(cli)* convert RGB to the encoder's pixel format automatically; honour -pix_fmt ([#179](https://github.com/Remade-With-Rust/remade_ffmpeg_rs/pull/179))

## [0.8.0](https://github.com/Remade-With-Rust/remade_ffmpeg_rs/compare/rff-core-v0.7.0...rff-core-v0.8.0) - 2026-11-21

### Added

- *(rff-format, rff-core)* codec IDs, FLAC/AAC/avcC helpers and a shareable SampleSource ([#114](https://github.com/Remade-With-Rust/remade_ffmpeg_rs/pull/114))

### Other

- Merge remote-tracking branch 'origin/main' into feat/mp4-mkv-114b

## [0.7.0](https://github.com/Remade-With-Rust/remade_ffmpeg_rs/compare/rff-core-v0.6.3...rff-core-v0.7.0) - 2026-11-21

### Fixed

- *(packaging)* ship the Apache-2.0 LICENSE text in every published crate

## [0.6.2](https://github.com/Remade-With-Rust/remade_ffmpeg_rs/compare/rff-core-v0.6.1...rff-core-v0.6.2) - 2026-11-19

### Added

- *(rff)* MP2 / MP1 codec ids, decoders and .mp2/.mp1/.mpa demuxing (#125 phase D)
- *(audio)* G.711 and ADPCM decoders, extended WAV, FLAC in Ogg (#125 phase A)

## [0.5.0](https://github.com/Remade-With-Rust/remade_ffmpeg_rs/compare/rff-core-v0.4.8...rff-core-v0.5.0) - 2026-11-14

### Added

- *(core)* [**breaking**] video metadata, block additions, copy-only codecs and wider PCM for FilmCraft ([#110](https://github.com/Remade-With-Rust/remade_ffmpeg_rs/pull/110))

## [0.4.0](https://github.com/Remade-With-Rust/remade_ffmpeg_rs/compare/rff-core-v0.3.1...rff-core-v0.4.0) - 2026-10-24

### Fixed

- *(mkv,vorbis,opus)* trim the stream end by Matroska DiscardPadding

## [0.3.0](https://github.com/Remade-With-Rust/remade_ffmpeg_rs/compare/rff-core-v0.2.4...rff-core-v0.3.0) - 2026-10-22

### Fixed

- *(ogg,vorbis)* trim the stream end to the final granule position

### Other

- *(clippy)* clear the warnings in the crates this fix touches
