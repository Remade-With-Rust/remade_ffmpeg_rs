# remade_ffmpeg_rs

[![Remade With Rust](https://img.shields.io/badge/Remade%20With-Rust-000?logo=rust&logoColor=fff)](https://github.com/remade-with-rust)
[![By Mata Network](https://img.shields.io/badge/by-Mata%20Network-5b2be0)](https://www.mata.network)
[![License: Apache-2.0](https://img.shields.io/badge/license-Apache--2.0-blue)](LICENSE)
![Platforms: Windows · macOS · Linux · Web](https://img.shields.io/badge/platforms-Windows%20%C2%B7%20macOS%20%C2%B7%20Linux%20%C2%B7%20Web-informational)

> **remade_ffmpeg_rs** is a memory-safe media toolkit — decode, encode,
> transcode, mux and probe audio/video — a ground-up **Rust** rebuild of
> [FFmpeg](https://github.com/FFmpeg/FFmpeg) (LGPL-2.1+/GPL-2.0+/C), under a
> permissive license, built for speed, safety, and zero copyleft strings.

---

## ⚡ The headline

<!-- Lead with the number. This is why someone clicks the repo. -->

> **Pre-release / scaffold.** The numbers below are **engineering targets**, not
> yet measured — the codec bodies are still being implemented. They state what
> "good" looks like and will be replaced with reproducible captures as each
> codec lands. We will not ship a benchmark we can't reproduce.

| | FFmpeg (C) | **remade_ffmpeg_rs (Rust)** | Target |
|---|---:|---:|:---:|
| Memory-safety CVEs in core path | many, historically | **0 (safe Rust)** | **structural** |
| H.264 decode throughput | baseline | **≥ parity** | **≥ 1.0×** |
| Cold-start / embed overhead | baseline | **lower (no FFI, no GPL)** | **≤ 1.0×** |

<sub>Methodology + raw captures will live in [docs/benchmarks.md](docs/benchmarks.md) once codecs are implemented.</sub>

---

## What is this?

`remade_ffmpeg_rs` rebuilds FFmpeg's pipeline — demux → decode → filter →
encode → mux — as a set of small, composable Rust crates. The goal is that
anyone using it *feels* like they're using FFmpeg (same `ffmpeg`/`ffprobe`
commands, same flags) while getting memory safety, a clean embeddable library
API, and a permissive license with no GPL/LGPL anywhere in the tree. It's a
reimplementation, not a fork: no FFmpeg source is copied — only its file
formats and command-line interface are matched.

## Remade With Rust

<!-- ORG BOILERPLATE — keep identical across repos -->

**Remade With Rust** is an initiative by [Mata Network](https://www.mata.network)
to rebuild essential C and C++ tools in Rust — for the memory safety, the
predictable performance, and the freedom of a permissive license. Each project is a reimplementation, not a fork: same wire protocols and file formats,
new code you can actually depend on.

We build the core to production grade and open-source it so the community can
extend it. No copyleft. No surprises. Just the tools we rely on, made faster and
safer.

→ More projects: **[github.com/remade-with-rust](https://github.com/remade-with-rust)**

<!-- /ORG BOILERPLATE -->

## Features

- **Drop-in CLI.** `ffmpeg` and `ffprobe` binaries that speak the flags you
  already know (`-i`, `-c:v`, `-c:a`, `-b:v`, `-f`, `-y`, `-codecs`, ...).
- **Layered, swappable architecture.** One crate per codec and per container,
  registered into a central engine — mirrors FFmpeg's `libav*` split. See
  [docs/architecture.md](docs/architecture.md).
- **API-first.** The CLI and the HTTP server are thin shells over the `rff`
  engine library, so AI agents and remote tools get first-class access.
- **Sovereign auth.** Server access uses [MATA mID](https://www.mata.network)
  verification — a locally-verified cryptographic identity, no central auth.
- **One UI, every target.** A [Dioxus](https://dioxuslabs.com) front-end for
  web, PWA, desktop (Windows/macOS) and mobile (iOS/Android) from one codebase.
- **Permissive license** (Apache-2.0) — embed it in closed-source software freely.
- **100% safe Rust** on the core path; every future `unsafe` boundary documented and isolated.

### Codecs & formats (growing)

See [docs/ffmpeg-parity.md](docs/ffmpeg-parity.md) for the full FFmpeg
tool/library parity map, the top-10 global-codec scorecard, and scope decisions.

| Kind | Supported | Status |
|---|---|---|
| Image codec | **avif** (AV1 still image) | **decode + encode**, 8- & 10-bit (rav1d / rav1e) |
| Image codec | **png** (RGB/RGBA) | **decode + encode** (pure-Rust `png`) |
| Image codec | **mjpeg** (JPEG/MJPEG) | **decode + encode** (pure-Rust `jpeg-decoder`/`jpeg-encoder`) |
| Image codec | **gif** | **decode + encode** (pure-Rust `gif`; first frame) |
| Image codec | **webp** (VP8/VP8L) | **decode + lossless encode** (pure-Rust `image-webp`) |
| Image codec | **jpegxl** (JPEG XL) | **decode** (pure-Rust `jxl-oxide`; no Rust encoder yet) |
| Audio codec | **opus** | **decode + encode** (pure-Rust `opus-rs`) |
| Audio codec | **vorbis** | **decode** (pure-Rust `lewton`; no permissive Rust encoder exists) |
| Audio codec | **flac** | **decode** (pure-Rust `claxon`; no permissive Rust encoder exists) |
| Audio codec | **pcm** (s16le / f32le) | **decode + encode** (in-house) |
| Container | **avif** (AV1 Image File Format) | **demux + mux** (reads foreign AVIFs too) |
| Container | **png** / **jpeg** / **gif** / **webp** / **jpegxl** | **demux + mux** |
| Container | **wav** (RIFF/WAVE) / **ogg** (Opus/Vorbis) / **flac** | **demux + mux** |
| Container | **avi** (Audio Video Interleaved) | **demux + mux** (RIFF/`hdrl`/`movi`/`idx1`) |
| Container | **mp4** / **mov** (ISOBMFF) | **demux + mux** — sample tables; **A/V**: AV1 (`av01`/`av1C`) or H.264 (`avc1`/`avcC`) video + Opus audio (`dOps`); AAC `esds`→config |
| Container | **matroska** / **webm** (EBML) | **demux** — track tree + Cluster/(Simple)Block packets; AV1/H.264 video + Opus/Vorbis/AAC/FLAC audio |
| Audio codec | **aac** | in-house **AAC-LC decoder**, all features (short blocks, M/S, intensity stereo, PNS, TNS) — verified bit-exact vs FFmpeg |
| Audio codec | **mp3** (MPEG-1/2 Layer III) | in-house **decoder + encoder** — framework scaffolded (`rff-codec-mp3`), building brick by brick 🚧 |
| Video codec | **vp9** (VP9) | **decode** — in-house pure-Rust decoder, **bit-exact against all 315 official libvpx conformance vectors**; profiles 0–3 (4:2:0/4:2:2/4:4:4, 8/10/12-bit), AVX2 + NEON kernels (no encoder; perf tuning to follow) |
| Video codec | **h264** (H.264 / AVC) | **decode + encode** — [`rusty_h264`](https://crates.io/crates/rusty_h264) with SIMD asm, **default** |

> **H.264 defaults to `rusty_h264` with SIMD asm on** — substantially faster.
> Like `rav1e`, the speedup is hand-written x86 **assembly, no C** (openh264's
> kernels, **vendored** under BSD-2 — no external source tree), isolated in a
> single `unsafe` crate (`rusty_h264-accel`). The one practical cost: the default
> build needs **`nasm`** (`choco install nasm` / `apt install nasm` /
> `brew install nasm`). `--no-default-features` drops to `rusty_h264`'s scalar
> path (no `nasm`, no asm, zero `unsafe`); `--features h264-openh264` swaps in
> Cisco's C `openh264` as a reference cross-check.

The **audio path** is real: `ffmpeg -i in.wav -c:a opus out.opus` decodes PCM, encodes Opus, and writes an Ogg file — through the same engine the image codecs use. Parametric codecs (PCM) and ones with out-of-band config (Opus' channels/rate from `OpusHead`) receive their parameters via a `Decoder::configure` step — the same plumbing H.264 will use for SPS/PPS.

**Audio resampling.** When an encoder only accepts certain sample rates, the transcode loop auto-inserts a resampler (a streaming windowed-sinc FIR, the `libswresample` equivalent) — exactly like FFmpeg's implicit `aresample`. So `ffmpeg -i in_44100.wav -c:a opus out.mp4` converts 44.1 kHz to Opus's nearest accepted rate (48 kHz) with no extra flags.

**A/V muxing.** Multiple inputs combine into one multi-stream output. `ffmpeg -i v -i a -c:v avif -c:a opus out.mp4` writes a single MP4 carrying **AV1 video + Opus audio** — entirely pure-Rust, no extra features. (Swap `-c:v h264` with the `h264-openh264` feature for H.264 video instead.) AVI muxing works the same way (`-c:v copy -c:a copy out.avi`). MP4 output carries **real timing** (each track's `stts`/timescale come from packet PTS, not a nominal frame rate) and is **time-interleaved** — samples are written in PTS order across tracks so players can read audio + video progressively.

**Stream selection (`-map`).** Pick exactly which input streams reach the output: `-map 0:v` (all video of input 0), `-map 0:a`, `-map 1:0` (stream 0 of input 1), or `-map 0` (everything) — repeatable and order-preserving. With no `-map`, every video + audio stream is carried by default. Combine with `-c copy` to losslessly lift a single track, e.g. `ffmpeg -i av.mp4 -map 0:a -c:a copy audio.mp4` pulls the Opus track out of an MP4 without re-encoding.

With the `format` filter bridging colorspaces, `ffmpeg -i photo.png -vf format=yuv420p -c:v avif out.avif` (and the reverse) does real PNG↔AVIF image conversion today.

**Codec backends — every one is 100% Rust (no C/C++ FFI) and permissively licensed.** Container (de)muxers are our own code. See [docs/pure-rust-codecs.md](docs/pure-rust-codecs.md) for the full vetted survey (what's clean, what's license-blocked, what has no pure-Rust option).

| Codec | Backing crate | License | Pure Rust |
|---|---|---|---|
| AV1 encode (avif) | [`rav1e`](https://github.com/xiph/rav1e) | BSD-2-Clause | ✅ (asm, no C) |
| AV1 decode (avif) | [`rav1d`](https://github.com/memorysafety/rav1d) | BSD-2-Clause | ✅ (Rust port of dav1d) |
| H.264 decode/encode | [`rusty_h264`](https://crates.io/crates/rusty_h264) | BSD-2-Clause | ✅ (vendored asm, no C; default needs `nasm`) |
| PNG encode/decode | [`png`](https://crates.io/crates/png) | MIT/Apache-2.0 | ✅ |
| JPEG decode | [`jpeg-decoder`](https://crates.io/crates/jpeg-decoder) | MIT/Apache-2.0 | ✅ |
| JPEG encode | [`jpeg-encoder`](https://crates.io/crates/jpeg-encoder) | MIT/Apache-2.0 AND IJG | ✅ |
| GIF encode/decode | [`gif`](https://crates.io/crates/gif) | MIT/Apache-2.0 | ✅ |
| WebP encode/decode | [`image-webp`](https://crates.io/crates/image-webp) | MIT/Apache-2.0 | ✅ |
| Opus encode/decode | [`opus-rs`](https://crates.io/crates/opus-rs) | BSD-3-Clause | ✅ |
| Vorbis decode | [`lewton`](https://crates.io/crates/lewton) | MIT/Apache-2.0 | ✅ |
| FLAC decode | [`claxon`](https://crates.io/crates/claxon) | Apache-2.0 | ✅ |
| JPEG XL decode | [`jxl-oxide`](https://crates.io/crates/jxl-oxide) | MIT/Apache-2.0 | ✅ |

The **avif** path is real end to end: a frame decodes (via the pure-Rust
[`rav1d`](https://github.com/memorysafety/rav1d)) and encodes (via
[`rav1e`](https://github.com/xiph/rav1e)) AV1 bitstream, wrapped/unwrapped in
HEIF/ISOBMFF boxes, and driven through the `demux → decode → encode → mux`
loop — so `ffmpeg -i in.avif -c:v avif out.avif` works today. Both AV1 crates
are BSD-2-Clause; the decode path adds zero `unsafe` to this tree.

"Scaffolded" = registered and wired through the engine, CLI and server; the
bitstream body is the next implementation step. More codecs/containers to come.

## Install

```sh
# From source (see "Building from source"); published crates/binaries to follow.
cargo install --path crates/rff-cli
```

This installs the `ffmpeg` and `ffprobe` binaries. Prebuilt binaries will be
posted to [Releases](https://github.com/Remade-With-Rust/remade_ffmpeg_rs/releases).

## Quick start

```sh
# List what this build supports — just like FFmpeg:
ffmpeg -codecs
ffmpeg -formats

# Inspect a file:
ffprobe input.avif

# Transcode AVIF → AVIF end to end (decode AV1, re-encode AV1, rewrap):
ffmpeg -i input.avif -c:v avif -y output.avif

# Other codecs/containers (h264, opus, avi) are wired but their bodies are
# still in progress, so those transcodes stop at the unimplemented stage.
```

Or talk to the engine over HTTP (API-first):

```sh
cargo run -p rff-server          # listens on 127.0.0.1:8080
curl localhost:8080/v1/codecs
curl localhost:8080/healthz
```

## Architecture

A Cargo workspace that mirrors FFmpeg's own library decomposition: a
dependency-free core (`rff-core`), codec/format abstraction layers
(`rff-codec`, `rff-format`), one crate per codec/container, an engine facade
(`rff`), and the front-ends (`rff-cli`, `rff-server`, `rff-ui`). Full details:
[docs/architecture.md](docs/architecture.md).

```
  rff-core ◀── rff-codec ◀── rff-codec-{h264,opus,avif} ┐
       ▲   ◀── rff-format ◀── rff-format-avi ────────────┤
       │                                                 ▼
       └──────────────────────────────────────────────▶ rff (engine facade)
                                                          ▲
                                  ┌───────────────────────┼──────────────┐
                               rff-cli (ffmpeg/ffprobe)  rff-server     rff-ui
```

## Authentication & deployment

- **MATA mID (default for MATA deployments).** Authenticate with a MATA mID — a
  locally-verified cryptographic identity; no interactive step, built for
  programmatic / headless / fleet deployments. Implemented behind the
  `rff-auth` `mata-mid` feature.
- **Bearer token / dev mode (universal compatibility).** A standard
  `Authorization: Bearer` mechanism is retained so stock clients work; the
  bundled `DevAllowAll` verifier is for local development only.

## Building from source

```sh
git clone https://github.com/Remade-With-Rust/remade_ffmpeg_rs
cd remade_ffmpeg_rs
cargo build              # engine + CLI + server (UI excluded; see below)
cargo run -p rff-ui      # build/run the Dioxus desktop UI on demand
```

**Requirements:** Rust 1.85+ (stable). The Dioxus UI additionally needs a system
webview (WebView2 on Windows, WebKitGTK on Linux) and, for web/mobile targets,
the `dx` CLI (`cargo install dioxus-cli`).

## Platform support

| Platform | Status |
|---|---|
| Windows / macOS / Linux (CLI + server) | ✅ builds |
| Web (WASM) / PWA / mobile (Dioxus UI) | 🚧 scaffolded |

Adding a codec or container backend is a first-class extension point —
implement the `Decoder`/`Encoder` or `Demuxer`/`Muxer` traits and call
`register(...)`, no engine-core changes required.

## Roadmap

- [x] Land the first real codec body end-to-end (AVIF: decode → re-encode → mux).
- [x] 10-bit AVIF and reading foreign AVIFs (sequence header in `av1C`). *(12-bit pending.)*
- [x] AVI container (demux + mux). *(Codec bodies — h264, opus — still pending.)*
- [x] Content-sniffing probe (magic bytes) in addition to extension matching.
- [x] Filter graph layer (`libavfilter` equivalent): `scale` (+ aspect `-1`), `crop`, `hflip`, `vflip`, `transpose`, `pad`.
- [ ] Implement the next codec bodies (h264, opus).
- [ ] Reproducible benchmark suite and published numbers.
- [ ] Wire real MATA mID verification (`sovereign-id-verify`).

## License

Apache-2.0 — see [LICENSE](LICENSE). The embeddable **core** — the library, the
`ffmpeg`/`ffprobe` CLI, the server, and every codec/format crate — has **no
copyleft anywhere** in its dependency tree, CI-enforced via `cargo-deny` (see
[deny.toml](deny.toml)). The optional Dioxus UI (`rff-ui`, built on demand and
never part of the published binaries) pulls MPL-2.0 crates transitively through
its webview stack, so it's scoped out of the gate and tracked separately.

## Patents

Licensing and patents are **separate** things. The clean-room work clears
*copyright* — there's no GPL/FFmpeg code here, hence the permissive license
above — but an independent implementation does **not** clear *patents*: a patent
covers a *technique in the standard*, which any implementation practices
regardless of language or authorship.

Most of the stack is **royalty-free or patent-expired** — AV1/AVIF, VP9, Opus,
FLAC, Vorbis, PNG, JPEG, GIF, WebP, JPEG XL, MP3 (expired 2017), PCM — and
carries no patent obligation for anyone.

Two codecs are **patent-relevant**: **H.264/AVC** (via `rusty_h264`) and
**AAC** (our in-house AAC-LC *decoder* only — the largely-expired, lowest-risk
corner; no HE-AAC). We take the same posture as FFmpeg: these ship in the
default build, **no patent license is granted or implied**, and any patent
royalties (e.g. to the Via LA pools) are the responsibility of the party that
distributes or commercially deploys a product incorporating them — not of the
project or of people simply running the tool. If that matters for your use,
gate H.264/AAC out behind a feature or obtain a pool license, and **consult IP
counsel** for commercial deployments. Full breakdown:
[docs/compatibility.md#patents](docs/compatibility.md#patents). *(This is
engineering context, not legal advice.)*

## Trademark

This is an independent, clean-room reimplementation. It is **not affiliated
with, endorsed by, or derived from the source code of the FFmpeg project**.
"FFmpeg" is a trademark of Fabrice Bellard. The `ffmpeg` and `ffprobe`
executable names are provided solely for command-line compatibility so existing
scripts and workflows keep working; the product itself is **remade_ffmpeg_rs**.

## About Mata Network

<!-- ORG BOILERPLATE — keep identical across repos -->

[Mata Network](https://www.mata.network) builds sovereign, self-hostable
infrastructure. **Remade With Rust** is our open-source home for the
permissively-licensed building blocks that work depends on.

<!-- /ORG BOILERPLATE -->
