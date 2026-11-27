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

The expected hashes in `../decode.rs` are libheif 1.23.7's decode of each
file (YCbCr planes, `nclx` passthrough); for `three-images.heic` they are
FFmpeg 9.1's decode of the alpha-carrying items and their alpha planes.
