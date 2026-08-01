# rff-ui

[![Remade With Rust](https://img.shields.io/badge/Remade%20With-Rust-000?logo=rust&logoColor=fff)](https://github.com/remade-with-rust)
[![By Mata Network](https://img.shields.io/badge/by-Mata%20Network-5b2be0)](https://www.mata.network)
[![License: Apache-2.0](https://img.shields.io/badge/license-Apache--2.0-blue)](https://github.com/Remade-With-Rust/remade_ffmpeg_rs/blob/main/LICENSE)

The **Dioxus front-end** for **remade_ffmpeg_rs** — one codebase targeting web,
PWA, desktop (Windows/macOS) and mobile (iOS/Android).

> **Not published to crates.io.** The Dioxus desktop renderer pulls a system
> webview, and that stack brings MPL-2.0 crates in transitively — so this crate
> is scoped out of the project's `cargo-deny` no-copyleft gate *and* out of the
> registry. It carries `publish = false`. The engine, CLI, server and every
> codec/format crate stay copyleft-free; this one is built on demand only.

- **One codebase, every target** via [Dioxus](https://dioxuslabs.com).
- **Excluded from the workspace default build** — `cargo build` skips it so the
  heavy webview toolchain is never a prerequisite for building the toolkit.
- Talks to the engine through [`rff`](https://crates.io/crates/rff), the same
  facade the CLI and server use.

## Building

```sh
cargo run -p rff-ui          # desktop (needs WebView2 on Windows, WebKitGTK on Linux)
```

For web and mobile targets, use the `dx` CLI:

```sh
cargo install dioxus-cli
dx serve --platform web
```

## About Mata Network

<!-- ORG BOILERPLATE — keep identical across repos -->

[Mata Network](https://www.mata.network) builds sovereign, self-hostable
infrastructure. **Remade With Rust** is our open-source home for the
permissively-licensed building blocks that work depends on.

<!-- /ORG BOILERPLATE -->

## License

Apache-2.0. See the workspace
[LICENSE](https://github.com/Remade-With-Rust/remade_ffmpeg_rs/blob/main/LICENSE).
