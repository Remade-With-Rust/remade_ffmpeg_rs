# rusty_h264 and rusty_jpeg upgrades for the chip

*Written 2026-09-02 for Janus, the Remade programme that puts these two
codecs on ESP32-class parts behind `rusty_esp_video`'s `VideoEncoder`
seam. Everything here is an upstream item in `rusty_h264`, `rusty_jpeg` or
`rff`; the Janus side is already written against the public APIs and moves
down the ladder unchanged as each item lands.*

## Why this plan exists

A chip is not a small host. What the two encoders need to be usable on an
ESP32-S3 (240 MHz, no SIMD, 512 KB SRAM plus 8 MB PSRAM on the boards Janus
targets) is a short, specific list:

1. **`no_std` + `alloc`** with the float math from `libm`, so the crate
   builds for `xtensa-esp32s3-none-elf` and `riscv32imac-unknown-none-elf`.
2. **No owned frames.** The camera hands over a borrowed buffer; copying a
   QVGA picture into three `Vec<u8>` and copying the bitstream back out is
   230 KB of traffic per frame that a PSRAM board tolerates and a 512 KB
   part cannot.
3. **Caller buffers for output.** The packetizer already owns a buffer; the
   encoder should write into it, not allocate one.
4. **Explicit configuration, no environment.** A chip has no `std::env`;
   every "set this variable to get Baseline" must be a field.
5. **A keyframe request.** A late joiner on the mesh needs an IDR now, not
   at the next GOP boundary.
6. **A memory model on paper.** Bytes per reference frame, per macroblock
   array, per cache, as a function of width and height, so a firmware can
   pick a size that fits before it is flashed.
7. **Determinism across host and chip.** The same source must produce the
   same bytes on the laptop and on the board, or the ledger cannot say the
   chip is correct. Pure-Rust `libm` on both sides gives that; the platform
   libm does not.
8. **A decoder that reads a slice.** `std::io::Read` is a host idea.

What Janus has today, measured (`rusty_esp_video/docs/LEDGER.md`): the
H.264 wrapper in its chip configuration produces a Constrained Baseline
stream at QVGA that `ffprobe` reads as `h264,Constrained Baseline,320,240,30`
and the house decoder round-trips, at 439–534 µs per frame on a laptop
core; the same crate checks on `riscv32imac` and `riscv32imafc` with the
encoder in. Sensor JPEG is passthrough only.

## rusty_h264

### Done: `no_std` + `alloc` for common, encoder and facade

