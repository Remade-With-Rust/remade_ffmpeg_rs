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
| `libavcodec` (core) | `rff-codec`        | `Decoder` / `Encoder` traits (send/receive shape) + `CodecRegistry`. `Decoder::configure(&CodecParams)` hands a parametric codec its stream params (sample rate/channels/format, or extradata like H.264 SPS/PPS) before the first packet; self-describing codecs ignore it. |
| `libavformat` (core)| `rff-format`       | `Demuxer` / `Muxer` traits + `FormatRegistry`, `Stream`. |
| codecs              | `rff-codec-{h264,opus,vorbis,flac,avif,png,jpeg,gif,webp,jxl,pcm,aac}` | One crate per codec, each `register(&mut CodecRegistry)`. Backends: `avif`→`rav1d`/`rav1e`, `png`→`png`, `jpeg`→`jpeg-decoder`/`jpeg-encoder`, `gif`→`gif`, `webp`→`image-webp`, `jxl`→`jxl-oxide`, `opus`→`opus-rs`, `vorbis`→`lewton`, `flac`→`claxon` — all pure Rust (see [pure-rust-codecs.md](pure-rust-codecs.md)); `pcm` and `aac` are in-house. `aac` is an **in-house AAC-LC decoder** (no permissive pure-Rust AAC crate exists, so we write our own). The long-window path is wired end-to-end: `AudioSpecificConfig`/ADTS framing → `raw_data_block` element loop (SCE/CPE/LFE) → section data, scalefactors and spectral Huffman decode (12 ISO codebooks, validated for count/Kraft/prefix-freedom) → pulse, inverse quantization, M/S stereo → 2048-point IMDCT with sine/KBD windowed overlap-add. All window sequences (long, LONG_START/STOP, **EIGHT_SHORT** with grouping + 8×256 IMDCT), **M/S** and **intensity stereo**, **PNS** noise substitution and **TNS** are implemented. It is **verified against FFmpeg** on real AAC-LC files: deterministic features (long/short windows, M/S, intensity stereo, TNS) are **bit-exact** (0.0% residual, offset only by the 1024-sample encoder priming; output normalized to float `[-1,1]`); PNS is **energy-exact** (RMS matches to 4 decimals, samples differ only by random phase as the spec intends). Decode-only ones register `encoder: None`. `rff-codec-openh264` (Cisco C/FFI) is a **temporary** H.264 stopgap behind the off-by-default `h264-openh264` feature — excluded from the default build and the pure-Rust guarantee. |
| (de)muxers          | `rff-format-{avi,avif,png,jpeg,gif,webp,jxl,wav,ogg,flac,mp4,mkv}` | One crate per container, each `register(&mut FormatRegistry)`. `mkv` is an EBML-based **Matroska / WebM demuxer** (track tree → Cluster/(Simple)Block packets; CodecPrivate→extradata), verified against FFmpeg on real AV1+Opus WebM. `mp4` extracts AAC's `AudioSpecificConfig` from `esds` into `extradata`. `rff-format-avif` reads/writes the HEIF/ISOBMFF box tree; image formats are single-image passthroughs; `wav` is RIFF/WAVE; `ogg` is page-based (Opus + Vorbis); `flac` is native FLAC; `mp4` reads + writes the sample table (`stsd`/`stsz`/`stsc`/`stco`/`stts`/`stss`) and converts H.264 AVCC↔Annex-B (`avcC` SPS/PPS); the muxer writes `ftyp`/`mdat`/`moov` with **video + audio** tracks (AV1 `av01`/`av1C`, H.264 `avc1`/`avcC`, Opus `Opus`/`dOps`). Each track keeps its own media timescale (from the stream `time_base`, e.g. 48000 for Opus) and derives per-sample `stts` durations from packet PTS — audio encoders timestamp in per-channel samples — so muxed timing is real, not a nominal frame rate. Multi-track output is **time-interleaved**: samples are written to `mdat` in PTS order across tracks and grouped into per-track chunks (`stco`/`stsc`), so a player reads A/V progressively instead of seeking between two contiguous blobs. |
| `libavfilter`       | `rff-filter`       | Frame filters (`scale`, `crop`, `hflip`, `vflip`, `transpose`, `pad`, `format`) as a `FilterChain`, parsed from a `-vf` spec and applied between decode and encode. `format` does RGB↔YUV conversion. |
| `libswresample`     | `rff-resample`     | Audio sample-rate conversion: a streaming windowed-sinc (Blackman) FIR resampler. The transcode loop inserts it automatically when an encoder (e.g. Opus) rejects the input rate, resampling to the nearest accepted rate. |
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
working end-to-end. The **AVIF/AV1 path is fully implemented**: `transcode::run`
drives a real `demux → decode → encode → mux` loop (with a `-c:v copy`
stream-copy fallback), so `ffmpeg -i in.avif -c:v avif out.avif` round-trips a
picture through `rav1d`/`rav1e` and the HEIF box layer. Coverage is verified by
codec, container, and engine-level round-trip tests.

The AVIF path also handles 8- and 10-bit YUV and reads *foreign* AVIFs (those
that keep the AV1 sequence header in `av1C` rather than `mdat`). The **AVI
container** is fully wired both ways — the demuxer parses the RIFF/`hdrl`/`movi`
tree and the muxer writes `hdrl`/`movi`/`idx1` — so `ffprobe in.avi` and
stream-copy remuxing through AVI work; a full *transcode* out of AVI still waits
on a decodable AVI codec body.

The remaining codec *bodies* (`h264`, `opus`) are still scaffolded: each returns
`Error::Unimplemented` with a precise label, so a transcode using them resolves
the whole graph and then stops at the first unimplemented stage. Implementation
order per codec lives in each crate's module docs.

## Design rules (from the project requirements)

* **API first.** The CLI and server are thin shells over `rff`'s public API;
  no capability is reachable from a front-end that isn't reachable
  programmatically.
* **Permissive only.** No copyleft anywhere in the *core* tree (library, CLI,
  server, codecs, formats); enforced in CI by `cargo deny check licenses` (see
  `deny.toml`). The optional `rff-ui` (Dioxus, built on demand) pulls MPL-2.0
  crates through its webview stack and is excluded from the gate via
  `[graph] exclude` — it's the one place weak copyleft is tolerated, and it
  never ships in the core or the published binaries.
* **Safe Rust on the core path.** Any future `unsafe` (e.g. SIMD intrinsics)
  must be isolated and documented at the FFI/intrinsic boundary.
* **Sovereign auth.** Server access uses MATA mID verification (`rff-auth`),
  with a clearly-labelled dev stub for local use.
