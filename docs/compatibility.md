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
- 🚧 **in development** — being built; not in the shipping build yet.

## Codecs

| Codec | Decode | Encode | Implementation | Verification |
|-------|:------:|:------:|----------------|--------------|
| VP9 | ✅ | ✅ | in-house | decode **bit-exact** (315/315 libvpx vectors); encode **pixel-exact vs libvpx & ffmpeg** (RDO, golden/ALT-REF, two-pass) |
| MP3 (MPEG-1/2 Layer III) | ✅ | ✅ | in-house | decode **bit-exact** vs FFmpeg; encode (CBR/VBR, joint stereo, block switching) |
| AAC&#8209;LC | ✅ | ✅ | in-house (`rusty_aac`) | validated (ffmpeg decodes our `.m4a` at unity) · ⚖ |
| PCM | ✅ | ✅ | in-house | validated |
| AV1 / AVIF | ✅ | ✅ (still-picture) | rav1d / rav1e forks (pure Rust) | validated · video-mode AV1 *encode* wiring is a known gap (the encoder exists; the adapter is still-image-only) · **decode robustness:** rav1d's input validation `abort()`s on malformed AV1 under `debug_assertions` (debug builds); **release returns `Err`** (verified). We pre-validate the sample at our boundary, but sandbox the AVIF path if you decode untrusted input in debug/CI. |
| AV2 | ✅ | — | in-house (`rusty_av2d`) | basic (decode registered; encoder in the workshop) · **decode robustness:** panics/aborts on malformed AV2 under `debug_assertions`; **release decodes byte-identically to the reference and survives the fuzz sweep** (verified) — see the note below |
| H.264 / AVC | ✅ | ✅ | rusty_h264 (pure Rust; opt-in SIMD asm) | validated · ⚖ |
| Opus | ✅ | ✅ | rusty-opus (pure Rust) | validated (12/12 official decoder conformance) |
| Vorbis | ✅ | ✅ | decode: lewton · encode: in-house `rusty_vorbis` | validated |
| FLAC | ✅ | ✅ | decode: claxon · encode: in-house | validated (parity with ffmpeg `-compression_level 8` within 0.03%) |
| PNG | ✅ | ✅ | png (pure Rust) | validated |
| JPEG | ✅ | ✅ | in-house `rusty_jpeg` (pure Rust; vendored merge of jpeg-decoder + jpeg-encoder) | validated |
| GIF | ✅ | ✅ | gif (pure Rust) | validated |
| WebP | ✅ | ✅ | image-webp (pure Rust) | validated |
| JPEG XL | ✅ | — | jxl-oxide (pure Rust) | validated |

