# rusty_image against the C references

`imgbench corpus FILE...` decodes each file with rusty_image, stb_image,
ImageMagick 7 and gdk-pixbuf and compares the pixels. `imgbench decode N
FILE...` / `imgbench encode N FILE...` time every engine in one process.

## Setup (WSL / Linux)
1. Put `stb_image.h`, `stb_image_write.h` (github.com/nothings/stb) and `qoi.h` (github.com/phoboslab/qoi) in `third_party/`.
2. Patch one line of stb_image v2.33. Its BMP size check compares the unsigned `s->img_y < -STBI_MAX_DIMENSIONS`, which is always true, so every BMP is refused. Make it `(int)s->img_y`.
3. ImageMagick 7 (`libMagickWand-7.Q16.so.10`; the prototypes are declared in `shim.c`) and gdk-pixbuf (with headers) come from the distribution.
4. Build inside the Linux filesystem (cargo over a 9p mount hangs): `cargo build --release`.

## Method
- One binary; every engine sees the same bytes.
- Pinned with `taskset -c 6`; 21 rounds for decode, 15 for encode, with the arm order reversed every round.
- In-process `Instant`, minimum of N. Each table cell is Mpx/s at that minimum.
- Paired win rate with z for every row, and a rusty_image null arm, which came in within 0.97–1.05.
- ImageMagick is limited to one thread. The C shim is built at `-O3` without `-march=native`, matching the Rust arms and the distro libraries.
- The Rust arms run under rusty_alloc.
- Content: a real 3840x2160 photograph (8.3 Mpx), written into each format by ImageMagick.
