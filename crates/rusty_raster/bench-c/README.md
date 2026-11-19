# rusty_raster / rusty_svgtree against tiny-skia, Cairo, resvg and nanosvg

- `rbench raster ROUNDS` draws four 2048x2048 workloads with rusty_raster, tiny-skia 0.12 and Cairo, then compares the pixels and times all three.
- `rbench svg ROUNDS FILE...` renders each SVG with rusty_svgtree, resvg 0.48 and nanosvg, then compares and times them.

## Setup (WSL / Linux)
1. Put `nanosvg.h` and `nanosvgrast.h` (github.com/memononen/nanosvg, `src/`) in `third_party/`.
2. Cairo comes from the distribution, with headers (`pkg-config cairo`).
3. Build inside the Linux filesystem: `cargo build --release`.
4. `rusty_svgtree` needs `rusty_raster`, `rusty_png` and `rusty_jpeg` from `[workspace.dependencies]`. Building outside the remade_ffmpeg_rs workspace needs a small wrapper workspace that declares them.

## Method
- One binary.
- Pinned with `taskset -c 6`; 15 rounds (raster) or 11 rounds (SVG), with the arm order reversed every round.
- In-process `Instant`, minimum of N, with a paired win rate and z. A rusty_raster / rusty_svgtree null arm came in within 0.98–1.02.
- The C shim is built at `-O3` with no `-march=native`. The Rust arms run under rusty_alloc.
- Pixels are compared premultiplied for raster. For SVG they are compared unpremultiplied, because nanosvg writes straight alpha.
- The SVG timing set is limited to files rusty_svgtree and resvg render pixel-identically, so both do the same work.
