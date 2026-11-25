# Changelog

All notable changes to this project will be documented in this file.

The format is based on [Keep a Changelog](https://keepachangelog.com/en/1.0.0/),
and this project adheres to [Semantic Versioning](https://semver.org/spec/v2.0.0.html).

## [Unreleased]

## [0.1.1](https://github.com/Remade-With-Rust/remade_ffmpeg_rs/compare/rusty_jbig2-v0.1.0...rusty_jbig2-v0.1.1) - 2026-11-25

### Fixed

- *(rusty_jbig2)* wrap where the refinement lanes shift out

### Other

- *(rusty_jbig2)* say where the unsafe block is
- *(rusty_jbig2)* no memcpy call for 8-byte patterns
- *(rusty_jbig2)* borrow symbols and the page instead of copying
- *(rusty_jbig2)* a region that is the page becomes the page
- *(rusty_jbig2)* short Huffman codes from a table
- *(rusty_jbig2)* draw rows of up to 512 pixels in words
- *(rusty_jbig2)* MMR from a bit buffer, runs painted as they are decoded
- *(rusty_jbig2)* skip 64 white pixels at a time in refinement regions
- *(rusty_jbig2)* refinement contexts from four multiplications a half
- *(rusty_jbig2)* refinement contexts eight pixels at a time
- *(rusty_jbig2)* decode a row's last byte apart from the whole ones
- *(rusty_jbig2)* the plain generic region in a function of its own
- *(rusty_jbig2)* carry the generic context from byte to byte
- *(rusty_jbig2)* generic regions with pixels entering from one word
- *(rusty_jbig2)* one check per symbol draw, then raw rows
- *(rusty_jbig2)* gather halftone gray values 8 cells at a time
- *(rusty_jbig2)* invert PDF samples in one run
- *(rusty_jbig2)* draw bitmaps a word or a whole run at a time

## [0.1.0]

- First version: a JBIG2 (ITU-T T.88) decoder. Generic, refinement, text and
  halftone regions; symbol and pattern dictionaries; arithmetic, Huffman and
  MMR coding; embedded streams with globals (PDF) and standalone files.
