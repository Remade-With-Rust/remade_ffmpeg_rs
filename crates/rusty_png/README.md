# rusty_png

[![Remade With Rust](https://img.shields.io/badge/Remade%20With-Rust-000?logo=rust&logoColor=fff)](https://github.com/remade-with-rust)
[![By Mata Network](https://img.shields.io/badge/by-Mata%20Network-5b2be0)](https://www.mata.network)
[![License: MIT OR Apache-2.0](https://img.shields.io/badge/license-MIT%20OR%20Apache--2.0-blue)](#licence)
[![crates.io](https://img.shields.io/crates/v/rusty_png.svg)](https://crates.io/crates/rusty_png)
[![docs.rs](https://img.shields.io/docsrs/rusty_png)](https://docs.rs/rusty_png)

Pure-Rust PNG **decoder + encoder**. No C, no FFI. Full colour-type and bit-depth
coverage, APNG, interlacing, and an opt-in pure-Rust zlib backend.

This crate is a performance fork of one upstream pure-Rust project, carried
forward in-tree:

| Half | Upstream | Licence |
|---|---|---|
| `decode` + `encode` | [`image-png`](https://github.com/image-rs/image-png) 0.17.16 | MIT OR Apache-2.0 |

See [`NOTICE.md`](NOTICE.md) for attribution and [`WHYS.md`](WHYS.md) for the
measured descent behind every claim below — including the hypotheses that were
**refuted** on the way.

## Performance vs FFmpeg

Measured against system **FFmpeg 8.1.2** on the same machine, **one pinned core
each**, on-core CPU **cycles** (not wall — wall on this box threw 8,285 ms
outliers on a 156 ms job), arms **ABBA-interleaved**, best-of-N with a paired
win-rate and z-score. **Null-arm floor 2.0–2.3%**; nothing inside that band is
reported as a result, and a row whose useful work sits under 3× its own process
launch is refused rather than estimated. Content is **real**, from two corpora:
the public **CLIC professional** validation set (native-RGB photography, so it
is citable and reproducible by anyone) and frame 0 of the lossless **Derf/xiph**
originals, plus real screenshots, matplotlib charts, diagrams and logos — the
two synthetic images in early runs were dropped once real graphics showed
different behaviour.

| | vs FFmpeg | verdict |
|---|---|---|
| **Decode, per core** | **2.55–2.89× faster** (median 2.60×) | decode-only, PNG → rgb24, 5 admissible images, z = 3 |
| **Encode, wall clock, multi-core** | **2.11–3.06× faster, 0.1–0.2% smaller** | end-to-end, matched filter + level, `parallel` |
| **Encode, per core, same filter + size** | **0.94–1.05×** on CLIC photographs (median 0.97×) · **0.86–0.91×** on Derf video frames | encode-only from raw; which DEFLATE is faster doing identical work |
| **Graphics size, default settings** | **−6.1%** *(was +115.6%)* | 9 real screenshots/charts/diagrams/logos |

Every row is one direction only. The **encode** row feeds both arms **raw
pixels**, so neither decodes — measuring encode by transcoding a PNG lets a
decode win (which we have, and it is large) inflate a number labelled encode,
and that is exactly how an earlier draft of this table briefly read 1.20× in our
favour. Likewise the wall-clock row is a **multi-core vs single-core**
comparison and is labelled as such: FFmpeg's PNG encoder is single-threaded for
one image, which is the structural point, but it is never quoted as a per-core
win, and the per-core row is kept directly beneath it.

Per core, encode is at **parity on photographs and ~13% behind on video
frames**. That split is real and reproduces under one instrument, so it is
reported as two ranges rather than averaged into one.

## Memory

The encoder used to hold several full copies of the image at once. Removing them
is where this fork found its largest reproducible wins — and they are **memory**
wins: every speed measurement taken alongside them landed inside the noise floor,
so none is claimed.

Measured as peak working set, **same configuration at both ends** (a 8.3 MPx
frame unless noted):

| configuration | before | after | |
|---|---|---|---|
| `-compression_level 6`, 1 thread | 94.9 MB | **57.6 MB** | **−39%** |
| `-compression_level 6`, `-threads 8` | 118.7 MB | **85.1 MB** | **−28%** |
| default (`Fast`) | 101.1 MB | **77.3 MB** | **−24%** |

Three redundancies went:

1. **A whole-frame clone** taken whenever the source rows were already tight —
   the common case — duplicating a buffer the encoder already held.
2. **The accumulated IDAT.** The whole compressed stream was built in one buffer
   and then copied into the writer, because a chunk carries its length ahead of
   its payload. Fixed 256 KiB chunks remove the need to know the total at all.
3. **Two of the parallel path's three copies.** It wrote each worker's block to
   its own `Vec`, concatenated them all into a second buffer, then copied that
   again to prepend two header bytes. Blocks now go out as each worker is
   joined, and the IDAT payload is **byte-identical at 2, 4 and 8 threads**.

`Fast` gains the least on purpose: it compresses, then compares the finished
size against a stored-mode bound and re-encodes if compression lost, so it
cannot stream. That check is not vestigial — fdeflate expands uniform random
bytes **1.3686×** and it does fire.

## Why the fork

Two things upstream cannot address for a drop-in FFmpeg replacement:

1. **DEFLATE, not PNG, was the whole encode gap.** At a matched size FFmpeg's
   encoder was **2.6–4.4× faster** than `Compression::Default`/`Best`, because
   upstream routes those through `flate2` → `miniz_oxide` while FFmpeg uses
   zlib. Switching to `zlib-rs` — flate2's **pure-Rust** zlib rewrite, which maps
   to `any_zlib`, *not* `any_c_zlib`, so no C enters the tree — measured
   **1.68–2.72× faster** at `Default` with size within ±3%. That took the gap
   from 2.6–4.4× to **parity on photographs (0.94–1.05×) and ~1.15× on video
   frames**, at 0.2–0.3% *smaller* output. Since the profiler puts DEFLATE at
   94–99.5% of encode, whatever residue remains *is* the deflate gap, not a PNG
   gap — closing the last of it means beating zlib's C, which is the open item.
2. **One hard-coded operating point is the wrong default for PNG.**
   `Fast`/`Sub`/non-adaptive is genuinely excellent on photographs — faster *and*
   smaller than every FFmpeg `-compression_level 1` configuration — and poor on
   graphics, where it ran **+130.1%** against FFmpeg's default across nine real
   screenshots/charts/diagrams. The winning configuration is content-dependent
   and measured so (`best/up` on charts, `best/sub` on screenshots,
   `default/sub/adaptive` on diagrams, `best/paeth` on UI art), which makes a
   single fixed default an unfinished dispatch rather than a tuning choice.
   `rff-codec-png` now dispatches on a measured content signal — repeated-pixel
   fraction, which separates photographs (0.0366–0.2037) from real graphics
   (0.5312–0.9790) with **nothing in between** — taking that corpus from
   **+115.6% to −6.1%** vs FFmpeg while leaving photographs byte-identical.

Every change is gated against upstream `png` 0.17.16, and since **streamed IDAT**
landed the gate reports two properties separately rather than one verdict:

- **Upstream decodes our output to the source pixels: 330/330.** This is the
  property that must never break, and it holds everywhere.
- **Encode bytes identical to upstream: 190/330.** The 140 that differ are
  `Default`/`Best` on images whose compressed stream exceeds one 256 KiB chunk —
  we emit a run of IDATs where upstream emits one. `Fast` is byte-identical on
  every image, and so is anything small enough to fit a single chunk.

The DEFLATE payload itself is unchanged — on a 14.6 MB stream the concatenated
IDAT contents are byte-for-byte what upstream produces; only the chunk framing
differs, at a cost of **+0.0045%** file size. The full upstream test suite —
pngsuite conformance included — runs green.

## Decode

```rust
use std::io::Cursor;

fn main() -> Result<(), rusty_png::DecodingError> {
    let bytes = std::fs::read("in.png").expect("read input");

    let decoder = rusty_png::Decoder::new(Cursor::new(bytes));
    let mut reader = decoder.read_info()?;

    let mut buf = vec![0; reader.output_buffer_size()];
    let info = reader.next_frame(&mut buf)?;

    println!("{}x{}, {:?}", info.width, info.height, info.color_type);
    println!("{} bytes of pixel data", info.buffer_size());
    Ok(())
}
```

## Encode

```rust
use rusty_png::{BitDepth, ColorType, Compression, FilterType};

fn main() -> Result<(), rusty_png::EncodingError> {
    // A 2x1 RGB image: red, blue.
    let pixels = [255u8, 0, 0, 0, 0, 255];

    let mut out = Vec::new();
    {
        let mut encoder = rusty_png::Encoder::new(&mut out, 2, 1);
        encoder.set_color(ColorType::Rgb);
        encoder.set_depth(BitDepth::Eight);
        // The knobs that matter: pick a point on the speed/size curve.
        encoder.set_compression(Compression::Default);
        encoder.set_filter(FilterType::Up);
        encoder.write_header()?.write_image_data(&pixels)?;
    }

    std::fs::write("out.png", out).expect("write output");
    Ok(())
}
```

`set_adaptive_filter(AdaptiveFilterType::Adaptive)` chooses a filter per row and
is the strongest setting on text and screenshot content.

## Features

| Feature | Default | Effect |
|---|---|---|
| `zlib-rs` | **yes** | DEFLATE via flate2's **pure-Rust** zlib rewrite instead of `miniz_oxide`. Measured **1.68–2.72×** faster at `Compression::Default`, size within ±3%. Maps to flate2's `any_zlib`, not `any_c_zlib` — no C is introduced. On by default: it dominates at `Default` (faster on 13/13, size within ±4.4%). At `Best` it is smaller on 9/9 real graphics but slower on 5/9 — recorded, not averaged away; reaching sizes miniz_oxide cannot reach at any speed is what `Best` is for. |
| `profile` | no | Per-row stage profiler (filter/deflate on encode; inflate/unfilter/transform on decode). Scopes are per *row*, so the tap costs <0.1% of a 1080p encode; compiles to nothing when off. |
| `parallel` | no | Multi-threaded DEFLATE for a **single** image (pigz-style block splitting). **2.11–3.06× end-to-end vs FFmpeg** at matched filter and level, while staying 0.1–0.2% smaller. Applies to `Compression::Default`/`Best` only — `Fast` is `fdeflate`, a single-stream path. Blocks are *sized* (≥1 MiB), never counted, so an image too small to split stays serial and pays **+0.00%**; forcing 24 blocks on a 1.44 MB chart would have cost **+7.44%**. |
| `benchmarks` | no | Expose internal kernels (`unfilter`, `expand_paletted`) for A/B oracle tests. |
| `unstable` | no | `crc32fast/nightly`. |

## Part of Remade With Rust

This crate is the standalone PNG engine of
**[remade_ffmpeg_rs](https://github.com/Remade-With-Rust/remade_ffmpeg_rs)** — a
ground-up, permissively-licensed Rust rebuild of FFmpeg: a drop-in
`ffmpeg`/`ffprobe` CLI on pure-Rust codecs, with no copyleft. Also check out our
sister project **[FFAI](https://github.com/Remade-With-Rust/FFAI)** — media for
an AI-first world — and the rest of
**[github.com/remade-with-rust](https://github.com/remade-with-rust)**, including
the sibling codec crates
[`rusty_h264`](https://crates.io/crates/rusty_h264),
[`rusty_jpeg`](https://crates.io/crates/rusty_jpeg),
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

`MIT OR Apache-2.0`, inherited unchanged from image-rs/image-png. See
[`LICENSE-MIT`](LICENSE-MIT), [`LICENSE-APACHE`](LICENSE-APACHE) and
[`NOTICE.md`](NOTICE.md).
