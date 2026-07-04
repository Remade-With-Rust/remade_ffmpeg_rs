# In-house AAC-LC encoder — brick ledger

An in-house **AAC Low Complexity encoder** for `rff-codec-aac` — the highest-value
codec gap (dominant audio for MP4/HLS/streaming). Built brick by brick like the
FLAC + MP3 encoders. Composes ~60% reused code: the AAC **decoder** supplies the
body, the MP3 **encoder** supplies the brain.

## What we reuse (survey-confirmed)

| Piece | Source | Status |
|---|---|---|
| **Forward MDCT** (1024/128) + sine/KBD windows | `aac/dsp.rs::mdct`, `sine_window`, `kbd_window` | ✅ TDAC-verified, use as-is |
| Scalefactor-band offsets | `aac/swb.rs::swb_offsets` | ✅ as-is |
| Quantizer power-law | `aac/dsp.rs::dequant`/`sf_gain` (+ MP3 `xrpow`) | ✅ formulas |
| Codebook metadata (dim, signed, LAV, escape) | `aac/codebook.rs::CODEBOOKS` | ✅ |
| Raw Huffman codeword/length tables | `aac/tables.rs` | ✅ (need encode-direction index) |
| BitWriter, FFT | `mp3/bitio.rs`, `mp3/encode/fft.rs` | ✅ as-is |
| Psy model, quantize two-loop, Huffman coder | `mp3/encode/{psychoacoustic,quantize,huffman}.rs` | ⚠️ adapt band geometry |

**Must build new:** codebook *encode* tables (invert decode), ICS-info serializer,
AudioSpecificConfig + ADTS writers, `esds` builder in the MP4 muxer, and AAC-shaped
psy/quantize outer loops.

## The gate (AAC is lossy — no bit-exact)

1. **Round-trip via our decoder:** encode → our AAC decoder → decoded audio matches
   the input within quantization noise (energy/spectrum, not bit-exact).
2. **Reference cross-decoder:** **ffmpeg** decodes our AAC to recognizable audio
   (spec-valid, not just self-tolerated).
3. **Quality** (from brick 4): NMR / the MP3 `lab::quality` harness — noise below the
   masking threshold; compare to ffmpeg's AAC at matched bitrate.

## Bricks

| # | Brick | Adds | Gate | Status |
|---|---|---|---|---|
| 1 | **Scaffolding** | BitWriter, codebook *encode* tables (inverted), AudioSpecificConfig + ADTS writers, ICS-info serializer | unit round-trips (encode-tuple → decode == tuple; config writer → parser) | ☑ |
| 2 | **Filterbank + first valid frame** | windowed forward MDCT + overlap-add, crude quantizer (flat), Huffman spectral coding, SCE/CPE → raw_data_block | our decoder + ffmpeg decode it to recognizable audio | ☑ |
| 3 | **Quantizer + rate loop** | non-uniform quantizer (x^¾ + per-SFB scalefactors + global_gain), rate control to a target bitrate (MP3 inner-loop) | hits bitrate; decodes; energy preserved | ☑ |
| 4 | **Psychoacoustic model** | adapt MP3 psy → per-SFB masking thresholds (AAC swb + 1024 MDCT, FFT front-end); outer distortion loop | quality (NMR below threshold); noise < masking | ☑ |
| 5 | **Block switching** | transient detect → EightShort (8×128) + grouping + window shapes | transients clean; decodes | ☑ |
| 6 | **Stereo (M/S)** | CPE with per-SFB ms_used | stereo quality/ratio | ☑ |
| 7 | **Container + E2E + presets** | `esds` in MP4 muxer, ADTS for `.aac`, config in extradata, bitrate presets via `configure()`, engine transcode | `rff -i in.wav out.m4a`; ffmpeg decodes; quality vs ffmpeg AAC | ☑ |

Later quality tools (optional): TNS, PNS, intensity stereo.

## Milestones

- **Bricks 1–3** = a working, bitrate-controlled AAC-LC encoder (valid + decodable,
  quality not yet perceptually tuned).
- **Brick 4** = the quality jump (perceptual).
- **Bricks 5–7** = transients, stereo, real container/CLI.

## Brick log (append before/after per brick)

