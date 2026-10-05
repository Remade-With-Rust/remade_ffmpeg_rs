# rusty_aac

[![crates.io](https://img.shields.io/crates/v/rusty_aac.svg)](https://crates.io/crates/rusty_aac)
[![docs.rs](https://img.shields.io/docsrs/rusty_aac)](https://docs.rs/rusty_aac)
[![aac-hardening](https://github.com/Remade-With-Rust/remade_ffmpeg_rs/actions/workflows/aac-hardening.yml/badge.svg)](https://github.com/Remade-With-Rust/remade_ffmpeg_rs/actions/workflows/aac-hardening.yml)
[![License: Apache-2.0](https://img.shields.io/badge/license-Apache--2.0-blue)](https://github.com/Remade-With-Rust/remade_ffmpeg_rs/blob/main/LICENSE)
[![Remade With Rust](https://img.shields.io/badge/Remade%20With-Rust-000?logo=rust&logoColor=fff)](https://github.com/remade-with-rust)

**A complete MPEG-4 AAC decoder and an AAC-LC encoder, written entirely in Rust.**
No C, no FFI, no runtime dependencies — conformance-verified against the
ISO/IEC 14496-26 suite, fuzzed, and audited for production use.

## Highlights

- **Complete decoder.** Every member of the MPEG-4 AAC family short of USAC:
  AAC-LC, Main, LTP, HE-AAC v1 (SBR), HE-AAC v2 (Parametric Stereo), ER AAC-LC/LTP,
  AAC-LD and AAC-ELD, with 1024- and 960-sample frames, all thirteen sampling
  rates, mono to 7.1 and arbitrary PCE layouts.
- **Conformance-verified.** All 83 non-USAC streams of the ISO/IEC 14496-26
  conformance suite decode within 1 LSB of FFmpeg 8.1.2, and every stream with
  ISO reference PCM is checked against it in CI.
- **Fast.** Runtime-dispatched SIMD (AVX / AVX2 on x86-64, NEON on AArch64) with
  bit-identical scalar fallbacks; the encoder runs 3–8× faster than FFmpeg's
  native AAC encoder.
- **Hardened.** Fuzzed on every entry point, property-tested, clippy pedantic
  clean, every `unsafe` block documented and inventoried, `cargo vet` fully
  audited — see [Security](#security).
- **Zero dependencies.** The library has no runtime dependencies at all;
  `--no-default-features` builds a scalar version with no SIMD `unsafe`.
- **Production encoder.** Psychoacoustic AAC-LC encoder with block switching,
  M/S stereo and a correlation-routed joint stereo rate loop; mono to 5.1.

## Installation

```toml
[dependencies]
rusty_aac = "1.1"
```

## Quick start

### Decode an ADTS stream

```rust
use rusty_aac::{parse_adts, AacDecoder, Error};

fn decode(adts: &[u8]) -> Result<Vec<f32>, Error> {
    let mut decoder = AacDecoder::new(); // configured from the ADTS headers
    let mut pcm = Vec::new();            // interleaved f32 in [-1, 1]
    let mut pos = 0;
    while pos + 7 <= adts.len() {
        let header = parse_adts(&adts[pos..])?;
        let frame = decoder.decode(&adts[pos..pos + header.frame_length], None)?;
        pcm.extend_from_slice(&frame.samples);
        pos += header.frame_length;
    }
    Ok(pcm)
}
```

For MP4 / MOV / Matroska tracks, construct the decoder from the container's
AudioSpecificConfig and feed it raw access units. The raw-bytes form preserves
HE-AAC (SBR / PS) signalling:

```rust,ignore
let mut decoder = rusty_aac::AacDecoder::with_config_bytes(&esds_payload)?;
let frame = decoder.decode(&access_unit, Some(pts))?;
```

### Encode to AAC-LC

```rust
use rusty_aac::{write_adts_header, AacEncoder, AacEncoderConfig, AdtsHeader, Error};

fn encode(pcm: &[f32], channels: u16, sample_rate: u32) -> Result<Vec<u8>, Error> {
    let mut encoder = AacEncoder::new(AacEncoderConfig {
        bitrate_bps: 128_000,
        ..AacEncoderConfig::default()
    });
    encoder.push_pcm(pcm, channels, sample_rate)?; // interleaved f32 in [-1, 1]
    encoder.finish();

    // Each packet is a raw access unit; wrap it in ADTS for a playable .aac file,
    // or store it raw in MP4 with `rusty_aac::audio_specific_config_bytes(..)`
    // as the esds DecoderSpecificInfo.
    let mut adts = Vec::new();
    while let Ok(packet) = encoder.next_packet() {
        adts.extend(write_adts_header(&AdtsHeader {
            object_type: 2,
            sample_rate,
            channels,
            frame_length: 7 + packet.data.len(),
            header_len: 7,
        }));
        adts.extend(packet.data);
    }
    Ok(adts)
}
```

### LATM / LOAS (MPEG-TS, broadcast)

```rust,ignore
use rusty_aac::latm::LatmDecoder;

let mut decoder = LatmDecoder::new(); // tracks StreamMuxConfig and mid-stream changes
let (frame, consumed) = decoder.decode(&loas_bytes, None)?;
```

## Supported formats

| | Decode | Encode |
|---|---|---|
| **Object types** | AAC-LC, Main, LTP, HE-AAC v1 (SBR), HE-AAC v2 (PS), ER AAC-LC, ER AAC-LTP, ER AAC-LD, ER AAC-ELD | AAC-LC |
| **Frame lengths** | 1024, 960; 512 / 480 (LD, ELD) | 1024 |
| **Sample rates** | all 13 standard rates, 7.35–96 kHz, plus escaped rates | all 13 standard rates |
| **Channels** | `channel_configuration` 1–7 and 11–14, PCE layouts (with the height extension), coupling channels | mono to 5.1, ISO element order |
| **Transport** | ADTS, raw access units (MP4 `esds`), LATM / LOAS | raw access units, ADTS headers, `esds` config |
| **Not supported** | xHE-AAC (USAC, ISO/IEC 23003-3 — a separate codec; refused with a typed error); the low-delay SBR of AAC-ELD (the ELD core decodes, reported by `sbr_support()`); AAC-LTP with 960-sample frames | 7+ channels (needs an encoder-side PCE) |

Decoded output is interleaved `f32` in the standard channel order
(`FL, FR, FC, LFE, BL, BR, …`).

## Conformance

Every stream in the FFmpeg FATE copy of the ISO/IEC 14496-26 AAC conformance suite
(plus FATE's own AAC streams) is decoded and compared sample by sample with FFmpeg
8.1.2 (`-flags +bitexact`):

| Verdict | Streams | Meaning |
|---|---:|---|
| **Exact** | **81** | peak difference ≤ 1 LSB |
| Exact, FFmpeg deviates | 2 | FFmpeg ignores a CRC-valid PCE height extension (honoured here), and emits an uninitialised channel for a stereo configuration carrying one SCE (silence here) |
| USAC | 8 | xHE-AAC — a separate codec, refused cleanly |

By object type: AAC-LC 55, HE-AAC v1 6, HE-AAC v2 15, Main 2, LTP 1, ER AAC-LD 1,
ER AAC-ELD 3. CI additionally gates every stream that ships ISO reference PCM to
within 2 LSB of it (FFmpeg's own FATE tolerance), independently of any FFmpeg
decoder. The harness is [`examples/aacconf.rs`](examples/aacconf.rs).

## Performance

**Decoder.** Every kernel change is proven by instruction count and bit-identical
to the scalar reference. Instructions to decode the same streams, 1.0.0 → 1.1.0:

| Stream | Change |
|---|---:|
| AAC-LC | −24% |
| HE-AAC v1 (SBR) | −35% |
| HE-AAC v2 (SBR + PS) | −29% |

**Encoder** — throughput against FFmpeg's native AAC encoder on 24-second clips
(best of 5, interleaved runs; measured at 0.5.0, and later releases are faster):

| Clip | rusty_aac | FFmpeg native AAC | Speed-up |
|---|---:|---:|---:|
| Piano | 214× realtime | 57× realtime | 3.8× |
| Guitar | 306× realtime | 39× realtime | 7.9× |
| Vocal | 299× realtime | 93× realtime | 3.2× |

## Encoder quality

Quality is measured with PEAQ (ITU-R BS.1387, ODG; 0 is transparent) against
FFmpeg's native AAC encoder, both decoded by the same neutral decoder. Values are
the ODG difference, rusty_aac minus FFmpeg — positive means rusty_aac scores higher:

| Content | 64 kbps | 96 kbps | 128 kbps | 192 kbps |
|---|---:|---:|---:|---:|
| Piano (real recording) | −1.29 | −0.25 | **+0.28** | **+0.44** |
| Guitar (real recording) | −1.02 | −0.03 | **+0.65** | **+0.47** |
| Vocal (real recording) | −1.92 | −0.38 | **+0.39** | **+0.60** |
| Guitar, stereo | −0.31 | −0.46 | −0.15 | **+0.35** |
| Percussive | **+0.49** | −0.01 | −0.17 | −0.31 |

rusty_aac leads at 128 kbps and above on real music; FFmpeg's encoder remains
stronger at 64–96 kbps. The method, corpus and harness are documented in the
[repository](https://github.com/Remade-With-Rust/remade_ffmpeg_rs/blob/main/tools/quality/README.md).

**Known limitations:** constant-bitrate output only (no bit reservoir, no VBR
mode); the encoder buffers the whole stream until `finish`, so encode long live
input in segments.

## Feature flags

| Feature | Default | Effect |
|---|:---:|---|
| `simd` | ✓ | Runtime-detected SIMD kernels (AVX / AVX2 / SSE on x86-64, NEON on AArch64), bit-identical to the scalar paths |
| `simd-avx512` | | Adds an AVX-512 encoder quantize tier (requires Rust 1.89) |
| `profile` | | Stage timers and SIMD-coverage counters, for measurement |
| `lab` | | Encoder quality lab: corpus, NMR metric, bitrate-ladder runner, WAV I/O |

## Platform support

- **x86-64**: SSE2 baseline; AVX / AVX2 kernels selected at runtime.
- **AArch64**: NEON kernels, verified bit-identical to the scalar paths under
  emulation.
- **Any other target**: the scalar paths, which are the reference for every SIMD
  kernel.
- **MSRV**: Rust 1.85 (default features), verified in CI.

## Security

rusty_aac is built to decode untrusted media in-process:

- **Threat model**: [`docs/threat-model.md`](docs/threat-model.md).
- **Unsafe code**: confined to SIMD kernels and capacity fills, each with a
  documented bounds argument — [`UNSAFE.md`](UNSAFE.md).
- **Testing**: coverage-guided fuzzing of every entry point (with overflow checks
  and AddressSanitizer), property tests, every past crasher kept as a regression
  test, and the ISO conformance gate in CI.
- **Supply chain**: no runtime dependencies; `cargo vet`, `cargo audit` and
  `cargo deny` clean; a CycloneDX SBOM with every release.
- **Reporting**: please disclose vulnerabilities privately — see
  [`SECURITY.md`](https://github.com/Remade-With-Rust/remade_ffmpeg_rs/blob/main/SECURITY.md).

The full audit status is in the table at the end of this page.

## Patents

The encoder implements AAC-LC coding tools only. The decoder implements the wider
family, including SBR, Parametric Stereo and the low-delay object types, whose
patents are more recent than AAC-LC's. The code is independently written from
ISO/IEC 14496-3 and licensed under Apache-2.0, but **the copyright license grants
no patent rights**. If you distribute products commercially, consult IP counsel.
See the repository's
[patent notes](https://github.com/Remade-With-Rust/remade_ffmpeg_rs/blob/main/docs/compatibility.md#patents).

## Versioning

rusty_aac follows [Semantic Versioning](https://semver.org/). The public API is
stable from 1.0. Decoder output is stable and gated by the conformance suite;
encoder output may change in minor releases as quality improves. See
[`CHANGELOG.md`](CHANGELOG.md).

## Part of Remade With Rust

rusty_aac is the AAC engine of
**[remade_ffmpeg_rs](https://github.com/Remade-With-Rust/remade_ffmpeg_rs)**, a
permissively licensed Rust rebuild of FFmpeg, where it is exposed to the codec
registry by [`rff-codec-aac`](https://crates.io/crates/rff-codec-aac). Sibling
codec crates include [`rusty_mp3`](https://crates.io/crates/rusty_mp3),
[`rusty-opus`](https://crates.io/crates/rusty-opus),
[`rusty_flac`](https://crates.io/crates/rusty_flac),
[`rusty_vp9`](https://crates.io/crates/rusty_vp9) and
[`rusty_h265`](https://crates.io/crates/rusty_h265).

## About Mata Network

<!-- ORG BOILERPLATE — keep identical across repos -->

[Mata Network](https://www.mata.network) builds sovereign, self-hostable
infrastructure. **Remade With Rust** is our open-source home for the
permissively-licensed building blocks that work depends on.

<!-- /ORG BOILERPLATE -->

## License

Apache-2.0. See
[LICENSE](https://github.com/Remade-With-Rust/remade_ffmpeg_rs/blob/main/LICENSE).

---

<!-- HARDENING-TABLE:BEGIN generated by use-protection-please — edit docs/plans/use-protection-please.md, not this block -->
## Hardening status

**Tier** critical-path · **Audited** 2026-10-04 (deep) · **v1.0.0 gates** 12/15 · [Full checklist](https://github.com/Remade-With-Rust/remade_ffmpeg_rs/blob/main/crates/rusty_aac/docs/plans/use-protection-please.md)

`████████████████░░░░` **82%** &nbsp;·&nbsp; 28 Completed · 0 Scheduled · 6 Incomplete · 21 N/A

| Phase | ✅ Completed | 🗓 Scheduled | ⬜ Incomplete | · N/A |
|---|--:|--:|--:|--:|
| 0 — Threat modeling | 2 | 0 | 0 | 0 |
| 1 — Toolchain | 3 | 0 | 1 | 0 |
| 2 — Supply chain | 7 | 0 | 1 | 0 |
| 3 — Code level | 6 | 0 | 0 | 1 |
| 4 — Static analysis | 1 | 0 | 0 | 0 |
| 5 — Dynamic analysis | 3 | 0 | 0 | 0 |
| 6 — Fuzzing and properties | 3 | 0 | 1 | 0 |
| 7 — Formal verification | 0 | 0 | 1 | 0 |
| 8 — Build and binary | 0 | 0 | 0 | 2 |
| 9 — Runtime privilege | 0 | 0 | 0 | 1 |
| 10 — Cryptography | 0 | 0 | 0 | 3 |
| 11 — CI/CD, release, and operations | 3 | 0 | 2 | 0 |
| 12 — Compliance controls | 0 | 0 | 0 | 14 |
| **Total** | **28** | **0** | **6** | **21** |

**Architect** — Tim Almond
<!-- HARDENING-TABLE:END -->
