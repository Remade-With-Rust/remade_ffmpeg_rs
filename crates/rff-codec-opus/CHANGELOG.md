# Changelog

All notable changes to this project will be documented in this file.

The format is based on [Keep a Changelog](https://keepachangelog.com/en/1.0.0/),
and this project adheres to [Semantic Versioning](https://semver.org/spec/v2.0.0.html).

## [Unreleased]

## [0.3.4](https://github.com/Remade-With-Rust/remade_ffmpeg_rs/compare/rff-codec-opus-v0.3.3...rff-codec-opus-v0.3.4) - 2026-10-24

### Fixed

- *(mkv,vorbis,opus)* trim the stream end by Matroska DiscardPadding

## [0.3.3](https://github.com/Remade-With-Rust/remade_ffmpeg_rs/compare/rff-codec-opus-v0.3.2...rff-codec-opus-v0.3.3) - 2026-10-23

### Other

- updated the following local packages: rff-core, rff-codec

## [0.3.2](https://github.com/Remade-With-Rust/remade_ffmpeg_rs/compare/rff-codec-opus-v0.3.1...rff-codec-opus-v0.3.2) - 2026-10-22

### Fixed

- *(opus)* honour OpusHead pre-skip/gain and trim to the final granule

### Other

- *(clippy)* clear the warnings in the crates this fix touches

## [0.3.1](https://github.com/Remade-With-Rust/remade_ffmpeg_rs/compare/rff-codec-opus-v0.3.0...rff-codec-opus-v0.3.1) - 2026-10-17

### Other

- adopt the adapter versions already on crates.io; unblock release-plz
