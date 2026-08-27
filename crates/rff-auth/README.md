# rff-auth

[![Remade With Rust](https://img.shields.io/badge/Remade%20With-Rust-000?logo=rust&logoColor=fff)](https://github.com/remade-with-rust)
[![By Mata Network](https://img.shields.io/badge/by-Mata%20Network-5b2be0)](https://www.mata.network)
[![License: Apache-2.0](https://img.shields.io/badge/license-Apache--2.0-blue)](https://github.com/Remade-With-Rust/remade_ffmpeg_rs/blob/main/LICENSE)

Authentication abstraction for **remade_ffmpeg_rs** — one `Authenticator`
trait with two verifiers: **MATA mID** sovereign identity, and a standard bearer
token for stock clients. Used by [`rff-server`](https://crates.io/crates/rff-server).

- **MATA mID** (the `mata-mid` feature) — a **locally-verified cryptographic identity**, no central auth service and no interactive step. Built for programmatic, headless and fleet deployments.
- **Bearer token** — a standard `Authorization: Bearer` path so stock HTTP clients work unchanged.
- **`DevAllowAll`** — a permissive verifier for local development. It authenticates everyone; never deploy it.
- **`Authenticator`** is `Send + Sync`, so a custom verifier drops straight into the server.

## Usage

```rust
use rff_auth::{Authenticator, DevAllowAll};

fn main() {
    // Local development only — this verifier accepts anything.
    let auth = DevAllowAll;
    match auth.authenticate("any-non-empty-credential") {
        Ok(identity) => println!("authenticated as {}", identity.subject),
        Err(e) => eprintln!("rejected: {e}"),
    }
}
```

For real deployments enable the `mata-mid` feature and use `MataMidVerifier`,
which verifies a [MATA mID](https://www.mata.network) locally — no round trip to
an auth server.

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