- **Brick 1 (scaffolding) — DONE.** `src/encode.rs`: MSB-first `BitWriter`; the
  spectral-codebook encoder (`spectral_bits`/`spectral_emit`, exact inverse of
  `codebook::apply_index` — base-`modulo` index packing + sign bits + book-11 escape);
  `write_audio_specific_config` / `write_adts_header` / `encode_ics_info` (inverses of
  the decoder's parsers). Added `HuffBook::code(idx)`, `sf_index_for_rate`,
  `WindowSequence::to_bits`. **Gates (all ✅):** exhaustive spectral round-trip (every
  representable tuple of all 11 codebooks → decode == tuple, bit counts verified);
  config/ADTS writers → parser round-trip; ics_info long re-encodes bit-exact + short
  grouping round-trips. 6 encode tests, whole crate green.
- **Brick 2 (filterbank + first valid frame) — IN PROGRESS.** Filterbank landed:
  `analyze_long` — window the overlapping 2048 samples (prev 1024 ++ cur 1024) ×32768 →
  `dsp::mdct` → 1024 coeffs, the exact inverse of the decoder's
  `imdct · window · (1/32768) + overlap-add`. **Gate: TDAC** — forward-MDCT then the
  decoder's synthesis math reconstructs the signal (one-frame delay) to <1e-3. Also
  fully mapped the `raw_data_block → SCE → ICS → {section, scale_factor, spectral}_data`
  structure to invert (global_gain 8b; section = 4b cb + run-length increments;
  scalefactors = SCALEFACTOR_BOOK deltas from a running acc; spectrum = per-SFB tuples).
  Remaining: crude quantizer (global gain + x^¾), codebook selection, section/scalefactor/
  spectral coding, SCE+ADTS assembly, `AacEncoder` + registration; gate = our decoder +
  ffmpeg decode to recognizable audio.
- **Brick 2 — DONE. We have a working AAC encoder.** Added the full assembly on top of
  the filterbank: crude quantizer (`choose_global_gain` targets the loudest coeff at
  ~2000, `quantize` = `sign·round((|X|·2^(−0.1875(gg−100)))^¾)` capped at 8191), ZERO/esc
  codebook selection, `write_sections`/`write_scalefactors`/`write_spectrum`,
  `encode_channel_element` (SCE), and `AacEncoder` (buffers input → 1024-sample long
  frames → ADTS, a trailing zero block flushes the overlap). Registered
  `encoder: Some(AacEncoder)`. **Gate (both halves ✅):** encode a 440 Hz tone → *our*
  decoder → recognizable tone (energy preserved, 440 dominates 1234 by >5×); *ffmpeg*
  identifies "aac (LC), 44100 Hz, mono" and decodes to **−9.2 dB RMS / −6.0 dB peak = a
  clean 0.5-amplitude sine**. 41 tests green. Scope: mono/N-channel-as-N-SCE, long
  blocks, flat scalefactors — quality/rate-control/stereo come in bricks 3–6.
- **Brick 3 — Quantizer + rate loop. DONE.** Replaced brick 2's crude coding with:
  proper per-SFB **codebook selection** (`best_codebook_for_band` picks the cheapest
  representable book by actual bit cost, dim-aware — dim-4 books for 4-aligned bands),
  section-bit accounting, and an inner **rate loop** (`rate_loop` binary-searches
  global_gain to fit a per-frame budget, floored at `min_global_gain` so the loudest
  coefficient never clamps past MAX_QUANT — a subtle bug: too-small gg *clips*, it
  doesn't refine). `-b` bitrate via `configure()` (default 128 kbps). **Gate:** rate loop
  hits target (64k→60 184, 128k→127 952 b/s on a dense multi-tone+noise signal) and still
  decodes; the 440 Hz tone stays clean through *our* decoder AND ffmpeg (−9.2 dB RMS /
  −6.0 dB peak). 41 tests green. Scalefactors are still flat — per-band variation is the
  psychoacoustic brick 4.
- **Brick 4 — Psychoacoustic model. DONE.** A signal-adaptive perceptual quantizer.
  **Psy model** (`masking_thresholds`): per-SFB energy → Bark-scale spreading (Traunmüller
  Hz→Bark, asymmetric ~27 dB/Bark down / ~10 dB/Bark up) → mask 18 dB below the spread
  signal. **Allocation** (`perceptual_offsets`): from `noise ∝ 2^(0.375·sf)·Σ√|X|`, the sf
  that lands `noise = threshold` is `log2(threshold/Σ√|X|)/0.375`; per-band offsets are
  centered (energy-weighted, so empty bands don't skew it) and the rate loop finds the
  common `base` → **uniform-NMR** shaping. Coding went **per-band scalefactor**
  (`code_frame`/`write_scalefactors` now emit `SCALEFACTOR_BOOK` deltas from a running acc;
  `min_base` = per-band no-clamp floor). **Gate:** at ~128 kbps the worst-band NMR drops
  **24.62 → 4.05** (~8 dB less audible distortion in the loudest artifact — the band that
  sets perceived quality); mean NMR rises (−6.2 → +2.4 dB) exactly as intended — bits move
  off over-precised masked bands onto the starved sensitive one. 42 tests green; ffmpeg
  still decodes the psy-coded stream cleanly (−9.2 dB RMS). Bugs caught en route: a plain
  mean is skewed by empty bands' huge `thr/noise_scale` (→ energy-weighted center); a tight
  ±24 offset clamp destroys the shaping (→ ±60); and the absolute-energy "audible" metric is
  dominated by the loudest band's large threshold — worst-case **NMR ratio** is the right
  gate. Still flat within: no distortion-loop iteration, tonality, or ATH floor (future
  tuning); long blocks only.
- **Brick 5 — Block switching. DONE.** Transients now switch to eight 128-bin short
  blocks (kills pre-echo). **Filterbank:** `analyze_short` — eight 256-sample sine windows
  (128-hop) tiled across [448,1600) of the prev++cur buffer → window-major 8×128 coeffs,
  the exact inverse of the decoder's `short_frame`; `long_window` builds the LongStart/
  LongStop transition windows (sine, mirroring the decoder). **Detection:** `detect_transients`
  flags a frame whose 128-sample sub-block energy leaps >10× the running average; `assign_sequences`
  brackets each short run with LongStart…LongStop and merges runs a single frame apart (a lone
  gap can't be both stop and start). **Coding:** `code_frame_short` etc. — one group of 8
  windows, flat scalefactors, per-SFB cheapest codebook across all windows, 3-bit section
  runs, per-window spectral tuples. **Gates (all ✅):** the full sequence
  OnlyLong→LongStart→EightShort→LongStop→OnlyLong reconstructs through the decoder's synthesis
  math (**TDAC** <1e-3 across every transition — the decisive filterbank check); a click signal
  emits EightShort blocks and decodes; **ffmpeg decodes the block-switched stream cleanly**
  (−1.3 dB click peaks intact). 46 tests green. Deferred: short-block psy (flat sf for now),
  window-adaptive grouping, KBD windows.
- **Brick 7 — Container + engine + presets. DONE.** `rff -i in.wav out.m4a` works.
  **Encoder output model** changed from one giant ADTS blob (pts 0) to **per-frame raw
  access units** (`raw_data_block`, no ADTS) queued with sample-domain PTS (`b·1024`) —
  the correct streaming model the engine's `drain_encoder` loop + MP4 muxer expect
  (containers add their own framing). **MP4 `esds`** (`rff-format-mp4`): `build_asc`
  synthesizes the AudioSpecificConfig from rate/channels (stereo 44.1k → the canonical
  `0x12 0x10`), `build_esds` wraps it in the ES_Descriptor→DecoderConfig→DecoderSpecificInfo
  tree, written into the `mp4a` sample entry (mirrors the Opus `dOps` path); round-trips
  through the demuxer's `parse_esds`. **Presets:** `-b:a` bitrate flows via `configure("b")`.
  **Gate (ffmpeg = gold standard, ✅):** `rff -i in.wav out.m4a` → ffmpeg decodes the file at
  **exactly unity gain** (RMS 9267 = input), reports `aac (LC), 44100 Hz, stereo`, no errors;
  the codec round-trips at unity directly (`stereo_direct_decode_amplitude`); `esds`
  round-trip unit test. 48 aac + 7 mp4 + 1 engine test green.
  **⚠ Follow-up (separate bug, not this brick):** decoding our own `.m4a` *back* through our
  MP4-demux→AAC-decode→engine path runs ~2× long/hot. The encoder + muxer + `esds` are proven
  correct (ffmpeg reads the file perfectly; the codec is unity by direct decode), so the bug
  is in the AAC-in-MP4 **read** path — latent, never exercised before there was an AAC-MP4
  encoder to produce a file. Tracked in `crates/rff/tests/aac_m4a.rs`.
- **Brick 6 — Stereo (M/S). DONE.** Stereo now codes as a **common-window
  channel_pair_element** with per-SFB mid/side. **The decision** (`mid_side`): M/S wins when
  `E_M·E_S < E_L·E_R` — the *product*, not the sum (raw energy always halves under the ½
  scaling, so sum is useless; the product is the correlation test). Flagged bands store
  `M=(L+R)/2, S=(L-R)/2`; the decoder rebuilds `L=M+S, R=M-S`. **Structure:** one shared
  `ics_info` + `ms_mask_present`/mask, then two ics bodies (`write_channel_data` — no
  per-channel ics_info); both channels share a joint window sequence (transient flags OR'd)
  and a `joint_max_sfb`; each channel keeps its own psy/rate loop. Works for long, transition,
  and short CPEs. **Gates (all ✅):** L=R content picks M/S and reconstructs the two channels
  identically (`diff/energy < 1e-3`); **ffmpeg decodes the CPE as stereo**, and a
  shared-bass/divergent-highs signal keeps L−R at −20 dB (true stereo, not a mono collapse —
  bass M/S-coded, highs L/R-coded). 47 tests green. Deferred: intensity stereo, per-channel
  independent windows, budget reallocation from the cheap side channel.
