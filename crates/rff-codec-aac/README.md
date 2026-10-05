# rff-codec-aac

[![crates.io](https://img.shields.io/crates/v/rff-codec-aac.svg)](https://crates.io/crates/rff-codec-aac)
[![docs.rs](https://img.shields.io/docsrs/rff-codec-aac)](https://docs.rs/rff-codec-aac)
[![aac-hardening](https://github.com/Remade-With-Rust/remade_ffmpeg_rs/actions/workflows/aac-hardening.yml/badge.svg)](https://github.com/Remade-With-Rust/remade_ffmpeg_rs/actions/workflows/aac-hardening.yml)
[![License: Apache-2.0](https://img.shields.io/badge/license-Apache--2.0-blue)](https://github.com/Remade-With-Rust/remade_ffmpeg_rs/blob/main/LICENSE)
[![Remade With Rust](https://img.shields.io/badge/Remade%20With-Rust-000?logo=rust&logoColor=fff)](https://github.com/remade-with-rust)

**The AAC codec for [remade_ffmpeg_rs](https://github.com/Remade-With-Rust/remade_ffmpeg_rs).**
This crate registers `aac` — decode and encode — with the remade_ffmpeg_rs codec
registry, backed by the pure-Rust [`rusty_aac`](https://crates.io/crates/rusty_aac)
engine. No C, no FFI, no `unsafe` code of its own.

## Capabilities

- **Decode** the MPEG-4 AAC family short of USAC: AAC-LC, Main, LTP, HE-AAC v1
  (SBR), HE-AAC v2 (Parametric Stereo), ER AAC-LC/LTP, AAC-LD and AAC-ELD; mono to
  7.1 and PCE layouts. Configured from the container's AudioSpecificConfig (so
  HE-AAC signalling is preserved) or from ADTS headers. Output: interleaved `f32`.
- **Encode** AAC-LC, mono to 5.1, from interleaved `s16` / `f32` or planar `f32`
  frames; the `b` option sets the bitrate. Packets are raw access units, with
  `rusty_aac::audio_specific_config_bytes` providing the MP4 `esds` configuration.
- **Conformance and performance** are those of `rusty_aac`: all 83 non-USAC
  ISO/IEC 14496-26 conformance streams decode within 1 LSB of FFmpeg, and the
  encoder runs several times faster than FFmpeg's native AAC encoder — see the
  [rusty_aac documentation](https://crates.io/crates/rusty_aac).

## Usage

```rust
use rff_codec::CodecRegistry;
use rff_core::{CodecId, Error};

fn main() -> Result<(), Error> {
    let mut codecs = CodecRegistry::new();
    rff_codec_aac::register(&mut codecs);

    // Reachable by id or by its FFmpeg-style name.
    let _decoder = codecs.find_decoder(CodecId::Aac)?;
    let _encoder = codecs.find_encoder(CodecId::Aac)?;
    let codec = codecs.by_name("aac").expect("registered");
    println!("{}: decode {}, encode {}", codec.long_name, codec.can_decode(), codec.can_encode());
    Ok(())
}
```

Most applications use the [`remade-ffmpeg`](https://crates.io/crates/remade-ffmpeg)
engine facade or the [`rff-cli`](https://crates.io/crates/rff-cli) binaries, which
register this codec automatically.

## Feature flags

| Feature | Default | Effect |
|---|:---:|---|
| `simd` | ✓ | Enables rusty_aac's runtime-detected SIMD kernels |
| `simd-avx512` | | Adds rusty_aac's AVX-512 encoder tier (requires Rust 1.89) |

## Security

The adapter treats every frame and packet as untrusted: a frame's planes, not its
declared sample count, bound what is encoded, and malformed frames are errors,
never panics. It is fuzzed, property-tested, and differentially tested against
`rusty_aac`; see the [threat model](docs/threat-model.md) and the audit status
at the end of this page. Report vulnerabilities privately via
[`SECURITY.md`](https://github.com/Remade-With-Rust/remade_ffmpeg_rs/blob/main/SECURITY.md).

## Patents

AAC is patent-relevant. The encoder implements AAC-LC only; the decoder also
implements SBR and Parametric Stereo (HE-AAC), whose patents are more recent. The
copyright license grants no patent rights; see the repository's
[patent notes](https://github.com/Remade-With-Rust/remade_ffmpeg_rs/blob/main/docs/compatibility.md#patents).

## Versioning

Semantic Versioning; see [`CHANGELOG.md`](CHANGELOG.md).

## Part of Remade With Rust

**[remade_ffmpeg_rs](https://github.com/Remade-With-Rust/remade_ffmpeg_rs)** is a
permissively licensed Rust rebuild of FFmpeg — a drop-in `ffmpeg` / `ffprobe` on
pure-Rust codecs. More projects at
**[github.com/remade-with-rust](https://github.com/remade-with-rust)**.

## About Mata Network

<!-- ORG BOILERPLATE — keep identical across repos -->

[Mata Network](https://www.mata.network) builds sovereign, self-hostable
infrastructure. **Remade With Rust** is our open-source home for the
permissively-licensed building blocks that work depends on.

<!-- /ORG BOILERPLATE -->

## License

Apache-2.0. See the workspace
[LICENSE](https://github.com/Remade-With-Rust/remade_ffmpeg_rs/blob/main/LICENSE).
