# remade_ffmpeg_rs

[![Remade With Rust](https://img.shields.io/badge/Remade%20With-Rust-000?logo=rust&logoColor=fff)](https://github.com/remade-with-rust)
[![By Mata Network](https://img.shields.io/badge/by-Mata%20Network-5b2be0)](https://www.mata.network)
[![License: Apache-2.0](https://img.shields.io/badge/license-Apache--2.0-blue)](LICENSE)
![Platforms: Windows · macOS · Linux · Web](https://img.shields.io/badge/platforms-Windows%20%C2%B7%20macOS%20%C2%B7%20Linux%20%C2%B7%20Web-informational)

> **remade_ffmpeg_rs** is a memory-safe media toolkit — decode, encode,
> transcode, mux and probe audio/video — a ground-up **Rust** rebuild of
> [FFmpeg](https://github.com/FFmpeg/FFmpeg) (LGPL-2.1+/GPL-2.0+/C), under a
> permissive license, built for speed, safety, and zero copyleft strings.

> **Status — pre-1.0, and not yet independently audited.** APIs and codec
> coverage are still moving; use it accordingly. See the
> [security policy](SECURITY.md), the
> [compatibility & patent matrix](docs/compatibility.md), and
> [how to contribute](CONTRIBUTING.md).

---

## The headline

<!-- Lead with the number. This is why someone clicks the repo. -->

> **Pre-1.0, measured honestly.** Where a number is benchmarked we report it as
> measured — flattering or not. We lead on what's structurally true today
> (safety, correctness, license); raw speed is younger than FFmpeg's and we say
> so. We will not ship a benchmark we can't reproduce.

| Dimension | FFmpeg (C) | **remade_ffmpeg_rs (Rust) — today** | Goal |
|---|:---:|:---:|:---:|
| Memory-safety CVEs (core path) | many, historically | **0 — safe Rust** | structural |
| Conformance | reference | **bit-exact** (VP9 315/315 vectors; MP3 vs FFmpeg) | maintain |
| VP9 decode, 1 thread | 1.0× | **~0.16–0.21×** — younger, optimizing | → parity |
| AAC encode (60 s stereo) | 1.0× | **~6× faster** — frame-parallel (ffmpeg's AAC is 1-thread); ~1.15× single-thread | maintain |
| Vorbis encode (stereo music) | 1.0× | **~5.3× faster** — frame-parallel; the **first permissive-Rust Vorbis encoder** | → single-thread |
| Opus encode (speech, `libopus`) | 1.0× | **~3× faster** wall-clock — frame-parallel, PEAQ-neutral (`libopus` is 1-thread); ~0.35× single-thread | → single-thread |
| License + embedding | LGPL/GPL · C FFI | **Apache-2.0 · pure Rust · no FFI** | — |

<sub>Real numbers + how to reproduce them: [docs/benchmarks.md](docs/benchmarks.md). The VP9 speed figure is decode throughput on an i7-14650HX vs FFmpeg's native decoder.</sub>

> **⚡ Performance spotlight — AAC encode, faster than the C.** Our in-house, pure-Rust
> AAC-LC encoder went from 0.79× realtime to **449× realtime** — a **~570× throughput gain**
> — landing **~6× faster than FFmpeg's own AAC encoder** (best-of-7, 60 s stereo @128k, 24
> cores), while its bitstream stays **byte-identical** and FFmpeg decodes it at unity. The
> wins, in the order profiling demanded them: an O(N²) MDCT replaced by an FFT (**940×** on
> that stage), a two-phase rate loop, cached psychoacoustic tables, an **N/4-point-FFT MDCT**,
> **AVX2** (+ opt-in AVX-512) quantize kernels — that reached single-thread parity — and
> finally **frame-parallel encoding**, the structural move FFmpeg's single-threaded AAC can't
> answer. Every step was gated **bit-exact against a kept scalar oracle**; the pure-safe
> `--no-default-features` build passes the same tests. Not a benchmark we can't reproduce —
> just the right algorithm, then the right hardware.

> **⚡ Performance spotlight — Vorbis encode: the first pure-Rust Vorbis encoder, and it beats
> libvorbis.** No permissively-licensed Vorbis *encoder* had ever existed in Rust — `lewton`
> decodes, nothing encoded. This is the first, and in a profile-gated campaign it went from
> **64× slower** than FFmpeg's libvorbis to **~5.3× faster** (stereo music, 24 cores, **~457×
> realtime**), ffmpeg-decodable throughout. The levers, in the order the profiler demanded them:
> an **N/4-point-FFT MDCT** (O(N²) → O(N log N), collapsing the transform from **46% of runtime
> to 1%**), a **separable-lattice** VQ quantizer, **structure-of-arrays + AVX2** for the
> residue-VQ nearest-neighbour search (**2.7×** on the classifier — the branch-split
> *reformulation*, not the intrinsics, was most of it) — all **byte-identical** — and finally an
> **energy-bucket class shortlist** (trial the RD-likely residue classes, not all ten), the one
> lever that changes the bitstream and so is gated **perceptually**: **PEAQ-neutral** (ΔODG ≤
> 0.03 vs the exhaustive search, on a CC0/PD music corpus) for a further **~1.5×**. Together they
> closed single-thread from **4.7× → ~1.4×** behind libvorbis; the parallel win is one FFmpeg's
> single-threaded encoder can't answer. `--no-default-features` stays a 100%-safe scalar build.

> **⚡ Performance spotlight — Opus encode: our fork beats `libopus` on wall-clock.** Opus
> uses our own **[rusty-opus](https://github.com/Remade-With-Rust/rusty-opus)** — a
> performance fork (BSD-3) of the pure-Rust `opus-rs`. The upstream had scalar x86 for the
> SILK noise-shaping path, so we wrote **three byte-identical AVX2 kernels** — the LPC
> short-prediction, the warped-autocorrelation correlation, and (the flagship) the
> **cross-state NSQ shaping filter**, where the 4 delayed-decision states run as **i64 lanes**
> of one register and the state is held in a **persistent SoA** transposed once per subframe
> (an isolated micro-benchmark found this after a naïve first attempt went *slower* — the
> lesson being that cross-lane SIMD wins on serial recurrences and *loses* on branchy
> straight-line code, which we proved by measuring the RD kernel at **5× slower** and
> shipping only the winners). Those make single-thread SILK **~30% faster than the upstream
> fork, byte-identical**. Then the structural move, as with AAC and Vorbis: **frame-parallel
> encoding** — chunk the stream and let each worker **prime** its inter-frame state (SILK LTP,
> CELT overlap) by re-encoding a few look-back frames, so chunk seams are **PEAQ-neutral**
> (ΔODG ≤ 0.03). Since `libopus` is single-threaded per stream, this puts **`rff -c:a opus` at
> ~3× `ffmpeg -c:a libopus` wall-clock** (and ~8× the opus-rs we forked) on speech, decoding
> at unity. Per-thread we're still ~2.9× behind `libopus`'s hand-written assembly NSQ — the
> honest single-thread gap — but the wall-clock race is one a single-threaded encoder can't win.

> **🎯 Quality & robustness vs `libopus` (measured head-to-head).** An 8-point
> content×rate matrix (speech/music × mono/stereo × 16–128 kbps, complexity 9, CBR),
> scored on **PEAQ ODG** with a **reconstruction-SNR guard** — the discipline that keeps
> us honest: a sub-0.1 ODG "loss" whose SNR is at parity is metric noise on
> differently-shaped-but-equal-fidelity output, not a real deficit. Result: **parity-or-better
> across the board.** We **win outright on stereo music** (**+0.25 ODG @96k, +0.6 @64k** —
> our CELT stereo shapes quantization noise more favorably), win or tie on mono speech/music,
> and have **one genuine deficit**: near-mono *stereo speech* at 64k+ (−0.18 ODG / −6 dB SNR,
> a diffuse stereo-coding-efficiency gap with no isolated lever — theta split, intensity, and
> dynalloc boosts were all ceiling-probed and refuted), plus a borderline 128k-stereo edge.
> On **speed** the same matrix (vs the `opus_demo` reference build) has us **encode-faster on
> every clip** and **decode ~20% slower** — but that's against a stock `-O2` reference, *not*
> ffmpeg's asm-tuned `libopus`, so the honest per-thread encode gap is still the ~2.9× above.
>
> **Streaming robustness is now feature-complete** (all ports of / equivalent to `libopus`,
> conformance untouched): packet-loss concealment for **both SILK** (LTP/LPC extrapolation +
> comfort noise) **and CELT** (`celt_decode_lost` — pitch-based repetition for short tonal
> losses, **+1.18 ODG** over the noise-based fallback; noise-based CNG-style fill for long
> bursts), **in-band FEC** (LBRR recovery), **DTX**, **comfort-noise generation** (`silk_CNG`
> — DTX/silence renders as natural background, not dead air), **multistream/surround**
> (5.1/7.1, libopus decodes our output zero-error), and a **repacketizer**. The **decoder is
> bit-exact on all 12 official RFC 6716/8251 vectors** and decodes `libopus`'s own streams to
> identical output. An optional **faithful float-SILK analysis path** (port of `silk/float/`)
> ships default-off.

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
| Video codec | **vp9** (VP9) | **decode + encode** — in-house pure-Rust. Decoder **bit-exact against all 315 official libvpx conformance vectors** (profiles 0–3, 8/10/12-bit, AVX2 + NEON). Encoder: RDO partition/mode, rate control (CBR + two-pass), golden/ALT-REF + temporal filtering, **validated pixel-exact vs libvpx & ffmpeg** (~+0.9% keyframe BD-rate; younger than libvpx, optimizing) |
| Video codec | **h264** (H.264 / AVC) | **decode + encode** — [`rusty_h264`](https://crates.io/crates/rusty_h264) with SIMD asm, **default** |
| Video codec | **AV1** (AV1) | **decode + encode** — the royalty-free next-gen codec, **100% pure Rust, no C/FFI**. Our [rusty-av1-toolkit](https://github.com/Remade-With-Rust/rusty-av1-toolkit) (`rusty_av1d` / `rusty_av1e`, BSD-2) forks **rav1d** (Rust port of VideoLAN's **dav1d**, the world's fastest AV1 decoder) + **rav1e** (the reference pure-Rust AV1 encoder), with a no-`nasm`, no-asm pure-Rust build path. Our encoder fork runs **~1.10× faster than stock rav1e at byte-identical output**, or up to **~1.69× faster** in opt-in `--racecar` mode |
| Image codec | **avif** (AV1 still image) | **decode + encode**, 8- & 10-bit (`rusty_av1d` / `rusty_av1e`) |
| Image codec | **png** (RGB/RGBA) | **decode + encode** (pure-Rust `png`) |
| Image codec | **mjpeg** (JPEG/MJPEG) | **decode + encode** (pure-Rust `jpeg-decoder`/`jpeg-encoder`) |
| Image codec | **gif** | **decode + encode** (pure-Rust `gif`; first frame) |
| Image codec | **webp** (VP8/VP8L) | **decode + lossless encode** (pure-Rust `image-webp`) |
| Image codec | **jpegxl** (JPEG XL) | **decode** (pure-Rust `jxl-oxide`; no Rust encoder yet) |
| Audio codec | **aac** | in-house **AAC-LC decoder + encoder** — decoder has all features (short blocks, M/S, intensity stereo, PNS, TNS), bit-exact vs FFmpeg; **encoder** (7 bricks) adds a psychoacoustic model (Bark-scale masking), bitrate rate-control, transient block switching, M/S stereo, and MP4 `esds` — **ffmpeg decodes our output at unity**; **~450× realtime** encode — **~6× faster than ffmpeg's own AAC** — via frame-parallel encoding (ffmpeg's AAC is single-threaded), an N/4-point-FFT MDCT, a two-phase rate loop, cached psychoacoustic tables, and AVX2 (+ opt-in AVX-512) quantize kernels. Single-thread it still edges ffmpeg (~1.15×) |
| Audio codec | **mp3** (MPEG-1/2 Layer III) | in-house **decoder + encoder** (`rff-codec-mp3`) — decoder **bit-exact vs FFmpeg**; encoder MPEG-1/2/2.5, CBR + VBR, joint stereo, block switching |
| Audio codec | **opus** | **decode + encode** — our own **[rusty-opus](https://github.com/Remade-With-Rust/rusty-opus)** (BSD-3 performance fork of the pure-Rust `opus-rs`). Three **byte-identical AVX2 SILK kernels** (LPC short-prediction, warped-autocorrelation, cross-state NSQ shaping filter) make single-thread SILK **~30% faster than the upstream fork**; **frame-parallel encoding** (chunked + state-primed, **PEAQ-neutral** ΔODG ≤ 0.03) makes `rff -c:a opus` **~3× faster than ffmpeg's `libopus` wall-clock** (libopus is single-threaded per stream, ~8× faster than the opus-rs we forked) — **ffmpeg decodes our output at unity**. Knobs: `-b:a`, `-compression_level`, `-opus_parallel` |
| Audio codec | **vorbis** | **decode + encode** — decode via pure-Rust `lewton`; **in-house encoder** — *the first permissively-licensed Vorbis encoder in Rust* (none existed before). Window → **N/4-FFT MDCT** → Bark-scale masking floor → channel coupling + point stereo → rate-distortion residue VQ, emitting an embedded libvorbis setup header; `-q:a 0–9`. **ffmpeg decodes our output**, validated packet-exact against `lewton` + libvorbis. **~5.3× faster than libvorbis wall-clock** (stereo music, 24 cores) via **frame-parallel** encoding (libvorbis is single-threaded) over a structure-of-arrays + AVX2 residue-VQ search and an **energy-bucket class shortlist** (PEAQ-validated perceptually neutral, ΔODG ≤ 0.03); per-thread ~1.4× behind libvorbis (was 4.7×) |
| Audio codec | **flac** | **decode + encode** — decode via pure-Rust `claxon`; **in-house lossless encoder** (LPC + stereo decorrelation + partitioned Rice + MD5), **at parity with ffmpeg's FLAC** |
| Audio codec | **pcm** (s16le / f32le) | **decode + encode** (in-house) |
| Container | **avif** (AV1 Image File Format) | **demux + mux** (reads foreign AVIFs too) |
| Container | **png** / **jpeg** / **gif** / **webp** / **jpegxl** | **demux + mux** |
| Container | **wav** (RIFF/WAVE) / **ogg** (Opus/Vorbis) / **flac** | **demux + mux** |
| Container | **avi** (Audio Video Interleaved) | **demux + mux** (RIFF/`hdrl`/`movi`/`idx1`) |
| Container | **mp4** / **mov** (ISOBMFF) | **demux + mux** — sample tables; **A/V**: AV1 (`av01`/`av1C`) or H.264 (`avc1`/`avcC`) video + Opus audio (`dOps`); **AAC `esds` config (demux + mux)** so `rff -i in.wav out.m4a` writes a playable AAC MP4 |
| Container | **matroska** / **webm** (EBML) | **demux** — track tree + Cluster/(Simple)Block packets; AV1/H.264 video + Opus/Vorbis/AAC/FLAC audio |

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
| AV1 encode (avif) | [`rusty_av1e`](https://github.com/Remade-With-Rust/rusty-av1-toolkit) | BSD-2-Clause | ✅ (our rav1e fork; pure-Rust, no asm) |
| AV1 decode (avif) | [`rusty_av1d`](https://github.com/Remade-With-Rust/rusty-av1-toolkit) | BSD-2-Clause | ✅ (our rav1d fork; Rust port of dav1d) |
| H.264 decode/encode | [`rusty_h264`](https://crates.io/crates/rusty_h264) | BSD-2-Clause | ✅ (vendored asm, no C; default needs `nasm`) |
| PNG encode/decode | [`png`](https://crates.io/crates/png) | MIT/Apache-2.0 | ✅ |
| JPEG decode | [`jpeg-decoder`](https://crates.io/crates/jpeg-decoder) | MIT/Apache-2.0 | ✅ |
| JPEG encode | [`jpeg-encoder`](https://crates.io/crates/jpeg-encoder) | MIT/Apache-2.0 AND IJG | ✅ |
| GIF encode/decode | [`gif`](https://crates.io/crates/gif) | MIT/Apache-2.0 | ✅ |
| WebP encode/decode | [`image-webp`](https://crates.io/crates/image-webp) | MIT/Apache-2.0 | ✅ |
| Opus encode/decode | [`rusty-opus`](https://github.com/Remade-With-Rust/rusty-opus) (our `opus-rs` fork) | BSD-3-Clause | ✅ (AVX2 SILK + frame-parallel; pure Rust, no C/FFI) |
| Vorbis decode | [`lewton`](https://crates.io/crates/lewton) | MIT/Apache-2.0 | ✅ |
| Vorbis encode | **in-house** (`rff-codec-vorbis`) | Apache-2.0 | ✅ (first permissive Rust Vorbis encoder) |
| FLAC decode | [`claxon`](https://crates.io/crates/claxon) | Apache-2.0 | ✅ |
| FLAC encode | **in-house** (`rff-codec-flac`) | Apache-2.0 | ✅ (lossless, no dep) |
| JPEG XL decode | [`jxl-oxide`](https://crates.io/crates/jxl-oxide) | MIT/Apache-2.0 | ✅ |

> **🎬 Spotlight — a full AV1 stack, both ways, in pure Rust with no C.** AV1 is the
> next-generation codec that's genuinely *free* — no patent pool, no per-unit royalties,
> unlike H.264/HEVC/VVC — and we ship the **complete pipeline in both directions**: the
> decoder is a Rust port of **dav1d** (VideoLAN's world-fastest AV1 decoder); the encoder
> is **rav1e** (the reference pure-Rust AV1 encoder). Both are forked into our permissively
> licensed BSD-2 [rusty-av1-toolkit](https://github.com/Remade-With-Rust/rusty-av1-toolkit),
> and — unlike a `libaom`/`libdav1d` C binding — a **pure-Rust build path** (no `nasm`, no C
> toolchain, zero FFI), with the decode side adding **zero `unsafe`** to this tree. One AV1
> core powers **both video and AVIF stills** (8- &
> 10-bit): a frame decodes → re-encodes → rewraps through the `demux → decode → encode → mux`
> loop, so `ffmpeg -i in.avif -c:v avif out.avif` and `ffmpeg -i v -i a -c:v avif -c:a opus
> out.mp4` (AV1 video + Opus in one MP4) work today.
>
> **And these aren't just repackaged upstreams — the fork is faster.** Our `rusty_av1e`
> encodes **~1.10× faster than stock rav1e while emitting its byte-identical bitstream**
> (whole-encode wall-clock, real CLI A/B on one machine), with an opt-in `--racecar` mode
> that trades bit-exactness for **~1.69× faster** (pair with `--tune Psnr`) — and the
> `rusty_av1d` decoder doubles as the encoder's own conformance oracle, so every speedup is
> checked against a safe-Rust dav1d port. **AV2 decode is already in progress** — we're onto
> the codec *after* next.

"Scaffolded" = registered and wired through the engine, CLI and server; the
bitstream body is the next implementation step. More codecs/containers to come.

## Install

```sh
# From source — needs `nasm` for the default H.264 SIMD path (see Building from
# source for the no-nasm alternative). Add `--features https` for https:// input.
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

# Encode audio with an in-house, pure-Rust encoder — WAV → AAC in an MP4
# (psychoacoustic model, transient block switching, M/S stereo, esds config):
ffmpeg -i input.wav -c:a aac -b:a 128k -y output.m4a

# …or FLAC (lossless, at ffmpeg parity), MP3, Vorbis, or Opus — same engine:
ffmpeg -i input.wav -c:a flac -y output.flac
ffmpeg -i input.wav -c:a vorbis -q:a 4 -y output.ogg
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

> **⚠ Build prerequisite — `nasm`.** The **default** build enables `h264-asm`
> (rusty_h264's hand-written SIMD kernels), which assembles with
> [`nasm`](https://nasm.us). **Without `nasm` on your `PATH`, `cargo build`
> fails.** Either install it first — `winget install NASM` (Windows) /
> `brew install nasm` (macOS) / `apt install nasm` (Debian/Ubuntu) — **or** skip
> the assembly entirely with `--no-default-features` for the pure-Rust scalar
> H.264 path (no `nasm` needed).

```sh
git clone https://github.com/Remade-With-Rust/remade_ffmpeg_rs
cd remade_ffmpeg_rs
cargo build                          # default: needs nasm (h264-asm)
cargo build --no-default-features    # pure-Rust scalar H.264 — no nasm
cargo build --features https         # add rustls TLS for https:// input
cargo run -p rff-ui                  # build/run the Dioxus desktop UI on demand
```

**Requirements:** Rust 1.85+ (stable), plus **`nasm`** for the default
(`h264-asm`) build — see the callout above. The Dioxus UI additionally needs a
system webview (WebView2 on Windows, WebKitGTK on Linux) and, for web/mobile
targets, the `dx` CLI (`cargo install dioxus-cli`).

## Platform support

| Platform | Status |
|---|---|
| Windows / macOS / Linux (CLI + server) | ✅ builds |
| Web (WASM) / PWA / mobile (Dioxus UI) | 🚧 scaffolded |

Adding a codec or container backend is a first-class extension point —
implement the `Decoder`/`Encoder` or `Demuxer`/`Muxer` traits and call
`register(...)`, no engine-core changes required.

## Roadmap

Prioritized **next-gen first** — full detail in [docs/roadmap.md](docs/roadmap.md).
What's shipped today is the [compatibility matrix](docs/compatibility.md).

- **Next-gen (priority):** AV2 decode *(in progress)* · fMP4/CMAF segments ·
  low-latency live (SRT / WebRTC / Media-over-QUIC) · IAMF spatial audio.
- **Current-modern:** DASH output · HLS completion (`-hls_time`, live playlists) ·
  `filter_complex` `concat` · two-pass execution · HTTPS in the default build.
- **Patent-gated (gate or skip):** HEVC/H.265 · VVC/H.266 · AC-3 — standard-
  essential-patent encumbered, unlike the royalty-free AV1/AV2 stack.

Also tracked: reproducible benchmark suite + published numbers, and real MATA mID
verification (`sovereign-id-verify`).

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
**AAC** (our in-house AAC-LC *decoder and encoder* — the largely-expired,
lowest-risk corner; no HE-AAC). We take the same posture as FFmpeg: these ship in the
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
