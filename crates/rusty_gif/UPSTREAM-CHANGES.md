# Changes relative to upstream

Baseline: `gif` 0.14.2 + `color_quant` 1.1.0 (crates.io tarballs).

## 0.1.0 — vendoring (no functional change)

- Renamed package `gif` → `rusty_gif`; lib name follows.
- Vendored `color_quant` as `pub mod neuquant` (`src/neuquant.rs`):
  - `mod math` / `use crate::math::clamp` replaced by an inline `clamp`
    (upstream `math.rs` was that one function).
  - Doc example updated to the `rusty_gif::neuquant` path.
  - License headers retained verbatim.
- `common.rs`: `color_quant::NeuQuant` → `crate::neuquant::NeuQuant`.
- Removed the `insert_as_doc!(include_str!("../README.md"))` block (the
  README doctest referenced the old crate name); README is ours now.
- Dropped upstream criterion benches from the tarball copy (they carried
  `criterion`/`glob`/`png`/`rayon` dev-deps; this repo benches through its
  own harnesses under `rusty_alloc` instead).
- `color_quant` cargo feature kept (API-compatible) but now gates only the
  vendored module — no external dependency behind it.

Gate for this release: `rff` CLI GIF encode output is byte-identical to the
pre-fork build (8-image corpus, SHA-256).

## 0.1.0 — content-routed quantization (output-changing, corpus-gated)

`Frame::from_rgba_speed` now routes by a color census instead of always
training NeuQuant on every pixel (`src/quantize.rs`):

- **Exact** (≤256 unique colors): unchanged upstream behavior, byte-identical.
- **Histogram** (≤65,536 unique): median-cut + frequency-weighted k-means over
  the unique colors (`src/kmeans.rs`), iteration count tiered by diversity
  (3 / 2 / 1 at ≤32k / ≤48k / ≤64k unique). New quantizer; deterministic
  integer/f64 math, loud failure modes (asserts, no silent fallbacks).
- **PixelStream** (>65,536 unique): NeuQuant with
  `samplefac = ceil(pixels / 125_000)` — constant training budget instead of
  cost ∝ resolution.

Supporting changes:

- `neuquant.rs` `contest()`: argmin scan split from the freq/bias decay loop
  (no cross-index dependency → bit-exact; decay loop now auto-vectorizes).
- Palette mapping memoized per unique color (one `index_of` per color, not
  per pixel) — bit-exact.
- Observability: `RUSTY_GIF_TRACE=1` prints route + census per frame;
  `RUSTY_GIF_Q_POLICY` (`auto`|`upstream`|`kmeans:<i>`|`unique:<cap>`|
  `budget:<px>`) overrides routing for A/B work; unknown values panic.
  The `upstream` policy reproduces pre-fork output byte-identically and is
  the standing oracle.

Gates (8-frame real-content corpus, CIF→1080p, vs FFmpeg 8.1.2
`palettegen/paletteuse dither=none` floors): PSNR above the floor on 8/8
(+0.20 to +4.07 dB), routing arms all validated live, encode CPU vs the
`palettegen` arm measured with the pinned ABBA harness (see repo bench
notes).
