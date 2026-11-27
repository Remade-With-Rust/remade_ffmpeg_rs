# rff-codec-exr

[![Remade With Rust](https://img.shields.io/badge/Remade%20With-Rust-000?logo=rust&logoColor=fff)](https://github.com/remade-with-rust)
[![By Mata Network](https://img.shields.io/badge/by-Mata%20Network-5b2be0)](https://www.mata.network)
[![License: Apache-2.0](https://img.shields.io/badge/license-Apache--2.0-blue)](https://github.com/Remade-With-Rust/remade_ffmpeg_rs/blob/main/LICENSE)

The **OpenEXR** still-image codec adapter for **remade_ffmpeg_rs**, backed by
the pure-Rust [`rusty_exr`](https://crates.io/crates/rusty_exr) crate.

- **Decode** every EXR compression (None, RLE, ZIPS, ZIP, PIZ, PXR24, B44,
  B44A, DWAA, DWAB), scanline or tiled, to 8-bit RGB/RGBA over the display
  window (linear values clamped to [0, 1], as FFmpeg does by default).
- **Encode** RGB/RGBA as half (default) or float channels:
  `-exr_compression none|rle|zip1|zip16|piz|pxr24|b44|b44a|dwaa|dwab`, `-exr_format half|float`.

```sh
rff -i render.exr out.png
rff -i photo.png -exr_compression piz out.exr
```

For full float data, multi-part files and every channel, use `rusty_exr`
directly.
