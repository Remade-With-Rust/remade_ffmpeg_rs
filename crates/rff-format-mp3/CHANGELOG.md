# Changelog

All notable changes to this project will be documented in this file.

The format is based on [Keep a Changelog](https://keepachangelog.com/en/1.0.0/),
and this project adheres to [Semantic Versioning](https://semver.org/spec/v2.0.0.html).

## [Unreleased]

## [0.6.2](https://github.com/Remade-With-Rust/remade_ffmpeg_rs/compare/rff-format-mp3-v0.6.1...rff-format-mp3-v0.6.2) - 2026-11-19

### Added

- *(rff)* MP2 / MP1 codec ids, decoders and .mp2/.mp1/.mpa demuxing (#125 phase D)

### Fixed

- *(mp3)* a Layer III stream stays Layer III, so pipelined decode equals serial

## [0.4.3](https://github.com/Remade-With-Rust/remade_ffmpeg_rs/compare/rff-format-mp3-v0.4.2...rff-format-mp3-v0.4.3) - 2026-10-24

### Fixed

- *(mp3)* patch the streamed MP3 tag frame on a seekable output
