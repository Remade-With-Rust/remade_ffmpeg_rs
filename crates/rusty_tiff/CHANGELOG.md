# Changelog

All notable changes to this project will be documented in this file.

The format is based on [Keep a Changelog](https://keepachangelog.com/en/1.0.0/),
and this project adheres to [Semantic Versioning](https://semver.org/spec/v2.0.0.html).

## [Unreleased]

## [0.1.2](https://github.com/Remade-With-Rust/remade_ffmpeg_rs/compare/rusty_tiff-v0.1.1...rusty_tiff-v0.1.2) - 2026-11-21

### Fixed

- *(rusty_tiff)* bound how much of a damaged image may be zero-filled

### Other

- *(rusty_tiff)* backticks for SamplesPerPixel in the max_fill_ratio doc (clippy doc_markdown)

## [0.1.1](https://github.com/Remade-With-Rust/remade_ffmpeg_rs/compare/rusty_tiff-v0.1.0...rusty_tiff-v0.1.1) - 2026-11-20

### Added

- *(rusty_tiff)* DecodingResult::into_ne_bytes; document strip size vs file size

### Fixed

- *(rusty_tiff)* CCITT vertical mode left of a0 clamps instead of failing
- *(rusty_tiff)* recover damaged files the tiff crate still reads

### Other

- *(rusty_tiff)* record the refuted PackBits overcopy decode
- *(rusty_tiff)* LZW decode overcopies short strings; encoder tables fixed-size
- *(rusty_tiff)* encode wide samples from a borrowed byte view (bytemuck)
- *(rusty_tiff)* wide-sample strips convert into the reused chunk buffer
- *(rusty_tiff, rusty_image)* remove the last avoidable codec copies
- *(rusty_tiff)* wide-sample encode converts in one sized pass
- *(rusty_tiff)* reuse one zlib compressor; recover more damaged strips
- *(rusty_tiff)* PackBits repeat-2 packets as one 2-byte append
- *(rusty_tiff)* CCITT rows paint black runs a byte at a time
- *(rusty_tiff)* clippy needless_borrow and manual_is_multiple_of
- *(rusty_tiff)* PackBits encoder finds the end of a literal 8 bytes at a time
- *(rusty_tiff)* encoder reuses its LZW table and buffers; strips straight from the input
- *(rusty_tiff)* horizontal predictor specialised per samples-per-pixel
- *(rusty_tiff)* LZW decodes by index into a sized buffer, 32-bit bit refills
- *(rusty_tiff)* codecs append into the output; strips decode straight into place
- *(rusty_tiff)* read chunks without zero-filling first
