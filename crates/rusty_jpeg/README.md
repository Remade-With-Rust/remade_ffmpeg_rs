# rusty_jpeg

[![Remade With Rust](https://img.shields.io/badge/Remade%20With-Rust-000?logo=rust&logoColor=fff)](https://github.com/remade-with-rust)
[![By Mata Network](https://img.shields.io/badge/by-Mata%20Network-5b2be0)](https://www.mata.network)
[![License: MIT OR Apache-2.0 AND IJG](https://img.shields.io/badge/license-MIT%20OR%20Apache--2.0%20AND%20IJG-blue)](#licence)
[![crates.io](https://img.shields.io/crates/v/rusty_jpeg.svg)](https://crates.io/crates/rusty_jpeg)
[![docs.rs](https://img.shields.io/docsrs/rusty_jpeg)](https://docs.rs/rusty_jpeg)

Pure-Rust JPEG / MJPEG **decoder + encoder**. No C, no FFI. Baseline and
progressive DCT, planar YUV in and out, with real quality and
chroma-subsampling control.

This crate is a vendored merge of two upstream pure-Rust projects, carried
forward in-tree as one codec:

| Half | Upstream | Licence |
|---|---|---|
| `decode` | [`jpeg-decoder`](https://github.com/image-rs/jpeg-decoder) 0.3.2 | MIT OR Apache-2.0 |
| `encode` | [`jpeg-encoder`](https://github.com/vstroebel/jpeg-encoder) 0.7.0 | (MIT OR Apache-2.0) AND IJG |

See [`NOTICE.md`](NOTICE.md) for attribution and the IJG obligations, and
[`CHANGES.md`](CHANGES.md) for everything changed since vendoring.

> This software is based in part on the work of the Independent JPEG Group.

## Performance vs FFmpeg

Both halves are measured against system **FFmpeg 8.1.2** on the same machine,
one pinned core each, **CPU time** (not wall), best-of-N with the arms
alternated. Content is a 1920×1080 4:2:0 photographic fixture (1/f fractal
noise, q90, 11.8× compression) — synthetic clips misprice JPEG stage shares
badly, so this one is calibrated to real photographic coefficient density.

| | vs FFmpeg | verdict |
|---|---|---|
| **Encode** | **at parity** (median 0.96x, i.e. ~4% slower) | matched output size, single-threaded, fixed Huffman tables both sides; paired N=41, z -3.59 |
| **Decode** | **at parity** (median 1.02x) | byte-identical bitstream, both discarding output; paired N=31, z -0.18, inside noise |

## Why merge them

Holding the two halves in one crate buys three things the separate crates
could not:

1. **The encoder is gated against the decoder as a round-trip oracle** — the
   standing correctness gate for every change, running in CI on real content
   rather than as a one-off bring-up check.
2. **Shared primitives** — quantization tables, zig-zag order, and the
   feature-gated stage profiler are defined once.
3. **The encoder's SIMD is on by default.** Upstream put its AVX2 FDCT and
   RGB→YCbCr kernels behind a non-default `simd` feature; consumers who took
   the default (as this workspace did) silently ran the scalar path.

## Decode

```rust
use std::io::Cursor;

fn main() -> Result<(), rusty_jpeg::decode::Error> {
    let bytes = std::fs::read("in.jpg").expect("read input");

    let mut decoder = rusty_jpeg::Decoder::new(Cursor::new(bytes));
    let pixels = decoder.decode()?;
    let info = decoder.info().expect("populated by decode()");

    println!("{}x{}, {:?}", info.width, info.height, info.pixel_format);
    println!("{} bytes of pixel data", pixels.len());
    Ok(())
}
```

## Encode

```rust
use rusty_jpeg::{ColorType, Encoder};

fn main() -> Result<(), rusty_jpeg::EncodingError> {
    // A 2x2 RGB image: red, green, blue, white.
    let pixels = [255, 0, 0, 0, 255, 0, 0, 0, 255, 255, 255, 255];

    let mut jpeg = Vec::new();
    let encoder = Encoder::new(&mut jpeg, 90); // quality 0-100
    encoder.encode(&pixels, 2, 2, ColorType::Rgb)?;

    std::fs::write("out.jpg", jpeg).expect("write output");
    Ok(())
}
```

Chroma subsampling (`SamplingFactor`) and quantization table selection
(`QuantizationTableType`) are set on the `Encoder` before `encode`.

## Features

| Feature | Default | Effect |
|---|---|---|
| `std` | yes | Standard library. |
| `simd` | yes | Encoder AVX2 FDCT + quantize + colour conversion on x86; NEON forward DCT + quantize on aarch64. |
| `rayon` | **no** | Decoder work-stealing threading. Off because it measured *slower* at every image size (see above); kept so the measurement is cheap to repeat. |
| `platform_independent` | no | Decoder: drop arch-specific code, `forbid(unsafe_code)`. |
| `benchmark` | no | Expose internal kernels for A/B oracle tests. |
| `profile` | no | Feature-gated stage profiler; zero-cost when off. |
| `counters` | no | Deterministic event counters (symbols, refills, LUT hits). Separate from `profile` on purpose: enabling both at once perturbed cycle counts 3×. |

## Part of Remade With Rust

This crate is the standalone JPEG engine of
**[remade_ffmpeg_rs](https://github.com/Remade-With-Rust/remade_ffmpeg_rs)** — a
ground-up, permissively-licensed Rust rebuild of FFmpeg: a drop-in
`ffmpeg`/`ffprobe` CLI on pure-Rust codecs, with no copyleft. Also check out our
sister project **[FFAI](https://github.com/Remade-With-Rust/FFAI)** — media for
an AI-first world — and the rest of
**[github.com/remade-with-rust](https://github.com/remade-with-rust)**, including
the sibling codec crates
[`rusty_h264`](https://crates.io/crates/rusty_h264),
[`rusty_vp9`](https://crates.io/crates/rusty_vp9),
[`rusty_mp3`](https://crates.io/crates/rusty_mp3),
[`rusty_aac`](https://crates.io/crates/rusty_aac),
[`rusty-opus`](https://crates.io/crates/rusty-opus),
[`rusty_vorbis`](https://crates.io/crates/rusty_vorbis), and the
[rusty-av1-toolkit](https://github.com/Remade-With-Rust/rusty-av1-toolkit) forks.

## About Mata Network

<!-- ORG BOILERPLATE — keep identical across repos -->

[Mata Network](https://www.mata.network) builds sovereign, self-hostable
infrastructure. **Remade With Rust** is our open-source home for the
permissively-licensed building blocks that work depends on.

<!-- /ORG BOILERPLATE -->

## Licence

`(MIT OR Apache-2.0) AND IJG` — the IJG clause attaches to the forward-DCT
files inherited from the encoder. See [`NOTICE.md`](NOTICE.md).