> **H.264 SIMD:** the default build uses `rusty_h264` with its hand-written
> assembly kernels on (`h264-asm`, needs `nasm`) — no C. A separate **opt-in** C
> path (`--features h264-openh264`, Cisco openh264) exists only as a cross-check.
>
> ⚖ = patent-relevant — see [Patents](#patents).

> **AV2 decode robustness** (`rff-codec-av2`, backed by `rusty_av2d 0.2.8`) —
> the same shape as the rav1d note above, and measured the same way:
>
> * **Debug builds abort.** `rusty_av2d` performs an unchecked subtraction
>   (`av2_gdf.rs:549`) that trips Rust's arithmetic-overflow check, and the
>   fuzz sweep (`fuzz_robustness`) takes the process down with
>   `STATUS_STACK_BUFFER_OVERRUN` on malformed OBU input. An `abort()` is not a
>   panic, so `catch_unwind` cannot contain it.
> * **Release builds are sound on both counts (verified 2026-08-27).** The
>   overflow is a wrap the decoder actually intends: with checks off,
>   `av2f_still` decodes **byte-identically to the reference** (4/4), and
>   `fuzz_robustness` passes (2/2) with AV2 in the sweep.
>
> So a shipped release binary does not crash on hostile AV2, and its output on
> valid AV2 is correct. **Sandbox the AV2 path if you decode untrusted input in
> a debug or CI build**, and treat the unchecked arithmetic as a defect to fix
> upstream in `rusty_av2d` rather than a property to rely on — a wrap that is
> correct today is correct by accident, not by contract.
>
> AV2 is also **experimental end-to-end**: the `av2f` still-image container's
> four-character codes are ours, not specified by AOM, so those files read back
> here and nowhere else.

## Containers / formats

Demux **and** mux: `avi`, `mp4`/`mov`/`m4a`, **`matroska` (mkv/mka)**,
**`webm`** (restricted doctype, codec-checked), `mpegts`, `flv`, `ogg`, `wav`,
`flac`, `mp3`, `y4m`, `ivf`, `avif`, `png`, `jpeg`, `mjpeg`, `mpjpeg`, `rtp`, `gif`, `webp`, `jxl`,
`srt` (SubRip), `webvtt`, **HLS** (`.m3u8` + TS segments: mux via the
registry's path-muxer, demux by expanding the playlist — local or HTTP(S),
master or media — into one TS stream).

Demux only: `ass`/`ssa` (Advanced SubStation Alpha, reduced to text cues).

Mux only: **DASH** (`.mpd` static manifest + fMP4 init/media segments,
`-seg_duration`).

Subtitles ride a single text-cue packet contract, so `srt ↔ vtt ↔ ass-in` ↔
Matroska (`S_TEXT/UTF8`, `S_TEXT/ASS` read-side) conversions are stream
copies; `-c:s subrip|webvtt` relabels for the target container.

## Conversion targets

`rffprobe -show_targets INPUT` (and `rff::targets` / the `rff-targets` crate)
answers the other half of "what is this file?": every container this build can
write for it, what happens to each stream (copy / re-encode / dropped), whether
the result is byte-exact, lossless or lossy, and the command that produces it.
`-of json` renders the same answer for a UI or an HTTP handler.

The answer is derived from `MuxCaps` — a declaration each format crate carries
next to its muxer — and from which codecs this build can encode and decode. Two
standing gates keep it honest: `crates/rff/tests/mux_caps.rs` drives every
declared `(format, codec)` pair through the real muxer, and
`crates/rff/tests/targets_end_to_end.rs` transcodes every advertised target for
a real input.

> **Known gap:** `avi`, `mpegts`, `hls`, `flv`, `srt` and `webvtt` accept a
> codec they cannot represent instead of refusing it — writing a zero fourcc
> (AVI), a "private data" stream type (TS/HLS), an AAC/AVC tag regardless
> (FLV), or the raw payload as cue text (SRT/VTT). `-show_targets` never
> *offers* those combinations, but a hand-written `-c:v` still reaches them.
> The `PERMISSIVE` list in `crates/rff/tests/mux_caps.rs` tracks exactly which
> muxers still behave this way.
>
> **`mp4` and `dash` are fixed.** MP4 refused nothing and wrote a `\0\0\0\0`
> sample entry for any codec it could not describe; readers then misparse the
> track (FFmpeg falls back to `rawvideo` and rejects every frame). `codec_fourcc`
> now returns `Option`, so the compiler forces the unmappable case to be handled,
> and `write_header` rejects it before a byte is written — with an error that
> names the codec and the way forward. The same check also closed a second
> silent case: a video track written with **no `avcC`/`av1C`**, which opens
> cleanly and decodes nothing (parameter sets in `extradata` are now recovered;
> a track with none anywhere is refused). DASH shares the mapping, so `.mpd` and
> `.mp4` can never disagree about what is legal.

## Filters

`-vf`: `scale`, `crop`, `hflip`, `vflip`, `transpose`, `pad`, `format`,
`setrange`, `negate`, `grayscale`, `fps` (CFR duplicate/drop — same engine
stage as `-r`).
`-af`: `volume` (linear or dB), `atrim` (sample-accurate), `aresample`,
`anull`.
`-filter_complex`: `overlay` (multi-input compositing).

Editing options with engine support: `-ss` / `-t` / `-to` (frame-accurate on
transcode, keyframe-cut on `-c copy`, sample-accurate for audio), `-r`, `-s`,
`-ar`, `-ac` (mono↔stereo), `-frames:v`, `-metadata` (Matroska `Title`).

## Streaming I/O

| Capability | Status |
|------------|--------|
| HTTP input | ✅ dependency-free pure-std client |
| HTTPS input | ✅ **on by default** (rustls + RustCrypto provider, pure Rust; `--no-default-features` for a TLS-free build) |
| Pipes | ✅ `-` / `pipe:` stdin and stdout, both directions |
| UDP | ✅ `udp://` input (idle-timeout → EOF) and output (1316-byte TS datagrams) |
| RTP | ✅ `rtp://` input: RFC 6184 H.264 and RFC 2435 JPEG depayloaders (single NAL / STAP-A / FU-A; in-band or `Q` quantisation tables, Annex K Huffman regenerated), frames timed on the 90 kHz RTP clock, loss reported per frame. `?pt=` pins the payload type, `?timeout=` the idle EOF. |
| MJPEG over HTTP | ✅ `http://device/stream` (`multipart/x-mixed-replace`) demuxes as `mpjpeg`: `Content-Length` and `X-Timestamp` honoured, the JPEG marker grammar delimits parts without a length |
| RTMP publish | ✅ `rtmp://host/app/key` output (FLV over the chunk protocol: handshake, AMF0 connect/createStream/publish) |
| HLS output | ✅ TS segmenter + VOD playlist, `-hls_time` |
| HLS input | ✅ `.m3u8` (master or media), local or HTTP(S) |
| DASH output | ✅ static MPD + fMP4 segments, `-seg_duration` |

## Rate control

`-b` (bitrate), `-crf`, `-qp`, `-preset` are plumbed to the encoders via
`Encoder::configure` (applied today by the AVIF/rav1e encoder).

## Hardware acceleration — a decision, not an omission

There is **no hwaccel seam** (`-hwaccel`, VAAPI/NVENC/QSV/D3D11) and none is
planned for launch. Every hardware path is a vendor C/C++ driver stack behind
FFI, which breaks the project's 100%-Rust, memory-safe contract; the
performance story here is CPU SIMD (hand-written AVX2/NEON kernels and
`rusty_h264`'s NASM kernels) plus the codec-campaign work that keeps the
pure-Rust encoders competitive. If a memory-safe GPU story emerges (e.g.
wgpu-compute filters), it will be revisited as its own design.

## Planned / not yet implemented

The prioritized, next-gen-first plan lives in [roadmap.md](roadmap.md). The
near-term items and their current state:

| Feature | Status |
|---------|--------|
| AV1 *video* encode wiring (multi-frame; the rav1e fork is ready) | **next up** — adapter is still-picture-only today |
| HEVC/H.265 | **not implemented** (decode or encode) — licensing posture undecided |
| `filter_complex` `concat` and arbitrary graphs | **planned** (only `overlay` today) |
| Two-pass rate control (execution) | `-pass` is **parsed but runs single-pass** (warns) |
| HLS live/event playlists, fMP4 (`EXT-X-MAP`) input | **planned** (HLS is VOD + TS today; `-hls_list_size` warns) |
| RTSP, MPEG-PS/ASF demux | **planned** (RTMP publish shipped; RTSP needs the RTP stack) |
| Capture devices (`libavdevice`), `ffplay`, `libpostproc` | **out of scope** for launch |
| Subtitle burn-in / `drawtext` | **planned** (needs a pure-Rust rasterizer; text subtitle *conversion* ships today) |

## Patents

**Important and easy to get wrong: an independent, clean-room Rust
implementation clears _copyright_ (and copyleft licensing — which is why the
core has no GPL code), but it does _not_ clear _patents_.** A patent covers a
*technique described in the standard*, so any implementation of that technique
practices the patent regardless of who wrote the code or in what language.
The permissive `Apache-2.0` license also does **not** help here: its patent
grant (§3) only licenses patents held by *contributors*, not the
standard-essential patents held by third-party pools.

**Royalty-free or expired** (ship freely): AV1/AVIF, **AV2**, VP9, Opus, FLAC,
Vorbis, PNG, JPEG (baseline), GIF, WebP, JPEG XL, **MP3** (core patents expired
2017), PCM.

AV1/AVIF, **AV2**, and VP9 are royalty-free *by design* under the **AOMedia
Patent License** — members cross-license the patents essential to the spec at no
charge. AV2 (the AOMedia successor to AV1) carries the same grant. The only
theoretical exposure is a third-party/non-member claim — e.g. the **disputed
Sisvel AV1 pool** — which AOMedia contests and which is an industry-wide matter,
not specific to this implementation.

**Patent-relevant** — decide a posture before distributing commercially:

- **H.264 / AVC** (via `rusty_h264`, decode **and** encode). Essential patents
  are administered by the Via LA (formerly MPEG LA) AVC pool. Some have expired;
  the pool is generally still treated as active. Encoding is typically
  higher-exposure than decoding.
- **AAC** — we implement **AAC-LC** (in-house, decode *and* encode). AAC-LC's
  core patents are of the same ~1997–1999 vintage as MP3 and are largely
  expired; the newer **HE-AAC** extensions (SBR/PS) are *not* implemented here.
  The oldest profile is the lower-exposure corner of AAC, but encode is
  typically higher-exposure than decode — same posture question as H.264.

**Project posture: ship and document (the FFmpeg model).** H.264 and AAC ship
in the default build. The project grants **no patent license, express or
implied**, and does not pay or collect any codec royalty. Any patent obligation
(e.g. to the Via LA AVC pool) falls on the party that **distributes or
commercially deploys** a product incorporating these codecs — *not* on the
project, and *not* on individuals who simply run the tool, who are not the
target of pool licensing.

If a patent-clean default artifact matters for your use, you can **gate H.264
and/or AAC out** behind a Cargo feature (they live in their own
`rff-codec-h264` / `rff-codec-aac` adapter crates — the cores are the standalone `rusty_h264` / `rusty_aac`), or obtain a pool license.
**Commercial deployments should consult IP counsel** — this section is
engineering context, not legal advice.
