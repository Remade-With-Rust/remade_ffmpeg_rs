# rusty_webp — attribution

`rusty_webp` is a fork of [image-rs/image-webp](https://github.com/image-rs/image-webp)
v0.2.4 ("WebP encoding and decoding in pure Rust", MIT OR Apache-2.0), carried
forward in-tree by the remade_ffmpeg_rs project. The upstream `LICENSE-MIT` and
`LICENSE-APACHE` files are preserved alongside this notice.

Fork motivation (2026-08-12 comparison vs FFmpeg 8.1.2 / libwebp):

- lossless encode was 16–23× faster than libwebp but produced 35–53% larger
  files on photographic content (+656% on repeated-content screen material) —
  the encoder did no backward-reference search, no color cache, no palette,
  and used one fixed predictor;
- lossy (VP8) decode used simple chroma upsampling (up to −11 dB RGB-PSNR vs
  libwebp's fancy bilinear on saturated content) and had no YUV output path;
- lossy (VP8) encoding is absent entirely.

Changes on top of upstream are documented in the git history of this crate.
