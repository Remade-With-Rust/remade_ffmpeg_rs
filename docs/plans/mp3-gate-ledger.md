# MP3 encoder — gate ledger

Great-gate P4 for `rusty_mp3`. One row per decision function: its gate status, the
per-class evidence, its bounded downside, and how to regenerate the table.

**Regenerate everything here with:**

```sh
cargo build -p rusty_mp3 --release --example encprof --example bscensus
cargo run -p rusty_aac --features lab --release --example aacexport -- <corpusdir>
python tools/quality/ladder.py --arms base,slack --rates 96,128,160,192 --dirs <corpusdir>
```

**Measurement method** (binding, from `codec-measurement`): quality verdicts are
external PEAQ (`tools/quality/PEAQ_python`) on a neutral decoder (ffmpeg), judged
per clip at 4 bitrates, never on a mean alone. PEAQ is deterministic, so the ladder
parallelises across processes and takes **no** timing. Encode timing is separate:
in-process stage total, arms ABBA-interleaved with the leading arm alternated, 12
rounds, min and median both reported.

---

## Corpus (P0)

| source | clips | classes covered |
|---|---|---|
| `corpus/corp_long_mus_*.wav` | guitar, piano, vocal — 24 s, real, CC0/PD | tonal music |
| `rusty_aac` `aacexport` | 8 × 2 s deterministic synthetic | speech-clean, speech-noisy, music-tonal, percussive, noise-like, stereo-wide, quiet-dynamic, mixed-speech-music |

The synthetic set closes the classes no CC0/PD real audio was available for
(a Commons search found no free castanet/glockenspiel material); great-gate §2
sanctions synthesizing gap classes. **Known corpus gaps:** no real percussive
recording; `stereo-wide` is scored on channel 0 only, because `peaq_run.py` takes
one channel — a stereo verdict needs per-channel scoring averaged.

> ⚠ **The synthetic set is valid for A/B deltas between our own arms, and NOT for
> cross-encoder absolute ranking.** Scored against LAME it claims we are 0.54–2.17
> ODG behind per class — while our decoded SNR is *higher* than LAME's on every one
> of those classes (noise-like 19.9 vs 9.9 dB, speech-noisy 24.6 vs 15.2,
> percussive 35.5 vs 29.6, music-tonal 59.9 vs 30.5 at 192 kbps). A 30 dB SNR
> advantage reading as a 0.79 ODG loss on synthetic tonal content, where *real*
> tonal content reads 0.28, is the metric failing on 2 s synthetic signals, not the
> codec. Both arms of an A/B get identical treatment so the deltas survive; an
> absolute ranking does not. **Take every vs-LAME number from the real clips.**

### Per-class truth table — does the corpus exercise the mechanism?

Measured at 128 kbps with `encprof` (deterministic counters, one run):

| class | % short blocks | shaped granules | refine steps | bits/granule unspent |
|---|---:|---:|---:|---:|
| mixed-speech-music | 31.2 | 68.9 | 196 | 57 |
| music-tonal | 0.6 | 64.7 | 261 | 39 |
| noise-like | 1.9 | 72.8 | 915 | 33 |
| percussive | 5.2 | 75.3 | 493 | 39 |
| quiet-dynamic | 2.6 | 76.0 | 416 | 41 |
| speech-clean | 31.8 | 81.0 | 902 | **350** |
| speech-noisy | 1.3 | 82.2 | 944 | 34 |
| stereo-wide | 3.9 | 51.4 | 374 | 19 |

Two open findings from this table, both unexplained:

- **The percussive class fires 5.2% short blocks while speech-clean fires 31.8%.**
  For the class that exists to stress the block-switch detector, that is backwards.
  Either the detector responds to speech onsets far more than to sharp attacks over
  near-silence, or the synthetic percussive signal's clicks are sparse enough that
  most granules are silence. Not yet diagnosed.
- **speech-clean leaves 350 bits/granule unspent** (~22% of payload at 128 kbps) --
  **DIAGNOSED, and mostly irrecoverable at CBR.** The granule-fill histogram puts the
  waste entirely in **19% near-empty granules** (music has 0% there), i.e. the silences.
  Those should donate to the reservoir, and measurably do not: across the whole speech
  file the reservoir lends **28 bits total**, 6 bits average on 3.2% of granules.
  The bound is the format's: `main_data_begin` is 9 bits, so the bank caps at 511 bytes
  = 4088 bits = **2.4 granules' worth**, while silence offers ~318 bits/granule
  continuously. The bank saturates and the rest is discarded -- it cannot be carried.
  Re-swept `MP3_RESV_GAIN` 0.5/1.0/2.0/4.0 on the class ladder (the previous sweep,
  recorded INERT, ran on three tonal clips with **0%** near-empty granules -- priced
  where the mechanism does not operate): mean still flat (-0.003..+0.002), and per
  class it is a **sign flip** -- quiet-dynamic +0.050 at gain 2.0, percussive -0.041 at
  the same gain, speech-clean -0.009. Both responses are **non-monotonic** (peak and
  dip both at 2.0, recovering at 4.0), which by the tune-quality law means state
  divergence rather than an RD trade, so **no constant is fitted**. Conclusion: for
  speech-like content with long silences the answer is VBR (smaller frames for
  silence), not a CBR reservoir knob.

