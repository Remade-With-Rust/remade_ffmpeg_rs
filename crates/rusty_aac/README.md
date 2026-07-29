# rusty_aac

[![Remade With Rust](https://img.shields.io/badge/Remade%20With-Rust-000?logo=rust&logoColor=fff)](https://github.com/remade-with-rust)
[![By Mata Network](https://img.shields.io/badge/by-Mata%20Network-5b2be0)](https://www.mata.network)
[![License: Apache-2.0](https://img.shields.io/badge/license-Apache--2.0-blue)](https://github.com/Remade-With-Rust/remade_ffmpeg_rs/blob/main/LICENSE)

A pure-Rust **AAC-LC decoder and encoder**. Zero dependencies, no C, no FFI,
Apache-2.0.

- **Decoder** — complete AAC-LC: long/short/transition windows with grouped
  scalefactors, all spectral Huffman codebooks, M/S and intensity stereo, PNS,
  TNS. Verified against FFmpeg: bit-exact on deterministic features.
- **Encoder** — to our knowledge the **first pure-Rust AAC encoder on
  crates.io** (the alternatives, `fdk-aac` and libxaac bindings, are C FFI).
  Psychoacoustic Bark-scale masking model, transient-driven block switching,
  per-band M/S stereo, and a two-phase bitrate rate loop. **~450× realtime /
  ~6× faster than FFmpeg's AAC encoder** on a 24-core machine via
  frame-parallel encoding (~1.15× single-thread), with an N/4-FFT MDCT and
  AVX2 (opt-in AVX-512) quantize kernels. FFmpeg decodes its output at unity
  gain.
- `--no-default-features` turns off the runtime-detected SIMD kernels and gives
  a **100%-safe scalar build** (`#![forbid(unsafe)]`-grade: every `unsafe`
  block in the crate is feature-gated SIMD).

## Patents

AAC-LC is the oldest, largely-expired, lowest-risk corner of the AAC family —
this crate implements **only** AAC-LC (no HE-AAC/SBR/PS, no xHE-AAC, which
carry much younger patents). The code is independently written and
Apache-2.0-licensed, but **no patent license is granted or implied** by the
copyright license. If you distribute products commercially, consult IP counsel
about your own position; see the main repo's
[patent notes](https://github.com/Remade-With-Rust/remade_ffmpeg_rs/blob/main/docs/compatibility.md#patents).
This is engineering context, not legal advice.

## Decode

```rust
use rusty_aac::{AacDecoder, Error};

fn main() -> Result<(), Error> {
    let adts = std::fs::read("input.aac").map_err(|e| Error::invalid(e.to_string()))?;

    // The decoder reads its configuration from the ADTS headers. For bare MP4
    // access units, build it from the esds config instead:
    //   AacDecoder::with_config(rusty_aac::parse_audio_specific_config(&esds_payload)?)
    let mut dec = AacDecoder::new();

    let mut pcm: Vec<f32> = Vec::new(); // interleaved, [-1, 1]
    let mut pos = 0;
    while pos + 7 <= adts.len() {
        let hdr = rusty_aac::parse_adts(&adts[pos..])?;
        let frame = dec.decode(&adts[pos..pos + hdr.frame_length], None)?;
        println!("{} Hz, {} ch, {} samples", frame.sample_rate, frame.channels, frame.frames());
        pcm.extend_from_slice(&frame.samples);
        pos += hdr.frame_length;
    }
    Ok(())
}
```

## Encode

```rust
use rusty_aac::{AacEncoder, AacEncoderConfig, AdtsHeader, Error};

fn main() -> Result<(), Error> {
    // 1 s of a 440 Hz sine, mono 44.1 kHz, interleaved f32 in [-1, 1].
    let sr = 44_100u32;
    let pcm: Vec<f32> = (0..sr)
        .map(|i| (i as f32 / sr as f32 * 440.0 * std::f32::consts::TAU).sin() * 0.5)
        .collect();

    let mut enc = AacEncoder::new(AacEncoderConfig { bitrate_bps: 128_000 });
    enc.push_pcm(&pcm, 1, sr)?;
    enc.finish(); // encodes everything, frame-parallel

    // Packets are raw access units; wrap each in ADTS for a playable .aac file
    // (or hand `rusty_aac::audio_specific_config_bytes(sr, 1)` to an MP4 muxer
    // as the esds DecoderSpecificInfo and store the packets raw).
    let mut out = Vec::new();
    while let Ok(p) = enc.next_packet() {
        out.extend_from_slice(&rusty_aac::write_adts_header(&AdtsHeader {
            object_type: 2, // AAC-LC
            sample_rate: sr,
            channels: 1,
            frame_length: 7 + p.data.len(),
            header_len: 7,
        }));
        out.extend_from_slice(&p.data);
    }
    std::fs::write("tone.aac", &out).map_err(|e| Error::invalid(e.to_string()))?;
    Ok(())
}
```

## Features

| feature | default | effect |
| --- | --- | --- |
| `simd` | yes | runtime-detected AVX2 quantize/`x^0.75` kernels (bit-exact vs scalar) |
| `simd-avx512` | no | adds an 8-lane AVX-512 tier (needs Rust ≥ 1.89; falls back at runtime) |

## Part of Remade With Rust

`rusty_aac` is the standalone AAC engine of
**[remade_ffmpeg_rs](https://github.com/Remade-With-Rust/remade_ffmpeg_rs)** — a
ground-up, permissively-licensed Rust rebuild of FFmpeg. Sister project:
**[FFAI](https://github.com/Remade-With-Rust/FFAI)** — media for an AI-first
world. More at **[github.com/remade-with-rust](https://github.com/remade-with-rust)**.

Sibling codec crates: [`rusty_h264`](https://crates.io/crates/rusty_h264) (on
crates.io), `rusty_vp9`, `rusty_mp3`, `rusty-opus`, and the rusty-av1-toolkit
forks.

## License

Apache-2.0. See
[LICENSE](https://github.com/Remade-With-Rust/remade_ffmpeg_rs/blob/main/LICENSE).
