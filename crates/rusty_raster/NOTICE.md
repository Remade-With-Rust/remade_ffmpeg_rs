# NOTICE: attribution for `rusty_raster`

`rusty_raster` is a **hardened fork** of the pure-Rust
[`tiny-skia`](https://crates.io/crates/tiny-skia) and
[`tiny-skia-path`](https://crates.io/crates/tiny-skia-path) crates, merged into
one crate and carried forward here.

- **Upstream:** <https://github.com/linebender/tiny-skia>
- **Forked from:** `tiny-skia` **0.12.0** and `tiny-skia-path` **0.12.0**, plus
  the subset of `strict-num` 0.1.1 they use (`src/path/strict.rs`)
- **Upstream authors:** Yevhenii Reizner and the linebender contributors;
  tiny-skia is itself a port of a subset of [Skia](https://skia.org/)
  (Copyright 2011 Google Inc., 2006 The Android Open Source Project)
- **Upstream license:** BSD-3-Clause, reproduced verbatim in
  `LICENSE-BSD-3-Clause`. The per-file copyright headers are kept.

This is a reimplementation-by-derivation, not a clean-room rewrite: most of the
code descends directly from tiny-skia and stays under its BSD-3-Clause terms.
Changes made in this repository are Apache-2.0 (`LICENSE`), so the crate is
`Apache-2.0 AND BSD-3-Clause`. Both are permissive, with no copyleft. This fork
is **not** endorsed by or affiliated with the tiny-skia project or Google.

## What the fork changes

- `#![forbid(unsafe_code)]`. tiny-skia uses `unsafe` for SIMD intrinsics,
  `bytemuck` casts and `strict-num` constructors; none of it is left.
- No dependencies. `arrayref`, `arrayvec`, `bytemuck`, `cfg-if`, `log`,
  `strict-num`, `libm` and `png` are gone. PNG I/O goes through the house
  `rusty_png` behind the default `png-format` feature.
- The SIMD lane types (`src/wide/`) are scalar arrays that reproduce
  tiny-skia's **SSE2** lane semantics (the oracle on a default x86-64 build),
  including `maxps`/`minps` operand order and `cvtps2dq` overflow behaviour.
- Pixels are stored and addressed as RGBA bytes, so `pixels()` and
  `pixels_mut()` return iterators instead of reinterpreted typed slices.
- Hostile input is rejected or clamped where tiny-skia panics (see the README).
