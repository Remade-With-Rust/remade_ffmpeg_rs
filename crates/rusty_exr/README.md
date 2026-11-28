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
OpenEXR's AVX routine — down to which NaN survives when a block holds
infinities or NaNs — and B44/DWA use OpenEXR's own transfer tables.

## What it writes

Single-part files, scanline or tiled (`EncodeOptions::tiles`, one
resolution level, any line order including `Random`, which the `exr`
crate calls `Unspecified`), with **every compression**: None, RLE, ZIPS,
ZIP, PIZ, PXR24, B44, B44A, DWAA and DWAB (`EncodeOptions::dwa_level`,
default 45). HALF, FLOAT and UINT channels; any channel names (sorted for
you, as the format requires); your header attributes.

By default files are **byte-identical to the `exr` crate's** for the same
image and settings, the lossy codecs included: the DWA writer reproduces
`exr`'s encoder operation for operation (its colour transform, its forward
DCT as `exr` runs it on x86-64, including which NaN survives in a block
holding infinities or NaNs), and B44 is OpenEXR's packer. Two exceptions:
for channel names longer than 31 bytes this crate sets the long-names flag
the format requires and `exr` leaves clear, and `exr`'s multi-threaded
writer stores `Unspecified`-order chunks in whatever order its threads
finish (this crate, and `exr` single-threaded, store them in order). `exr`
derives three of its DWA DCT constants from the platform's `cosf`, which
differs in the last bit between glibc and the Windows CRT; this crate
computes them the same way, so it matches `exr` on each platform.

OpenEXR's own DWA encoder transforms colour and frequency slightly
differently, so its bytes differ, but OpenEXR (and `exr`) decode every file
this crate writes, and decode it to exactly the samples this crate's
decoder produces.

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
- **Lossy and tiled encode, one thread:** B44/B44A 1.0-1.15x faster than
  OpenEXR on half images (2.3x on float layers, which B44 stores raw), DWA
  within 0.89-1.10x of OpenEXR (inside the noise band on most rows), tiled
  RLE 1.02-1.14x faster; B44 and DWA 1.05-1.6x faster than `exr`, tiled
  RLE 3-9% behind it. With 24 threads OpenEXR leads on all but one row, by
  up to 1.5x, most on the small image.
- **DWA decode** is 1.7-2.3x behind OpenEXR single-threaded (its inverse
  DCT is hand-written AVX assembly; this crate reproduces its arithmetic in
  scalar code) and 1.06-1.33x behind with 24 threads; it is 1.1-1.3x ahead
  of `exr`.

What remains is mostly deflate: OpenEXR links libdeflate, this crate uses
pure-Rust zlib-rs (inflate, and `Deflate::Fast`) and miniz_oxide (the
exr-compatible writer). The pull request that introduced the crate has the
per-file tables and the method.

**Into your planes, against `exr`'s one-pass reader and writer** (1080p
RGBA from a real photo, HALF and FLOAT, into or from four `f32` planes,
Windows x86-64, median of 21 interleaved rounds against `exr` 1.74.2's
custom-`ChannelsReader` reader and its `SpecificChannels` writer):
`decode_part_into` is as fast or faster on every compression tried (None,
RLE, ZIPS, ZIP, PXR24, B44, DWAB) single-threaded (0.34-1.00x the time) and
with all cores (0.27-0.97x; 0.18-0.67x with `decode_bands` on a rayon
pool); `encode_ref` takes 0.18-0.97x `exr`'s time on one core and
0.16-0.99x on all cores, ZIP on all cores being at parity.

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

## Decoding into your buffers, encoding from your planes

`Decoder::decode_part_into` writes every chunk straight into planes you
own, one `ChannelSink` per channel in channel-list (name) order, in one
pass: no `Image`, no per-chunk block, no copy afterwards. A sink can
convert on the way (any channel into `f32`, FLOAT into `f16` bits),
skip a channel, or use a row stride so the plane can be a window into a
wider buffer. The planes come out exactly as `decode_part` (followed by
`Samples::to_f32` or `f32_to_f16`, where a sink converts) would give
them, for any thread count.

```rust,no_run
use rusty_exr::{ChannelSink, Decoder, EncodeOptions, ImageRef, SamplesRef};

let bytes = std::fs::read("render.exr")?;
let dec = Decoder::new(&bytes)?;
let (w, h) = dec.plane_sizes(0, 0, 0)?[0];
let [mut r, mut g, mut b, mut a] = [(); 4].map(|()| vec![0f32; w * h]);
let mut sinks: Vec<ChannelSink> = Vec::new();
let mut want = [("A", Some(&mut a)), ("B", Some(&mut b)), ("G", Some(&mut g)), ("R", Some(&mut r))];
for c in dec.header(0).unwrap().channels() {
    let plane = want.iter_mut().find(|(n, _)| *n == c.name).and_then(|(_, p)| p.take());
    sinks.push(plane.map_or_else(ChannelSink::skip, |p| ChannelSink::f32(p)));
}
dec.decode_part_into(0, &mut sinks)?; // HALF, FLOAT and UINT all land as f32
drop(sinks);

// And back out, borrowing the planes: the same bytes `encode` writes.
let image = ImageRef::new(w as u32, h as u32)
    .with_channel("R", SamplesRef::F32(&r))
    .with_channel("G", SamplesRef::F32(&g))
    .with_channel("B", SamplesRef::F32(&b))
    .with_channel("A", SamplesRef::F32(&a));
let file = rusty_exr::encode_ref(&image, &EncodeOptions::default())?;
# let _ = file;
# Ok::<(), Box<dyn std::error::Error>>(())
```