---

## Gates

### G1 — CBR noise shaping: `quantize::loops` (✅ gated, default ON)

**Unit** granule · **arms** `slack` (default) / `outer` / off · **fallback**
`MP3_SHAPE=outer MP3_PSY_DOMAIN=0`, proven **byte-identical** to the pre-change
encoder with `cmp` on all three real clips.

The rate loop picks the smallest `global_gain` that fits. A gain step is ~1.5 dB, so
it cannot land on the budget exactly and the remainder was emitted as stuffing.
`loops_slack` holds that gain **fixed** and amplifies bands while the result still
fits. Each band's levels depend only on the gain and its own scalefactor, so no band
ever gets coarser — every accepted step is a strict improvement, not a trade, paid
from bits that were already bought.

Bitstream census, 192 kbps (`bscensus`, counts from the decoded side info):

| | shaped granules | non-zero sfb | sf spread | bits unspent |
|---|---:|---:|---:|---:|
| before | **0.0%** | — | — | 63–89 |
| after | 67–71% | 1.9–2.7 | 1.3–2.0 | 17–27 |
| LAME (same clips, same rate) | 71–77% | 2.5–3.8 | 2.5–2.8 | 5–7 |

**Verdict:** synthetic 8-class ladder **+0.0447 ODG mean, 29/32 points better**
(sign z = +4.6); real music ladder **+0.0199, 10/12 better**. Combined 39/44,
z = +5.1, positive at all four bitrates.

**Bounded downside:** worst single point −0.027 ODG (`corp_long_mus_guitar` @192k,
reproduced); three classes are MIXED (mixed-speech-music, percussive, stereo-wide)
with worst points −0.016, −0.020, −0.017. Every loss is ≤0.027 ODG, an order of
magnitude inside PEAQ's own perceptual validity (~±0.2), and the win is spread
across all 8 classes rather than carried by a few — a broad win, not a clip list, so
no per-class dispatch is fitted. **Provisional on:** the guitar@192k point, which is
the row to re-check whenever the rate loop underneath moves.

**Speed:** +15–18% encode (12 interleaved rounds: min 1.150×, median 1.148× in one
session; 1.165×/1.175× in another — the box drifts ~5%, so read it as ~+16%).
~173× realtime. Refinement is band-local: it re-quantizes only the band it changed
and updates one entry of the noise vector, which took the cost from +33% to +16% at
**byte-identical** output.

**The bug this gate sits on.** Before it, the loop never shaped anything, because
`n > psy.thresholds[b]` compared MDCT-domain squared error against unnormalized
1024-point FFT power. Measured gap **10^4.90 (~79,000×, 49 dB)**, reading 4.90 /
4.90 / 4.92 on three different clips — a units mismatch, not a property of the
signal. Consequences, all counted: 98.7% of bands scored >6 decades under their
mask, **zero** bands were ever at or above it, the loop broke on its first iteration
in **100%** of granules, and we shipped one global gain per granule. `loops_vbr` has
carried the identical correction since 0.6.0 with a comment describing the same
~10⁴ discovery; it was never carried across. This **voids** the earlier conclusion
that "at a hard per-granule budget every within-frame trade is zero-sum" — that
reasoned about a mechanism that never executed — and explains why the
`SMR_OFFSET_DB` sweep read inert.

### G2 — scalefactor range (✅ fixed, no arm)

`MAX_SF = 15` guarded every band, but `scalefac_compress` gives bands 11..20 only
`slen2 ≤ 3`, i.e. 0..7. `choose_compress` found no covering entry, fell back to
`(4,3)`, and the serializer **silently truncated** — the decoder then requantized
that band with a scalefactor up to 16× too small. Measured symptom: decoded peak
1.92 against a source peaking 1.30, max sample error 1.24 on a 1.30 signal.

Fixed by `max_sf(b)` (15 below band 11, 7 above), a `debug_assert` in
`choose_compress` so the case cannot recur silently, and two regression tests.
**Reachability:** `prof::SF_OVERFLOW` counts **0** across the whole `-q:a` ladder on
the music corpus, so this was latent, not shipping — it became reachable the moment
G1 started shaping and corrupted a granule on the first clip.

### G3 — shaping is MPEG-1 only (✅ gated, no arm)

