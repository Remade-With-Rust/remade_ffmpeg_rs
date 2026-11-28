# Test fixtures

Copied unchanged from [heic-rs](https://github.com/tbraun96/heic-rs)
(`tests/fixtures/`, commit `621014b8`), where every file is generated from
synthesised PNGs (no third-party image data) and licensed **MIT OR
Apache-2.0**:

| File | Made by | What it exercises |
|---|---|---|
| `checker-1024.heic` | Apple `sips` | `grid`, 2x2 tiles of 512x512 |
| `gradient-512.heic` | Apple `sips` | a single `hvc1` item |
| `with-exif.heic` | Apple `sips` | `grid` 3x4, an `Exif` item and an XMP `mime` item |
| `full-range-no-colr.heic` | libheif `heif-enc` | `grid` 2x2, 4:2:0 10-bit labelled with a range-extensions profile, full range said only in the HEVC VUI (no `colr`) |
| `three-images.heic` | libheif `heif-enc -t 16` | three images with a thumbnail each; the 32x80 one (item 5) and its thumbnail (item 7) carry an `auxl` alpha coded as monochrome (4:0:0) HEVC; every item `clap`-cropped |

Also from heic-rs at the same commit, same licence:

| File | Made by | What it exercises |
|---|---|---|
| `grid-422-10bit.heic` | libheif `heif-enc` | `grid`, 2x3 tiles of 64x64, 4:2:2 10-bit (range extensions), the layout of Canon and Sony `.HIF` files |

Made for this crate (MIT OR Apache-2.0): libx265 encodes of FFmpeg's own
synthetic sources (`testsrc2`, `mandelbrot`), wrapped as single-item HEIF by a
small muxer, each with a `clap` whose right/bottom (and, in 4:4:4, top) edge is
odd:

| File | Source | What it exercises |
|---|---|---|
| `yuv444-8bit-clap.heic` | `testsrc2` 64x48, x265 QP 24 | 4:4:4 8-bit, `clap` 58x44 at (2, 3) |
| `yuv422-12bit-clap.heic` | `mandelbrot` 96x64, x265 QP 24 | 4:2:2 12-bit, `clap` 90x60 at (4, 1) |

For these three the expected hashes are FFmpeg 9.1's decode.

The expected hashes in `../decode.rs` are libheif 1.23.7's decode of each
file (YCbCr planes, `nclx` passthrough); for `three-images.heic` they are
FFmpeg 9.1's decode of the alpha-carrying items and their alpha planes.
