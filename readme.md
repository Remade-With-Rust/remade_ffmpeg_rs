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

| Kind | Supported | Status |
|---|---|---|
| Video codec | **h264** (H.264 / AVC) | scaffolded |
| Audio codec | **opus** | scaffolded |
| Image codec | **avif** (AV1 still image) | scaffolded |
| Container | **avi** (Audio Video Interleaved) | scaffolded |

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
ffprobe input.avi

# Transcode (interface is wired; codec bodies are in progress):
ffmpeg -i input.avi -c:v h264 -b:v 2M -c:a opus output.avi
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

- [ ] Land the first real codec body end-to-end (decode → re-encode → mux).
- [ ] Content-sniffing probe (magic bytes) in addition to extension matching.
- [ ] Filter graph layer (`libavfilter` equivalent) + scaling/resampling.
- [ ] Reproducible benchmark suite and published numbers.
- [ ] Wire real MATA mID verification (`sovereign-id-verify`).

## License

Apache-2.0 — see [LICENSE](LICENSE). No GPL/LGPL anywhere in the dependency tree
(CI-enforced via `cargo-deny`; see [deny.toml](deny.toml)).

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
