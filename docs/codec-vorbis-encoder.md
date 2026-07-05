# In-house Vorbis encoder — brick ledger + deployment plan

An in-house **Ogg Vorbis I encoder** for `rff-codec-vorbis` — the last major
**royalty-free** audio gap (we decode Vorbis via lewton; **no permissive-Rust
Vorbis *encoder* exists anywhere**). Built brick by brick like the FLAC / MP3 /
AAC encoders, validated against a decoder oracle + ffmpeg at every step.

## Why Vorbis is different from AAC (read this first)

Vorbis quality does **not** come from a per-frame rate loop (AAC) or a two-loop
quantizer (MP3). It comes from a **floor + residue** split driven by a psy model,
coded through **pre-trained VQ codebooks** that ship in the stream's *setup header*:

- **Floor** = the spectral *envelope* (the masking curve itself). Coarse shape.
- **Residue** = spectrum ÷ floor (the "flattened" fine structure), vector-quantized.
- **Codebooks** are *not designed per file* — real encoders embed libvorbis's
  pre-trained sets (one per quality mode). We do the same: **embed a reference
  setup, emit it verbatim, parse it once for the encode-side lookups.**
- Bit packing is **LSB-first** (opposite of AAC's MSB-first) — a fresh bit writer.
- Vorbis is **VBR-native** (`-q`); CBR/ABR is a layered bitrate-management add-on.

So the "brain" is: **psy → floor fit → channel coupling → residue classify + VQ**,
using fixed codebooks — not a bit-budget search.

## What we reuse (survey-confirmed)

| Piece | Source | Status |
|---|---|---|
| **Ogg container (mux)** | `rff-format-ogg::OggMuxer` | ✅ done — takes the 3 headers as extradata + pages the audio packets |
| **Decode oracle** | `lewton` (dep, pure-Rust, MIT/Apache) | ✅ `read_header_{ident,setup}` + `read_audio_packet` — our encode → lewton decode gate, and a reference for the setup format |
| **Forward MDCT + FFT** | `rff-codec-aac::dsp::{mdct_fast, fft}` | ⚠️ reuse the O(N log N) engine; **re-match Vorbis's MDCT normalization/phase** (TDAC vs lewton's imdct) |
| Psy concepts (masking, Bark spread) | `rff-codec-aac::encode` | ⚠️ adapt — in Vorbis the *floor IS* the masking curve |
| Frame-parallel encode (`std::thread::scope`) | `rff-codec-aac::encode::encode_stream` | ✅ pattern reuses verbatim (frames independent) |
| MD5 / bit primitives | mp3/flac | ✅ concepts |

**Must build new:** LSB-first bit writer, codebook *encode* tables (VQ + scalar,
from the embedded setup), the 3 header writers, Floor 1 fit+encode, Residue 2
classify+encode, channel coupling, the psy→floor→residue "brain", block switching,
`-q`/`-b` presets, and the engine transcode wiring.

## The codebook / setup strategy (the crux — de-risks the whole thing)

Do **not** train codebooks. Instead:

1. Encode a reference with `ffmpeg -c:a libvorbis -q:a 4 …`, pull the **setup
   packet** (3rd Ogg header) out of the `.ogg`. That blob *is* a known-good
   libvorbis setup for one quality: all codebooks + floor/residue/mapping/mode
   configs.
2. **Embed the setup bytes verbatim** → emit them as our setup header (no
   re-serialization needed; guaranteed valid, lewton + ffmpeg accept it).