`bitstream.rs` serializes scalefactors with `SCALEFAC_COMPRESS_V1` unconditionally,
but MPEG-2/2.5 read `scalefac_compress` as a different 9-bit four-group field. V2/2.5
had always emitted flat scalefactors so it never mattered; enabling G1 took MPEG-2
round-trip SNR to **5.7 dB**. `shaping_allowed()` restricts shaping to MPEG-1 until
the LSF scalefactor scheme exists. **Missing arm** (great-gate §3 category 1) —
build the arm before gating it.

---

## P1 signal audit — what the psychoacoustic model contributes

Each row re-ranks which band receives the next refinement bit; all else identical.
Ladder vs the shipped noise-to-mask ranking, 8 classes × 4 rates:

| ranking signal | mean Δ ODG | better/worse |
|---|---:|---|
| noise-to-mask (shipped) | — | — |
| raw noise energy (ignores the model) | **−0.0298** | 4/27 |
| noise per line | −0.0364 | 9/23 |
| lowest frequency first (model-free null) | −0.0475 | 2/30 |

**The masking model's ranking is worth +0.030 ODG over ignoring it** (27/32,
z = +3.9). The model earns its place.

### The level/shape law

| knob | mean Δ ODG | better/worse | z |
|---|---:|---|---:|
| `MP3_SMR_DB=9` | +0.0068 | 22/10 | +2.12 |
| `=15` | −0.0018 | 18/14 | +0.71 |
| `=21` | −0.0022 | 17/15 | +0.35 |
| `=27` | +0.0004 | 17/15 | +0.35 |

The old "SMR is not the lever" refutation **survives, for a new reason.** It used to
be untestable (a few dB against a 49 dB scale error); now it is live and still moves
almost nothing — because:

> **At a fixed bit budget the refinement is a RANKING problem, and a global
> signal-to-mask offset is very nearly a common factor on every threshold, so it
> cancels out of the ranking by construction. The psymodel's absolute LEVEL cannot
> matter here; only its SHAPE across bands can.**

That retires level-tuning as a lever and points calibration at anything that changes
the threshold's shape: the spreading function, tonality weighting, the ATH curve's
shape, and the band-energy cap.

### The band-energy cap — measured, NOT banked

`thresholds[i] = (masker·smr).max(ath).min(energy[i])`. That cap binds on **37–48%**
of bands, and where it binds the threshold *is* the band's own energy, so the ratio
becomes `1/SNR_band` — a signal-independent quantity. It flattens the masking shape
on nearly half the spectrum. `MP3_THR_CAP` scales it (1.0 = shipped, byte-identical).

| arm | mean Δ ODG | better/worse | worst | z |
|---|---:|---|---:|---:|
| `cap4` | +0.0126 | 20/12 | −0.064 | +1.41 |
| `cap16` | +0.0149 | 22/10 | −0.070 | +2.12 |
| `cap1e6` | +0.0133 | 19/13 | −0.157 | +1.06 |

**Not banked.** The three settings span a 250,000× range and give the same
+0.013 ± 0.002 — a plateau, not an optimum, meaning the whole effect is "the cap
binding at all" and there is no threshold to fit. z = 2.12 at best, with a −0.07
worst point, is one ladder short of a verdict under the three-probe rule. Recorded
with its knob in place so it is a one-line re-test rather than a rediscovery.

---

## Instruments added

| tool | what it answers |
|---|---|
| `examples/bscensus.rs` | reads ANY mp3 (ours, LAME's) and counts the encoder's decisions from the side info: block mix, % flat vs shaped granules, non-zero sfb, sf spread, scfsi/preflag/subblock_gain use, gain range, and payload utilisation |
| `examples/encprof.rs` (extended) | outer-loop iterations, NMR histogram over coded bands, threshold-source split (cap / ATH / masker), the FFT-vs-MDCT domain ratio, accepted refine steps, `SF_OVERFLOW`, `VBR_Q` to drive the VBR quantizer, optional mp3 output |
| `tools/quality/ladder.py` | the per-class × per-bitrate PEAQ ladder, arms as env configs, per-clip/per-rate tables with sign splits, CSV for the gate calculator |

`prof::note_nmr` buckets by comparison rather than `log10`: it runs once per band per
granule on the shipping path, and a transcendental there would be the instrument
charging the thing it measures.

---

## Next

1. **Shape-changing psymodel knobs** — spreading function slopes, tonality-dependent
   SMR (per-band, not global — a per-band offset changes shape and is *not* excluded
   by the level/shape law above, unlike the global one that was refuted twice).
2. **The percussive/speech short-block inversion** (5.2% vs 31.8%) -- still open.
   The speech-clean unspent-bits finding is closed (see above).
3. **Short-block thresholds** — `psychoacoustic::analyze` still hardwires
   `block_type: Long` and emits no short-block grid; `detect_attack`'s `RATIO = 10`
   is coupled to that and cannot be tuned until it exists.
4. **A real percussive clip** and per-channel stereo PEAQ, to close the corpus gaps.