- `Decoder::threads(..)` sets the workers `decode_part_into` uses
  (`Threads::Auto` by default).
- An application with its own thread pool can call
  `Decoder::decode_bands` instead. It returns one `DecodeBand` job per row
  band, each holding its own disjoint rows of your planes; run them on the
  pool with a `DecodeScratch` per thread, e.g. with rayon:
  `bands.into_par_iter().try_for_each_init(DecodeScratch::default, |s, b| b.decode(s))`.
  A persistent pool skips the thread start-up that `Threads::Auto` pays on
  every call, which is most of the cost of the cheap codecs (None, RLE)
  on many cores.
- `Decoder::decode_chunk_into(part, index, sinks, scratch)` decodes one
  chunk into sinks that cover just that chunk (`Decoder::chunk_rect` gives
  its rectangle; `ChunkRect::channel_size` its size per channel).

## Migrating from `exr`

| `exr` 1.74 | `rusty_exr` |
|---|---|
| `read_first_rgba_layer_from_file(path, create, set)` | `rusty_exr::decode(&bytes)?.to_rgba_f32("")` (interleaved RGBA `f32`; missing alpha is 1, `Y`-only is grey) |
| `read_all_flat_layers_from_file(path)` / `read().no_deep_data().largest_resolution_level().all_channels().all_layers()` | `let d = Decoder::new(&bytes)?;` then `d.decode_part(i)` for `i in 0..d.part_count()`; channels keep full names (`diffuse.R`), `Image::layers()` lists the prefixes |
| `.all_resolution_levels()` | `d.level_count(i)` and `d.decode_level(i, lx, ly)` |
| `FlatSamples::F16(Vec<f16>)` | `Samples::F16(Vec<u16>)` — the raw half bits; `rusty_exr::f16_to_f32` converts |
| one-pass reader into your storage: a custom `ReadChannels`/`ChannelsReader` whose `read_block` converts each `UncompressedBlock`'s rows into your planes, or `rgba_channels(create, set)` / `specific_channels()` with a per-pixel setter | `d.decode_part_into(part, &mut sinks)`: one `ChannelSink::f32(&mut plane)` (or `f16`/`u32`) per channel in name order, `ChannelSink::skip()` for the ones you do not want; rows are converted into your planes as each chunk decompresses |
| `f16` → `f32` in `FromNativeSample` | `ChannelSink::f32` on a HALF channel (exact, SIMD) |
| `.non_parallel()` / exr's parallel decompression (a rayon pool per read) | `d.threads(Threads::Single)` / `Threads::Auto`, or `d.decode_bands(..)` run on your own pool |
| `.largest_resolution_level()` / `.specific_resolution_level(..)` | `decode_part_into` / `d.decode_level_into(part, lx, ly, &mut sinks)` |
| `exr::block` raw chunks | `d.decode_chunk_into(part, index, &mut sinks, &mut scratch)` into your buffers, or `d.decode_chunk(part, index)` → `Block { pos, x0, y0, width, height, data }`, rows via `Block::rows()` / `Block::row(channel, y)` |
| `meta::MetaData::read_from_file` | `rusty_exr::read_headers(&bytes)` |
| `write_rgba_file(path, w, h, f)` | `Image::new(w, h).with_channel("R", ..)...`, then `rusty_exr::encode(&img, &EncodeOptions::new(Compression::Zip))` |
| `Image::from_channels(size, SpecificChannels/AnyChannels).write().to_file(path)` with `Encoding { compression, .. }` | `Image { channels: vec![Channel::new(name, w, h, samples), ..], .. }` + `EncodeOptions { compression, line_order, threads, .. }`; byte-identical output for every compression |
| `SpecificChannels::rgba(\|pos\| (r[i], g[i], b[i], a[i]))` reading your planes | `ImageRef::new(w, h).with_channel("R", SamplesRef::F32(&r))...` + `rusty_exr::encode_ref(&image, &opts)`: no copy of the planes, same bytes as `encode` |
| `Compression::DWAA(Some(level))` / `DWAB(..)` | `EncodeOptions { compression: Compression::Dwaa, dwa_level: Some(level), .. }` (`None` = 45, no attribute, as `exr`) |
| `Encoding { blocks: Blocks::Tiles(Vec2(64, 64)), line_order: LineOrder::Unspecified, .. }` | `EncodeOptions { tiles: Some((64, 64)), line_order: LineOrder::Random, .. }` |
| `layer_attributes.other` / `shared_attributes.chromaticities` | `image.header.set("chromaticities", AttributeValue::Chromaticities(..))`; any attribute via `header.get(name)` / `header.set(name, value)` |
| `f16::from_f32(v)` for output prep | `rusty_exr::f32_to_f16(v)` (round to nearest even, NaN kept); `f32_to_f16_clamped` saturates at ±65504 |

Not yet: deep data, multi-part output, and writing mipmap/ripmap levels.

## License

Apache-2.0. See `NOTICE.md` for the OpenEXR and exrs notices.
