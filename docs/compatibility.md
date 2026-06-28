# Compatibility & support matrix

Where `remade_ffmpeg_rs` stands against FFmpeg's surface. The project is
**pre-1.0**: everything listed works and is tested, but coverage and APIs are
still moving. The default build is **100% Rust, no C/FFI**, permissively
licensed (CI-enforced by `cargo-deny`).

### Verification levels

- **bit-exact** — output matches the reference decoder bit-for-bit on a
  conformance suite.
- **validated** — output is round-tripped or read back by upstream FFmpeg
  (`ffmpeg` / `ffprobe`) in the test suite.
- **basic** — implemented with unit tests; not yet cross-checked against FFmpeg
  at scale.

## Codecs

| Codec | Decode | Encode | Implementation | Verification |
|-------|:------:|:------:|----------------|--------------|
| VP9 | ✅ | — | in-house | **bit-exact** (315/315 libvpx vectors) |
| MP3 (MPEG-1/2 Layer III) | ✅ | ✅ | in-house | decode **bit-exact** vs FFmpeg; encode basic |
| AAC&#8209;LC | ✅ | — | in-house | validated · ⚖ |
| PCM | ✅ | ✅ | in-house | validated |
| AV1 / AVIF | ✅ | ✅ | rav1d / rav1e (pure Rust) | validated |
| H.264 / AVC | ✅ | ✅ | rusty_h264 (pure Rust; opt-in SIMD asm) | validated · ⚖ |
| Opus | ✅ | ✅ | opus-rs (pure Rust) | validated |
| Vorbis | ✅ | — | lewton (pure Rust) | validated |
| FLAC | ✅ | — | claxon (pure Rust) | validated |
| PNG | ✅ | ✅ | png (pure Rust) | validated |
| JPEG | ✅ | ✅ | jpeg-decoder / jpeg-encoder (pure Rust) | validated |
| GIF | ✅ | ✅ | gif (pure Rust) | validated |
| WebP | ✅ | ✅ | image-webp (pure Rust) | validated |
| JPEG XL | ✅ | — | jxl-oxide (pure Rust) | validated |

> **H.264 SIMD:** the default build uses `rusty_h264` with its hand-written
> assembly kernels on (`h264-asm`, needs `nasm`) — no C. A separate **opt-in** C
> path (`--features h264-openh264`, Cisco openh264) exists only as a cross-check.
>
> ⚖ = patent-relevant — see [Patents](#patents).

## Containers / formats

Demux **and** mux: `avi`, `mp4`, `mkv`, `mpegts`, `flv`, `ogg`, `wav`, `flac`,
`avif`, `png`, `jpeg`, `gif`, `webp`, `jxl`, `srt` (SubRip), `webvtt`.

Output only: **HLS** (`.m3u8` playlist + MPEG-TS segments).

## Filters

`-vf`: `scale`, `crop`, `hflip`, `vflip`, `transpose`, `pad`, `format`,
`negate`, `grayscale`.
`-filter_complex`: `overlay` (multi-input compositing).

## Streaming I/O

| Capability | Status |
|------------|--------|
| HTTP input | ✅ dependency-free pure-std client |
| HTTPS input | ✅ **opt-in** `--features https` (rustls + RustCrypto provider, pure Rust) |
| HLS output | ✅ TS segmenter + playlist |

## Rate control

`-b` (bitrate), `-crf`, `-qp`, `-preset` are plumbed to the encoders via
`Encoder::configure` (applied today by the AVIF/rav1e encoder).

## Planned / not yet implemented

| Feature | Status |
|---------|--------|
| `filter_complex` `concat` and arbitrary graphs | **planned** (only `overlay` today) |
| DASH (`.mpd`) output | **planned** |
| Two-pass rate control (execution) | `-pass` is **parsed but runs single-pass** (warns) |
| `-hls_time` / `-hls_list_size` / live playlists | **planned** (segment length fixed ~4 s; VOD only) |
| HTTPS in the default build | **intentional opt-in** — the pure-Rust TLS provider is pre-1.0 / unaudited |
| `ffplay`, `libavdevice`, `libpostproc` | **out of scope** for launch |

## Patents

**Important and easy to get wrong: an independent, clean-room Rust
implementation clears _copyright_ (and copyleft licensing — which is why the
core has no GPL code), but it does _not_ clear _patents_.** A patent covers a
*technique described in the standard*, so any implementation of that technique
practices the patent regardless of who wrote the code or in what language.
The permissive `Apache-2.0` license also does **not** help here: its patent
grant (§3) only licenses patents held by *contributors*, not the
standard-essential patents held by third-party pools.

**Royalty-free or expired** (ship freely): AV1/AVIF, VP9, Opus, FLAC, Vorbis,
PNG, JPEG (baseline), GIF, WebP, JPEG XL, **MP3** (core patents expired 2017),
PCM.

**Patent-relevant** — decide a posture before distributing commercially:

- **H.264 / AVC** (via `rusty_h264`, decode **and** encode). Essential patents
  are administered by the Via LA (formerly MPEG LA) AVC pool. Some have expired;
  the pool is generally still treated as active. Encoding is typically
  higher-exposure than decoding.
- **AAC** — we implement **AAC-LC decode only** (in-house). AAC-LC's core
  patents are of the same ~1997–1999 vintage as MP3 and are largely expired; the
  newer **HE-AAC** extensions (SBR/PS) are *not* implemented here. Decode-only of
  the oldest profile is the lower-exposure corner of AAC.

This project ships these implementations with **no patent grant** and, like
FFmpeg, leaves distribution/use responsibility to the distributor/user. Options:
(a) document and ship (the FFmpeg model), (b) gate H.264/AAC behind a Cargo
feature so the default artifact excludes them, or (c) obtain a pool license.
**Commercial users should consult IP counsel** — this section is engineering
context, not legal advice.
