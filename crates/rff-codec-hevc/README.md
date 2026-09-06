# rff-codec-hevc

[![Remade With Rust](https://img.shields.io/badge/Remade%20With-Rust-000?logo=rust&logoColor=fff)](https://github.com/remade-with-rust)
[![By Mata Network](https://img.shields.io/badge/by-Mata%20Network-5b2be0)](https://www.mata.network)
[![License: Apache-2.0](https://img.shields.io/badge/license-Apache--2.0-blue)](https://github.com/Remade-With-Rust/remade_ffmpeg_rs/blob/main/LICENSE)

The **HEVC / H.265** video decoder adapter for **remade_ffmpeg_rs**, backed by
[`rusty_h265`](https://crates.io/crates/rusty_h265) — pure Rust end to end.
Registers `hevc` for decode.

- **Pure Rust, no C/FFI, no build scripts**, and the decoder core is
  `#![forbid(unsafe_code)]`.
- **Bit-exact**: `rusty_h265` decodes all 147 streams of the JCT-VC HEVC_v1
  conformance suite bit-exactly, and every picture matches its
  decoded-picture-hash SEI.
- **Scope: HEVC version 1** — Main, Main 10 and Main Still Picture, 4:2:0,
  8 and 10 bit. That is what phones, cameras, broadcast and Blu-ray produce.
  Range extensions, screen content coding and the layered profiles are refused
  by name rather than decoded wrongly.
- **Decode only.** An HEVC encoder is a separate mission with a separate patent
  conversation.
- Packets are **Annex-B**; the MP4 (`hvc1`/`hev1`), Matroska
  (`V_MPEGH/ISO/HEVC`) and MPEG-TS (stream type `0x24`) demuxers convert their
  length-prefixed samples and `hvcC` records for you.
- **Patent note:** HEVC is patent-relevant, more so than H.264. No patent
  licence is granted or implied; royalties are the responsibility of whoever
  distributes or commercially deploys a product incorporating it. See the
  [patents section](https://github.com/Remade-With-Rust/remade_ffmpeg_rs#patents).

## Usage

```rust
use rff_codec::CodecRegistry;

let mut codecs = CodecRegistry::new();
rff_codec_hevc::register(&mut codecs);
```

Through the CLI, an HEVC file just works:

```sh
rff -i phone.mov -c:v h264 out.mp4
rffprobe clip.mkv
```

## License

Apache-2.0.