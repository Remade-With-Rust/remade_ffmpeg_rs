# Threat model: remade_ffmpeg_rs

## What this project does and where untrusted input enters

remade_ffmpeg_rs is a pure-Rust remake of FFmpeg: a media engine (the `rff` library, package `remade-ffmpeg`), dozens
of container demuxers/muxers and audio/video/image codecs (most also published on their own, such as `rusty_mp3`,
`rusty_aac`, `rusty_vp9`, `rusty_h265`, `rusty_flac`, `rusty_vorbis`, `rusty_png`), FFmpeg-compatible CLIs (`rff`,
`rffprobe`, and the `ffmpeg`/`ffprobe` aliases), and an HTTP API server (`rff-server`). Downstream users embed the
crates in services that process media uploaded by strangers, so **assume every media byte is attacker-controlled.**

Untrusted input enters at:

1. **Demuxers** (`crates/rff-format-*`, plus `crates/rff-subtitle` and `crates/rff-format-{srt,ass,webvtt}`): MP4/MOV,
   MKV/WebM, MPEG-TS, FLV, AVI, Ogg, WAV, FLAC, MP3, IVF, Y4M, AV2F, HLS playlists, RTP, MJPEG, still-image containers
   (PNG, JPEG, GIF, WebP, AVIF, JXL) and subtitle text. These are reached first, from a file, a pipe or the network.
2. **Decoders** (`crates/rff-codec-*` adapters and the `crates/rusty_*` cores): H.264, HEVC/H.265, VP9, AV1 (via AVIF),
   AV2, AAC, MP3, Opus, Vorbis, FLAC, PCM, and the image codecs. Bitstream parsing, entropy decoding, inverse
   transforms, prediction and in-loop filters all run on attacker-chosen values.
3. **`crates/rff-io`**: turns a path or URL into a byte source. Plain `http://` (redirects, chunked encoding) is always
   built; `https://` (rustls with the RustCrypto provider) is behind the `https` feature. Also UDP and RTP input and
   RTMP publish (`rtmp.rs`, which also reads server-to-client traffic). The peer is untrusted: oversized or endless
   responses, redirect loops, slow-loris, malformed chunking, hostile TLS.
4. **`crates/rff-server`**: an axum HTTP API (`/v1/probe`, `/v1/transcode`) that takes JSON with input/output paths,
   format and codec names, and filter-graph strings, then drives the same engine. `/v1/probe` needs no credential and
   its `path` may be a local path, an `http(s)://` URL, a `udp://`/`rtp://` address or an HLS playlist.
5. **Filter-graph strings** (`crates/rff-filter`, `-vf` / `-filter_complex`), which come from users of the CLI and,
   through `rff-server`, from remote callers.

Encoders (`crates/rusty_*` encode paths, `crates/rff-codec-*` encode side) take raw frames from the application. That
input is usually trusted, but encoder options and frame geometry can come from a remote caller through `rff-server`.

## Components that matter most / least

Most important:

- **Every demuxer and decoder.** The project contract is that hostile input returns `Err`, never panics, hangs, reads
  out of bounds or allocates without bound (see `SECURITY.md` and `crates/rff/tests/demuxer_fuzz.rs`). Release builds
  use `panic = "unwind"`, and decoders catch panics at their API boundary, but an uncaught panic is still a bug.
- **The `unsafe` code.** Nearly all of it is SIMD and pointer arithmetic in `crates/rusty_h265-accel/src`
  (`mc.rs`, `pixel.rs`, `intra.rs`, `sao.rs`, `itx.rs`, `deblock.rs`), plus smaller amounts in `crates/rusty_mp3/src`,
  `crates/rusty_flac/src/encode.rs`, `crates/rusty_vorbis/src` and `crates/rff-resample`. Any bitstream value that can
  drive an index, a stride or a block size into these kernels without validation is the highest-value finding here.
- **`crates/rff-io`** and **`crates/rff-format-hls`**: memory exhaustion from a hostile server, TLS
  certificate-validation mistakes, request smuggling through redirects, and anything that lets a URL reach a scheme,
  host or local file the caller did not ask for. An HLS playlist chooses the segment URLs that get fetched next, so a
  hostile playlist is a hostile URL list.
- **`crates/rff-server`**: path traversal or arbitrary file write through `output`, server-side request forgery through
  `input`, and anything that gets past a real `Authenticator`.

Less important but in scope: muxers (fed by our own demuxers and encoders), the encoders, `rff-filter`, and the CLIs'
argument parsing.

## How to exercise it

The image is built in `/src` with the dev profile at opt-level 1, so **debug assertions and integer overflow checks
are on**: an overflow that would silently wrap in a release build panics here, and that is a finding worth reporting.
Do not rebuild with `--release`: the release profile uses thin LTO with one codegen unit and takes far too long on the
audit machine.

