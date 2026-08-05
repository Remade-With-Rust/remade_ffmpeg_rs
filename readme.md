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
> coverage are still moving, but nothing will ship that is not byte exact. See the
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
| PNG decode | 1.0× | **2.67–2.95× faster** — 1 core each, identical work, 15/15 paired wins | maintain |
| PNG encode | 1.0× | **0.69–0.92× per core** at matched filter *and* size — behind, and we say so · **2.11–3.06× faster** wall-clock (block-parallel), at 0.1–0.2% *smaller* | → per-core parity |
| License + embedding | LGPL/GPL · C FFI | **Apache-2.0 · pure Rust · no FFI** | — |

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

- **Drop-in CLI.** `rff` and `rffprobe` binaries that speak the flags you
  already know (`-i`, `-c:v`, `-c:a`, `-b:v`, `-f`, `-y`, `-codecs`, ...), and
  install as `ffmpeg`/`ffprobe` on request (`--features drop-in-names`).
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

| Codec | Backing crate | License | Pure Rust |
|---|---|---|---|
| AV1 encode (avif) | [`rusty_av1e`](https://github.com/Remade-With-Rust/rusty-av1-toolkit) | BSD-2-Clause | ✅ (our rav1e fork; pure-Rust, no asm) |
| AV1 decode (avif) | [`rusty_av1d`](https://github.com/Remade-With-Rust/rusty-av1-toolkit) | BSD-2-Clause | ✅ (our rav1d fork; Rust port of dav1d) |
| H.264 decode/encode | [`rusty_h264`](https://crates.io/crates/rusty_h264) | BSD-2-Clause | ✅ (vendored asm, no C; default needs `nasm`) |
| VP9 decode/encode | **in-house** ([`rusty_vp9`](https://crates.io/crates/rusty_vp9)) | Apache-2.0 | ✅ (bit-exact vs all 315 libvpx vectors; standalone crate) |
| AAC decode/encode | **in-house** ([`rusty_aac`](https://crates.io/crates/rusty_aac)) | Apache-2.0 | ✅ (AAC-LC; frame-parallel encoder; standalone crate) |
| MP3 decode/encode | **in-house** ([`rusty_mp3`](https://crates.io/crates/rusty_mp3)) | Apache-2.0 | ✅ (decoder bit-exact vs FFmpeg; standalone crate) |
| PNG encode/decode | **in-house** ([`rusty_png`](https://crates.io/crates/rusty_png)) | MIT OR Apache-2.0 | ✅ (performance fork of `image-rs/image-png`; pure-Rust `zlib-rs` DEFLATE, parallel encode; standalone crate) |
| JPEG decode/encode | **in-house** ([`rusty_jpeg`](https://crates.io/crates/rusty_jpeg)) | (MIT OR Apache-2.0) AND IJG | ✅ (vendored merge of `jpeg-decoder` + `jpeg-encoder`; baseline + progressive; standalone crate) |
| GIF encode/decode | [`gif`](https://crates.io/crates/gif) | MIT/Apache-2.0 | ✅ |
| WebP encode/decode | [`image-webp`](https://crates.io/crates/image-webp) | MIT/Apache-2.0 | ✅ |
| Opus encode/decode | [`rusty-opus`](https://crates.io/crates/rusty-opus) (our `opus-rs` fork) | BSD-3-Clause | ✅ (AVX2 SILK + frame-parallel; pure Rust, no C/FFI) |
| Vorbis decode | [`lewton`](https://crates.io/crates/lewton) | MIT/Apache-2.0 | ✅ |
| Vorbis encode | **in-house** ([`rusty_vorbis`](https://crates.io/crates/rusty_vorbis)) | Apache-2.0 | ✅ (first permissive Rust Vorbis encoder; standalone crate) |
| FLAC decode | [`claxon`](https://crates.io/crates/claxon) | Apache-2.0 | ✅ |
| FLAC encode | **in-house** (`rff-codec-flac`) | Apache-2.0 | ✅ (lossless, no dep) |
| JPEG XL decode | [`jxl-oxide`](https://crates.io/crates/jxl-oxide) | MIT/Apache-2.0 | ✅ |

**Codec backends — every one is 100% Rust (no C/C++ FFI) and permissively licensed.** Container (de)muxers are our own code. See [docs/pure-rust-codecs.md](docs/pure-rust-codecs.md) for the full vetted survey (what's clean, what's license-blocked, what has no pure-Rust option).

| Kind | Supported | Status |
|---|---|---|
| Video codec | **vp9** (VP9) | **decode + encode** — in-house pure-Rust (**`rusty_vp9`**, also usable standalone). Decoder **bit-exact against all 315 official libvpx conformance vectors** (profiles 0–3, 8/10/12-bit, AVX2 + NEON). Encoder: RDO partition/mode, rate control (CBR + two-pass), golden/ALT-REF + temporal filtering, compound (bi-directional) prediction, **validated pixel-exact vs libvpx & ffmpeg**. Read at a **matched operating point** — both encoders in constant-quality mode at the same cq ladder, speed preset and lookahead, compared at equal PSNR — it needs **~+36% bitrate** and runs **~3× slower** than libvpx on the Derf CIF set. (Keyframe-only BD-rate is ~+0.9%: the gap is the **inter** path, not residual coding.) Younger than libvpx, actively optimizing |
| Video codec | **h264** (H.264 / AVC) | **decode + encode** — in-house pure-Rust (**[`rusty_h264`](https://crates.io/crates/rusty_h264)**, also usable standalone), **default**. Codec core is `#![forbid(unsafe_code)]`; `unsafe`/asm is confined to one acceleration crate. Decoder **bit-exact vs Cisco's `h264dec`** across openh264's conformance corpus (35/35 clean streams) and **pixel-exact vs FFmpeg** on the CABAC paths; encoder decodes **bit-exactly under FFmpeg across QP 0–51**. Decode speed measured on **x264-encoded** 720p (our own encoder's output understates decode cost — its fast preset emits no sub-pel motion, skipping the whole interpolation path), one pinned core, CPU time, ABBA-alternated, 9 pairs, **9/9 with z = 3.00**: **2.34× / 2.70× / 2.49× behind FFmpeg's native `h264`** at x264 `veryfast` (baseline/CAVLC) / `medium` (main/CABAC) / `slower` (high) — i.e. **150 / 107 / 85 Mpx/s** vs 314 / 289 / 239. Younger than FFmpeg's h264, actively optimizing |
| Video codec | **AV1** (AV1) | **decode + encode** — the royalty-free next-gen codec, **100% pure Rust, no C/FFI**. Our [rusty-av1-toolkit](https://github.com/Remade-With-Rust/rusty-av1-toolkit) (`rusty_av1d` / `rusty_av1e`, BSD-2) forks **rav1d** (Rust port of VideoLAN's **dav1d**, the world's fastest AV1 decoder) + **rav1e** (the reference pure-Rust AV1 encoder), with a no-`nasm`, no-asm pure-Rust build path. Our encoder fork runs **~1.10× faster than stock rav1e at byte-identical output**, or up to **~1.69× faster** in opt-in `--racecar` mode |
| Image codec | **avif** (AV1 still image) | **decode + encode**, 8- & 10-bit (`rusty_av1d` / `rusty_av1e`) |
| Image codec | **png** | **decode + encode** — in-house pure-Rust (**[`rusty_png`](https://crates.io/crates/rusty_png)**, also usable standalone). A performance fork of `image-rs/image-png`, carried forward in-tree, gated **byte-identical to upstream across 600 comparisons** (20 images × 30 configurations, encode bytes *and* decoded pixels) before anything changed. Full colour-type and bit-depth coverage, APNG, interlacing. **DEFLATE via `zlib-rs`** — flate2's pure-Rust zlib rewrite, so no C enters the tree — which measured **1.68–2.72× faster** than the stock `miniz_oxide` backend at size parity. **Multi-threaded DEFLATE for a single image** (pigz-style block splitting; ffmpeg's PNG encoder is single-threaded per image): **2.11–3.06× faster end-to-end at 0.1–0.2% smaller**, with blocks *sized* not counted so an image too small to split stays serial and pays **+0.00%**. Encoder settings **dispatch on content** — a measured repeated-pixel signal that separates photographs (0.0366–0.2037) from graphics (0.5312–0.9790) with nothing between — plus automatic gray/palette narrowing, together taking nine real screenshots/charts/diagrams from **+115.6% to −6.1%** against ffmpeg's default while leaving photographs byte-identical. 16-bit input reduces to 8-bit **matching ffmpeg exactly** (100.00% agreement over 6.2 M samples, against 66.01% for the truncation it replaced). `-compression_level`, `-pred`, `-threads`. **Decode 2.67–2.95× faster** than ffmpeg (6 real images, 15/15 paired wins each, z = 3.87); per-core encode at matched filter *and* matched size is **0.69–0.92×** — that residual gap is `zlib-rs` vs zlib, not PNG, since the profiler puts DEFLATE at 94–99.5% of encode. 175/175 pngsuite files and 2,100 randomly-mutated inputs: **zero panics** |
| Image codec | **mjpeg** (JPEG/MJPEG) | **decode + encode** — in-house pure-Rust (**[`rusty_jpeg`](https://crates.io/crates/rusty_jpeg)**, also usable standalone). A vendored merge of `jpeg-decoder` + `jpeg-encoder`, carried forward as one codec so the encoder is gated against the decoder as a round-trip oracle. Baseline **and** progressive DCT, planar YUV in/out (no needless upsample→resample round trip), real `-q:v` / sampling / `-optimize_huffman` control. **Trellis quantization** (`-trellis 1`, opt-in — RD-optimal EOB placement plus coefficient-magnitude lowering, **−2.51% mean BD-rate** on synthetic content; off by default because it costs **+144% encode time** on real footage) and **box-averaged chroma downsampling** (**−17.12% BD-rate** on chroma detail, **+2.30 dB** on saturated chroma edges — the old path decimated, aliasing chroma no bitrate could recover). Fuzz-tested: 80,000 malformed inputs, zero panics. Measured on one pinned core vs FFmpeg 8.1.2, at **matched output size** for encode and on a **byte-identical bitstream** for decode: **encode 1.45× faster** (40-frame 1080p clip, fixed Huffman tables both sides — FFmpeg's MJPEG has no optimized-table mode; our own `-optimize_huffman` default trades ~2x speed for 7.4% smaller files), **decode ~1.04× faster** (paired ABBA, N=15, 3000-frame arms, median 1.0390). AVX2 FDCT+quantize, an AVX2 IDCT doing **two 8×8 blocks per instruction stream** (byte-identical to SSSE3, +7.8% whole-decode), entropy decode **fused into the IDCT** so a baseline scan never buffers an MCU row of coefficients (deletes an ~18.8 MB/frame write→zero→read round trip), DC-only block shortcut, and rayon **removed** after it measured slower at *every* image size (1.32× / **1.91×** / 1.32× at 480p / 1080p / 4K — fork-join costs more than intra-frame parallelism buys) |
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

## Install

```sh
# Needs `nasm` for the default H.264 SIMD path (see Building from source for the
# no-nasm alternative). Add `--features https` for https:// input.
cargo install rff-cli
```

This installs the **`rff`** and **`rffprobe`** binaries.

Want the drop-in `ffmpeg`/`ffprobe` names, so existing scripts work unchanged?

```sh
cargo install rff-cli --features drop-in-names
```

That's opt-in on purpose: those names shadow a real FFmpeg on your `PATH`, which
should be your explicit choice rather than a side effect of installing. The
prebuilt archives on
[Releases](https://github.com/Remade-With-Rust/remade_ffmpeg_rs/releases) ship
both name pairs.

To embed the engine in your own application, take the library instead:

```sh
cargo add rff
```

From a source checkout: `cargo install --path crates/rff-cli`.

## Quick start

```sh
# List what this build supports — just like FFmpeg:
rff -codecs
rff -formats

# Inspect a file:
rffprobe input.avif

# Transcode AVIF → AVIF end to end (decode AV1, re-encode AV1, rewrap):
rff -i input.avif -c:v avif -y output.avif

# Encode audio with an in-house, pure-Rust encoder — WAV → AAC in an MP4
# (psychoacoustic model, transient block switching, M/S stereo, esds config):
rff -i input.wav -c:a aac -b:a 128k -y output.m4a

# …or FLAC (lossless, at ffmpeg parity), MP3, Vorbis, or Opus — same engine:
rff -i input.wav -c:a flac -y output.flac
rff -i input.wav -c:a vorbis -q:a 4 -y output.ogg
```

<sub>Installed with `--features drop-in-names` (or taken from a release archive),
the same commands work as `ffmpeg` / `ffprobe`.</sub>

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
