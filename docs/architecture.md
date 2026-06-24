# Architecture

`remade_ffmpeg_rs` is a clean-room reimplementation of FFmpeg's pipeline in
Rust. It deliberately mirrors FFmpeg's own library decomposition so the mental
model transfers, while being a fresh codebase with a permissive license.

## The pipeline

Every transcode is the same five-stage flow FFmpeg uses:

```
  ┌─────────┐   ┌─────────┐   ┌──────────┐   ┌─────────┐   ┌────────┐
  │ Demuxer │──▶│ Decoder │──▶│ Filters  │──▶│ Encoder │──▶│ Muxer  │
  │ (read   │   │ packets │   │ (raw     │   │ frames  │   │ (write │
  │ container)  │ → frames│   │ frames)  │   │ → packets   │ container)
  └─────────┘   └─────────┘   └──────────┘   └─────────┘   └────────┘
     Packet         Frame         Frame          Packet
```

* **Packet** — a chunk of *compressed* bytes for one stream (`rff_core::Packet`).
* **Frame** — a chunk of *raw* pixels/samples (`rff_core::Frame`).

Probing (`ffprobe`) runs only the left half: demux + read headers.

## Crate map (vs. FFmpeg libraries)

| FFmpeg library      | Our crate          | Responsibility |
|---------------------|--------------------|----------------|
| `libavutil`         | `rff-core`         | Shared primitives: `Frame`, `Packet`, `Error`, `CodecId`, pixel/sample formats, `Rational`, `Dictionary`. No codec logic. |
| `libavcodec` (core) | `rff-codec`        | `Decoder` / `Encoder` traits (send/receive shape) + `CodecRegistry`. |
| `libavformat` (core)| `rff-format`       | `Demuxer` / `Muxer` traits + `FormatRegistry`, `Stream`. |
| codecs              | `rff-codec-h264`, `rff-codec-opus`, `rff-codec-avif` | One crate per codec. Each exposes a `register(&mut CodecRegistry)`. |
| (de)muxers          | `rff-format-avi`   | One crate per container. Exposes `register(&mut FormatRegistry)`. |
| —                   | `rff`              | Facade: builds an `Engine` with all built-ins registered; high-level `transcode` + `probe` APIs. |
| `ffmpeg` / `ffprobe`| `rff-cli`          | FFmpeg-compatible CLI front-ends (drop-in binary names). |
| —                   | `rff-server`       | axum HTTP API exposing the engine (API-first; AI/remote/UI consume it). |
| —                   | `rff-auth`         | `Authenticator` trait + MATA mID verifier (feature `mata-mid`). |
| —                   | `rff-ui`           | Dioxus app (web / PWA / desktop / mobile). |

## Dependency direction

```
  rff-core  ◀── everything

  rff-codec ──▶ rff-core            rff-format ──▶ rff-core
      ▲                                  ▲
      │ register()                       │ register()
  rff-codec-{h264,opus,avif}        rff-format-avi
      ▲                                  ▲
      └──────────── rff ◀────────────────┘   (facade: registers built-ins)
                     ▲
        ┌────────────┼─────────────┐
     rff-cli     rff-server      rff-ui
                     ▲
                  rff-auth
```

Nothing depends "upward". A new codec only touches its own crate plus one line
in `rff/src/lib.rs` (`register_builtin_codecs`).

## Adding a codec

1. `cargo new --lib crates/rff-codec-<name>`; depend on `rff-core` + `rff-codec`.
2. Add a `CodecId::<Name>` variant in `rff-core` (`media.rs`) and wire its
   `name()` / `media_type()` / `from_name()`.
3. Implement `Decoder` and/or `Encoder`, plus a `register(&mut CodecRegistry)`.
4. Call it from `register_builtin_codecs` in `rff/src/lib.rs`.
5. Add the crate to the workspace `members` / `default-members`.

Adding a container is the same shape against `rff-format` /
`register_builtin_formats`.

## Current status

The full skeleton compiles and the registries/facade/CLI/server are wired and
working end-to-end. The codec and container *bodies* are scaffolded: each
returns `Error::Unimplemented` with a precise label, so an end-to-end transcode
resolves the entire graph (demuxer, decoders, encoders, muxer) and then stops at
the first unimplemented stage. Implementation order per codec lives in each
crate's module docs.

## Design rules (from the project requirements)

* **API first.** The CLI and server are thin shells over `rff`'s public API;
  no capability is reachable from a front-end that isn't reachable
  programmatically.
* **Permissive only.** No GPL/LGPL anywhere in the tree; enforced in CI by
  `cargo deny check licenses` (see `deny.toml`).
* **Safe Rust on the core path.** Any future `unsafe` (e.g. SIMD intrinsics)
  must be isolated and documented at the FFI/intrinsic boundary.
* **Sovereign auth.** Server access uses MATA mID verification (`rff-auth`),
  with a clearly-labelled dev stub for local use.
