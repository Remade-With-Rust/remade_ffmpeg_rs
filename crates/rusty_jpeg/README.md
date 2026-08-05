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

## Performance

Measured against system **FFmpeg 8.1.2**, same machine, one pinned core each,
CPU time, the two binaries interleaved, paired win-rate with a z-score at
N &ge; 31.

| | vs FFmpeg | |
|---|---|---|
| **Encode** | **parity** — median 0.96&times; | matched output size (ours 3.1% smaller), fixed Huffman tables both sides; N=41, z &minus;3.59 |
| **Decode** | **parity** — median 1.02&times; | byte-identical bitstream, both arms discarding output; N=31, z &minus;0.18, inside noise |

Encode is measured on a 40-frame 1920&times;1080 4:2:0 clip at ~680 KB/frame;
decode on a photographic still stream-copied to 300 frames.

## Compression

Defaults match FFmpeg's MJPEG so the comparison above is like-for-like. Two
options trade encode time for smaller files:

| Option | Size | Encode time | Lossless? |
|---|---|---|---|
| default | baseline | baseline | — |
| `-optimize_huffman 1` | **&minus;7.4%** | 2.09&times; | yes — changes the coding, not the image |
| `-trellis 1` | smaller again | +144% | no — RD-optimal coefficient decisions |

Chroma is **box-averaged** on downsample rather than point-sampled. Point
sampling aliases detail above the subsampled Nyquist into the baseband and no
bitrate recovers it; averaging is worth **&minus;17.12% BD-rate** on
chroma-detailed content and **+2.30 dB** on saturated chroma edges, at no cost
in speed.

`-trellis`'s BD-rate (&minus;2.51% mean) comes from a synthetic corpus that
mispriced its *cost* by 46&times;, so treat the quality figure as unverified on
real material.

## Correctness

- The encoder is gated against the decoder as a **round-trip oracle**, in CI.
- Every SIMD kernel has a **scalar twin** and a test asserting they agree
  bit-exactly; the twin stays in the tree as the fallback.
- **NEON kernels are verified on real ARM hardware** in CI, not merely
  cross-compiled.
- A foreign-encoder corpus (libjpeg progressive and baseline, 4:4:4/4:2:2/4:2:0,
  greyscale, restart intervals, 1&times;1 to 255&times;127) runs through both
  decode paths; with 60 mutations per file that is 117,120 decodes, zero panics.
- `fuzz/` carries cargo-fuzz targets for `decode`, `decode_planar` and a
  round-trip; a deterministic mutation suite runs on stable in ordinary CI.

## Decode

```rust
use std::io::Cursor;

fn main() -> Result<(), rusty_jpeg::decode::Error> {
    let bytes = std::fs::read("in.jpg").expect("read input");

    let mut decoder = rusty_jpeg::Decoder::new(Cursor::new(bytes));
    let pixels = decoder.decode()?;
    let info = decoder.info().expect("populated by decode()");

    println!("{}x{}, {:?}", info.width, info.height, info.pixel_format);
    Ok(())
}
```

`decode_planar()` returns the YCbCr planes directly, skipping upsampling and
colour conversion — use it when the consumer wants planar YUV.

## Encode

```rust
use rusty_jpeg::{ColorType, Encoder};

fn main() -> Result<(), rusty_jpeg::EncodingError> {
    let pixels = [255, 0, 0, 0, 255, 0, 0, 0, 255, 255, 255, 255];

    let mut jpeg = Vec::new();
    let encoder = Encoder::new(&mut jpeg, 90); // quality 0-100
    encoder.encode(&pixels, 2, 2, ColorType::Rgb)?;

    std::fs::write("out.jpg", jpeg).expect("write output");
    Ok(())
}
```

Chroma subsampling (`SamplingFactor`), quantization tables
(`QuantizationTableType`), progressive mode, optimized Huffman tables and
trellis are set on the `Encoder` before `encode`. `PlanarYcbcrImage` accepts
planar YUV input without a round trip through RGB.

## Features

| Feature | Default | Effect |
|---|---|---|
| `std` | yes | Standard library. |
| `simd` | yes | AVX2/SSE4.1 kernels on x86; NEON on aarch64. |
| `rayon` | **no** | Decoder threading. Off because it measured slower at every image size — fork-join costs more than intra-frame parallelism buys. |
| `platform_independent` | no | Drop arch-specific code, `forbid(unsafe_code)`. |

`Decoder::set_single_threaded(true)` selects the synchronous worker: ~38% less
CPU than the threaded default, which remains faster in wall-clock on a
multi-core box.

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