- **CLIs:** `target/debug/rff`, `target/debug/rffprobe` (also `target/debug/ffmpeg`, `target/debug/ffprobe`), with
  FFmpeg-style arguments, for example `target/debug/rffprobe in.mp4` or
  `target/debug/rff -i in.mkv -c:a pcm_s16le out.wav`.
- **Server:** `target/debug/rff-server` listens on `127.0.0.1:8080` (override with `RFF_SERVER_ADDR`). Example:
  `curl -s localhost:8080/v1/probe -d '{"path":"/src/vp9-vectors/vp90-2-02-size-130x132.webm"}' -H 'content-type: application/json'`.
- **Coverage-guided fuzzers** (libFuzzer + AddressSanitizer, already built): binaries are in
  `crates/<crate>/fuzz/target/x86_64-unknown-linux-gnu/release/<target>`. Targets: `crates/rff/fuzz` has `demux`
  (first byte picks a demuxer) and `decode` (first byte picks a decoder); `crates/rff-codec-aac/fuzz` and
  `crates/rff-codec-mp3/fuzz` have `decode_packets` and `encode_frames`; `crates/rusty_aac/fuzz` has `decode_adts`,
  `decode_config`, `latm`, `parsers` and `encode`; `crates/rusty_mp3/fuzz` has `decode`, `decode_frame`, `encode` and
  `pipelined`. Seed corpora are in `crates/<crate>/fuzz/corpus/<target>`. To run one:
  `cd crates/rff && cargo +$FUZZ_NIGHTLY fuzz run demux -- -max_total_time=300`.
- **Robustness tests:** `cargo test -p remade-ffmpeg --test demuxer_fuzz` and `--test fuzz_robustness`, and
  `VP9_FUZZ_ITERS=50000 cargo test -p rff-codec-vp9 fuzz_robustness -- --ignored`.
- **Real media:** libvpx VP9 conformance vectors in `/src/vp9-vectors` (`VP9_VECTORS_DIR` is set), ISO MP3
  conformance vectors in `/opt/mp3-vectors/vectors` (`MP3_ISO_VECTORS` is set), and the fuzz seed corpora above. Most
  crates' tests synthesize their own inputs.
- **HTTP input:** there is no network, but loopback works: serve a hostile response with
  `python3 -m http.server` or a short Python socket script and point `target/debug/rffprobe http://127.0.0.1:PORT/x`
  at it. To exercise TLS, build with `cargo build -p rff-cli --features https`.

## How you rate severity

- **Critical:** memory corruption (out-of-bounds write, use-after-free, double free) reachable from a media file, a
  network peer or an `rff-server` request, or any remote code execution.
- **High:** out-of-bounds read or uninitialized-memory disclosure reachable the same way; TLS certificate validation
  that can be bypassed; path traversal or arbitrary file write through `rff-server`; anything that gets past a real
  `Authenticator`; server-side request forgery through `rff-server` into a scheme or host the deployment did not allow.
- **Medium:** a small malformed input that panics, aborts, hangs, loops forever, or allocates memory or CPU far out of
  proportion to its size (for example a 1 KB file that asks for gigabytes) in a demuxer, decoder or `rff-io`. These are
  availability bugs for every service that embeds the crates. An integer overflow that panics in this debug build counts
  here, and goes up to High if the wrapped value reaches an `unsafe` kernel.
- **Low:** the same robustness problems in an encoder or muxer fed by the application rather than an attacker, and
  panics from CLI argument parsing.
- Output that decodes but is not bit-exact with the reference decoder is a correctness bug, not a security finding.
  It is still welcome as Low if it comes from reading memory it should not.

## Anything to leave alone

- **`crates/rff-ui`** (the Dioxus front-end) and **`crates/rff-codec-openh264`** (opt-in C/FFI H.264): neither is a
  default workspace member nor part of the published binaries, and neither is built in this image.
- **`crates/hevc-duel`**, `tools/`, `scripts/` and `video-tests/`: developer tooling for benchmarks and quality
  campaigns, never shipped.
- **rav1d debug aborts:** in debug builds, the external `rav1d` AV1 decoder (used for AVIF) calls
  `std::process::abort()` on some malformed AV1 through its `debug_abort()` path. Release builds return `Err`. This is
  documented in `SECURITY.md`; do not report the abort itself, but do report it if the same input misbehaves in a
  release build.
- **The development authenticator.** `rff-server` ships with `DevAllowAll`, which admits every caller, and binds to
  `127.0.0.1` by default; production deployments swap in a MATA mID verifier. That the stub admits everyone is known.
  What an unauthenticated caller can reach through `/v1/probe` is in scope.
- Denial of service that needs a genuinely enormous but well-formed input (decoding a 100 GB file uses a lot of
  memory), and slow encodes at high-effort settings.
- Bugs in third-party crates (rav1d, rustls, axum, tokio and others) that are not reachable through this project.
  If one is reachable, report it here and we will take it upstream.
