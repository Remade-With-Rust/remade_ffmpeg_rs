# Pure-Rust codec survey

**The rule.** Every codec on the core path must be **100% Rust** — no C/C++ FFI,
no wrapping a C library — **and** permissively licensed. The `deny.toml`
allow-list rejects all copyleft (GPL/LGPL/AGPL/**MPL**), so "pure Rust but MPL"
(e.g. Symphonia) is *not* acceptable, exactly like a C wrapper isn't.

A crate qualifies only if **both** boxes are checked:

1. **Pure Rust** — the codec itself is implemented in Rust. Hand-written
   assembly/SIMD for hot paths is fine (rav1e/rav1d do this); calling into a C
   library is not.
2. **Permissive** — MIT, Apache-2.0, BSD, ISC, Zlib, Unicode-3.0, CC0, NCSA, or
   IJG. No copyleft.

Licenses below were verified against crates.io (2026-06).

## In use today

| Media | Codec | Crate | License | Pure Rust | Role |
|---|---|---|---|---|---|
| image | AV1 (AVIF) | `rav1e` | BSD-2-Clause | ✅ (asm, no C) | encode |
| image | AV1 (AVIF) | `rav1d` | BSD-2-Clause | ✅ (Rust port of dav1d) | decode |
| image | PNG | `png` | MIT/Apache-2.0 | ✅ | encode + decode |
| image | JPEG/MJPEG | `jpeg-decoder` / `jpeg-encoder` | MIT/Apache-2.0 (+ IJG) | ✅ | encode + decode |
| image | GIF | `gif` | MIT/Apache-2.0 | ✅ (LZW in Rust) | encode + decode (first frame) |
| image | WebP | `image-webp` | MIT/Apache-2.0 | ✅ (VP8/VP8L) | decode + lossless encode |
| image | **JPEG XL** | `jxl-oxide` | MIT/Apache-2.0 | ✅ (no FFI) | **decode** (no permissive Rust encoder) |
| audio | **Opus** | `opus-rs` | BSD-3-Clause | ✅ (port of libopus 1.6, no FFI) | encode + decode (in Ogg) |
| audio | **Vorbis** | `lewton` | MIT/Apache-2.0 | ✅ (no FFI) | **decode** (in Ogg; no permissive Rust encoder) |
| audio | **FLAC** | `claxon` | Apache-2.0 | ✅ (no FFI) | **decode** (native container; no permissive Rust encoder) |
| audio | PCM / WAV | — (in-house) | — | ✅ | encode + decode |

## Recommended next — verified pure-Rust **and** permissive

| Media | Codec | Crate | License | Notes |
|---|---|---|---|---|
| image | JPEG (fast decode) | `zune-jpeg` | MIT/Apache-2.0/Zlib | faster decode-only alternative to jpeg-decoder |
| audio | FLAC | `claxon` | Apache-2.0 | decode |
| audio | Vorbis | `lewton` | MIT/Apache-2.0 | decode |
| audio | WAV / PCM | `hound` | Apache-2.0 | container + PCM, enc + dec |

## Pure Rust but **license-blocked** (copyleft → fails the gate)

| Crate | License | Covers | Why blocked |
|---|---|---|---|
| `symphonia` | **MPL-2.0** | MP3, AAC, FLAC, Vorbis, ALAC, ADPCM | MPL is copyleft; rejected by `deny.toml`. Use `claxon`/`lewton` for FLAC/Vorbis instead; MP3/AAC/ALAC have no permissive substitute. |
| `rav1d-safe` | **AGPL-3.0** | AV1 decode | AGPL copyleft; we use upstream `rav1d` (BSD) instead. |

## Gaps — no clean pure-Rust permissive option today

| Codec | Situation |
|---|---|
| ~~**Opus**~~ | **Resolved (2026):** `opus-rs` (restsend) is a permissive (BSD-3) pure-Rust enc/dec — see "Recommended next". `opus`/`audiopus` remain C FFI; avoid those. |
| **AAC** | No permissive pure-Rust crate (`symphonia` is MPL, `fdk-aac` is C FFI), so we write our own: `rff-codec-aac`, an in-house AAC-LC decoder. **Complete AAC-LC decoder** (all window types incl. EIGHT_SHORT, M/S, intensity stereo, PNS, TNS), **verified vs FFmpeg** on real files: deterministic features bit-exact (0.0% residual), PNS energy-exact (random-phase by design). |
| **MP3** | `symphonia` (MPL), `minimp3` (C FFI), `puremp3` (incomplete). No solid permissive pure-Rust. |
| **H.264** | No mature public pure-Rust decoder (`openh264` = Cisco C FFI). Being built in-house. |
| **VP9** | No mature pure-Rust decoder, so we write our own: `rff-codec-vp9` (in-house, staged like AAC). Done: MSB + boolean arithmetic readers; the **full uncompressed + compressed headers** (verified to consume exactly `header_size` across three real libvpx frames — caught a real coef-probs loop bug); the **inverse transforms** (idct4/8/16/32 vs a float DCT reference, iadst4/8/16 by orthogonality, iwht4, the 2D driver + tx_type), the **dequantization** (dc/ac qlookup + derivation), and the **probability model** — every default table (coef-probs model form, Pareto-8 expansion, kf y/uv-mode, kf-partition, skip, cat-bits, inv-map) extracted from libvpx and validated to exact count + range, plus the `inv_remap_prob` sub-exp update and `model_to_full` Pareto expansion; and the **coefficient/token decoder** — `decode_coefs` ported exactly from libvpx (EOB/ZERO/token cascade, Pareto branch, category extra-bits, sign, per-block DC/AC dequant with the 32×32 shift), over the scan/neighbour/band tables (extracted + validated: scans are permutations, neighbours in range); and the **block-structure primitives** — geometry lookups + intra-mode/partition trees (validated: trees cover every leaf, `num_4x4 = 2^log2`), the partition/skip/tx-size contexts, neighbour-mode derivation and key-frame mode-probability selection, all ported exactly from libvpx; and **intra prediction** — all 10 modes (DC + variants, V, H, TM, the six directional) cross-checked **bit-exact** against an independent port of the C reference, plus the edge assembly (127-above/129-left defaults, frame-border extension, above-right replication); and the **reconstruction loop** (`decode.rs`) — compressed-header FrameContext, tile boolean decoder, `decode_partition` recursion, per-block mode-info → predict → token decode → dequant → inverse transform → add, emitting a `Frame::Video`. The **intra key-frame decoder is now 100% bit-exact vs FFmpeg**, including the
**in-loop deblocking filter** — the real `keyframe.vp9` (4:4:4, loop_filter_level=4)
and a battery of FFmpeg-encoded controlled frames (smooth gradients and
high-frequency noise) all decode pixel-identical (maxerr 0) on Y/U/V. The FFmpeg
diff caught six deep bugs (bool-decoder marker bit, sub-8×8 geometry, ADST
orientation, coef-probs update structure, and the **d45/d63 4×4 intra predictors**
which need libvpx's distinct full-above-right 4×4 specials, not the 8×8+ plateau
form). The loop filter is the general `non420` path (limits/levels, 16/8/4 +
internal-4×4 edge masks, verbatim filter4/8/16 kernels). Chroma **4:2:0, 4:2:2
and 4:4:4** plus **lossless (Walsh–Hadamard)** keyframes are all verified
**100% bit-exact** against FFmpeg (8-bit). **Inter prediction is implemented and
bit-exact**: a stateful decoder with 8 reference slots, the full inter
uncompressed/compressed headers, MV entropy decode, `find_mv_refs`, inter
mode-info, and 8-tap sub-pel motion compensation (with compound averaging) — an
8-frame motion sequence decodes **100% pixel-identical** to FFmpeg (maxerr 0).
The inter primitives were each validated in isolation (filter kernels + all
inter/MV prob tables diff-matched to libvpx; convolution + MV decode unit-tested).
**Backward probability adaptation** (all coef/mode/mv contexts, the 4 saved frame
contexts with load/save + `setup_past_independence`) and **temporal MV
prediction** (`use_prev_frame_mvs`) are implemented and verified: a **default,
non-error-resilient** 8-frame stream — the kind a normal `ffmpeg -c:v libvpx-vp9`
produces — decodes **100% bit-exact on every frame** (so do error-resilient
streams). **Compound prediction** (reference-mode + comp-ref neighbour contexts,
two-ref MC averaging) and **superframe parsing** (alt-ref hidden frames) are
done: a 2-pass compound/alt-ref stream decodes **20/20 bit-exact**.
**Segmentation** (spatial + temporal segment-id, per-segment quant / loop-filter,
the `SEG_LVL_ALT_Q/ALT_LF/REF_FRAME/SKIP` features, feature-data persistence
across frames) is bit-exact on an aq-mode stream. **Multi-tile** (tile column/row
layout, per-tile boolean decoders + size prefixes, tile-boundary neighbour
clipping) is bit-exact on a 512×128 multi-tile stream. **10-bit and 12-bit
high-bit-depth** (profiles 2 and 3) are complete: a unified `u16` pixel pipeline
(predictors, inverse transforms, sub-pel convolution and the deblocking filter
all bit-depth-parameterised), the 10/12-bit dc/ac quantiser tables, and the
bit-depth-dependent category-6 token extra-bits — 4:2:0 10-bit, 4:2:0 12-bit and
4:4:4 10-bit streams all decode **bit-exact**, with **no regression** to the 8-bit
path. **Reference frame scaling** (an inter frame whose references were coded at a
different resolution) is implemented — scale factors, the scaled two-pass convolve
with per-pixel `x_step/y_step`, and `vp9_scale_mv` — transcribed from libvpx
`dec_build_inter_predictors`. The decoder is validated **bit-exact against the
official libvpx resize conformance vectors** (`vp90-2-21-resize_inter_*`, which ship
per-frame MD5s): `320×180_5_1-2` (10/10), `320×240_5_1-2` (10/10), and
`320×240_5_3-4` (**30/30**, which exercises *real* reference scaling — 240×180 frames
referencing 320×240 and vice versa). Those vectors surfaced **three real
inter-decoder bugs, all fixed with no regression**: (1) the per-block loop-filter
level was indexed by `is_inter` instead of `ref_frame[0]`, so GOLDEN/ALTREF blocks
took LAST's ref-delta level; (2) `NEARMV` selected the candidate `tmp[count-1]`
rather than slot 1 (`mv_ref_list[1]`, i.e. the zero MV when no distinct second
candidate exists); (3) the **temporal segment-id prediction map was not cleared on
`setup_past_independence` frames** — these resize streams are `error_resilient`, which
(like key/intra frames) resets `last_frame_seg_map` to all-zero, so temporal
segment prediction must predict 0. Carrying the previous frame's map over gave some
blocks the wrong `segment_id` → wrong per-segment quantizer (half-magnitude residual)
→ content-dependent ±N errors. This was the subtle one: a wrong predicted
`segment_id` consumes no bits (it is a map lookup, not a tree read), so it never
desyncs — it was isolated by proving the prediction/residual/IDCT were each
individually correct, then bisecting the segment map source. VP9 decode is now
**fully bit-exact** across intra/inter, compound, segmentation, multi-tile, 8/10/12-bit,
and reference scaling. **Performance:** profiling guided hand-written **AVX2**
(`std::arch`, no FFI) for the two real hotspots — the 8-tap sub-pel convolution and
the in-loop deblocking filter (the latter ~43% of decode; the vertical edges use an
in-register 8×8 transpose so both orientations vectorise). A second pass attacked
the post-SIMD profile: a wide-window **bulk-refill boolean decoder** (libvpx
`vpx_reader` form, replacing the per-bit renorm loop), elimination of all per-block
heap allocations (the inverse-transform and motion-comp scratch moved to reused
thread-local buffers; the coefficient/`token_cache` buffers reused on the decoder),
and **DC-only + sparse-EOB transform fast paths** (skip all-zero coefficient rows).
Every kernel/fast-path is checked **bit-identical to its scalar reference** (a
randomised unit test + the conformance MD5s), so correctness is unchanged. Net
**~1.7× single-thread** on a high-detail 640×480 clip (≈640 vs 384 fps), closing the
gap to FFmpeg's hand-tuned native decoder from ~4.1× to ~2.4×; on lower-detail
realistic content the sparse-transform/DC paths fire constantly and it runs
≈1300–1460 fps. The decoder shares no mutable state between instances, so
multi-stream throughput scales ~linearly (≈4400 fps aggregate at 16 threads).
**Robustness (untrusted input):** a fuzz harness (mutated/truncated/random byte
streams, seeded from real coded frames, via the public `send_packet`/`receive_frame`
API) drove the panic rate from ~28% to 0 — header-size/reference-bounds validation
upstream, a defensive `RefPlane`/intra-edge border path, a frame-dimension cap
(rejects the pathological 65535×65535 ≈26 GB allocation before it happens — a
malformed header otherwise balloons the decoder to >1.8 GB; capped it stays ~7 MB),
and a `catch_unwind` net at the decode boundary (release is now `panic = "unwind"`)
so any residual malformed input surfaces as `Err`, never a process abort.
**AddressSanitizer** is clean on both valid streams (full SIMD coverage) and 15k+
malformed inputs — no out-of-bounds in the `unsafe` AVX2 kernels. **Conformance:**
bit-exact against the full broad official-vector subset tested — quantizer sweep,
odd/non-aligned sizes (incl. 351×287), tile columns **and tile rows** (4×1, 4×4),
frame-parallel, show-existing-frame, droppable, bilinear, Δq, **lf-deltas**,
segmentation, sub-pixel, and the **resize** suite (mid-stream frame-size changes
with scaled references). **Profiles 0–3** are cross-validated bit-exact against
FFmpeg across every chroma layout (4:2:0 / 4:2:2 / 4:4:0 / 4:4:4) at 8/10/12-bit.
Three feature gaps surfaced by the broad suite were fixed: loop-filter delta
persistence across frames, above-context handling across tile-row boundaries, and
intra prediction reading reconstructed neighbours at the coded (vs display) frame
border. |
| **Theora** | No mature pure-Rust implementation. |

## Explicitly avoid — C/C++ FFI or wrapping

`dav1d-rs` (libdav1d), `opus`/`audiopus` (libopus), `fdk-aac`, `minimp3-rs`,
`mozjpeg`/`mozjpeg-sys`, `libwebp-sys`, and the `qoi` crate (bundles reference C
— use a pure-Rust QOI like `rapid-qoi` instead). These are memory-unsafe at the
boundary and/or pull a C toolchain — both against the project's thesis.

### The one sanctioned exception: `openh264` (TEMPORARY)

`openh264` (Cisco, BSD-2-Clause, C/FFI) is wired in **behind the off-by-default
`h264-openh264` feature** (crate `rff-codec-openh264`) as a stopgap so H.264
works before the in-house pure-Rust decoder is finished. It is **not** part of
the default build, the default binaries, or the "pure Rust" guarantee, and it
requires a C toolchain to compile. Remove it once the Rust H.264 lands.

---

*Maintenance: when adding a codec, confirm (1) pure Rust via the repo, not just
the crate description, and (2) the license against `deny.toml`. Add a row to
"In use today" and update the README codec table.*
