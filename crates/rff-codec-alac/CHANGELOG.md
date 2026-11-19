# Changelog

All notable changes to this project will be documented in this file.

The format is based on [Keep a Changelog](https://keepachangelog.com/en/1.0.0/),
and this project adheres to [Semantic Versioning](https://semver.org/spec/v2.0.0.html).

## [Unreleased]

## [0.6.3](https://github.com/Remade-With-Rust/remade_ffmpeg_rs/compare/rff-codec-alac-v0.6.2...rff-codec-alac-v0.6.3) - 2026-11-19

### Other

- *(alac)* 1.97x decode, from a register-resident predictor and a 64-bit bit reader

## [0.6.2](https://github.com/Remade-With-Rust/remade_ffmpeg_rs/compare/rff-codec-alac-v0.6.1...rff-codec-alac-v0.6.2) - 2026-11-19

### Added

- *(audio)* ALAC decoder and CAF demuxer, rff-codec-alac + rff-format-caf (#125 phase C)

### Fixed

- *(flac)* a damaged frame no longer takes the frames after it with it (#125 phase E)

### Other

- keep each file's line endings as on main
