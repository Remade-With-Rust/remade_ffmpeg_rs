# rff-format-jpeg

[![Remade With Rust](https://img.shields.io/badge/Remade%20With-Rust-000?logo=rust&logoColor=fff)](https://github.com/remade-with-rust)
[![By Mata Network](https://img.shields.io/badge/by-Mata%20Network-5b2be0)](https://www.mata.network)
[![License: Apache-2.0](https://img.shields.io/badge/license-Apache--2.0-blue)](https://github.com/Remade-With-Rust/remade_ffmpeg_rs/blob/main/LICENSE)

The **JPEG** single-image container for **remade_ffmpeg_rs** — for still images
the file *is* the codec stream, and this crate models that.

- **Demux + mux** — a one-stream, one-packet container wrapping the whole file.
- **JPEG Multi-Picture Format (MPF)** — demux and mux, our equivalent of
  FFmpeg 9.1's `jpeg_mpf` (see below).
- Pairs with [`rff-codec-jpeg`](https://crates.io/crates/rff-codec-jpeg).
- Pure Rust, no C/FFI, `#![forbid(unsafe_code)]`.

## JPEG Multi-Picture Format

MPF (CIPA DC-007) stores several JPEG images in one file: an `APP2` `MPF\0`
index in the first image lists the others, appended after its `EOI`. It is
what HDR gain-map photos (Ultra HDR / ISO 21496-1: an SDR image plus a
gain-map JPEG), stereo `.mpo` pairs and camera previews are made of.

- **Demux** — one stream per image, in index order, each one packet holding
  that image's bytes exactly (FFmpeg 9.1's stream layout). Stream metadata
  names the image: `mpf_type` (`primary`, `gain_map`, `disparity`,
  `multi_angle`, `panorama`, `large_thumbnail_vga`, ..., `undefined`),
  `mpf_type_code`, `mpf_entry`, `mpf_flags`, `mpf_source`. Big- and
  little-endian indexes are read; every offset and size is bounds-checked.
- **Mux** — `-f jpeg_mpf` (or a `.mpo` name) writes N JPEG streams as one MPF
  file, laid out byte for byte as FFmpeg 9.1's muxer writes it. Stream 0 is
  the primary image; an image carrying ISO 21496-1 metadata becomes its gain
  map (with the Ultra HDR segments FFmpeg adds to the primary); an `mpf_type`
  stream tag picks any other type, so an MPF remux keeps its types.
- **Without an index** — when a JPEG has no usable MP Index but whole JPEGs
  follow its `EOI`, each becomes a stream of its own (`mpf_source=trailing`).
  FFmpeg reads such a file as one image; anything after the `EOI` that is not
  a JPEG (a motion photo's MP4) still stays part of the one image.

```sh
rff -i photo.jpg -map 0:1 gainmap.jpg      # extract the second image (stream copy)
rff -i left.jpg -i right.jpg -map 0 -map 1 -f jpeg_mpf pair.jpg
rffprobe photo.jpg                          # streams + their mpf_type
```

`rff` stream-copies the extracted image (byte-identical); FFmpeg re-encodes it
unless given `-c copy`. The index parser and writer are the `mpf` module: no
I/O, `core` + `alloc` only. Reading or applying the gain-map metadata itself
is out of scope.

## Usage

```rust
use rff_format::FormatRegistry;
use rff_core::Error;

fn main() -> Result<(), Error> {
    let mut formats = FormatRegistry::new();
    rff_format_jpeg::register(&mut formats);

    let file = std::fs::File::open("input.jpeg").expect("open input");
    let mut demuxer = formats.open_demuxer("jpeg", Box::new(file))?;
    for stream in demuxer.read_header()? {
        println!("stream {}: {:?}", stream.index, stream.codec_id);
    }
    Ok(())
}
```

## Part of Remade With Rust

This crate is one layer of
**[remade_ffmpeg_rs](https://github.com/Remade-With-Rust/remade_ffmpeg_rs)** — a
ground-up, permissively-licensed Rust rebuild of FFmpeg: a drop-in
`ffmpeg`/`ffprobe` CLI on pure-Rust codecs, with no copyleft. Most users want
the [`remade-ffmpeg`](https://crates.io/crates/remade-ffmpeg) engine facade or the
[`rff-cli`](https://crates.io/crates/rff-cli) binaries rather than this crate
directly.

Also check out our sister project
**[FFAI](https://github.com/Remade-With-Rust/FFAI)** — media for an AI-first
world — and the rest of
**[github.com/remade-with-rust](https://github.com/remade-with-rust)**, including
the standalone codec crates
[`rusty_h264`](https://crates.io/crates/rusty_h264),
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

## License

Apache-2.0. See the workspace
[LICENSE](https://github.com/Remade-With-Rust/remade_ffmpeg_rs/blob/main/LICENSE).
