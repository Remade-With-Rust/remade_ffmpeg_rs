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
one pinned core each, **CPU time** (not wall), with the two binaries
**interleaved** (alternating which runs first each round) so machine drift
cancels — the same FFmpeg command drifted 953 → 1266 ms within one session here.

The verdict is a **paired win-rate with a z-score**, not a best-of-N minimum, and
at **N ≥ 31**. That is not pedantry: see [the note below](#on-the-numbers-moving).

| | vs FFmpeg | verdict |
|---|---|---|
| **Encode** | **at parity** (median 0.96×, i.e. ~4% slower) | matched output size, single-threaded, fixed Huffman tables both sides; paired N=41, z −3.59 |
| **Decode** | **at parity** (median 1.02×) | byte-identical bitstream, both arms discarding output; paired N=31, z −0.18, inside noise |

**Content.** Encode is measured on a 40-frame 1920×1080 4:2:0 clip at ~680 KB
per frame, at settings giving matched output size (ours 27.17 MB vs FFmpeg's
28.04 MB — 3.1% smaller, so the match slightly favours FFmpeg). Decode is
measured on a 1/f-fractal photographic still (q90, 11.8× compression),
stream-copied into a 300-frame clip so both arms consume a byte-identical
bitstream.

Two different fixtures on purpose. Synthetic content misprices anything whose
cost scales with coefficient density — this crate shipped a trellis quantizer at
a published "+3.1% encode time" that measured **+144%** on real footage, because
the corpus it was calibrated on was ~223 KB/frame.

### On the numbers moving

Releases 0.1.x–0.2.x claimed **1.19× / 1.22× / 1.45× faster than FFmpeg**. None
of those were real. The same comparison, same clip, same matched size:

| N | median | z | what it looked like |
|---|---:|---:|---|
| 15 | 1.4510 | 2.32 | "45% faster" — and the z cleared the bar |
| 21 | 1.0286 | 0.65 | parity |
| **41** | **0.9608** | **−3.59** | **~4% slower, a verdict the other way** |

The estimator *trends* with N for comparisons between unlike binaries, so a
low-N reading is not a noisy estimate of the truth — it is a different number.
Same-binary A/Bs on this crate reached z 3.36 and 4.20 by N=25 and held; the
cross-binary one wandered from +45% to −4% over the same range.

What is solid are the **same-binary deltas**, where both arms are this crate and
drift cancels. Recent, each byte-identical and each a verdict:

| change | effect | evidence |
|---|---|---|
| AC coefficients found by SIMD mask instead of a branch per coefficient | **1.28×** whole encode | 25 pairs, z 3.36 |
| SIMD block extraction (`cvtepu8_epi16` luma, `maddubs` 4:2:0 chroma) | **1.25×** whole encode | 25 pairs, z 4.20 |

The codec got materially faster; it is at parity with FFmpeg rather than ahead
of it. Both statements are true and only the second one is a claim about FFmpeg.

### Compression, not just speed

The speed story is parity. The compression story is not — these are wins over
what this crate shipped before, and over a decoder that decimated chroma:

- **Chroma is box-averaged on downsample.** Point-sampling — taking one of every
  four chroma samples — aliases detail above the subsampled Nyquist into the
  baseband, and no bitrate recovers it. BD-rate **−17.12%** on chroma-detailed
  content, **+2.30 dB** on saturated chroma edges, neutral on smooth photos.
- **`-optimize_huffman 1`** builds tables from the image's own symbol
  statistics: **−7.4%** file size for **2.09×** encode time. Lossless — it
  changes the coding, not the image. Off by default, because FFmpeg's MJPEG has
  no equivalent and the like-for-like comparison above is with fixed tables.
- **`-trellis 1`** picks each block's EOB position with a real rate model, then
  lowers coefficient magnitudes where the bits saved outweigh the distortion:
  **−2.51% mean BD-rate** across six content types. Off by default: it costs
  **+144% encode time** on real footage, and its BD-rate comes from the same
  synthetic corpus that mispriced the cost by 46×, so treat it as unverified on
  real material until re-measured.

The first is gated on a corpus BD-rate across 5 quality points and 3 content
types plus an FFmpeg round-trip — never a single operating point.

### Where the speed comes from, all gated on byte-identical output

Encoder:

- **AVX2 forward-DCT + quantize**, and a **NEON** quantize + forward DCT on
  aarch64 (verified bit-exact against the scalar oracle on real ARM hardware in
  CI, not merely cross-compiled);
- **AC coefficients located by a SIMD mask** rather than a branch per
  coefficient — one AVX2 compare builds a 64-bit non-zero mask and
  `trailing_zeros` steps the set bits, so the loop runs `popcount` times instead
  of 63. Worth **1.28×** whole encode;
- **SIMD block extraction** — `cvtepu8_epi16` for luma rows, `maddubs` for the
  4:2:0 chroma box filter. Worth **1.25×** whole encode.

Decoder:

- an **AVX2 IDCT that transforms two 8×8 blocks per instruction stream** (block A
  in the low 128-bit lane, B in the high one — every op involved is
  lane-independent, so it is byte-identical to running the SSSE3 kernel twice,
  which a 64-round oracle test asserts). Worth 7.8% of whole decode;
- a **whole-block DC-only shortcut** ahead of the SIMD dispatch (~31% of blocks
  on photographic content have no AC energy);
- **entropy decode fused into the IDCT**: a baseline interleaved scan transforms
  each block the moment it is decoded rather than accumulating an MCU row of
  coefficients first. That row buffer existed only to ship work to another
  thread, and cost a write → zero → read round trip of ~18.8 MB per 1080p frame
  through a buffer too large for L1;
- **buffered entropy reads with a bulk 8-byte refill**, and **recycled** output
  planes;
- **no rayon.** Upstream `jpeg-decoder` enables it by default; measured here it
  is a net loss at **every** image size — 1.32× slower at 640×480, **1.91×** at
  1920×1080, 1.32× at 3840×2160. Fork-join costs more than the parallelism it
  buys within a single frame.

`Decoder::set_single_threaded(true)` selects the synchronous worker, which uses
**~38% less CPU** than the threaded one; the threaded default is still faster in
wall-clock on a multi-core box (6.97 vs 7.96 ms/frame).

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
