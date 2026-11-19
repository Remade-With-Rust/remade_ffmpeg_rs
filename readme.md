### In The Wild with 572,498 Active Installs (6 hour Issue response, 72 hour Issue closure)

<a href="https://mata.network">MATA Disco</a> is a sovereign distributed cloud and digital freedom toolkit enabling ownership and accessibility of data. remade_ffmpeg_rs powers all media across the Disco Party.

Build the new internet with our <a href="https://drive.google.com/drive/u/0/folders/1VjvP1zSJWH1DmkTqL1qEPt3OM8Ub8jPX">AI skills for Rust</a>.

FREE RAG Converter Online -- <a href="https://RAGconverter.com">RAGconverter.com</a>

# remade_ffmpeg_rs

[![Remade With Rust](https://img.shields.io/badge/Remade%20With-Rust-000?logo=rust&logoColor=fff)](https://github.com/remade-with-rust)
[![By Mata Network](https://img.shields.io/badge/by-Mata%20Network-5b2be0)](https://www.mata.network)
[![License: Apache-2.0](https://img.shields.io/badge/license-Apache--2.0-blue)](LICENSE)
![Platforms: Windows · macOS · Linux · Web](https://img.shields.io/badge/platforms-Windows%20%C2%B7%20macOS%20%C2%B7%20Linux%20%C2%B7%20Web-informational)

**A memory-safe, ground-up Rust rebuild of [FFmpeg](https://github.com/FFmpeg/FFmpeg).**
- **What it does:** decode, encode, transcode, mux and probe audio, video and images.
- **How you use it:** the same `ffmpeg` / `ffprobe` commands and flags, or the same engine embedded as a library.
- **License:** Apache-2.0, with no copyleft anywhere in the tree.
- **Provenance:** a reimplementation, not a fork. Only FFmpeg's file formats and command line are matched; none of its code is used.

Sister project: [FFAI](https://github.com/remade-with-rust/ffai), media for an AI-first world.

> **Status:** pre-1.0 and not yet independently audited. APIs and codec coverage
> are still moving; nothing ships that is not byte-exact against its oracle.
> See [SECURITY.md](SECURITY.md), the [compatibility & patent matrix](docs/compatibility.md)
> and [CONTRIBUTING.md](CONTRIBUTING.md).

---

## Built for files that fight back

Media decoders are where memory-safety bugs live: they parse untrusted bytes at high speed. Our codecs are built to the opposite standard:
- each one is checked against an external oracle;
- each one is fed damaged and hostile input;
- each speed-up is gated on identical output.

| Gate | Evidence |
|---|---|
| **Conformance** | **VP9:** all 315 libvpx vectors bit-exact, checked in CI. **HEVC:** 147/147 JCT-VC streams bit-exact. **AV2:** a 47-clip corpus byte-identical to AOM's `avmdec`. **H.264:** 35/35 openh264 conformance streams bit-exact against `h264dec`, and our encoder's output decodes bit-exactly in FFmpeg at QP 0–51. **AAC:** all 83 non-USAC ISO/IEC 14496-26 streams within 1 LSB of FFmpeg, re-checked in CI against FATE. **MP3:** bit-exact against FFmpeg; Layer I/II within 1 LSB of the ISO vectors. |
| **Hostile input** | **VP9:** 50,000 malformed streams per CI run, none may panic. **JPEG:** 117,120 mutated decodes, 0 panics. **PNG:** 175/175 pngsuite files plus 2,100 mutated inputs, 0 panics. **AAC and MP3:** cargo-fuzz corpus replay on every change, plus nightly fuzz sessions. |
| **Damaged files** | **Our sweep:** 824 damaged audio files (truncated, bit-flipped, zeroed blocks), decoded side by side with symphonia. We recover at least as much audio on every one, and more on 184. **Independent check:** a paired run by ArtCraft over 434 files, 235 of them damaged ([#125](https://github.com/Remade-With-Rust/remade_ffmpeg_rs/issues/125)). Its one shortfall is fixed in 0.6.3. |
| **Memory safety** | Safe Rust on the core path. The H.264 and HEVC codec cores are `#![forbid(unsafe_code)]`. SIMD is isolated behind runtime dispatch, and each kernel keeps a scalar twin as its test oracle. Miri runs on the AAC and MP3 libraries. |
| **Speed without drift** | Each optimization is gated against the code it replaces: identical output on exact paths, a bounded error plus end-to-end SNR on float paths. Example: ALAC decode got about 2× faster in 0.6.3, with byte-identical output on every fixture. |
| **Supply chain** | `cargo-deny` (no copyleft), `cargo-audit`, `cargo-vet` and a CycloneDX SBOM per crate closure, with MSRV 1.85. |

When a downstream team finds a fault, the fix ships with a regression test and measured before/after numbers. [#125](https://github.com/Remade-With-Rust/remade_ffmpeg_rs/issues/125) and [#129](https://github.com/Remade-With-Rust/remade_ffmpeg_rs/issues/129) show the loop.

## Speed against FFmpeg

Each comparison runs on one pinned core with paired, interleaved runs and the same work in both arms. [docs/benchmarks.md](docs/benchmarks.md) has the methods.

| | vs FFmpeg (C) |
|---|---|
| **AAC encode** | **3.2–7.9× faster**, frame-parallel (FFmpeg's encoder is single-threaded) |
| **Vorbis encode** | **~5.3× faster**, frame-parallel; the first permissive-Rust Vorbis encoder |
| **PNG decode** | **2.55–2.89× faster** |
| **Opus encode** (vs `libopus`) | **1.50×** on CELT speech and **1.60×** on stereo music, per core, at quality parity (−0.015 BD-ODG over 18 classes) |
| **HEVC decode** | **1.37×** on weighted prediction; parity on deblocking-heavy and 10-bit streams; **0.80–0.86×** on mainstream 8-bit inter |
| **MP3 decode** | **1.24×** on 1 core and **1.39×** on 2, at lower total CPU |
| **VP9** | younger, still being optimized: decode about 0.2×, encode about 0.3× at about +36% bitrate (both measured on 0.1) |

## Codecs and formats

Every backend is pure Rust with no C/C++ FFI, and every container (de)muxer is our own code. [docs/codecs.md](docs/codecs.md) has the full per-codec detail and measurements.

| Codec | Decode | Encode | Backend |
|---|:---:|:---:|---|
| H.264 | ✅ | ✅ | [`rusty_h264`](https://crates.io/crates/rusty_h264) |
| HEVC | ✅ | | [`rusty_h265`](https://crates.io/crates/rusty_h265) |
| VP9 | ✅ | ✅ | [`rusty_vp9`](https://crates.io/crates/rusty_vp9) |
| AV1 / AVIF | ✅ | ✅ | [`rusty_av1d` / `rusty_av1e`](https://github.com/Remade-With-Rust/rusty-av1-toolkit) |
| AV2 (research preview) | ✅ | | [`rusty_av2d`](https://crates.io/crates/rusty_av2d) |
| AAC (LC, Main, LTP, HE v1/v2, LD/ELD) | ✅ | ✅ LC | [`rusty_aac`](https://crates.io/crates/rusty_aac) |
| MP3 / MP2 / MP1 | ✅ | ✅ MP3 | [`rusty_mp3`](https://crates.io/crates/rusty_mp3) |
| Opus | ✅ | ✅ | [`rusty-opus`](https://crates.io/crates/rusty-opus) |
| Vorbis | ✅ | ✅ | [`rusty_vorbis`](https://crates.io/crates/rusty_vorbis) |
| FLAC | ✅ | ✅ | [`rusty_flac`](https://crates.io/crates/rusty_flac) |
| ALAC | ✅ | | in-house |
| PCM, G.711, MS/IMA ADPCM | ✅ | ✅ PCM | in-house |
| PNG, JPEG, WebP | ✅ | ✅ | [`rusty_png`](https://crates.io/crates/rusty_png), [`rusty_jpeg`](https://crates.io/crates/rusty_jpeg), [`rusty_webp`](https://crates.io/crates/rusty_webp) |
| GIF · JPEG XL | ✅ | ✅ GIF | `gif` · `jxl-oxide` |

**Containers:**
- **Demux and mux:** MP4/MOV (including fragmented read), Matroska/WebM, Ogg, WAV (RF64/BW64), FLAC, AVI, IVF, AVIF and the image formats.
- **Demux only:** AIFF/AIFF-C and CAF.

## Install

```sh
cargo install rff-cli                           # rff + rffprobe
cargo install rff-cli --features drop-in-names  # also ffmpeg + ffprobe
cargo add remade-ffmpeg                         # the engine as a library
```

The drop-in names are opt-in because they shadow a real FFmpeg on your `PATH`. Prebuilt archives with both name pairs are on [Releases](https://github.com/Remade-With-Rust/remade_ffmpeg_rs/releases).

## Quick start

```sh
rff -codecs                                          # what this build supports
rffprobe input.mp4                                   # inspect a file
rff -i input.wav -c:a aac -b:a 128k -y output.m4a    # WAV -> AAC in MP4
rff -i input.wav -c:a flac -y output.flac            # lossless
rff -i input.avif -c:v avif -y output.avif           # AV1 decode + re-encode
```

The engine also runs as an API-first HTTP server:

```sh
cargo run -p rff-server   # listens on 127.0.0.1:8080
curl localhost:8080/v1/codecs
```

## Architecture

The Cargo workspace mirrors FFmpeg's library split:
- a dependency-free core (`rff-core`);
- codec and format layers (`rff-codec`, `rff-format`);
- one crate per codec and per container;
- the engine facade (`rff`, published as `remade-ffmpeg`);
- thin front-ends: `rff-cli`, `rff-server`, and a Dioxus `rff-ui`.

Adding a backend means implementing `Decoder`/`Encoder` or `Demuxer`/`Muxer` and calling `register(...)`. [docs/architecture.md](docs/architecture.md) has the details, and [docs/roadmap.md](docs/roadmap.md) has what's next: AV2 encode, fMP4/CMAF, low-latency live, IAMF.

**Server authentication:**
- **MATA mID:** a locally verified cryptographic identity, with no central auth server (`rff-auth`, feature `mata-mid`).
- **Bearer tokens:** a standard bearer-token option for stock clients.

**Building from source:**
- **Toolchain:** Rust 1.85+.
- **Commands:** `cargo build`. The default `h264-asm` feature is portable Rust SIMD with no assembler or C; `--no-default-features` gives scalar H.264, and `--features https` adds TLS input.

## License, patents, trademark

- **License:** Apache-2.0. The library, CLI, server and every codec/format crate have **no copyleft** in their dependency tree, enforced in CI by `cargo-deny`. The optional Dioxus UI is scoped out of that gate, since its webview stack pulls in MPL-2.0 crates.
- **Patents:** this is a clean-room reimplementation, which clears copyright but not patents.
  - **No obligations:** most of the stack is royalty-free or patent-expired (AV1, VP9, Opus, FLAC, Vorbis, PNG, JPEG, WebP, MP3).
  - **Patent-relevant:** **H.264 and AAC**. As with FFmpeg, no patent license is granted, and any royalties fall to whoever distributes or commercially deploys a product that includes them.
  - Details are in [docs/compatibility.md#patents](docs/compatibility.md#patents). This is not legal advice.
- **Trademark:** not affiliated with or endorsed by the FFmpeg project. "FFmpeg" is a trademark of Fabrice Bellard. The `ffmpeg`/`ffprobe` names exist only for command-line compatibility.

## Remade With Rust

<!-- ORG BOILERPLATE — keep identical across repos -->

**Remade With Rust** is an initiative by [Mata Network](https://www.mata.network)
to rebuild essential C and C++ tools in Rust — for the memory safety, the
predictable performance, and the freedom of a permissive license. Each project is a reimplementation, not a fork: same wire protocols and file formats,
new code you can actually depend on.

We build the core to production grade and open-source it so the community can
extend it. No copyleft. No surprises. Just the tools we rely on, made faster and
safer.

→ More projects: **[remadewithrust.com](https://remadewithrust.com)** and **[github.com/remade-with-rust](https://github.com/remade-with-rust)** 

<!-- /ORG BOILERPLATE -->

## About Mata Network

<!-- ORG BOILERPLATE — keep identical across repos -->

[Mata Network](https://www.mata.network) builds sovereign, self-hostable
infrastructure. **Remade With Rust** is our open-source home for the
permissively-licensed building blocks that work depends on.

<!-- /ORG BOILERPLATE -->
