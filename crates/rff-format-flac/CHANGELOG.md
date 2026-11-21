# Changelog

All notable changes to this project will be documented in this file.

The format is based on [Keep a Changelog](https://keepachangelog.com/en/1.0.0/),
and this project adheres to [Semantic Versioning](https://semver.org/spec/v2.0.0.html).

## [Unreleased]

## [0.7.0](https://github.com/Remade-With-Rust/remade_ffmpeg_rs/compare/rff-format-flac-v0.6.3...rff-format-flac-v0.7.0) - 2026-11-21

### Fixed

- *(packaging)* ship the Apache-2.0 LICENSE text in every published crate

## [0.6.2](https://github.com/Remade-With-Rust/remade_ffmpeg_rs/compare/rff-format-flac-v0.6.1...rff-format-flac-v0.6.2) - 2026-11-19

### Fixed

- *(flac)* a damaged frame no longer takes the frames after it with it (#125 phase E)

## [0.4.2](https://github.com/Remade-With-Rust/remade_ffmpeg_rs/compare/rff-format-flac-v0.4.1...rff-format-flac-v0.4.2) - 2026-10-24

### Other

- frame scan by 8-byte words and CRC-16 by carry-less multiply
