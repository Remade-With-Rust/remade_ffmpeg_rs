# Changelog

All notable changes to this project will be documented in this file.

The format is based on [Keep a Changelog](https://keepachangelog.com/en/1.0.0/),
and this project adheres to [Semantic Versioning](https://semver.org/spec/v2.0.0.html).

## [Unreleased]

## [0.1.4](https://github.com/Remade-With-Rust/remade_ffmpeg_rs/compare/rusty_raster-v0.1.3...rusty_raster-v0.1.4) - 2026-11-22

### Other

- *(rusty_raster)* compound assignment in tile_gather (clippy)
- *(rusty_raster)* fuse Repeat/Reflect into the nearest gather; fuse on the stage list
- *(rusty_raster)* pin try_zeroed's contract; calloc probe refuted
- *(rusty_raster)* AlphaRuns::add returns the start pixel as the next hint

## [0.1.3](https://github.com/Remade-With-Rust/remade_ffmpeg_rs/compare/rusty_raster-v0.1.2...rusty_raster-v0.1.3) - 2026-11-21

### Other

- *(rusty_raster)* fuse highp Gather + Scale1Float into one stage
- *(rusty_raster)* nearest gather computes its index in f32
- *(rusty_raster)* fuse highp SeedShader + Transform into one stage
- *(rusty_raster)* f32x8::floor takes a five-op path below 2^22
- *(rusty_raster)* #[inline] on the highp lane methods

## [0.1.2](https://github.com/Remade-With-Rust/remade_ffmpeg_rs/compare/rusty_raster-v0.1.1...rusty_raster-v0.1.2) - 2026-11-21

### Other

- Merge pull request #176 from Remade-With-Rust/perf/loss-rows-raster
- *(rusty_raster)* highp gradient groups that straddle one stop skip the lookup
- *(rusty_raster)* highp tail stores pack all eight pixels as vectors
- lossbench, the downstream gradient scenes
- *(rusty_raster)* highp gradient and premultiply as one stage
- *(rusty_raster)* highp gradient finds a shared interval with one scan
- *(rusty_raster)* filtered taps gather without a bounds branch per pixel
- *(rusty_raster)* bilinear/bicubic taps share their column and row coordinates
- *(rusty_raster)* mask coverage runs written without the pipeline
- lossbench, the trailing rows against tiny-skia

## [0.1.1](https://github.com/Remade-With-Rust/remade_ffmpeg_rs/compare/rusty_raster-v0.1.0...rusty_raster-v0.1.1) - 2026-11-21

### Other

- speed numbers in five image crate READMEs; rusty_tiff needs flate2 1.1.11
- *(rusty_raster)* highp gradient stop search, one stop at a time
- Revert "WIP perf(rusty_raster): gather index computed in floats"
- WIP perf(rusty_raster): gather index computed in floats (Linux gate owed, clock flat)
- *(rusty_raster)* lowp tail load reads only a short tail's pixels
- *(rusty_raster)* highp gradient, one lookup for a group between two stops
- *(rusty_raster)* lowp gradient, one lookup for a group between two stops
- *(rusty_raster)* lowp gradient colours to lanes without a scalar cast
- *(rusty_raster)* lowp store_8888 as one packed word per pixel
- *(rusty_raster)* lowp load_8888 from one word per pixel
- *(rusty_raster)* highp load_8888 from one word per pixel
- *(rusty_raster)* sprite blit for draw_pixmap without a transform
- *(rusty_raster)* gather index without a float-to-int conversion
- *(rusty_raster)* f32x8::floor without the i32 round trip
- *(rusty_raster)* veinbench layer and mask workloads
- *(rusty_raster)* highp store without a float-to-int conversion
- *(rusty_raster)* branch-free f32x8::round for the lowp tiling stages
- *(rusty_raster)* round lanes without libm rintf
- *(rusty_raster)* truncate lanes without libm truncf
- *(rusty_raster)* veinbench, a deterministic instruction-count instrument
