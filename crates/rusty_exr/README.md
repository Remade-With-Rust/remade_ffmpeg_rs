# rusty_exr

[![Remade With Rust](https://img.shields.io/badge/Remade%20With-Rust-000?logo=rust&logoColor=fff)](https://github.com/remade-with-rust)
[![By Mata Network](https://img.shields.io/badge/by-Mata%20Network-5b2be0)](https://www.mata.network)
[![License: Apache-2.0](https://img.shields.io/badge/license-Apache--2.0-blue)](https://github.com/Remade-With-Rust/remade_ffmpeg_rs/blob/main/LICENSE)

A pure-Rust OpenEXR decoder and encoder. No C, no `unsafe`, two
dependencies (`flate2` on its pure-Rust zlib-rs backend, and `miniz_oxide`).

## What it reads

- Single-part and multi-part files (list the parts, decode any one).
- Scanline and tiled parts, with every resolution level: one level,
  mipmaps and ripmaps (`decode_level`).
- HALF, FLOAT and UINT channels, any channel names (`diffuse.R`, `Z`,
  `crypto00.R`), x/y subsampling (luminance/chroma `Y`/`RY`/`BY`), data
  and display windows, any line order.
- Every compression: None, RLE, ZIPS, ZIP, PIZ, PXR24, B44, B44A, DWAA and
  DWAB.
- Header attributes: the standard ones typed (`chromaticities`, `preview`,
  `timeCode`, `keyCode`, matrices, ...), anything else kept as raw bytes —
  Cryptomatte manifests come through as `string` attributes.
- A chunk-level API (`Decoder::decode_chunk`) that hands out each
  decompressed scanline block or tile with its geometry, for readers that
  stream or place channels themselves.
- Deep parts are reported as `Error::Unsupported`; the other parts of the
  same file still decode.

Decoded samples are **bit-identical to OpenEXR C++ 3.5.5** on all 85
non-deep files of the official
[openexr-images](https://github.com/AcademySoftwareFoundation/openexr-images)
set, on 460 more files OpenEXR wrote in all ten compressions (scanline,
64x64 and 37x19 tiles; half, float and uint; subsampled), and on 48 files
the `exr` crate wrote, its B44/B44A/DWAA/DWAB output included. That covers
the lossy codecs: DWA's inverse DCT reproduces the operation order of
OpenEXR's AVX routine, and B44/DWA use OpenEXR's own transfer tables.

## What it writes

Single-part scanline files with None, RLE, ZIPS, ZIP, PIZ or PXR24
compression; HALF, FLOAT and UINT channels; any channel names (sorted for
you, as the format requires); your header attributes. By default files are
**byte-identical to the `exr` crate's** for the same image and settings
(the one exception: for channel names longer than 31 bytes this crate sets
the long-names flag the format requires and `exr` leaves clear).
`EncodeOptions { deflate: Deflate::Fast, .. }` swaps miniz_oxide for
zlib-rs on ZIP/ZIPS/PXR24: about twice the deflate speed and slightly
smaller files, with different bytes.

## Speed

Measured against OpenEXR C++ 3.5.5 and `exr` 1.74.2 on x86-64 Linux, each
single-threaded (pinned to one core) and with 24 threads, decoding into
planes in memory; the medians below come from a shared machine whose
same-binary noise floor was up to about 10% per row.

- **Decode, one thread:** RLE about 2.1x faster than OpenEXR and PIZ on
  64x64 tiles 1.3x faster; uncompressed at parity; ZIP and PIZ 1.12-1.29x
  slower; PXR24 1.3-1.6x slower. Against `exr`: PIZ 1.7-4.5x faster, RLE,
  PXR24 and uncompressed 1.1-1.2x faster, ZIP within about 1.2x.
- **Decode, 24 threads:** tiled files 1.6-4.6x faster than OpenEXR, 4K
  scanline RLE and uncompressed 3-4x faster, 4K ZIP/PIZ within 1.1x, 4K
  PXR24 1.25x, multi-layer float 1.3-1.7x and the small image 1.8-1.9x
  slower; faster than `exr` on all but uncompressed.
- **Encode:** with the default exr-compatible deflate, ZIP and PXR24 run at
  `exr`'s speed and about 2x behind OpenEXR; with `Deflate::Fast` they are
  within 1.03-1.3x of OpenEXR single-threaded and 1.6-2x faster than
  `exr`. PIZ is 1.2x behind OpenEXR single-threaded; RLE and uncompressed
  are faster than OpenEXR.

What remains is mostly deflate: OpenEXR links libdeflate, this crate uses
pure-Rust zlib-rs (inflate, and `Deflate::Fast`) and miniz_oxide (the
exr-compatible writer). The pull request that introduced the crate has the
per-file tables and the method.

## Safety

Malformed or hostile input returns an `Error` and never panics. Every size
read from a file is checked against `Limits` (pixels, channels, image and
chunk bytes, attribute size, parts, chunks) before anything is allocated.

## Example

```rust,no_run
use rusty_exr::{Compression, EncodeOptions, Image, Samples};

let bytes = std::fs::read("render.exr")?;
let image = rusty_exr::decode(&bytes)?;
let rgba: Vec<f32> = image.to_rgba_f32("").expect("RGB or Y channels");

let out = Image::new(2, 1)
    .with_channel("R", Samples::F32(vec![1.0, 0.5]))
    .with_channel("G", Samples::F32(vec![0.0, 0.25]))
    .with_channel("B", Samples::F32(vec![0.0, 0.0]));
std::fs::write("out.exr", rusty_exr::encode(&out, &EncodeOptions::new(Compression::Zip))?)?;
# Ok::<(), Box<dyn std::error::Error>>(())
```

## Migrating from `exr`

| `exr` 1.74 | `rusty_exr` |
|---|---|
| `read_first_rgba_layer_from_file(path, create, set)` | `rusty_exr::decode(&bytes)?.to_rgba_f32("")` (interleaved RGBA `f32`; missing alpha is 1, `Y`-only is grey) |
| `read_all_flat_layers_from_file(path)` / `read().no_deep_data().largest_resolution_level().all_channels().all_layers()` | `let d = Decoder::new(&bytes)?;` then `d.decode_part(i)` for `i in 0..d.part_count()`; channels keep full names (`diffuse.R`), `Image::layers()` lists the prefixes |
| `.all_resolution_levels()` | `d.level_count(i)` and `d.decode_level(i, lx, ly)` |
| `FlatSamples::F16(Vec<f16>)` | `Samples::F16(Vec<u16>)` — the raw half bits; `rusty_exr::f16_to_f32` converts |
| custom `ChannelsReader` over `UncompressedBlock` | `d.decode_chunk(part, index)` → `Block { pos, x0, y0, width, height, data }`, rows via `Block::rows()` / `Block::row(channel, y)` |
| `meta::MetaData::read_from_file` | `rusty_exr::read_headers(&bytes)` |
| `write_rgba_file(path, w, h, f)` | `Image::new(w, h).with_channel("R", ..)...`, then `rusty_exr::encode(&img, &EncodeOptions::new(Compression::Zip))` |
| `Image::from_channels(size, SpecificChannels/AnyChannels).write().to_file(path)` with `Encoding { compression, .. }` | `Image { channels: vec![Channel::new(name, w, h, samples), ..], .. }` + `EncodeOptions { compression, line_order, threads }`; byte-identical output for scanline ZIP/ZIPS/PIZ/RLE/PXR24/None |
| `layer_attributes.other` / `shared_attributes.chromaticities` | `image.header.set("chromaticities", AttributeValue::Chromaticities(..))`; any attribute via `header.get(name)` / `header.set(name, value)` |
| `f16::from_f32(v)` for output prep | `rusty_exr::f32_to_f16(v)` (round to nearest even, NaN kept); `f32_to_f16_clamped` saturates at ±65504 |

Not yet: writing tiled files and B44/B44A/DWAA/DWAB (planned next), deep
data and multi-part output.

## License

Apache-2.0. See `NOTICE.md` for the OpenEXR and exrs notices.