[PR #7](https://github.com/Remade-With-Rust/rusty_h264/pull/7), branch
`no-std`. The ladder: `std` (default) keeps every behaviour; without it the
crates are `no_std` + `alloc` and need `libm`. Environment knobs read as
unset, censuses and harvest sinks are no-ops, `encode_all` runs GOPs in
order, per-frame scratch is allocated per frame. The decoder stays
`std`-only behind the facade's `std` feature. Every existing gate passes
with `std` and on the `no_std` code paths.

Two things the PR changed that downstreams must know: `default` now
includes `std`, so the scalar arm is `--no-default-features --features std`;
and `signal_probes_golden` is pinned only against the platform libm.

### Upgrade 1 — borrowed-frame input

**What:** an input type that borrows the caller's planes,
`YuvPlanes<'a> { y: &'a [u8], u: &'a [u8], v: &'a [u8], stride_y, stride_c }`,
accepted by `encode_planes(&mut self, &YuvPlanes) -> …`. The owned
`YuvFrame` stays as a convenience that implements the borrowed view.

**Where the copies are today:** the lookahead queue keeps
`frame.clone()` (`la_queue.push`) and the AQ grain probe keeps
`pending_aq_probe: Option<YuvFrame>` (a copy of the previous source). With
`lookahead = 0` and AQ off — the chip configuration — neither is needed, and
the encoder should not take a copy it will not use. The reconstruction
already lives in `RefFrame` (`AlignedBytes` planes), so the source is not
needed after the picture is coded.

**Gate:** byte-identical streams between `encode(&YuvFrame)` and
`encode_planes(&view)` on the conformance corpus; the ffmpeg decode gate
unchanged.

**Janus effect:** removes one 115 KB copy per QVGA frame and lets the
camera's DMA buffer feed the encoder directly.

### Upgrade 2 — output into a caller buffer

**What:** `encode_into(&mut self, frame, out: &mut [u8]) -> Result<usize, EncodeError>`
(and the same for `flush`), returning `BufferTooSmall { needed }` when the
access unit does not fit. Internally the slice writer (`code_picture` and
the CABAC/CAVLC payload assembly) targets a `&mut [u8]` cursor instead of
growing a `Vec<u8>`.

**Gate:** bytes identical to `encode`; a deliberately short buffer returns
the needed size and leaves the encoder state usable.

**Janus effect:** the packetizer's buffer is the only buffer; removes the
second copy per frame.

### Upgrade 3 — a public keyframe request

**What:** `Encoder::request_keyframe(&mut self)` setting the existing
private `force_idr`. Janus today recreates the encoder to get an IDR, which
resets rate control and the DPB.

**Gate:** the next picture after the call is an IDR; rate-control state
survives.

### Upgrade 4 — Baseline as configuration, not environment

**What:** `EncoderConfig::baseline(width, height)` (Constrained Baseline,
CAVLC, no 8×8 transform, no B-frames, one reference, `Preset::Fast`,
`lookahead = 0`, `scenecut = 0`) as a named constructor, and the
`RUSTY_H264_LEGACY_CAVLC` variable demoted to a host-only convenience that
selects the same constructor. The fields are already public; Janus sets
them by hand today, which is exactly the kind of knowledge that should live
upstream once.

**Gate:** the constructor's stream is byte-identical to the current
env-selected one.

### Upgrade 5 — the memory model, written down and reported

**What:** a documented formula and a `EncoderConfig::memory_estimate() ->
MemoryEstimate { per_ref_frame, mb_arrays, hpel_cache, scratch, total }`
for the configuration, plus a test that the estimate is within a stated
margin of a measured allocation count on the host.

**Known today:** a reference frame is three planes (`AlignedBytes`) plus
per-macroblock `mv`/`ref_idx` vectors; the half-pel plane cache is three
more frame-sized planes per reference, **built lazily and only when the
search is not `Fast`** (`if !self.fast { reference.hpel(..) }`), so the chip
configuration never pays for it — that fact should be a test, not a code
comment.

**Janus effect:** the firmware picks QVGA vs QCIF from a number, and the
ledger's "PSRAM use" row has something to compare against.

### Upgrade 6 — the decoder without `std`

**What:** the single-threaded decode path behind `no_std` + `alloc`, with
`frame_mt` (threads, `Mutex`, `yield_now`) behind `std`. Lower priority for
Janus: the chip encodes, the home computer decodes. It matters for the P4,
which has a hardware decoder Janus may prefer to bypass for conformance
checks, and for a device that displays a peer's stream.

### Upgrade 7 — `rff` catches up

- `rff-codec-h264` pins `rusty_h264 0.8` while the crate is at 0.12 and the
  `no-std` branch changes the default feature set. Bump to the released
  `no_std` version with `features = ["std"]` (the facade needs `std` for
  the decoder rff uses).
- Expose the preset and profile on the CLI (`-preset fast`, `-profile
  baseline`) so an `rff` encode can reproduce the chip configuration
  byte-for-byte on the host — that is the oracle the S3 row in Janus's
  hardware plan compares against.
- `rtp://` input (`rff-io`) with the RFC 6184 H.264 and RFC 2435 JPEG
  depayloaders; Janus's `rusty_esp_video::rtp` already writes both, and
  `udp://` MPEG-TS is the only path `rff` can read from a device today.

## rusty_jpeg

### State today

`extern crate alloc` and a `std` feature exist, `JfifWrite` is the
`no_std` writer trait with an impl for `Vec<u8>`, and `simd` implies `std`.
But `lib.rs` has no `#![cfg_attr(not(feature = "std"), no_std)]`, so the
crate is not actually `no_std` yet. The encoder's remaining `std` uses are
small: the `BufWriter<File>` convenience constructor, `std::path::Path`,
`EncodingError::IoError(std::io::Error)`, two `OnceLock` + `std::env`
trellis knobs, and runtime CPU detection under `simd`. The decoder is
`Decoder<R: Read>` over `std::io::Read` with `io::Error` in its error type
and worker threads behind features.

### Upgrade 1 — encoder `no_std` (small)

**What:** the crate attribute; `IoError` and the file constructor behind
`std`; the trellis knobs through a `std`-gated shim (the `rusty_h264` shape:
`knob(name) -> Option<String>`, `None` without `std`); `JfifWrite for
&mut [u8]` (a caller buffer, `BufferTooSmall` on overflow) beside the
existing `Vec<u8>` impl. Float math: the trellis has ~20 float sites; a
`libm` feature with an `F32Ext` extension trait as in `rusty_h264` covers
them without touching call sites.

**Gate:** the encoder's conformance tests unchanged; a bare-metal `cargo
check` in CI; the same frame encoded on the host with `std` and with the
`no_std` code paths byte-identical.

**Janus effect:** the first non-passthrough JPEG on a chip.

### Upgrade 2 — YUV input to the encoder

**What:** `ImageBuffer` implementations for what a camera produces —
YUYV 4:2:2 packed and planar 4:2:0 — so no RGB conversion happens on the
chip. Today `encode` takes `ColorType` RGB/RGBA/Luma/CMYK-shaped packed
input and converts.

**Gate:** decode-encode-decode PSNR unchanged against the RGB path on the
test corpus.

### Upgrade 3 — decoder from a slice, `no_std`

**What:** a crate-local `Source` trait (`read`/`read_exact`) implemented for
`&[u8]` everywhere and for `std::io::Read` under `std`; the error type
without `io::Error`; the worker modules (`multithreaded`, `rayon`) behind
`std`. The decoder's DCT-domain scaling (`Decoder::scale`, 1/8 to 1/1) is
the important part for a chip: a 1600×1200 sensor JPEG decoded at 1/4 is a
400×300 picture for a fraction of the work.

**Gate:** the decoder conformance suite unchanged over the slice source;
bare-metal check in CI.

**Janus effect:** on-chip transcode — decode a sensor JPEG at reduced
scale, feed the H.264 encoder or the presence detector — without ever
holding the full-size picture.

### Upgrade 4 — `rff` reads what a device sends

- **MJPEG over HTTP** (`multipart/x-mixed-replace`): a demuxer in
  `rff-format-jpeg` (or a new `rff-format-mjpeg`) so `rff -i
  http://device/stream` works. Janus's `mjpeg_reader` already parses this
  shape on the Pi side and can be the reference.
- Un-vendor: `rff` carries `crates/rusty_jpeg` as a path copy at 0.3.2.
  Once the `no_std` release exists, depend on it from crates.io.

## Priority and sequencing

| # | item | crate | size | Janus effect |
|---|---|---|---|---|
| 1 | borrowed-frame input | rusty_h264 | days | one copy per frame gone; DMA buffer feeds the encoder |
| 2 | output into a caller buffer | rusty_h264 | days | the second copy gone |
| 3 | public keyframe request | rusty_h264 | hours | no encoder recreation per IDR |
| 4 | `EncoderConfig::baseline()` | rusty_h264 | hours | the chip configuration lives upstream |
| 5 | memory model + estimate | rusty_h264 | day | a number before flashing |
| 6 | encoder `no_std` | rusty_jpeg | day | first on-chip JPEG encode |
| 7 | YUV input | rusty_jpeg | day | no RGB conversion on the chip |
| 8 | decoder from a slice | rusty_jpeg | days | reduced-scale transcode |
| 9 | `rff` pin bump + preset/profile flags | rff | hours | the host oracle for the S3 row |
| 10 | `rtp://` input | rff | days | Janus RTP readable without TS |
| 11 | MJPEG-over-HTTP demuxer | rff | day | `rff -i http://device/stream` |
| 12 | decoder `no_std` | rusty_h264 | days | later; the P4 and peer display |

Items 1–5 are one PR each on `rusty_h264` after #7 merges, in that order;
6–8 are one `no-std` branch on `rusty_jpeg` in the shape #7 took (format-only
commit first if the tree is not rustfmt-clean, then the pass); 9–11 are
`rff` PRs and can go in parallel.

## The rules every item keeps

- The slow scalar path stays in the tree as the oracle; a faster path is
  gated byte-identical against it (integer paths) or within a stated error
  plus the ffmpeg decode gate (float paths).
- No new `unsafe` in the codec cores; `forbid(unsafe_code)` stands.
- No environment variables on any path a chip runs; a knob is a field.
- Every claim in a README is a row in a ledger with its method line.
