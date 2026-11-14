# rff-format-mp4

[![Remade With Rust](https://img.shields.io/badge/Remade%20With-Rust-000?logo=rust&logoColor=fff)](https://github.com/remade-with-rust)
[![By Mata Network](https://img.shields.io/badge/by-Mata%20Network-5b2be0)](https://www.mata.network)
[![License: Apache-2.0](https://img.shields.io/badge/license-Apache--2.0-blue)](https://github.com/Remade-With-Rust/remade_ffmpeg_rs/blob/main/LICENSE)

The **MP4 / QuickTime MOV (ISOBMFF)** container demuxer and muxers for
**remade_ffmpeg_rs** — our own code, not a binding.

- **Two formats, one reader.** `mp4` writes ISO files (`isom`), `mov` writes
  QuickTime (`qt  `); the demuxer reads both, progressive or **fragmented**
  (`moof`/`traf`/`trun`/`tfdt`/`tfhd` with `mvex`/`trex` defaults).
- **Video**: H.264 (`avc1`/`avcC`), HEVC (demux), AV1 (`av01`/`av1C`),
  VP9 (`vp09`/`vpcC`), APV (`apv1`/`apvC`), and in MOV **ProRes**
  (`apco` `apcs` `apcn` `apch` `ap4h` `ap4x`, the profile kept as the
  stream's `codec_tag`) and **DNxHD/DNxHR** (`AVdn`/`AVdh`).
- **Audio**: AAC (`esds`), Opus (`dOps`), FLAC (`fLaC`/`dfLa`), ALAC,
  AC-3 / E-AC-3 (`dac3`/`dec3`, derived from the first frame when the source
  has none), and **PCM**: QuickTime `sowt` `twos` `in24` `in32` `fl32` `fl64`
  `lpcm` `raw ` with sound description v0/v1/v2 and `enda` (read and written
  by `mov`), and ISO `ipcm`/`fpcm` (read). PCM comes out little-endian (one
  packet per chunk) and goes back in the source's own entry on a remux. `mp4`
  refuses PCM, as most MP4 players cannot play it.
- **Descriptive boxes**, read and written: `colr` (`nclc`/`nclx`), `pasp`,
  `clap`, `mdcv`, `clli`, and the QuickTime **timecode track** (`tmcd` +
  `tref`), surfaced as the video stream's start `timecode`.
- Pure Rust, no C/FFI.

## Usage

```rust
use rff_format::FormatRegistry;
use rff_core::Error;

fn main() -> Result<(), Error> {
    let mut formats = FormatRegistry::new();
    rff_format_mp4::register(&mut formats);

    let file = std::fs::File::open("input.mp4").expect("open input");
    let mut demuxer = formats.open_demuxer("mp4", Box::new(file))?;
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
