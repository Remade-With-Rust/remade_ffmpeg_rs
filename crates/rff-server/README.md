# rff-server

[![Remade With Rust](https://img.shields.io/badge/Remade%20With-Rust-000?logo=rust&logoColor=fff)](https://github.com/remade-with-rust)
[![By Mata Network](https://img.shields.io/badge/by-Mata%20Network-5b2be0)](https://www.mata.network)
[![License: Apache-2.0](https://img.shields.io/badge/license-Apache--2.0-blue)](https://github.com/Remade-With-Rust/remade_ffmpeg_rs/blob/main/LICENSE)

The HTTP API of **remade_ffmpeg_rs** — exposes the engine over REST so AI
agents, remote clients and the UI get first-class access to transcoding and
probing. Built on `axum`/`tokio`.

- **API-first.** The CLI and this server are peers, both thin shells over the [`rff`](https://crates.io/crates/rff) engine — nothing is CLI-only.
- **Endpoints** for codec/format discovery, probing and transcoding, plus `/healthz`.
- **Sovereign auth** via [`rff-auth`](https://crates.io/crates/rff-auth): MATA mID locally-verified cryptographic identity, or a standard `Authorization: Bearer` token for stock clients.
- Binds `127.0.0.1:8080` by default — loopback, not `0.0.0.0`.

## Install

```sh
cargo install rff-server
rff-server                          # listens on 127.0.0.1:8080

curl localhost:8080/healthz
curl localhost:8080/v1/codecs
```

The bundled `DevAllowAll` verifier authenticates every request and is for local
development only. Configure a real [`rff-auth`](https://crates.io/crates/rff-auth)
verifier before exposing the server beyond loopback.

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