3. **Parse it once** at encoder init (lewton's setup parser is the reference) into
   *encode-side* structures: scalar entropy encode (value→codeword) and VQ encode
   (vector→best index), plus the floor/residue/mapping/mode configs to drive coding.
4. Start with **one quality** (q4 ≈ 128 kb/s stereo). Add more `-q` setups later —
   each is just another embedded blob + parsed tables.

This is the same move as the MP3/AAC canonical codebooks, just a bigger asset.

## The gate (Vorbis is lossy — no bit-exact)

1. **Headers valid**: our 3 headers parse in lewton *and* ffmpeg (setup accepted).
2. **Round-trip via oracle**: encode → **lewton** decode → audio matches input
   within quantization (energy/spectrum, not bit-exact).
3. **Reference cross-decoder**: **ffmpeg (libvorbis)** decodes our `.ogg` to
   recognizable audio (spec-valid, not just self-tolerated).
4. **Quality** (from brick 5): NMR / the shared quality harness; compare to
   ffmpeg's libvorbis at matched `-q`.

## Bricks

| # | Brick | Adds | Gate | Status |
|---|---|---|---|---|
| 1 | **Scaffolding** | LSB-first bit writer; embed + parse a reference setup; codebook *encode* tables (scalar + VQ); ident/comment/setup header writers | headers parse in lewton + ffmpeg; codebook encode↔decode round-trips | ☑ |
| 2 | **Filterbank + first frame** | Vorbis window + overlap, forward MDCT (normalization matched to lewton's imdct via TDAC), a *crude* floor + residue → one decodable audio packet, Ogg paging | lewton + ffmpeg decode it to recognizable audio | ☑ |
| 3 | **Floor 1** | fit a piecewise-linear envelope to the log-mag spectrum (fixed X-posts, differential Y via floor codebooks + floor1 classes) | floor reconstructs the envelope; decoded quality jumps | ☑ |
| 4 | **Residue 2** | residue = spectrum ÷ floor; partition → classify (pick codebook per partition) → VQ-encode the interleaved vectors | full spectral reconstruction; near-target quality | ☑ |
| 5 | **Psychoacoustic model + `-q`** | tone/noise masking → the floor curve + the per-partition residue precision (noise floor); `-q` → psy tuning (+ setup selection) | quality (NMR below mask); vs ffmpeg libvorbis at matched `-q` | ☑ |
| 6 | **Channel coupling** | Vorbis stereo: polar/square couple channel pairs (magnitude+angle) pre-residue, per-partition point/phase stereo | stereo quality/ratio (correlated stereo compresses) | ☑ |
| 7 | **Container + engine + presets** (✱ block switching deferred) | streaming encoder; Ogg mux wiring; `rff -i in.wav out.ogg -c:a vorbis`; `-q:a` presets | `rff -i in.wav out.ogg`; ffmpeg + lewton decode | ☑ |

Later quality tools (optional): Floor 0 (LSP), Residue 0/1, multiple `-q` setups,
true ABR/CBR reservoir, `lowpass`/`impulse` tuning.

## Milestones

- **Bricks 1–2** = a valid, decodable Vorbis stream (crude quality) — proves the
  container + setup + filterbank end to end.
- **Bricks 3–4** = real quality (floor + residue) — the codec sounds right.
- **Brick 5** = the perceptual jump + `-q`.
- **Bricks 6–7** = stereo compression, transients, real CLI/container.

## Sequencing & deployment plan

1. **Spike (pre-brick-1):** extract a q4 setup blob from ffmpeg; write it into the
   crate as bytes; confirm lewton parses it. Confirms the codebook strategy before
   any encode code. *De-risks the crux first.*
2. Bricks in order; **one brick per commit**, each gated (lewton + ffmpeg decode,
   quality check) exactly like the AAC ledger. Keep a scalar/direct oracle for any
   fast path (the MDCT reuses the kept `mdct` oracle + `mdct_fast_matches_direct`).
3. Register `encoder: Some(VorbisEncoder)` only once brick 2 emits a decodable
   packet (mirror the AAC `encoder: None → Some` flip).
4. **Perf comes AFTER correctness** (per the playbook): reuse the AAC frame-parallel
   `std::thread::scope` pattern + the fast MDCT once quality is locked.

## Risks & mitigations

| Risk | Mitigation |
|---|---|
| Codebook/setup complexity | **Embed** a reference libvorbis setup, emit verbatim, parse once — don't train (the crux, handled in the spike + brick 1). |
| Floor/residue intricacy | Brick-by-brick against the lewton oracle; crude→real, each step decodes. |
| Vorbis MDCT ≠ our AAC MDCT (norm/phase) | Match to lewton's `imdct` via a TDAC test; keep the direct `mdct` as oracle. |
| No *in-house* decoder oracle | lewton (pure-Rust dep) is the oracle; ffmpeg (libvorbis) is the independent cross-check. Both must decode our output. |
| VBR-native → CBR is hard | Ship `-q` (native) first; layer bitrate management (`-b`) last, in brick 7. |
| Encode-side codebook access from lewton | If lewton's parsed structs aren't encode-friendly, write a thin setup parser into our own encode tables (format is well-defined). |

## Brick log (append before/after per brick)

### Scaffold (brick 1, partial) — DONE ☑ framing + spike green

- **Spike (crux de-risked):** encoded `ffmpeg -c:a libvorbis -q:a 4` → parsed the Ogg
  pages → pulled the 3 header packets. ident = 30 B (v0, 2ch, 44100, br_nom 128k,
  blocksizes 256/2048), comment = 64 B, **setup = 4140 B** — embedded verbatim as
  `crates/rff-codec-vorbis/src/setup_q4_stereo.bin`.
- **`encode.rs` scaffold:** LSB-first `BitWriter` (opposite of AAC's MSB-first);
  `write_ident_header` / `write_comment_header` (byte-aligned LE records); the setup
  emitted verbatim; `VorbisEncoder` skeleton (buffers S16/F32 input per-channel, the
  `Encoder` trait impl, `headers()`). Wired as `mod encode;` — **still `encoder: None`**
  (flips to `Some` at brick 2, per plan).
- **Gate:** `headers_parse_in_lewton` — our generated ident + comment **and the
  embedded q4 setup** all parse in lewton (`read_header_ident/comment/setup`). Proves
  the embed-reference-setup strategy end to end. `bitwriter_lsb_first` verifies packing.
  5/5 crate tests pass; clippy clean.
- lewton gotcha: `IdentHeader.blocksize_{0,1}` are **log2 exponents** (u8: 8, 11), not
  the sizes; `read_header_setup(bytes, channels, (bs0_log2, bs1_log2))`.

### Brick 1 — Codebook encode tables + full setup parse — DONE ☑

- **`encode/setup.rs`** — a self-contained, LSb-first setup parser mirroring lewton
  field-for-field: `BitReader`, `ilog`, `float32_unpack`, `lookup1_values`, and full
  readers for codebooks / floors (type 0+1) / residues / mappings / modes.
- **Huffman encode tables:** `make_words_natural` is a verbatim port of libvorbis
  `_make_words` (marker algorithm), producing the canonical codeword per entry; a
  final `reverse_bits` gives the write-ready (LSb-first) codeword. `Codebook::encode(e)
  → (codeword, len)`.
- **VQ encode tables:** `vq_lookup` reconstructs the dictionary (`entries × dims`,
  bit-identical to lewton's `lookup_vec_val_decode`); `Codebook::quantize_vector(v)`
  is the nearest-entry (squared-error) encoder.
- **The setup parses into `SetupTables`** (codebooks + floor/residue/mapping/mode
  configs kept for bricks 3/4/6/7) and is wired into `VorbisEncoder` init.
- **Gates (all green, 12/12 crate tests, clippy clean):**
  - `setup_parses_and_lands_on_framing_bit` — the whole q4 blob parses and lands
    **exactly** on the framing bit + end-of-packet: proves every field width is
    byte-perfect against a real libvorbis setup.
  - `make_words_matches_spec_example` — reproduces the Vorbis I §3.2.1 worked example
    (`[2,4,4,4,4,2,3,3] → [00,0100,0101,0110,0111,10,110,111]`) — independent proof the
    Huffman assignment is correct without needing brick 2.
  - `huffman_roundtrips` (valid prefix code, encode→decode identity),
    `vq_nearest_is_exact_for_dict_vectors`, and primitive checks
    (`ilog`/`float32_unpack`/`lookup1_values` vs lewton's own vectors).
- **q4/stereo setup shape (parsed):** 42 codebooks (13 VQ), 2 floors (type 1),
  2 residues, 2 mappings, 2 modes (blockflags `[long, short]`). Big residue book is
  dim-8 / 6561 (= 3⁸) entries.
- Deferred to brick 2: codeword↔real-decoder bit-exactness (the first audio packet
  through lewton/ffmpeg is the definitive gate) and ffmpeg-side header validation.

### Brick 2 — Filterbank + first decodable frame — DONE ☑ (lewton 0.9996, ffmpeg 0.9998)

The full audio pipeline, inverting lewton's decode path step for step. **Both
independent decoders reconstruct a test sine at ~0.9997 correlation.**

- **`encode/mdct.rs`** — the Vorbis window (`sin(π/2·sin²(π/2·(x+½)/n))`, verified
  Princen–Bradley `w[i]²+w[i+n/2]²=1`) + forward MDCT. lewton's IMDCT is an
  *unnormalized* DCT-IV + unfold, so the forward carries the `2/M` scale — pinned by an
  end-to-end round-trip test (window → MDCT → lewton IMDCT → window → overlap-add
  reconstructs, max err <1e-3). The MDCT-normalization risk is retired.
- **`encode/frame.rs`** — audio-packet assembly:
  - **Flat floor-1** (crude): posts 0/1 = a dB index near the spectrum RMS, interior
    posts coded 0 → the decoder interpolates a flat curve. Conditions the residue ≈ O(1).
  - **Forward channel coupling** — exact inverse of lewton's `inverse_couple` (unit-tested
    both directions); q4's long mode couples ch0/ch1.
  - **Residue-2 cascade VQ** — interleave channels, partition (psize 32, 55 partitions),
    classify each partition by min cascade-residual energy, then emit classifications +
    cascade VQ in lewton's *exact* pass/partition order (classbook words in pass 0 only).
  - Packet bits: type flag → mode (`ilog` bits) → 2 window flags (long) → per-channel
    floor → residue.
- **Key insight:** lewton's `read_audio_packet` decodes *individual packets* (no Ogg
  needed), so the audio encode is validated directly. The **first packet emits 0 samples**
  (primes overlap) — audio starts at packet 2.
- **Ogg paging** (test helper): minimal pager (CRC-32 `0x04c11db7`, segment lacing,
  BOS/EOS, granule) → a real `.ogg` that ffmpeg's `libvorbis` decodes cleanly
  (`Audio: vorbis, 44100 Hz, stereo`) at 0.9998 correlation.
- **Gates:** `mdct_imdct_reconstructs_via_overlap_add`, `window_satisfies_princen_bradley`,
  `forward_couple_inverts_lewton`, `packets_decode_in_lewton` (corr 0.9996), and the
  opt-in `emit_ogg_for_ffmpeg` (`$VORBIS_OGG_OUT`) for the ffmpeg cross-check. 16/16
  crate tests pass, clippy clean.
- Deferred to later bricks: real per-post floor fit (brick 3), better classification /
  perceptual bit allocation (bricks 4–5), streaming `Encoder`-trait + Ogg-muxer wiring
  and `encoder: Some` registration (brick 7).

### Brick 3 — Floor 1 real envelope fit — DONE ☑

Replaced the flat floor with a per-post piecewise-linear spectral-envelope fit
(`encode/floor.rs`), inverting lewton's floor-1 decode + synthesis.

- **Fit:** each post → the local spectral RMS (window, tracks the envelope without a
  max-window's plateau), scaled by `FLOOR_SCALE` (floor sits *below* the envelope so the
  residue lands >1 and reaches a finer cascade class), held above a per-frame **noise
  floor** (`peak × 1e-4`) so inaudible bins spend ~0 residue bits.
- **Differential coding:** `encode_val` is the exact inverse of lewton's per-post
  `compute_amplitude` branch (zigzag within "room", asymmetric beyond) — `encode_val_roundtrips`
  verifies it for *every* (predicted, target) in range. Posts are committed incrementally
  (predicted from already-fit neighbours via `low_neighbor`/`high_neighbor`/`render_point`).
- **Huffman:** posts emitted through the class / subclass / masterbook structure; a
  `fit_val_to_books` step clamps each residual to what the class's subclass books can encode.
- **Exact curve:** the reconstructed curve is synthesized on the encode side
  (`render_line` + inverse-dB table) and divided out to form the residue, so encode/decode
  agree bit-for-bit on the floor.
- **Results (lewton decode correlation):** sine 0.9956, multitone 0.9964, broadband 0.9901;
  ffmpeg decodes the fitted-floor `.ogg` cleanly (0.9974 on the sine). 20/20 tests, clippy clean.
- **Rate-distortion:** the floor scale is a rate knob — swept on the broadband corpus:
  0.1→774 B/0.9996, 0.35→609 B/0.9951, 0.7→424 B/0.9842, 1.0→328 B/0.9711 per packet.
  q4's ~128 kb/s target ≈ scale 0.7–1.0; **0.5 is the current placeholder** (rate control +
  a psychoacoustic floor land in bricks 4–5). The noise floor kept the sine `.ogg` at 7.4 KB
  (a naïve fit without it bloated to 42 KB by coding sub-audible numerical noise).

### Brick 4 — Residue 2 rate-distortion coding — DONE ☑

Replaced the crude min-error residue classification with **rate-distortion coding** at two
levels, both driven by one Lagrange knob `LAMBDA` (the residue rate/quality control):

- **RD classification** (`cascade_cost` + the classify loop): for each partition, pick the
  class minimizing `distortion + λ·bits` (min-error is just λ=0). `vq_pass` now returns its
  codeword-bit count so the cascade's rate is measured, not guessed.
- **RD-optimal VQ** (`Codebook::quantize_vector(v, λ)`): within each pass, pick the entry
  minimizing `‖v−vq‖² + λ·codeword_len` instead of pure nearest-neighbour — trades a hair of
  match error for a shorter codeword. Measurably tightens the curve (at ~185 B it gives
  0.9145 vs class-only 0.8962 — *both* better quality and fewer bytes).
- **Rate-distortion curve** (dense 64-partial broadband, lewton decode): λ=0→276 B/0.9914,
  0.15→262 B/0.9900, 0.4→245 B/0.9816, 1.0→184 B/0.9145. min-error is one point (λ=0); RD gives
  the whole curve = an explicit rate knob. Default `LAMBDA = 0.15` (≈0.99 quality, mildly
  rate-aware); brick 5's `-q`/rate control sets it per target.
- **Gates:** sine 0.9911 / multitone 0.9958 / broadband 0.9900 (lewton); ffmpeg decodes the
  RD-residue `.ogg` cleanly. 20/20 tests, clippy clean.
- **Note:** distortion is measured in *residue* space (what the VQ minimises). Perceptual
  (floor-weighted / masking) distortion is brick 5; on the whitened residue the two are close.

### Brick 5 — Psychoacoustic model + `-q` — DONE ☑

The floor stopped being an energy envelope and became a **masking threshold** (`encode/psy.rs`),
and one normalized `quality` knob now drives both rate levers coherently.

- **`masking_curve`** — per-Bark-band energy → asymmetric spreading (−10 dB/Bark up, −27 down,
  same as the AAC model) → sit the threshold a tonality-dependent ratio below the spread
  signal (tonal bands ~24 dB = coded precisely, noise ~6 dB = coarse; SFM = spectral flatness),
  bounded to `[peak·1e-4, peak·0.5]`. The lower bound is a crude ATH; the **upper bound keeps
  the floor below the frame peak** so dense signals whose summed masking exceeds every bin
  don't black out (found + fixed via the `-q` sweep). The floor is fit to this curve (brick 3).
- **`quality` (0..1) → two coordinated effects:** shifts the threshold (`q_db = (q−0.5)·30`,
  offset clamped ≥0) *and* sets the residue `lambda` (`lambda_for_quality`). `VorbisEncoder`
  reads `-q` (−1..10 → `quality01_from_vorbis_q`, clamped [0.05, 0.98]); default 0.6.
- **`-q` sweep (dense broadband, lewton):** monotonic and dropout-free — q=0.1→20 kb/s/0.24,
  0.5→47/0.96, 0.7→76/0.99, 0.9→106/0.997.
- **vs ffmpeg libvorbis (real audio, first 4 s of bench_in.wav):** our q=0.3→23 kb/s (corr
  0.915), 0.7→71 kb/s (0.994), 0.9→134 kb/s (0.998); ffmpeg -q1→80, -q3→95, -q5→120 kb/s. We
  span *lower* than ffmpeg and reach its −q3–5 bitrate range at the top, at high reconstruction
  fidelity. ffmpeg decodes our masking-floor `.ogg` cleanly. 23/23 tests, clippy clean.
- **Honest caveats:** correlation ≠ perceptual parity (ffmpeg's psy is more mature; a real
  NMR/PEAQ bench + the shared quality harness is the deeper gate), and we're **long-blocks-only**
  so transient handling (short blocks) is still brick 7.

### Brick 6 — Channel coupling — DONE ☑

- **Coupling (the win, validated):** the forward polar couple (brick 2, inverts lewton's
  `inverse_couple`) already turns correlated stereo into (magnitude, ~0 angle) — the angle
  vanishes for L=R and the residue RD drops it for free. Gate `coupling_compresses_correlated_stereo`:
  on a broadband signal, **decorrelated stereo costs ~1.75× the bytes of correlated** at q=0.5.
- **Point stereo (`psy::point_stereo_bin`):** collapses the coupling angle to mono above a cutoff.
  Investigated thoroughly and characterized honestly — it only saves bits by dropping *audible*
  high-frequency stereo (collapsing already-masked content saves nothing, so it's **redundant
  with the masking floor** in the safe range). So it's gated as a **low-bitrate lever**: active
  only below q 0.55 with an aggressive ~5.5 kHz cutoff (17% saving on wide stereo at q=0.5), off
  (full stereo) at normal/high quality, and a no-op for correlated stereo. Decodes in lewton + ffmpeg.
- 24/24 tests, clippy clean.

### Brick 7 — Container + engine + presets — DONE ☑ (block switching deferred)

**The encoder is usable end to end: `rff -i in.wav out.ogg -c:a vorbis -q:a N` produces a valid
Ogg Vorbis file that ffmpeg + our own stack decode.**

- **Streaming `Encoder`** (`encode/mod.rs`): buffers input, encodes long blocks (hop 1024) via
  `encode_long_packet`, pads the final block on flush. **Emits the 3 setup headers as the first
  packets** (the natural Ogg logical-stream order) — this sidesteps the extradata-before-first-frame
  timing problem cleanly, with no changes to the generic `Encoder` trait or engine.
- **Registration flipped** to `encoder: Some(VorbisEncoder::new)`; `pub use VorbisEncoder`.
- **Ogg muxer Vorbis support** (`rff-format-ogg`): `write_header` accepts Vorbis; `write_vorbis`
  pages the first 3 packets as header pages (BOS + 2) then the audio packets (1024-sample granule),
  matching the demuxer's "first 3 are headers" convention. Opus path untouched.
- **CLI `-q:a` / `-qscale:a`** wired (`rff-cli/args.rs` → `options["q"]`, encoder reads it) — a
  real quality knob: on 4 s of real music, **-q:a 0→40, 3→42, 6→72, 9→156 kb/s** (monotonic).
- **Validation:** `ffmpeg` decodes our `.ogg` cleanly (recognized `vorbis, 44100 Hz, stereo`),
  ~0.92 waveform correlation on real music; **round-trips through our own lewton-backed decoder**
  (`rff -i our.ogg out.wav`). Regression tests: `streaming_encode_decodes_in_lewton`,
  `ogg_mux_then_demux_roundtrips_vorbis`. 25 crate + 4 ogg tests pass, clippy clean.

**✱ Deferred — block switching (short blocks for transients).** Long-blocks-only produces valid,
good-quality Vorbis; transient handling is a bounded quality enhancement (mode-0 short blocks use
floor 0 / residue 0, plus the LongStart/LongStop window-flag transitions matching lewton's
overlap-add — the AAC-style block-switch, adapted to Vorbis's per-packet window flags). The
generalized `encode_packet(mode)` + a transient detector + the transition windows are the work.

### Performance pass (profile-first) — DONE ☑ (64× slower → 5.3× faster than libvorbis, PEAQ-neutral)

Side-by-side vs ffmpeg's libvorbis on 6 s of real music revealed we were **64× slower**
(0.8× realtime). Profiled the encode phases (`encode/frame.rs` test-only `prof` counters):

- **MDCT was 85%** — a direct O(N²) transform with a `cos()` in the inner loop. Fixed with a
  **cached cosine twiddle table** (the cos values never change; `encode/mdct.rs::mdct_twiddles`)
  + an **8-accumulator vectorized dot product** (`dot8`). MDCT 4.56 s → 0.19 s (**24×**), validated
  against the kept `mdct_direct` scalar oracle.
- **Classify then dominated (64%)** — the residue classifier brute-forces VQ over the dim-8 /
  6561-entry book 28. That book (and 5 others) is a **full separable lattice** (lookup-1,
  non-sequential): each dimension picks independently, so the nearest entry is the per-dimension
  nearest — `O(dim·levels)` vs `O(entries·dim)`, ~24 ops vs 52 k. `Codebook::lattice` +
  `lattice_quantize_matches_brute_force` (bit-exact to brute force). Classify 0.75 s → 0.36 s.
- **Single-thread result:** the 159-block bench went 5.35 s → 0.58 s (**9.3×**); the CLI encode
  went 0.8× → 6.4× realtime, ffmpeg gap **64× → 7×**. Quality-neutral (correlations unchanged).
- **Frame-parallel (the structural win, `encode/mod.rs::produce_all`):** each `encode_long_packet`
  is a pure function of its 2048-sample window and libvorbis is single-threaded per stream, so the
  streaming encoder now *buffers* input and encodes every block **in parallel across cores**
  (`std::thread::scope`, chunked, order preserved → deterministic, identical output). On a 24-core
  box: **CLI encode 0.8× → 65× realtime**, closing the ffmpeg gap from 7× to **1.2× — near parity**
  (ours 918 ms vs libvorbis 746 ms on a 60 s file). ffmpeg + lewton still decode the output.
- **Compact used-entry VQ (the parity-crossing brick):** the profile found the dim-8 residue book
  is *sparse* — only **81 of 6561 entries used** (as are books 29/30/31/32/33/36), so the brute-force
  looped all 6561 skipping 6480 via a branch. Packing the **live entries contiguously**
  (`Codebook::used_vq`/`used`) and iterating only those (~81, dense, no gather) cut classify
  **2.3× (0.33→0.14 s)**, bit-exact/quality-neutral. This pushed the 60 s CLI encode past libvorbis:
  **ours 602 ms (99.7× realtime) vs libvorbis 739 ms (81×) → we are 1.23× FASTER.**
- **FFT MDCT (O(N log N)) — the 46% lever, cashed:** replaced the O(N²) cos-twiddle-table + `dot8`
  transform with the textbook **N/4-point FFT MDCT** (fold N→L=N/2 by TDAC, pre-rotate into M=N/4
  complex, one M-point radix-2 f64 FFT, post-rotate to the N/2 coeffs — 4× fewer FFT points than a
  length-N transform). Ported verbatim from the AAC crate's verified `dsp::mdct_fast`: the AAC and
  Vorbis MDCTs share the **exact cosine basis** (both n₀ = M/2+½), differing *only* in scale — AAC
  ×2, Vorbis 2/M, so `vorbis_mdct = aac_mdct / M`; the only edit was swapping the `×2` unpack for
  `×(2/l)`. Twiddles cached per block size (2048 now, 256 pre-wired for future short blocks). The
  f32 `mdct_direct` stays as the `#[cfg(test)]` oracle. **MDCT per-core 0.148 s (46%) → 0.003 s
  (1%) — a ~50× stage collapse**, quality-neutral (oracle ≤1e-3, TDAC round-trip <1e-3, lewton +
  ffmpeg decode clean). The 60 s CLI encode: **ours ~348 ms (~173× realtime) vs libvorbis ~626 ms
  (~96×) → we are 1.80× FASTER.**
- **Classify (residue-VQ search) — the 85% lever, hammered.** With MDCT gone, the RD classifier
  (each partition trials all 10 residue classes; the hot ones are brute-force nearest-neighbour VQ
  over the sparse books — class 6 = 217-entry dim-2, class 8 = 143, class 1 = dim-8) dominated at
  85%. Four byte-identical bricks took the stage **0.175 s → 0.065 s (~2.7×)**: (1) **structure-of-
  arrays** layout for the used-entry dictionaries (each dimension's column contiguous); (2) a
  **branchless two-pass** distance loop (compute all squared distances, *then* argmin — the fused
  `best_cost` branch was a loop-carried dependency); (3) reformulating the argmin as **min-value +
  first-equal** (a vectorizable reduction plus an early-exit scan, exactly the scalar first-wins
  result); (4) an **AVX2** distance kernel (`mul`+`add`, *no* FMA → bit-identical; runtime-detected,
  `simd` feature, scalar fallback) plus a **reusable scratch buffer** (killed 550 `to_vec` allocs
  per block). All gated by a new `brute_quantize_matches_reference` test at λ∈{0, .05, .15, .4}
  (the RD term participates in the argmin — the brute books have non-uniform lengths, so the
  lattice shortcut does *not* apply). ★ Two findings: the pure-Rust distance loop **would not
  auto-vectorize** even at SSE2 baseline (`--emit asm` showed 0 packed ops — a two-provenance
  scratch pointer defeated alias analysis; the real win was the *reformulation*, not the SIMD), and
  a fully-AVX2 argmin (vectorized find-first + horizontal min) **regressed** vs the scalar
  early-exit `position` — reverted.
- **Net journey:** **64× slower → 3.6× FASTER** (60 s stereo, 24 cores; ~331× realtime), every step
  quality-neutral (byte-identical output, ffmpeg-decodable), every step decoder-validated.
  Single-thread the gap to libvorbis closed **4.7× → 2.3× behind** (2755 → 1415 ms).
- **Energy-bucket class shortlist (the PEAQ-gated lever) — DONE ☑.** Classify was still ~73%
  per-core because the RD classifier trialed all 10 residue classes per partition. The residue
  classes form an energy ladder (verified: chosen-class medians climb monotonically with partition
  energy, ≈ `0.47·dB − 2.3` classes), so instead of trialing all ten we predict a centre class from
  the partition's residual energy and trial a **±3 window** around it (plus class 0, always a
  candidate). This changes the encoding decision, so it is **not** byte-identical — it is gated
  **perceptually** against the exhaustive search: **ΔODG ≤ 0.03** (PEAQ, CC0/PD piano+guitar at
  q 0.6–0.8), i.e. perceptually neutral, with bitrate within ~0.7%. Window default 3 (env
  `VORBIS_CLASS_WINDOW` overrides for A/B; a large value restores the exhaustive RD baseline).
  **Classify 2.3× faster** → on stereo music: **single-thread 2.2× → 1.4× behind libvorbis**
  (1.53×), **parallel 3.7× → 5.3× faster** (~457× realtime). ★ The bench must be **real music**:
  a dense synthetic signal (uniform high energy) overstates the win to single-thread *parity*;
  real music has many cheap low-energy partitions, so the honest figure is ~1.4× behind.
- **PEAQ perceptual gate (unblocked + first benchmark).** The `tools/quality` PEAQ oracle was
  broken under numpy≥2 (`computeBW` returned size-1 arrays / called `int()` on them); fixed in
  `setup_peaq.py` (validated against the bundled MATLAB reference, ODG −3.875). Our decoder isn't
  end-trimmed (leading priming samples, so the decode *leads* the reference) — the stock coarse
  aligner reads garbage, so `tools/quality/peaq_align.py` does a **signed** sample-accurate
  alignment. First real perceptual numbers (the long-open "corr ≠ perceptual, never benchmarked"
  question): at ~155 kb/s our encoder is **ODG ≈ −0.24** (lewton decode) / −0.36 (ffmpeg) vs
  libvorbis −0.06 — genuinely decent, libvorbis still ahead. (Also confirmed the encoder core is
  sound end-to-end: ffmpeg and lewton decode our packets identically, corr 0.996.)

### Also still open (quality validation)
- Real NMR/PEAQ perceptual bench vs libvorbis at matched `-q` (correlation ≠ perceptual parity).
- Sample-accurate granule/trim at the stream ends (currently ~1 hop of encoder delay).
