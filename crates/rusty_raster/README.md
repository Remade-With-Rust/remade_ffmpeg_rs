# rusty_raster

**A pure-Rust 2D rasteriser with a tiny-skia-shaped API.** Anti-aliased fill
and stroke (every join and cap, dashes, hairlines), non-zero and even-odd
fill rules, linear, two-point-conical and sweep gradients with pad, repeat
and reflect spread, all Porter-Duff and SVG blend modes, alpha and
luminance masks, and pixmap drawing with nearest, bilinear and bicubic
sampling. `#![forbid(unsafe_code)]`, no dependencies (PNG I/O through the
house `rusty_png`, behind the default `png-format` feature).

It is a hardened fork of [tiny-skia](https://github.com/linebender/tiny-skia)
0.12.0 (itself a port of a subset of Skia), with tiny-skia-path merged in.
See [NOTICE.md](NOTICE.md) for attribution and what changed.

Part of **[remade_ffmpeg_rs](https://github.com/Remade-With-Rust/remade_ffmpeg_rs)**
by **[Mata Network](https://www.mata.network/)**.

## What is verified

**Oracle: tiny-skia 0.12.0** (dev-dependency only). A deterministic corpus of
645 scenes is described once and rendered by both crates; the RGBA8 buffers
are compared byte for byte
(`cargo test -p rusty_raster --release --test oracle -- --nocapture`):

| category | scenes | pixel-identical | max channel diff | min PSNR |
|---|---:|---:|---:|---:|
| shapes (AA on/off x both fill rules) | 36 | 36 | 0 | identical |
| self-intersecting stars | 6 | 6 | 0 | identical |
| random lines/quads/cubics under transforms | 40 | 40 | 0 | identical |
| every join x cap x width, open/closed, miter limits | 156 | 156 | 0 | identical |
| thin strokes and hairlines (width 0 to 1.25, transformed) | 42 | 42 | 0 | identical |
| dashes | 31 | 31 | 0 | identical |
| gradients (every kind x spread, random, colour spaces) | 66 | 66 | 0 | identical |
| transforms | 20 | 20 | 0 | identical |
| all 29 blend modes, solid and gradient source | 58 | 55 | 1 | 74.96 dB |
| masks (path and pixmap, alpha and luminance) | 20 | 20 | 0 | identical |
| `draw_pixmap` (every filter quality, transforms, opacity) | 42 | 42 | 0 | identical |
| mixed random scenes | 120 | 119 | 1 | 80.48 dB |
| edge cases (huge coordinates, 1-pixel canvases) | 8 | 8 | 0 | identical |
| **total** | **645** | **641** | **1** | **74.96 dB** |

The four differing scenes all use `ColorDodge` or `ColorBurn` and differ by
one level on at most 0.83% of pixels. Those two modes divide through
`recip_fast`, which tiny-skia computes with the SSE2 `rcpps` instruction, a
12-bit hardware approximation that differs between CPU vendors. This crate
divides exactly. The test asserts this: any other difference fails it.

tiny-skia's own integration suite (157 tests against 121 reference PNGs) is
ported under `tests/integration` and passes unchanged.

**Hostile input: 11 tests** (`tests/hostile.rs`, 1.99 s in a release build).
Each draw call must return without panicking and in bounded time and memory.
The tests cover:

- zero-size, 1-pixel and oversized pixmaps and masks: a 10^18-byte request
  returns `None` instead of aborting;
- non-finite path points, including a NaN first point;
- 14 nasty values (NaN, infinities, `f32::MAX`, 1e30, 1e-30, subnormal,
  signed zeros) in every transform slot of every draw call, over 10,000 calls;
- the same values as gradient stops, centres, radii and angles, and 10,000
  unsorted gradient stops;
- the same values as stroke widths and miter limits for every join and cap;
- degenerate, tiny and huge dash arrays, and a 10^9-dash request (refused
  past 1M segments);
- coordinates up to 3e38, the milestone-2 repro (a 1e-31 circle scaled by
  up to 1e35), and a 200,000-segment path;
- wrong-size masks and `i32::MIN`/`MAX` pixmap offsets;
- a seeded random sweep of paths, paints, strokes, dashes, masks and
  transforms drawn from those values (3,000 iterations by default; 20,000
  were run once, in 20.97 s, with no panic).

tiny-skia 0.12 panics or aborts on several of these. The fixes are all in
this crate:

- **Out-of-bounds spans.** Hostile edges overflow fixed point and produce
  spans outside the clip. tiny-skia then indexes out of bounds in
  `AlphaRuns` and the pipeline blitter, and its `SuperBlitter` `Drop` flush
  panics a second time, which aborts the process. This crate clips every span
  at the blitter instead.
- **NaN first point.** `Rect::from_points` let a NaN first point through, and
  the stroker then requested a 16 GiB allocation.
- **Unbounded subdivision.** The stroker's depth limits allow about 2^78
  binary splits on numerics that never converge. It now has a budget of
  65,536 splits per curve.
- **Pixmap offsets.** `draw_pixmap` unwrapped an `i32` overflow on extreme
  offsets.

None of these fixes changes a pixel of the oracle corpus.

## Usage

```rust
use rusty_raster::{Color, FillRule, LineCap, Paint, PathBuilder, Pixmap, Stroke, Transform};

let mut pixmap = Pixmap::new(200, 200).unwrap();
pixmap.fill(Color::WHITE);

let mut paint = Paint::default();
paint.set_color_rgba8(30, 90, 200, 255);
paint.anti_alias = true;

let mut pb = PathBuilder::new();
pb.move_to(20.0, 180.0);
pb.cubic_to(60.0, 20.0, 140.0, 20.0, 180.0, 180.0);
let path = pb.finish().unwrap();

let stroke = Stroke { width: 8.0, line_cap: LineCap::Round, ..Stroke::default() };
pixmap.stroke_path(&path, &paint, &stroke, Transform::from_rotate_at(10.0, 100.0, 100.0), None);
let circle = PathBuilder::from_circle(100.0, 100.0, 30.0).unwrap();
pixmap.fill_path(&circle, &paint, FillRule::Winding, Transform::identity(), None);
let png: Vec<u8> = pixmap.encode_png().unwrap(); // `png-format` feature (default)
```

To render into memory you already own, `PixmapMut::from_bytes(&mut buf, w, h)`
takes any `&mut [u8]` of `w * h * 4` premultiplied RGBA bytes.

## Differences from tiny-skia's API

- `Pixmap::pixels()` returns an iterator of `PremultipliedColorU8` and
  `pixels_mut()` an iterator of `&mut [u8; 4]`, where tiny-skia returns
  reinterpreted typed slices. Doing that needs `unsafe` or `bytemuck`. `data()`
  and `data_mut()` are unchanged, and `set_pixel` is new.
- `FiniteF32::new_unchecked`, `NormalizedF32::new_unchecked` and
  `Color::from_rgba_unchecked` are safe functions that still enforce their
  invariant (by clamping, or by mapping non-finite values to zero).
- `Pixmap::new` and `Mask::new` return `None` when the allocator refuses the
  request, instead of aborting the process.
- tiny-skia-path's types live in `rusty_raster::path` and are re-exported at
  the root.

## Scope and what is not done

- Same feature set as tiny-skia 0.12: no text, no image filters, no
  GPU path. Output is 8-bit premultiplied RGBA only.
- Speed has not been measured against tiny-skia yet. The pipeline lanes are
  scalar arrays that LLVM auto-vectorises; tiny-skia uses SSE2/AVX/NEON
  intrinsics, so expect this crate to be slower until it is measured and
  tuned. No speed number is claimed.
