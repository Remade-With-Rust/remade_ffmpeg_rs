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
| **Encode** | **1.19× faster (16%)** | at matched output size |
| **Decode** | **~1.04× faster (4%)** | paired ABBA, N=15, 3000-frame arms, median 1.0390 |

Both numbers are deliberately unflattering to us where the methodology allows a
choice:

- **Encode** is compared at **matched output size**, not matched `-q:v`. At
  equal `-q:v` our files are 41% larger, which is a different operating point
  and would price our extra bits as speed. Ours at `-q:v 5` is 27.17 MB against
  FFmpeg's 28.04 MB at `-q:v 3` — we are *slightly smaller* and still faster.
  Both run fixed Huffman tables in one pass, because our CLI defaults to
  optimized tables (two passes) and FFmpeg's does not; comparing defaults would
  price our better compression as a speed loss.
- **Decode** is compared on a **byte-identical bitstream** — the reference clip
  is stream-copied from the exact fixture and `cmp`-gated before timing, after
  an earlier harness was caught feeding FFmpeg a 2.67× smaller file. Arms are
  3000 frames (~18 s) each, because at 2 s the per-run transients swamped the
  effect. The quoted figure is the **paired** one: single-instrument medians read
  1.072 for the same build, and the paired test has been the conservative and
  reproducible number every time, so that is what we publish.

### Compression, not just speed

- **Chroma is box-averaged on downsample.** Point-sampling — taking one of every
  four chroma samples — aliases detail above the subsampled Nyquist into the
  baseband, and no bitrate recovers it. BD-rate **-17.12%** on chroma-detailed
  content, **+2.30 dB** on saturated chroma edges, neutral on smooth photos.
- **Trellis quantization** (on by default) picks each block's EOB position with
  a real rate model instead of keeping whatever rounding produced: **-3.14%
  BD-rate for +3.1% encode time**.

Both are gated on a corpus BD-rate across 5 quality points and 3 content types
plus an ffmpeg round-trip — never a single operating point.

### Where the decode speed comes from, all gated on byte-identical output:

- an **AVX2 forward-DCT + quantize** kernel in the encoder;
- an **AVX2 IDCT that transforms two 8×8 blocks per instruction stream**
  (block A in the low 128-bit lane, block B in the high one — every op involved
  is lane-independent, so it is byte-identical to running the SSSE3 kernel
  twice, which a 64-round oracle test asserts). Worth 7.8% of whole decode;
- a **whole-block DC-only shortcut** ahead of the SIMD dispatch (~31% of blocks
  on photographic content have no AC energy);
- the fused path is reached through a **concrete, inlinable** call resolved once
  per MCU rather than a `dyn` dispatch per block — worth 5.4% of whole decode,
  mostly because it lets the transform dispatch inline into the decode loop;
- **entropy decode fused into the IDCT**: a baseline interleaved scan transforms
  each block the moment it is decoded, rather than accumulating an MCU row of
  coefficients first. That row buffer existed only to ship work to another
  thread, and cost a write → zero → read round trip of ~18.8 MB per 1080p frame
  through a buffer too large for L1;
- **buffered entropy reads with a bulk 8-byte refill**, and **recycled** output
  planes;
- **no rayon.** Upstream `jpeg-decoder` enables it by default; measured here it
  is a net loss at **every** image size — 1.32× slower at 640×480, **1.91×** at
  1920×1080, 1.32× at 3840×2160. The fork-join costs more than the parallelism
  it buys within a single frame.

`Decoder::set_single_threaded(true)` selects the synchronous worker, which uses
**~38% less CPU** than the threaded one; the threaded default is still the
faster choice in wall-clock on a multi-core box (6.97 vs 7.96 ms/frame).

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
| `simd` | yes | Encoder AVX2 FDCT + quantize + colour conversion. |
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
