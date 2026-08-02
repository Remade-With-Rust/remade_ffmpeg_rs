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
reported as a result. Content is **real**: frame 0 of the lossless Derf/xiph
originals, plus real screenshots, matplotlib charts, diagrams and logos — the
two synthetic images in early runs were dropped once real graphics showed
different behaviour.

| | vs FFmpeg | verdict |
|---|---|---|
| **Decode** | **2.67–2.95× faster** | 6 real images, 15/15 paired wins each, z = 3.87 |
| **Encode, wall clock, multi-core** | **2.11–3.06× faster, 0.1–0.2% smaller** | end-to-end, matched filter + level, `parallel` |
| **Encode, per core, same filter + size** | **0.69–0.92× (FFmpeg 1.09–1.45× faster)** | which DEFLATE is faster doing identical work |
| **Graphics size, default settings** | **−6.1%** *(was +115.6%)* | 9 real screenshots/charts/diagrams/logos |

The wall-clock row is a **multi-core vs single-core** comparison and is labelled
as such: FFmpeg's PNG encoder is single-threaded for one image, which is the
structural point, but it is never quoted as a per-core win. Per core we are
still behind, and that row is kept directly above it.

Both encode rows are true; they answer different questions, and the second is
the one that flatters us least, so read that one as the codec comparison.

- **The two encode numbers differ only in what FFmpeg is allowed to do.**
  Against FFmpeg's *default* (`paeth`, zlib L6) we are at parity-to-faster and
  smaller. But FFmpeg's `up` filter is cheaper than its `paeth`, so scoring our
  `up` against its `paeth` quietly hands us the easier side. Matching the filter
  **and** the size (within 0.3%), FFmpeg wins: 0.83×/0.90×/0.89× at
  `Compression::Default`, 0.92×/0.69×/0.89× at `Best`. An earlier revision of
  this file quoted only the flattering row; that was an operating-point error of
  exactly the kind this project keeps catching in other people's benchmarks.
- **Our shipped default is a different point again.** `Fast`/`Sub` is 6.0×
  faster than FFmpeg's default and **+16.5% larger** — quoting *that* as a speed
  win would price our missing bits as throughput.
- **Decode is compared with identical work on both sides** — one process per
  arm, same input, same job, output discarded on both. An earlier probe read the
  *opposite* way (FFmpeg ahead) purely because it charged FFmpeg for process
  launch, demux and file read while timing our side in-process with none of
  those. A second bug had our arm decoding *twice* per iteration. Both are
  recorded in `WHYS.md`; neither number is quoted here.

### Where the time actually goes

From the crate's own per-row stage profiler (`--features profile`), on real
content. This is what set the optimisation order — and what ruled work *out*:

| stage | photographic | graphics |
|---|---|---|
| **encode** `deflate` | **97.8–99.5%** | 94.3–99.0% |
| encode `filter` | 0.2% | 0.5–3.0% |
| **decode** `inflate` | **50.3–63.9%** | 7.5–27.6% |
| **decode** `unfilter` | 30.0–40.5% | **53.7–66.3%** |
| decode `transform` | 3.8–7.4% | 6.5–28.1% |

At quality settings encode is **almost entirely DEFLATE** — so the PNG layer
(filtering) is not worth optimising there, and the backend is. FFmpeg's encode
is deflate-dominated too, by ablation against its own flags. (That ablation's
*filter* term came out **negative** — it was differencing two ~1,200 Mcyc
measurements to extract a ~10 Mcyc one, so only the deflate term is admissible.)

Decode inverts on graphics: **unfiltering**, not inflate, is the majority there.

## Why the fork

Two things upstream cannot address for a drop-in FFmpeg replacement:

1. **DEFLATE, not PNG, was the whole encode gap.** At a matched size FFmpeg's
   encoder was **2.6–4.4× faster** than `Compression::Default`/`Best`, because
   upstream routes those through `flate2` → `miniz_oxide` while FFmpeg uses
   zlib. Switching to `zlib-rs` — flate2's **pure-Rust** zlib rewrite, which maps
   to `any_zlib`, *not* `any_c_zlib`, so no C enters the tree — measured
   **1.68–2.72× faster** at `Default` with size within ±3%. That closed most of
   the gap but **not all of it**: at matched filter and size FFmpeg's zlib is
   still 1.09–1.45× ahead, and since the profiler puts DEFLATE at 94–99.5% of
   encode, that residual gap *is* the deflate gap. It is the open item.
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

Every change is gated: the fork is **byte-identical to upstream `png` 0.17.16**
across **600 comparisons** (20 images × 30 configurations, encode bytes *and*
decoded pixels), and the full upstream test suite — pngsuite conformance
included — runs green.

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
