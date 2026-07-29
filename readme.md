# remade_ffmpeg_rs

[![Remade With Rust](https://img.shields.io/badge/Remade%20With-Rust-000?logo=rust&logoColor=fff)](https://github.com/remade-with-rust)
[![By Mata Network](https://img.shields.io/badge/by-Mata%20Network-5b2be0)](https://www.mata.network)
[![License: Apache-2.0](https://img.shields.io/badge/license-Apache--2.0-blue)](LICENSE)
![Platforms: Windows · macOS · Linux · Web](https://img.shields.io/badge/platforms-Windows%20%C2%B7%20macOS%20%C2%B7%20Linux%20%C2%B7%20Web-informational)

> **remade_ffmpeg_rs** is a memory-safe media toolkit — decode, encode,
> transcode, mux and probe audio/video — a ground-up **Rust** rebuild of
> [FFmpeg](https://github.com/FFmpeg/FFmpeg) (LGPL-2.1+/GPL-2.0+/C), under a
> permissive license, built for speed, safety, and zero copyleft strings.
> Check out [FFAI](https://github.com/remade-with-rust/ffai), a sister project providing media for AI first world.

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
| VP9 encode, 1 thread | 1.0× | **~0.3×**, at **~+36% bitrate** — matched settings, equal PSNR | → parity |
| AAC encode (60 s stereo) | 1.0× | **~6× faster** — frame-parallel (ffmpeg's AAC is 1-thread); ~1.15× single-thread | maintain |
| Vorbis encode (stereo music) | 1.0× | **~5.3× faster** — frame-parallel; the **first permissive-Rust Vorbis encoder** | → single-thread |
| Opus encode (`libopus`) | 1.0× | **1.0–1.5× faster single-thread** (fair, 1 core each; speech + music) · **2–4× faster** wall-clock (frame-parallel) | maintain |
| License + embedding | LGPL/GPL · C FFI | **Apache-2.0 · pure Rust · no FFI** | — |

<sub>Real numbers + how to reproduce them: [docs/benchmarks.md](docs/benchmarks.md). The VP9 decode figure is throughput on an i7-14650HX vs FFmpeg's native decoder. The VP9 encode figures are against `libvpx-vp9` at a **matched operating point** — both encoders in constant-quality mode at the same cq ladder, speed preset and lookahead, then read at **equal PSNR** — on the Derf CIF set (`video-tests/`, reproduce with `analyzer pareto`). Comparing default-to-default instead is meaningless here: the two default configurations differ in rate-control mode, operating point, speed preset and lookahead at once, which on 1080p put the two arms **72× apart in bitrate**.</sub>

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

> **⚡ Performance spotlight — Opus encode: faster than `libopus` per core, on speech *and*
> music.** Opus uses our own **[rusty-opus](https://github.com/Remade-With-Rust/rusty-opus)** —
> a pure-Rust fork of `opus-rs` with **three byte-identical AVX2 SILK kernels** (LPC
> short-prediction, warped-autocorrelation, and the flagship cross-state NSQ shaping filter,
> whose 4 delayed-decision states run as **i64 lanes** of one register over a **persistent SoA**
> transposed once per subframe). But the biggest recent win was **structural, and the profiler
> found it**: a full-transcode profile showed the *codec* was fast (~240× realtime) while the
> encoder **wrapper burned ~5× the codec's own time** — it buffered the whole stream, then
> pulled each 20 ms frame off the **front** of that buffer, an **O(n²)** memmove per frame. A
> cursor-and-single-drain fix cut single-thread encode **4.7×** (full transcode 3.4×) and
> flipped us from *behind* `libopus` to **ahead** of it.
>
> **Fresh head-to-head, single-thread — both encoders on one core (the fair codec comparison),**
> full-CLI wall-clock, best-of-7, real synthesized speech (SILK/Hybrid) + music (CELT):
>
> | config | ours · 1-thread | `libopus` · 1-thread | ours · frame-parallel |
> |---|---:|---:|---:|
> | 8 kHz VoIP @16k · speech | **0.116s (1.06×)** | 0.123s | 0.054s |
> | 16 kHz VoIP @24k · speech | **0.168s (1.07×)** | 0.179s | 0.068s |
> | 48 kHz Hybrid @32k · speech | **0.046s (1.39×)** | 0.064s | 0.047s |
> | 48 kHz stereo Audio @128k · music | **0.175s (1.46×)** | 0.255s | 0.074s |
> | 44.1 kHz stereo Audio @128k · music | **0.196s (1.33×)** | 0.260s | 0.100s |

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
| Video codec | **vp9** (VP9) | **decode + encode** — in-house pure-Rust (**`rusty_vp9`**, also usable standalone). Decoder **bit-exact against all 315 official libvpx conformance vectors** (profiles 0–3, 8/10/12-bit, AVX2 + NEON). Encoder: RDO partition/mode, rate control (CBR + two-pass), golden/ALT-REF + temporal filtering, compound (bi-directional) prediction, **validated pixel-exact vs libvpx & ffmpeg**. Read at a **matched operating point** — both encoders in constant-quality mode at the same cq ladder, speed preset and lookahead, compared at equal PSNR — it needs **~+36% bitrate** and runs **~3× slower** than libvpx on the Derf CIF set. (Keyframe-only BD-rate is ~+0.9%: the gap is the **inter** path, not residual coding.) Younger than libvpx, actively optimizing |
| Video codec | **h264** (H.264 / AVC) | **decode + encode** — [`rusty_h264`](https://crates.io/crates/rusty_h264) with SIMD asm, **default** |
| Video codec | **AV1** (AV1) | **decode + encode** — the royalty-free next-gen codec, **100% pure Rust, no C/FFI**. Our [rusty-av1-toolkit](https://github.com/Remade-With-Rust/rusty-av1-toolkit) (`rusty_av1d` / `rusty_av1e`, BSD-2) forks **rav1d** (Rust port of VideoLAN's **dav1d**, the world's fastest AV1 decoder) + **rav1e** (the reference pure-Rust AV1 encoder), with a no-`nasm`, no-asm pure-Rust build path. Our encoder fork runs **~1.10× faster than stock rav1e at byte-identical output**, or up to **~1.69× faster** in opt-in `--racecar` mode |
| Image codec | **avif** (AV1 still image) | **decode + encode**, 8- & 10-bit (`rusty_av1d` / `rusty_av1e`) |
| Image codec | **png** (RGB/RGBA) | **decode + encode** (pure-Rust `png`) |
| Image codec | **mjpeg** (JPEG/MJPEG) | **decode + encode** (pure-Rust `jpeg-decoder`/`jpeg-encoder`) |
| Image codec | **gif** | **decode + encode** (pure-Rust `gif`; first frame) |
| Image codec | **webp** (VP8/VP8L) | **decode + lossless encode** (pure-Rust `image-webp`) |
| Image codec | **jpegxl** (JPEG XL) | **decode** (pure-Rust `jxl-oxide`; no Rust encoder yet) |
| Audio codec | **aac** | in-house **AAC-LC decoder + encoder** (**`rusty_aac`**, also usable standalone) — decoder has all features (short blocks, M/S, intensity stereo, PNS, TNS), bit-exact vs FFmpeg; **encoder** (7 bricks) adds a psychoacoustic model (Bark-scale masking), bitrate rate-control, transient block switching, M/S stereo, and MP4 `esds` — **ffmpeg decodes our output at unity**; **~450× realtime** encode — **~6× faster than ffmpeg's own AAC** — via frame-parallel encoding (ffmpeg's AAC is single-threaded), an N/4-point-FFT MDCT, a two-phase rate loop, cached psychoacoustic tables, and AVX2 (+ opt-in AVX-512) quantize kernels. Single-thread it still edges ffmpeg (~1.15×) |
| Audio codec | **mp3** (MPEG-1/2 Layer III) | in-house **decoder + encoder** (**`rusty_mp3`**, also usable standalone) — decoder **bit-exact vs FFmpeg**; encoder MPEG-1/2/2.5, CBR + VBR, joint stereo, block switching, bit reservoir |
| Audio codec | **opus** | **decode + encode** — our own **[rusty-opus](https://github.com/Remade-With-Rust/rusty-opus)** (BSD-3 performance fork of the pure-Rust `opus-rs`). Three **byte-identical AVX2 SILK kernels** + an O(n²)-copy fix in the encode wrapper make `rff -c:a opus` **1.0–1.5× faster than `libopus` per core** (fair, 1 thread each, speech + music); **frame-parallel encoding** (chunked + state-primed, **PEAQ-neutral** ΔODG ≤ 0.03) takes wall-clock to **2–4× faster** (libopus is single-threaded per stream) — **ffmpeg decodes our output at unity**. Knobs: `-b:a`, `-compression_level`, `-opus_parallel` |
| Audio codec | **vorbis** | **decode + encode** — decode via pure-Rust `lewton`; **in-house encoder** (**[`rusty_vorbis`](https://crates.io/crates/rusty_vorbis)**, also usable standalone) — *the first permissively-licensed Vorbis encoder in Rust* (none existed before). Window → **N/4-FFT MDCT** → Bark-scale masking floor → channel coupling + point stereo → rate-distortion residue VQ, emitting an embedded libvorbis setup header; `-q:a 0–9`. **ffmpeg decodes our output**, validated packet-exact against `lewton` + libvorbis. **~5.3× faster than libvorbis wall-clock** (stereo music, 24 cores) via **frame-parallel** encoding (libvorbis is single-threaded) over a structure-of-arrays + AVX2 residue-VQ search and an **energy-bucket class shortlist** (PEAQ-validated perceptually neutral, ΔODG ≤ 0.03); per-thread ~1.4× behind libvorbis (was 4.7×) |
| Audio codec | **flac** | **decode + encode** — decode via pure-Rust `claxon`; **in-house lossless encoder** (LPC + stereo decorrelation + partitioned Rice + MD5), **at parity with ffmpeg's FLAC** |
| Audio codec | **pcm** (s16le / f32le) | **decode + encode** (in-house) |
| Container | **avif** (AV1 Image File Format) | **demux + mux** (reads foreign AVIFs too) |
| Container | **png** / **jpeg** / **gif** / **webp** / **jpegxl** | **demux + mux** |
| Container | **wav** (RIFF/WAVE) / **ogg** (Opus/Vorbis) / **flac** | **demux + mux** |
| Container | **avi** (Audio Video Interleaved) | **demux + mux** (RIFF/`hdrl`/`movi`/`idx1`) |
| Container | **mp4** / **mov** (ISOBMFF) | **demux + mux** — sample tables; **A/V**: AV1 (`av01`/`av1C`) or H.264 (`avc1`/`avcC`) video + Opus audio (`dOps`); **AAC `esds` config (demux + mux)** so `rff -i in.wav out.m4a` writes a playable AAC MP4 |
| Container | **matroska** / **webm** (EBML) | **demux** — track tree + Cluster/(Simple)Block packets; AV1/H.264 video + Opus/Vorbis/AAC/FLAC audio |

**Codec backends — every one is 100% Rust (no C/C++ FFI) and permissively licensed.** Container (de)muxers are our own code. See [docs/pure-rust-codecs.md](docs/pure-rust-codecs.md) for the full vetted survey (what's clean, what's license-blocked, what has no pure-Rust option).

| Codec | Backing crate | License | Pure Rust |
|---|---|---|---|
| AV1 encode (avif) | [`rusty_av1e`](https://github.com/Remade-With-Rust/rusty-av1-toolkit) | BSD-2-Clause | ✅ (our rav1e fork; pure-Rust, no asm) |
| AV1 decode (avif) | [`rusty_av1d`](https://github.com/Remade-With-Rust/rusty-av1-toolkit) | BSD-2-Clause | ✅ (our rav1d fork; Rust port of dav1d) |
| H.264 decode/encode | [`rusty_h264`](https://crates.io/crates/rusty_h264) | BSD-2-Clause | ✅ (vendored asm, no C; default needs `nasm`) |
| VP9 decode/encode | **in-house** ([`rusty_vp9`](https://crates.io/crates/rusty_vp9)) | Apache-2.0 | ✅ (bit-exact vs all 315 libvpx vectors; standalone crate) |
| AAC decode/encode | **in-house** ([`rusty_aac`](https://crates.io/crates/rusty_aac)) | Apache-2.0 | ✅ (AAC-LC; frame-parallel encoder; standalone crate) |
| MP3 decode/encode | **in-house** ([`rusty_mp3`](https://crates.io/crates/rusty_mp3)) | Apache-2.0 | ✅ (decoder bit-exact vs FFmpeg; standalone crate) |
| PNG encode/decode | [`png`](https://crates.io/crates/png) | MIT/Apache-2.0 | ✅ |
| JPEG decode | [`jpeg-decoder`](https://crates.io/crates/jpeg-decoder) | MIT/Apache-2.0 | ✅ |
| JPEG encode | [`jpeg-encoder`](https://crates.io/crates/jpeg-encoder) | MIT/Apache-2.0 AND IJG | ✅ |
| GIF encode/decode | [`gif`](https://crates.io/crates/gif) | MIT/Apache-2.0 | ✅ |
| WebP encode/decode | [`image-webp`](https://crates.io/crates/image-webp) | MIT/Apache-2.0 | ✅ |
| Opus encode/decode | [`rusty-opus`](https://crates.io/crates/rusty-opus) (our `opus-rs` fork) | BSD-3-Clause | ✅ (AVX2 SILK + frame-parallel; pure Rust, no C/FFI) |
| Vorbis decode | [`lewton`](https://crates.io/crates/lewton) | MIT/Apache-2.0 | ✅ |
| Vorbis encode | **in-house** ([`rusty_vorbis`](https://crates.io/crates/rusty_vorbis)) | Apache-2.0 | ✅ (first permissive Rust Vorbis encoder; standalone crate) |
| FLAC decode | [`claxon`](https://crates.io/crates/claxon) | Apache-2.0 | ✅ |
| FLAC encode | **in-house** (`rff-codec-flac`) | Apache-2.0 | ✅ (lossless, no dep) |
| JPEG XL decode | [`jxl-oxide`](https://crates.io/crates/jxl-oxide) | MIT/Apache-2.0 | ✅ |

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

- **Next-gen (priority):** AV2 encode and decode *(in progress)* · fMP4/CMAF segments ·
  low-latency live (SRT / WebRTC / Media-over-QUIC) · IAMF spatial audio.
- **Current-modern:** DASH output · HLS completion (`-hls_time`, live playlists) ·
  `filter_complex` `concat` · two-pass execution · HTTPS in the default build.

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
