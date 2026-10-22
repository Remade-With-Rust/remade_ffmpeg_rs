# Changelog

All notable changes to this project will be documented in this file.

The format is based on [Keep a Changelog](https://keepachangelog.com/en/1.0.0/),
and this project adheres to [Semantic Versioning](https://semver.org/spec/v2.0.0.html).

## [Unreleased]

## [0.1.2](https://github.com/Remade-With-Rust/remade_ffmpeg_rs/compare/rusty_vorbis-v0.1.1...rusty_vorbis-v0.1.2) - 2026-10-22

### Added

- *(rusty_vorbis)* Decoder::packet_blocksize; correct the output-split docs
- *(rusty_vorbis)* pure-Rust Vorbis I decoder (packet level)

### Other

- *(rusty_vorbis)* document the decoder; it is no longer encoder-only
- *(rusty_vorbis)* vectorisable stereo residue-2 de-interleave
- *(rusty_vorbis)* frame-major (de)interleave for 3+ channels
- *(rusty_vorbis)* keep the bit reader in registers through residue partitions
- *(rusty_vorbis)* render floor-1 lines as constant-gain runs
- *(rusty_vorbis)* decoder stage profiler + vorbisdec example
- *(rusty_vorbis)* spec-coverage gate vs lewton, robustness fuzz, oracle twins
