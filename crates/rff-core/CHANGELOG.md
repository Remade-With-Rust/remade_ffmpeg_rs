# Changelog

All notable changes to this project will be documented in this file.

The format is based on [Keep a Changelog](https://keepachangelog.com/en/1.0.0/),
and this project adheres to [Semantic Versioning](https://semver.org/spec/v2.0.0.html).

## [Unreleased]

## [0.4.0](https://github.com/Remade-With-Rust/remade_ffmpeg_rs/compare/rff-core-v0.3.1...rff-core-v0.4.0) - 2026-10-24

### Fixed

- *(mkv,vorbis,opus)* trim the stream end by Matroska DiscardPadding

## [0.3.0](https://github.com/Remade-With-Rust/remade_ffmpeg_rs/compare/rff-core-v0.2.4...rff-core-v0.3.0) - 2026-10-22

### Fixed

- *(ogg,vorbis)* trim the stream end to the final granule position

### Other

- *(clippy)* clear the warnings in the crates this fix touches
