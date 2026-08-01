# rff-format-hls

[![Remade With Rust](https://img.shields.io/badge/Remade%20With-Rust-000?logo=rust&logoColor=fff)](https://github.com/remade-with-rust)
[![By Mata Network](https://img.shields.io/badge/by-Mata%20Network-5b2be0)](https://www.mata.network)
[![License: Apache-2.0](https://img.shields.io/badge/license-Apache--2.0-blue)](https://github.com/Remade-With-Rust/remade_ffmpeg_rs/blob/main/LICENSE)

**HLS output** for **remade_ffmpeg_rs** — an MPEG-TS segmenter plus `.m3u8`
playlist writer. Our own code, not a binding.

- **Not a `FormatRegistry` format.** Unlike the other `rff-format-*` crates it has no `register()` — HLS output is driven directly through `HlsSegmenter`, because segmenting needs to own the output *directory*, not a single byte sink.
- Writes numbered MPEG-TS segments (via [`rff-format-ts`](https://crates.io/crates/rff-format-ts)) plus the `.m3u8` media playlist that indexes them.
- **Segments roll over on keyframes** of a reference stream, so each one is independently decodable; `target_duration` is nominal seconds per segment.
- `-hls_time` and live playlist support are on the roadmap.
- Pure Rust, no C/FFI.

## Usage

```rust
use rff_core::{CodecId, Error};
use rff_format::{Muxer, Stream};
use rff_format_hls::HlsSegmenter;

fn main() -> Result<(), Error> {
    // Writes out.m3u8 plus out0.ts, out1.ts, ... next to it.
    let mut hls = HlsSegmenter::new("stream/out.m3u8".as_ref(), 6.0)?;

    let streams = vec![Stream::new(0, CodecId::H264)];
    hls.write_header(&streams)?;
    // hls.write_packet(&packet)? for each packet...
    hls.write_trailer()?;
    Ok(())
}
```

`HlsSegmenter` implements [`rff_format::Muxer`](https://crates.io/crates/rff-format),
so it drops into the same packet-writing loop as any other muxer — bring the
trait into scope to call `write_header` / `write_packet` / `write_trailer`.

## Part of Remade With Rust

This crate is one layer of
**[remade_ffmpeg_rs](https://github.com/Remade-With-Rust/remade_ffmpeg_rs)** — a
ground-up, permissively-licensed Rust rebuild of FFmpeg: a drop-in
`ffmpeg`/`ffprobe` CLI on pure-Rust codecs, with no copyleft. Most users want
the [`rff`](https://crates.io/crates/rff) engine facade or the
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
