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
| **Encode** | **parity to 1.17× faster, at 0.2–6.0% smaller** | at a matched operating point, with `zlib-rs` |

Both numbers are deliberately unflattering to us where the methodology allows a
choice:

- **Encode is compared at a matched operating point, and the operating point is
  chosen against us.** PNG is lossless, so "faster" is meaningless without
  fixing size. Our shipped default (`Fast`/`Sub`) is 6.0× faster than FFmpeg's
  default but **+16.5% larger** — a different point on the curve, and quoting it
  would price our missing bits as speed. At `Compression::Default` + `Up` with
  `zlib-rs` we are **1.00×/1.06×/1.14×/1.17×** against FFmpeg's default while
  producing **−6.0%/−0.2%/−3.3%/−4.5%** bytes. Against `Compression::Best` the
  fair reference is FFmpeg `-compression_level 9`, not its default; comparing
  our maximum to their default is the same error in the other direction.
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
   **1.68–2.72× faster** at `Default` with size within ±3%, closing that gap to
   parity-or-better.
2. **One hard-coded operating point is the wrong default for PNG.**
   `Fast`/`Sub`/non-adaptive is genuinely excellent on photographs — faster *and*
   smaller than every FFmpeg `-compression_level 1` configuration — and poor on
   graphics, where it ran **+130.1%** against FFmpeg's default across nine real
   screenshots/charts/diagrams. The same corpus at the crate's *best reachable*
   settings comes out **−5.7%**. The winning configuration is content-dependent
   and measured so (`best/up` on charts, `best/sub` on screenshots,
   `default/sub/adaptive` on diagrams, `best/paeth` on UI art), which makes a
   single fixed default an unfinished dispatch rather than a tuning choice.

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
| `zlib-rs` | **no** | DEFLATE via flate2's **pure-Rust** zlib rewrite instead of `miniz_oxide`. Measured **1.68–2.72×** faster at `Compression::Default`, size within ±3%. Maps to flate2's `any_zlib`, not `any_c_zlib` — no C is introduced. Off by default pending the `Best`-on-graphics dispatch question (see `WHYS.md`). |
| `profile` | no | Per-row stage profiler (filter/deflate on encode; inflate/unfilter/transform on decode). Scopes are per *row*, so the tap costs <0.1% of a 1080p encode; compiles to nothing when off. |
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
