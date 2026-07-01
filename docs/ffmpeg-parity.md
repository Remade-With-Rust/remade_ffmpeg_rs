# FFmpeg parity & scope

A living map of how `remade_ffmpeg_rs` lines up against FFmpeg, what we
deliberately leave out, and what's in flight. FFmpeg is a *composite* of three
command-line tools and eight libraries; this tracks our 1:1 coverage of that
surface plus the codecs that actually move global traffic.

## Tools & libraries (8 of 11 in place)

| FFmpeg component | Kind | Our equivalent | Status |
|---|---|---|---|
| `ffmpeg` | tool | `rff-cli` → `ffmpeg` binary | ✅ |
| `ffprobe` | tool | `rff-cli` → `ffprobe` binary | ✅ |
| `ffplay` | tool | — | ⛔ **out of scope** — minimal SDL demo player; real playback is `rff-ui` (Dioxus) / `rff-server`, or VLC/mpv/browsers |
| `libavcodec` | lib | `rff-codec` + the codec crates | ✅ core; codec set partial (below) |
| `libavformat` | lib | `rff-format` + 12 container crates | ✅ |
| `libavfilter` | lib | `rff-filter` (scale, crop, pad, hflip, vflip, transpose, format) | ◑ 7 filters |
| `libavutil` | lib | `rff-core` | ✅ |
| `libswscale` | lib | `rff-filter` `scale` + `format` (RGB↔YUV) | ◑ scaling + pixfmt convert |
| `libswresample` | lib | `rff-resample` (windowed-sinc FIR) | ✅ |
| `libavdevice` | lib | — | ⏸ **deferred** — capture/render device I/O (webcam/screen/mic) is platform-heavy and orthogonal to an API-first transcode engine; revisit only if local live-capture becomes a product need |
| `libpostproc` | lib | — | ⛔ **out of scope** — legacy MPEG-1/2/4 deblock/dering (`-vf pp`/`spp`), effectively deprecated; modern deblock/denoise lives in `libavfilter` |

Beyond FFmpeg we also ship `rff-server` (HTTP API), `rff-auth` (sovereign MATA
mID), `rff-ui` (Dioxus cross-platform app), and the `rff` engine facade.

### Scope decisions (recorded)

- **ffplay — won't build.** The UI/server layer is our playback story.
- **libavdevice — deferred** (not at launch). Decision gate: a concrete
  screen/webcam-capture product requirement.
- **libpostproc — won't build.** Legacy/deprecated.

## Top-10 globally-used codecs (7 ✅ · 1 🟡 · 2 ❌)

Ranking is approximate (it shifts by metric — streaming volume vs file count vs
device support) but defensible.

| # | Codec | Type | Status |
|---|---|---|---|
| 1 | **H.264 / AVC** | video | ✅ **decode + encode** — `rusty_h264`, default with SIMD asm (vendored openh264 BSD-2, needs `nasm`); `--no-default-features` for scalar pure Rust |
| 2 | **AAC** | audio | ✅ in-house decoder, bit-exact vs FFmpeg (decode-only) |
| 3 | **H.265 / HEVC** | video | ❌ not yet |
| 4 | **MP3** | audio | ✅ in-house **decoder + encoder** — decode **bit-exact vs FFmpeg**; encode MPEG-1/2/2.5, CBR/VBR, joint stereo, block switching (`rff-codec-mp3`) |
| 5 | **VP9** | video | ✅ in-house **decoder + encoder** — decode 315/315 libvpx conformance; encode **pixel-exact vs libvpx & ffmpeg** (RDO, golden/ALT-REF, two-pass) |
| 6 | **JPEG** | image | ✅ decode + encode |
| 7 | **AV1** | video | ✅ decode + encode (rav1d/rav1e) |
| 8 | **Opus** | audio | ✅ decode + encode |
| 9 | **PNG** | image | ✅ decode + encode |
| 10 | **WebP** | image | ✅ decode + lossless encode |

Beyond the top 10 we also cover GIF (enc+dec), Vorbis (dec), FLAC (dec), PCM
(enc+dec), JPEG XL (dec).

With H.264 now pure-Rust by default, **9 of the top 10** have a pure-Rust
decoder (HEVC is the lone gap).

## In flight

- **AV2** — in-house pure-Rust decoder, forward-looking (successor to AV1; not
  yet deployed globally).
- **MP3 encoder** — psychoacoustic model + two-loop quantizer (the decoder is
  done and bit-exact).

### MP3: why in-house

The Rust MP3 landscape forces a license fork against our "no copyleft in core"
rule (enforced by `cargo-deny`):

| Option | Pure Rust? | License | Verdict |
|---|---|---|---|
| [Symphonia](https://github.com/pdeljanov/Symphonia) (`symphonia-bundle-mp3`) | ✅ robust | **MPL-2.0** (weak copyleft) | ❌ trips the core license gate; core/published binaries can't use it |
| [puremp3](https://github.com/Herschel/puremp3) | ✅ | MIT / CC0 (permissive) | ✅ passes the gate, but WASM-focused, less complete, lightly maintained |
| rmp3 / minimp3-rs | ❌ FFI to C minimp3 | permissive | ❌ breaks the pure-Rust guarantee |

So the robust option is the one license we can't use in core, and the permissive
option is the incomplete one. MP3 patents **expired in 2017** (no patent risk)
and Layer III is exhaustively documented, so we write our own — consistent with
AAC and VP9. `puremp3` stays a possible fast-start reference.

## Highest-impact next additions

With H.264 and MP3 decode done, **HEVC** is the biggest remaining traffic-mover
(the one top-10 gap). Just outside the top 10, **AC-3 / E-AC-3** (Dolby) and
**MPEG-2** (DVD/broadcast legacy) are the next tier; an in-house **MP3 encoder**
rounds out MP3.
