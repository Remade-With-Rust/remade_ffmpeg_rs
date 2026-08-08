# AAC — the Great Gate campaign: missing audio structures → competitive with ffmpeg AAC

**Mission.** Close the structural gap between `rusty_aac`'s encoder and
`ffmpeg -c:a aac` at matched bitrate, and do it under the Great Gate law: every
structure we add is an **arm**, every arm ships behind a **gate**, and the finish
line is **worst content class ≤ 0 ODG, verified per class, never on average**.

Governing documents: [`_greatgate/great-gate.md`](../_greatgate/great-gate.md)
(the seven architecture laws, the canonical gate form, the calculator's instrument
audit, the symbolic-leaf deployment law). Governing skills:
`codec-content-adaptive-dispatch` (gate construction), `codec-tune-quality`
(external-oracle verdicts), `codec-measurement` (what makes a number admissible).
Allocator convention: [`CLAUDE.md`](../CLAUDE.md).

Companion doc: [`codec-aac-encoder.md`](codec-aac-encoder.md) — the brick 1–7 ledger
that got us to a working encoder. **This doc starts where that one stopped.**

---

## 0. The census (great-gate §3) — what is actually missing

A full sweep of [`crates/rusty_aac/src/encode.rs`](../crates/rusty_aac/src/encode.rs)
against the seven census categories. The dominant category is **1 — missing arms**,
which sets the campaign's shape: *no gate can route to an arm that doesn't exist.*

| # | Structure | Census category | Evidence | Decoder side |
|---|---|---|---|---|
| A1 | **Short-block psy model** | 1 — missing arm | `rate_loop_short` takes no `offsets`; flat SF across all bands ([encode.rs:1152](../crates/rusty_aac/src/encode.rs#L1152), [:1298](../crates/rusty_aac/src/encode.rs#L1298)) | n/a (SF already per group) |
| A2 | **Tonality-adaptive SMR** | 4 — named-signal-shipping-as-constant | `const SMR: f64 = 0.0158` with a doc-comment naming the physics it tracks ([encode.rs:384-385](../crates/rusty_aac/src/encode.rs#L384-L385)) | n/a (encoder-only) |
| A3 | **TNS** | 1 — missing arm | `tns_data_present` hardcoded 0 at all 3 emit sites ([:954](../crates/rusty_aac/src/encode.rs#L954), [:1238](../crates/rusty_aac/src/encode.rs#L1238), [:1382](../crates/rusty_aac/src/encode.rs#L1382)) | ✅ **`apply_tns` live** ([decode.rs:600](../crates/rusty_aac/src/decode.rs#L600)) |
| A4 | **Absolute threshold of hearing** | 2 — signal with no consumer | zero hits for ATH/loudness/SPL in the encoder; mask is purely relative | n/a (encoder-only) |
| A5 | **Bit reservoir / VBR** | 1 — missing arm | `frame_budget` constant for the whole stream ([encode.rs:1635](../crates/rusty_aac/src/encode.rs#L1635)); `buffer_fullness` hardcoded `0x7FF` ([:215](../crates/rusty_aac/src/encode.rs#L215)) | n/a |
| A6 | **PNS** | 1 — missing arm | encoder never emits `NOISE_HCB` | ✅ **noise fill live** ([decode.rs:310](../crates/rusty_aac/src/decode.rs#L310)) |
| A7 | **Intensity stereo** | 1 — missing arm | encoder never emits `INTENSITY_HCB` | ✅ **`apply_is` live** ([decode.rs:711](../crates/rusty_aac/src/decode.rs#L711)) |
| A8 | **Window shape (KBD/sine)** | **3 — free syntax element** | `window_shape_kbd: false` hardcoded ([:1227](../crates/rusty_aac/src/encode.rs#L1227)); "we always use sine shapes" ([:304](../crates/rusty_aac/src/encode.rs#L304)) | ✅ both shapes ([decode.rs:65-67](../crates/rusty_aac/src/decode.rs#L65-L67)) |
| A9 | **Transient detector threshold** | 5 — threshold ignoring content | `const RATIO: f64 = 10.0`, an **absolute** ratio — violates law 1 ([:968](../crates/rusty_aac/src/encode.rs#L968)) | n/a |
| A10 | **M/S decision objective** | 5 — threshold ignoring content | pure energy product `E_M·E_S < E_L·E_R`; no bit cost, no BMLD ([:1273](../crates/rusty_aac/src/encode.rs#L1273)) | ✅ M/S live |
| A11 | **Coded bandwidth** | 2 — signal with no consumer | `max_sfb` falls out of nonzero bands; no deliberate band limiting | ✅ by construction |
| A12 | **Distortion loop** | 1 — missing arm | `rate_loop` searches ONE global base; per-band offsets computed once analytically, never iterated against per-band threshold ([:836](../crates/rusty_aac/src/encode.rs#L836)) | n/a |
| — | **Category 7 hygiene** | 7 | three stale doc claims, see §2 | — |

**The de-risk.** A3/A6/A7 — three of the four biggest missing arms — already have
working decode support. Gate #1 of the encoder gate (round-trip through our own
decoder) works for them on day one. The stale [decode.rs:8-10](../crates/rusty_aac/src/decode.rs#L8-L10)
doc-comment claiming otherwise is precisely the category-7 finding great-gate warns
about: *a fit against a mis-documented codec fits the wrong codec.*

**What we are NOT missing.** Frequency masking (Bark spreading), block switching,
M/S, frame-parallel encode, and AVX2/AVX-512 quantize kernels are all present and
gated by tests. This campaign adds structures; it does not rebuild the foundation.

---

## 1. P0 — Corpus + the verdict instrument (blocking everything)

**There is no AAC quality harness today.** No PEAQ driver, no NMR bench, no bitrate
ladder — unlike MP3 (`mp3quality`, `mp3lab`, `lab::quality`). Per
`codec-tune-quality`, no brick below can be banked until this exists. This is the
first work item, not a preamble.

### 1.1 Content classes (great-gate §2, audio family)

| Class | Source | Which arms it stresses |
|---|---|---|
| Clean speech | corpus | A3 TNS, A2 tonality |
| Noisy speech | corpus | A6 PNS, A4 ATH |
| Tonal/harmonic music (guitar, piano) | corpus | A2, A8 KBD, **A6 PNS anti-class** |
| **Percussive/transient (castanets, glockenspiel)** | corpus + synth | **A1, A3, A9** — the block-switch stressors |
| Noise-like (applause) | corpus | **A6 PNS** — the win class |
| Wide-stereo music | corpus | **A7 intensity, A10 M/S** |
| Quiet / wide-dynamic-range | **synthesize — corpus gap** | **A4 ATH, A5 reservoir** |
| Mixed speech+music | **synthesize — corpus gap** | the variable-content class; exposes every unfinished dispatch |

The last two are **corpus gaps** under great-gate §2 ("corpus-neutral on a feature
with a known physical premise is a corpus gap"). A4 and A5 have explicit physical
premises about absolute level and about level *varying over time*; a corpus of
normalized-loudness music clips cannot judge them. Synthesize: a −40 dBFS passage
spliced against a 0 dBFS passage, and a speech/music alternation at ~5 s grain.

### 1.2 The ladder harness

- **External oracle: PEAQ/ODG**, reusing `tools/quality/` (`peaq_run.py`,
  `peaq_align.py`, `PEAQ_python`) — the same instrument that produced the Opus and
  MP3 rankings. A self-metric alone is refused (`codec-tune-quality`).
- **≥4 operating points**: 64/96/128/192 kbps ladder, per clip.
- **Anchor**: `ffmpeg -c:a aac` at matched bitrate. Decode both candidates with the
  **same neutral decoder** (ffmpeg) so the comparison isolates the encoder.
- **NMR as the fast iteration metric** (via a new `rusty_aac::lab::quality`
  mirroring `rusty_mp3`'s), validated once against PEAQ then used for inner loops.
  PEAQ is the verdict; NMR is the screen.

### 1.3 New target roots — and the allocator (see §6)

| New root | Purpose | Allocator |
|---|---|---|
| `crates/rusty_aac/examples/aacquality.rs` | ODG/NMR ladder driver | **required** |
| `crates/rusty_aac/examples/aacharvest.rs` | emits the gate-calculator CSV | **required** |
| `crates/rusty_aac/benches/aac_encode.rs` | the `cpu_ms` half of every gate's speed pair | **required** |

`rusty_aac/Cargo.toml` has **no `[dev-dependencies]` block today** — it must gain
one carrying `rusty_alloc-api.workspace = true`, and each root the
`#[global_allocator]` declaration. This is audit trap #2 from the root
`Cargo.toml`: benches and examples are separate compilation units, and the measured
gap on AV2 decode was **1.38×**. A ladder measured on the system heap is not
comparable to the shipped binary.

**Exit P0:** `aacquality` reproduces a per-clip ODG table vs ffmpeg across 8 classes
× 4 bitrates, and a null arm (ours vs ours) reads 0.00 ± noise floor.

---

## 2. P0.9 — The hygiene batch (before any fitting)

Great-gate §3 category 7: *fix these BEFORE any fitting.*

1. **[decode.rs:8-10](../crates/rusty_aac/src/decode.rs#L8-L10)** claims TNS is
   "parsed but not applied" and PNS/intensity are "rejected." All three are live.
   Correct the doc.
2. **[codec-aac-encoder.md](codec-aac-encoder.md) brick 4** claims an "outer
   distortion loop." There is none — `rate_loop` searches a single global base and
   the per-band offsets are computed once, analytically, never iterated against the
   per-band threshold. Correct the ledger, and open A12.
3. **`encode.rs` module doc** still describes the crate as "brick 1 … the
   scaffolding." Seven bricks landed. Correct.
4. **Consistency sweep of the psy/λ forms.** `perceptual_offsets` (long) and the
   flat-SF short path use structurally different objectives. Any fit spanning both
   learns the inconsistency, not the content.

---

## 3. P1 — The signal audit: one vector, all gates

Great-gate §6 P1: *consolidate duplicated probe skeletons into ONE per-frame signal
vector that all gates read.* Today `detect_transients` computes sub-block energies
and `perceptual_offsets` recomputes band energies — two walks over the same data,
neither reusable.

Build `AacSignals`, computed once per frame per channel, harvested **at decision
time**, and validated against a per-class truth table **before** any gate is wired:

| Field | Axis (great-gate §2) | Definition | Consumers |
|---|---|---|---|
| `attack_ratio[8]` | Transient density | sub-block energy vs running avg (exists, needs exposing) | A1, A9 |
| `sfm[sfb]` | Tonality vs noise | spectral flatness = geo-mean / arith-mean of \|X\|² per band | **A2, A6, A8** |
| `lpc_gain` | Transient (spectral domain) | prediction gain of order-N LPC **over the spectrum** | **A3** |
| `loudness_dbfs` | Silence / activity | frame RMS and peak, dBFS | **A4, A5** |
| `pe` | — (derived) | perceptual entropy `Σ nlines·log2(E/thr)` | **A5** (the reservoir demand signal) |
| `xcorr[sfb]` | Stereo correlation | inter-channel correlation per band | **A7, A10** |
| `rolloff` | Bandwidth | spectral rolloff frequency (95% energy) | **A11** |

Two binding rules from great-gate §2:

- **Harvest at decision time.** A tap placed after quantization measures
  quantization, not content.
- **The signal must predict the ODG verdict, not "activity."** Instrument ≥3
  candidates per axis (e.g. tonality: SFM vs the classic unpredictability measure
  vs LPC gain) and keep the one that correlates with the per-class ODG delta.

**Law 1 conversion.** Every threshold derived from these is a **percentile of this
clip's / this frame's own distribution**, never an absolute. That is the specific
defect in A9 today (`RATIO = 10.0`), and it is why the detector will miss soft
transients on quiet content and false-fire on loud content.

---

## 4. The arms and their gates — the rungs

Each rung banks before the next starts (great-gate §4 pilot-ladder discipline).
Each is written to the canonical form
`GATE := (unit, signal, threshold-form, arms, fallback, ledger-entry)`, and each
passes the calculator (§5) before a line of dispatch code is written.

Ordered by *proof-cost first, then expected ODG per unit of work* — deliberately
mirroring the great-gate pilot ladder, where Rung 0 exists to validate the harvest
path with no new machinery at risk.

---

### Rung 0 — A8 window shape (KBD vs sine) — *the `cabac_init_idc` analog*

The cheapest gate in the campaign, and the reason it goes first: **a free syntax
element, both arms already implemented on both sides, zero bitstream cost.** It
validates the whole CSV → calculator → transcribed-branch path with nothing
speculative in flight.

- **unit** — frame (`window_shape` is 1 bit per `ics_info`)
- **signal** — `sfm` aggregate (KBD's steeper stopband suits tonal content; sine's
  narrower main lobe suits transients)
- **threshold-form** — per-clip percentile of frame SFM
- **arms** — `{sine (arm 0), KBD}` — `dsp::kbd_window` already exists and is
  TDAC-verified
- **fallback** — sine everywhere = **byte-identical** with today, `cmp`-proven
- **work counter** — window selection is O(1); `work` = 0 by construction, so this
  rung is a pure-quality gate and the audit's speed pair is trivially satisfied

*Expected: small (~0.02–0.05 ODG on tonal). The point is the harness, not the win.*

---

### Rung 1 — A1 short-block psy + window grouping *(largest single hole)*

Two coupled pieces, because per-group scalefactors are meaningless with one group:

**Arm 1a — real window grouping.** Today `num_window_groups: 1, window_group_length:
vec![8]` is hardcoded ([encode.rs:1229-1231](../crates/rusty_aac/src/encode.rs#L1229-L1231)).
The `scale_factor_grouping` serializer (`grouping_bits`) already exists and
round-trips. Group the 8 windows by their energy profile so pre-attack windows get
their own scalefactor set.

**Arm 1b — psy offsets per group.** Apply `perceptual_offsets` per window-group
using the short-block SWB geometry (`swb_offsets(false, fs_index)` — already
available and used).

- **unit** — frame (grouping) × group (offsets)
- **signal** — `attack_ratio[8]`: the position of the attack within the frame
  determines the grouping split
- **threshold-form** — per-frame percentile of sub-block energy (law 1: the split
  point is where *this frame's* profile jumps, not an absolute)
- **arms** — `{1 group + flat SF (arm 0, today), N groups + psy SF}`
- **fallback** — one group, flat SF = **byte-identical** with today
- **ledger risk** — grouping costs bits (more SF sets). The gain must be net of
  that, which is exactly what the calculator's signed `gain` measures.

*Expected: the campaign's largest win on the percussive/transient class. Also the
riskiest to over-fit — transient clips are few and dense, so watch the calculator's
**top3 clip-concentration** column: a win carried by three castanet clips is a clip
list, not a rule.*

---

### Rung 2 — A2 tonality-adaptive SMR *(first Prometheus symbolic leaf)*

Replaces `const SMR = 0.0158` with `SMR(tonality, bark)`. The classic form is a
linear tonality interpolation between a tonal-masker offset and a noise-masker
offset; we can do better than a hand-fitted line.

**This is the campaign's designated CASC-style deployment point** (great-gate §4,
"Symbolic leaves"), because it splits exactly where the deployment law says to
split:

- **branch (calculator-owned)** — a shallow threshold tree partitioning by
  band class / bark region. Discrete. Discovered by `gate_calculator`.
- **leaf (Prometheus-owned)** — the continuous `SMR = f(sfm, bark, energy_ratio)`
  law inside each partition, fitted by `prom-distill`, proven by `prom-prove`,
  emitted by `prom-forge`.

The six binding rules for symbolic leaves apply verbatim, with one audio-specific
reading of each:

1. **The calculator remains the sole banking authority** — the leaf re-enters as a
   candidate arm in a calculator CSV with `gain` = per-clip ODG delta.
2. **Stream-order replay is the quality instrument** — for audio the analog is
   *frame-order* evaluation with causal state (the psy model has inter-frame
   history once A5's PE tracker lands). Row-shuffled scoring of an adaptive
   candidate is a leaked fit and is refused.
3. **Shipping precision before any banked number** — the leaf is scored through the
   integer scalefactor grid the coder actually ships (SF are integers, clamped
   0..255), never in f64. f64 ODG is a screen, never a verdict.
4. **Same split, both halves** — branch fit and leaf fit share one name-keyed clip
   split; leaves fit on train clips, judged once on holdout.
5. **Prove-red blocks forge** — an SMR law failing `prom-prove`'s interval audit
   (e.g. producing a negative or unbounded threshold at some corner of the feature
   box) replays for the ceiling but never ships. A masking threshold that can go
   negative is an encoder that can hang the rate loop.
6. **Bits-per-op is the leaf's speed law** — the psy model runs per frame per band;
   a law needing a `powf` per band is dominated by one needing a multiply-add. The
   distill complexity budget is the knob. Note the precedent from
   `codec-symbolic-discovery`: symreg **cannot** strength-reduce a pure power —
   `x^0.75 = √(x·√x)` came from algebraic rewrite, and the same trap applies here.
7. **The per-class no-sign-flip law applies** — an SMR law that wins on music and
   loses on speech is a dispatch, not a win.

- **fallback** — `SMR = 0.0158` constant = **byte-identical** with today

*Expected: broad, moderate gain across every class — the single most general lever,
since today a pure tone and band-limited noise at equal energy get identical
treatment.*

---

### Rung 3 — A3 TNS

The AAC tool for pre-echo, and the reason AAC beats MP3 on speech. Decoder side is
**already live**.

- **Arm** — order ≤ 12 LPC over the spectrum, reflection coefficients quantized to
  the 4-bit/3-bit grids, `tns_data` emitted. Up to 3 filters per window.
- **unit** — window (per ICS; short blocks get per-window filters)
- **signal** — `lpc_gain` (spectral prediction gain), the classic on/off criterion
- **threshold-form** — per-frame percentile of `lpc_gain`. *Note:* the textbook uses
  an absolute latch (`gain > 1.4`). Law 1 says start population-relative and only
  fall back to a single-sided latch **if the calculator shows a wide natural gap** —
  which is a legitimate threshold-form under great-gate §4, not a violation.
- **arms** — `{no TNS (arm 0), TNS order N}`
- **fallback** — `tns_data_present = 0` = **byte-identical** with today
- **work counter** — LPC over 1024 bins is real cost. `work` = autocorrelation +
  Levinson ops per frame; this is the first rung where the speed half of the audit
  bites.

*Expected: large on clean/noisy speech and transients; ~0 on sustained tonal music —
a benign sign profile (no negative class), unlike A6.*

---

### Rung 4 — A4 absolute threshold of hearing *(the "quiet" axis)*

The axis with **no consumer at all** today. Two arms fall out of one addition:

**Arm 4a — the ATH floor.** `thr[sfb] = max(spread·SMR, ath(f_center)·cal)`.
**Arm 4b — sub-ATH band zeroing.** A band whose entire energy sits below ATH codes
as `ZERO_HCB` — pure bit saving at zero perceptual cost.

The calibration `cal` (dBFS → SPL) is a real design decision, not a constant to
guess: it must be a config knob with a documented default (full-scale = 96 dB SPL
is the common assumption), because it is the one place where an *absolute* value is
legitimately required — law 1 governs *content-derived* thresholds, and the hearing
curve is a property of the listener, not the content.

- **unit** — SFB (zeroing) and frame (calibration trust)
- **signal** — `loudness_dbfs`; the gate is on how far the frame sits above the
  noise floor
- **threshold-form** — single-sided latch on the ATH curve (physical), with a
  per-clip percentile guard on `loudness_dbfs` for the calibration-trust branch
- **arms** — `{no ATH (arm 0), ATH floor, ATH floor + zeroing}`
- **fallback** — `cal = -∞` disables the floor = **byte-identical**
- **abstention (law 6)** — on content whose absolute level is unknowable (heavily
  normalized or clipped masters) the gate should **refuse** rather than apply a
  mis-calibrated floor. This is the rung where law 6 earns its place.

*Expected: bit savings on quiet/wide-DR content that Rung 5 then redistributes.
Alone it is close to ODG-neutral — its value is realized in combination with A5,
which is why they are adjacent.*

---

### Rung 5 — A5 perceptual-entropy reservoir + VBR

Today a near-silent frame gets the same ~2976 bits at 128 kbps as a dense tutti
frame, and since `rate_loop` finds the *smallest fitting* base it will actively
spend them encoding inaudible detail.

- **Arm 5a — bit reservoir** (CBR-compatible). Requires `buffer_fullness` to become
  real rather than the `0x7FF` VBR marker at
  [encode.rs:215](../crates/rusty_aac/src/encode.rs#L215).
- **Arm 5b — true VBR mode**: a quality target, bitrate free. Needs a new
  `AacEncoderConfig` field — today the struct carries `bitrate_bps` only, and
  `rff-codec-aac` exposes nothing else.
- **unit** — frame, with **clip context as a feature** (law 2: decide per unit, feed
  group statistics as features — the group feature took 52% of the importance in
  the reference fit)
- **signal** — `pe` (perceptual entropy), the classic reservoir demand signal, now
  computable because the psy model already produces per-band thresholds
- **threshold-form** — **integral time-budget controller**, named explicitly in
  great-gate §4 as the right form for this shape of problem. Donation flows from
  low-PE frames (quiet, steady) to high-PE frames (transients, onsets).
- **fallback** — reservoir depth 0 = **byte-identical** with today
- **threading caveat** — great-gate §3 records the recurring trap: *online
  dispatchers that restart per frame for parallel determinism run the first ~N
  units un-dispatched.* Our encoder is **frame-parallel** ([encode.rs:1664](../crates/rusty_aac/src/encode.rs#L1664)),
  and a reservoir is inherently **sequential state**. This rung must either
  serialize the rate stage (keeping analysis parallel) or adopt a
  chunk-local reservoir with documented per-chunk reset cost. **Decide this before
  fitting, not after** — it changes the encoder the gate is fitted against, which is
  exactly the 2026-08-06 full-stack law.

*Expected: large on the wide-DR and mixed speech+music classes — the two synthesized
corpus gaps. This is the rung those clips exist for.*

---

### Rung 6 — A6 PNS + A7 intensity stereo *(the sign-flip pair)*

Grouped because they share a shape: both are **lossy substitutions** that win big on
one class and are catastrophic on another. Textbook dispatch material, and the
place where `sign-flip means dispatch` is load-bearing.

**A6 PNS** — substitute noise-like high bands with `NOISE_HCB` + an energy
scalefactor. Decoder already fills them.
- **unit** — SFB (above a bark cutoff)
- **signal** — `sfm[sfb]`, percentile within frame
- **arms** — `{normal coding (arm 0), PNS}`
- **the anti-class** — tonal music. PNS destroys phase; applied to a harmonic band
  it is audibly destructive. Expect a hard sign flip between applause and piano.

**A7 intensity stereo** — this converts the stereo decision from binary to a
**3-arm dispatch**: `{L/R, M/S, IS}`, replacing the current binary `mid_side`.
- **unit** — SFB (above a bark cutoff)
- **signal** — `xcorr[sfb]` + band index
- **the anti-class** — wide-stereo music, where IS collapses the image.

- **fallback (both)** — never emit the substitution = **byte-identical** with today
- **calculator discipline** — for both arms, the *force-on-everywhere* number must
  nearly tie the anchor on the full ladder before a dispatch is built (great-gate
  §4, arms clause: *a big force-on gap predicts a dominated dispatch*). If force-on
  PNS is catastrophic everywhere, the gate is not rescuable and the honest outcome
  is **refuse**, not a narrower threshold.

*Expected: significant at low bitrates (64–96 kbps) where ffmpeg leans on both;
neutral-to-negative at 192 kbps. The bitrate axis is itself a gate feature here.*

---

### Rung 7 — A11 bandwidth + A12 distortion loop + A9/A10 rework

The cleanup rung, folding in the two category-5 findings.

- **A11 coded bandwidth** — deliberate band limiting at low rates. `signal` =
  `rolloff`; `unit` = frame; the classic large low-bitrate lever, currently absent.
- **A12 distortion loop** — the real outer loop: iterate per-band scalefactors
  against per-band threshold (`noise[sfb] > thr[sfb]` → lower that band's SF) inside
  the rate loop, rather than computing offsets once analytically. This is the
  structure ffmpeg's `aacenc` two-loop has and we do not. **Sequence it last among
  the psy arms** — it changes the objective every earlier gate was fitted against,
  and re-fitting after it lands is cheaper than fitting against a moving target.
- **A9 detector rework** — `RATIO = 10.0` → per-clip percentile + high-band
  perceptual weighting.
- **A10 M/S objective** — add the bit-cost term and a BMLD term to the pure energy
  product.

---

## 5. The calculator — the banking authority for every rung

No rung above ships a branch that has not passed
[`_greatgate/gate-calculator`](../_greatgate/gate-calculator/). One harvest CSV per
rung, emitted by `aacharvest`:

| Column | Meaning for this campaign | Required? |
|---|---|---|
| `gain` | **signed** per-unit ODG delta if the gate fires (ΔODG, or Δbits for rate arms) | **yes** |
| `clip` | corpus clip name — the macro denominator | **yes** |
| `work` | deterministic ops saved/spent per unit (LPC ops, rate-loop iterations, band walks) | **yes** for a bankable verdict |
| `cpu_ms` | pinned-CPU delta per unit | **yes** for a bankable verdict |
| `split` | `train` / `holdout`, name-keyed and **stable across rungs** | yes |
| `shipped` | does an already-banked rung route this unit? | from Rung 1 on |
| `macro_gain` / `clip_total` | per-clip aggregation — what the ladder actually reports | yes |
| every other numeric column | auto-discovered candidate feature (the §3 signal vector) | — |

Then `--attest-full-stack`, and only when it is honestly true: the harvest ran the
**complete routed arm** — every kernel, cost term, and psy update the rung will ship
with. Without it, the audit downgrades the run to **HYPOTHESES ONLY** and nothing
can be banked. That downgrade is the audit working.

Three calculator outputs to read before celebrating any rung:

1. **micro vs macro sign disagreement** — flagged automatically; a rung whose micro
   and macro disagree is not understood yet.
2. **top3 clip-concentration** — approaching 100% means the "rule" is a clip list.
   Rung 1 (transients) and Rung 6 (applause) are the two most exposed to this,
   because their win classes are genuinely small. The fix is a **density-gated
   form**, not a narrower threshold.
3. **positive-on-BOTH-splits** — enforced inside the search; a rule whose train and
   holdout disagree in sign never surfaces.

**Where Prometheus enters, and where it does not.** Rung 2's SMR law is the
designated symbolic leaf. Rungs 0/1/3–7 stay pure calculator work. The deployment
law is absolute: *Prometheus deploys at the LEAF of a gate, never at the branch.*
A discovered law that replaces a branch is a model artifact by another name, and
great-gate law 3 forbids it — gates ship as transcribed depth-≤4 trees in plain
code, one documented field per feature, one unit test per branch, no runtime
fitting.

---

## 6. rusty_alloc — the measurement substrate

We validated (this session) that `rusty_aac` is currently compliant *by having no
executable targets at all*. This campaign creates three, and that compliance
becomes something to actively maintain.

**6.1 The three new roots.** `examples/aacquality.rs`, `examples/aacharvest.rs`,
`benches/aac_encode.rs` each get:

```rust
#[global_allocator]
static RUSTY_ALLOC: rusty_alloc_api::RustyAlloc = rusty_alloc_api::RustyAlloc;
```

plus a **new `[dev-dependencies]` block** in `crates/rusty_aac/Cargo.toml` carrying
`rusty_alloc-api.workspace = true`. The crate has none today. Follow
`crates/rff/Cargo.toml` lines 82–84 as the house pattern (dev-dependencies, so the
published lib target is untouched — a library must never hijack a downstream
binary's allocator choice).

**6.2 Why it is load-bearing here, not ceremonial.** Every rung's `cpu_ms` column
feeds the calculator's instrument audit, and `cpu_ms` is the *only* column an
allocator can move. The AV2 precedent is **1.38×** — larger than the speed
difference most of these rungs will produce. An arm measured on the system heap
while the shipped binary runs on rusty_alloc is not a comparison; it is two
different encoders. The CLAUDE.md rule ("performance measurements must run under
rusty_alloc, since it is what ships") makes this binding, and great-gate §6's
measurement law makes it auditable.

**6.3 The rungs where the allocator is most likely to move the number.** Not
uniform — worth knowing in advance which `cpu_ms` columns to distrust if the
allocator is ever wrong:

- **Rung 1 (grouping)** — variable group counts mean variable-size scalefactor and
  codebook vectors per frame. Allocation-rate sensitive.
- **Rung 3 (TNS)** — per-window LPC workspaces.
- **Rung 5 (reservoir)** — if it forces serialization of a currently frame-parallel
  encoder, the allocator's **multithreaded** behavior is directly in the comparison,
  which is precisely where a mimalloc-class allocator differs most from the system
  heap.

Rung 0 is allocation-neutral by construction, which is another reason it is the
right harness-validation rung.

**6.4 The tap-and-clock trap.** `aacharvest` allocates per recorded unit (the same
shape as `rusty_vp9::telemetry`, which pushes a `CoefBin` per bin). **The tap and
the clock must never run in the same pass** — a harvest pass measures the harvest's
own allocation tax, not the arm. This is why `prom-entropy` deliberately emits *no*
`cpu_ms` and accepts the HYPOTHESES-ONLY downgrade. Mirror that discipline: harvest
quality and `work` in a tapped pass (feature-gated, `aac-telemetry`), then take
`cpu_ms` from a separate untapped pinned run of `benches/aac_encode.rs`. The `work`
counter is deterministic and therefore allocator-independent — which is exactly why
great-gate makes it the **primary** evidence and the clock merely confirmatory.
When they disagree, the counter rules.

**6.5 Audit trap #1 restated for this campaign.** Any CLI-level A/B (e.g.
`rff -i in.wav out.m4a` end-to-end) must be built with **`-p rff-cli`**, not
`-p rff`, with the exe mtime printed at the gate. `cargo build -p rff` never
relinks the binary.

---

## 7. Sequencing, gates, and exit criteria

```
P0   corpus + aacquality + PEAQ ladder + null arm      ← blocking; no verdict exists today
P0.9 hygiene batch (3 stale docs + λ/psy consistency)  ← before ANY fitting
P1   AacSignals vector + per-class truth tables
     ├─ Rung 0  A8  window shape          (free syntax; validates the CSV path)
     ├─ Rung 1  A1  short-block psy + grouping   (largest hole)
     ├─ Rung 2  A2  tonality SMR          (+ first Prometheus symbolic leaf)
     ├─ Rung 3  A3  TNS                   (decoder already live)
     ├─ Rung 4  A4  ATH / loudness        (the "quiet" axis)
     ├─ Rung 5  A5  PE reservoir + VBR    (⚠ decide the threading model first)
     ├─ Rung 6  A6+A7 PNS + intensity     (the sign-flip pair; force-on tie required)
     └─ Rung 7  A11+A12+A9+A10 cleanup    (A12 last — it moves the objective)
P4   gate ledger + regression harness + published-version column
```

**Per-rung gate (all four must pass):**
1. Round-trip through **our** decoder — no error, recon within quantization noise.
2. **ffmpeg decodes it** — spec-valid, not merely self-tolerated.
3. Per-clip ODG at ≥4 bitrates, all 8 classes: **no sign flip, worst class ≤ 0.**
4. Calculator verdict **bankable** — `--attest-full-stack` + `work` + pinned
   `cpu_ms` under rusty_alloc. Anything less is a hypothesis, not a gate.

**Campaign exit:** a per-class ODG table over the full corpus where every default-on
feature has **no sign-flips and worst class ≤ 0** against `ffmpeg -c:a aac`, all of
it regenerated by a single harness run, and a CI gate that fails if a sign flip
appears on any tracked class.

**A note on the target.** ffmpeg's native `aacenc` is a moderate encoder — FDK is
the stronger reference. Beating ffmpeg-native is the stated goal here and is
realistic; matching FDK is not this campaign's scope, and the ladder should record
FDK as a third column for context rather than as a gate.

---

## 8. Rung log (append before/after each rung)

### P0.9 — hygiene batch ✅ DONE (2026-08-08)

Three stale doc claims corrected before any fitting:

1. **`decode.rs` module doc** claimed TNS "parsed but not applied" and PNS/intensity
   "rejected." **All three are live** (`apply_tns` at both SCE and CPE sites,
   `NOISE_HCB` noise fill, `apply_is` with the M/S sign flip). Only
   `gain_control_data` is genuinely unsupported. This is a large de-risk: arms
   A3/A6/A7 need no decode-side work.
2. **`codec-aac-encoder.md` brick 4** claimed an "outer distortion loop." There is
   none. Row corrected; the real loop is arm A12.
3. **`encode.rs` module doc** still described the crate as "brick 1 scaffolding."
   Replaced with an explicit list of what the encoder does *not* do, so a fit can
   never again be run against a mis-documented encoder.

### P0 — corpus + verdict instrument ✅ DONE (2026-08-08)

Built `rusty_aac::lab` (feature `lab`), the instrument that did not exist:

| Piece | What |
|---|---|
| `lab::corpus` | 8 deterministic classes incl. both synthesized gap classes |
| `lab::quality` | NMR in the encoder's own MDCT/SWB domain (no bin-remap approximation) |
| `lab::ladder` | per-clip × per-bitrate runner + **null arm** |
| `examples/aacquality.rs` | driver, **under `rusty_alloc`** + new `[dev-dependencies]` |

**23 lab tests green.** Null arm CLEAN (byte-identical re-encode on every class).
RD curve monotone in bitrate on every class — the flat-RD bug the MP3 campaign hit
is not present here.

**Baseline (audible%, lower better):**

| clip | 64k | 96k | 128k | 192k |
|---|---|---|---|---|
| speech-clean | 17.33 | 2.50 | 0.14 | 0.00 |
| speech-noisy | 99.50 | 99.45 | 92.12 | 0.02 |
| music-tonal | 19.62 | 12.15 | 6.05 | 0.00 |
| percussive | 25.69 | 22.47 | 15.82 | 0.00 |
| noise-like | 99.95 | 99.78 | 93.78 | 0.00 |
| stereo-wide | 45.71 | 31.72 | 22.83 | 14.09 |
| quiet-dynamic | 28.21 | 13.97 | 8.36 | 0.07 |
| mixed-speech-music | 22.18 | 11.28 | 5.83 | 0.02 |

Two baseline findings worth carrying forward:

- **The noisy classes fall off a cliff, not a curve.** `speech-noisy` and
  `noise-like` sit at 99%+ audible from 64–128k and then snap to ~0 at 192k. Both
  are the lowest-tonality classes. This is the strongest possible motivation for
  **A6 (PNS)** — broadband noise has no maskers to hide under and is simply
  unaffordable at low rates, which is the problem noise substitution exists to
  solve. (Caveat: NMR is our own psy model scoring our own encoder; PEAQ is the
  verdict.)
- **The rate loop undershoots by up to 17.5%** (speech-clean 64k reaches only
  52.8 kbps). Sparse content cannot spend its budget through a single global base
  offset — an **A5 (reservoir/VBR)** signal, visible in the very first baseline.

### P1 — signal audit ✅ DONE (2026-08-08)

`lab::signals::AacSignals` — one per-frame vector for all seven axes, replacing the
duplicated probe skeletons. Consumed through `percentile_of` (law 1) and `mean_of`
(law 2, group context). **Per-class truth table passes for every axis**: attack
separates percussive from tonal, tonality separates harmonic from noise, spectral
LPC gain separates impulsive from steady, loudness resolves the quiet class, xcorr
marks wide stereo, PE varies on mixed content.

**★ A9 FINDING (found by the truth table, before any gate was fitted).** The
shipping transient detector's guard is `avg > 1e-3` — an **absolute** energy
threshold. Two consequences, both pinned by tests that will flip to parity
assertions when A9 lands:

1. **Level dependence.** The identical waveform scaled −40 dB stops being detected
   entirely (`shipping_detector_is_level_dependent`).
2. **Sparse-attack blindness, at any level.** The running average decays 0.75× per
   sub-block, so across the ~75 quiet sub-blocks between castanet-like clicks it
   falls under the absolute floor and the ratio is never evaluated. The percussive
   class is flagged **zero** times at full scale despite a p95 attack ratio of
   ~30 000× (`shipping_detector_misses_sparse_attacks`).

This is worse than the plan's original wording ("misses soft transients"). Every
one of those frames codes as a long block and takes the full pre-echo hit — which
is a large part of what arm A1 will be measured against. The replacement signal
(`frame_attack_ratios`, population-relative floor) is level-invariant and is now
used by Rung 0.

Two harness bugs the truth table also caught, both of which would have silently
corrupted later fits: adjacent LCG seeds produce near-identical streams (fixed with
a murmur3 finalizer — the stereo channels were playing identical notes), and
peak-normalized mixing let the shared centre channel dominate `stereo-wide`'s
energy (fixed with RMS normalization).

### Rung 0 — A8 window shape ⚠️ BUILT, **NOT BANKED** (2026-08-08)

Arm and gate implemented; **`WindowShape::Auto` stays off by default** (law 7).

**What landed.** `WindowShape::{Sine, Kbd, Auto}`; the shape-aware `long_window`
mirroring the decoder's prev/cur handling; `time_tonality` (order-2 time-domain LPC
gain — chosen over SFM because the shape must be decided *before* the spectrum
exists, and a probe MDCT per frame would be dominated on bits-per-op); a whole-
stream `decide_shapes` pre-pass that keeps `encode_frame` frame-parallel and
byte-deterministic. **5 Rung 0 tests green**, including the byte-identical neutral
end proven against an inlined pre-A8 oracle, and Princen-Bradley TDAC verified
across all four ordered shape transitions.

**The measurement (NMR audible%, Δ vs sine, 96k/128k):**

| clip | kbd Δ | auto Δ (final) |
|---|---|---|
| speech-clean | +0.62 / +2.86 | +0.12 / +0.00 |
| speech-noisy | −0.24 / −0.72 | +0.00 / −0.12 |
| music-tonal | +0.34 / +0.34 | +0.00 / +0.00 |
| **percussive** | **+18.63 / +16.52** | **−0.29 / +0.12** |
| noise-like | −0.10 / −1.49 | −0.05 / −0.74 |
| stereo-wide | +2.23 / +0.19 | +0.82 / −0.22 |
| quiet-dynamic | +0.10 / −1.30 | +0.94 / +0.43 |
| mixed-speech-music | −0.74 / +0.10 | −0.12 / −0.02 |
| **mean** | **+2.333** | **+0.054** |

**Two refutations, in order.**

1. *A tonality-only gate routes exactly the frames KBD hurts most.* Ringing
   percussion is a decaying sinusoid — it reads as **highly tonal** — so the first
   fit sent it to KBD and inherited +10% of force-on's +18.6% regression. Fixed
   with a transient veto, making the gate a depth-2 tree.
2. *Vetoing only the onset frame is not enough.* Overlap-add means frame b's window
   shapes the overlap with both neighbours, so the ring-down frame stayed on KBD
   and kept the entire regression (+9.99 → unchanged). Widening the veto to a
   ±1-frame neighbourhood collapsed percussive to −0.29/+0.12.

**Verdict: REFUSED for default-on.** Three reasons, any one sufficient:

- **Worst class > 0.** quiet-dynamic +0.94, stereo-wide +0.82. The exit criterion
  is worst class ≤ 0 per class, never on average — and the mean (+0.054) is exactly
  the kind of number that law exists to stop us banking.
- **The force-on gap predicted this.** Great-gate §4: *force-on-everywhere must
  nearly tie the anchor before a dispatch is built on it; a big force-on gap
  predicts a dominated dispatch.* Force-on KBD is +2.33 mean with a +18.6 worst
  cell. The dispatch's ceiling was ~neutral, and that is what it reached.
- **HYPOTHESES ONLY by the instrument audit.** No `work` column, no pinned
  `cpu_ms`, no `--attest-full-stack`. Nothing here is bankable regardless of sign.

This is the intended outcome of Rung 0. Its stated job was to validate the harness
end-to-end with nothing speculative at risk — and it did: it exercised the corpus,
the ladder, the null arm, the byte-identical fallback, the sign table, and the
force-on law, and it produced two genuine refutations plus the A9 finding. **The
win was never the point; the instrument was.**

Follow-ups this rung opens: the `stereo-wide` and `quiet-dynamic` positives are
unexplained and worth one probe each before A8 is revisited; and A8 should be
re-measured *after* A2 (tonality-adaptive SMR), since a psy model that knows about
tonality may change which frames want which window.

### Rungs 1–3 — A1 / A2 / A3 ⚠️ BUILT, **NONE BANKED** (2026-08-08)

All three arms implemented, all off by default, **7 rung tests + full suite green**.
Every arm's OFF state is `cmp`-proven byte-identical with the shipped encoder.

**Per-class sign table (Δaudible% vs shipped, 64/96/128k, negative = better):**

| clip | A1 short-psy | A2 ton-SMR | A3 TNS |
|---|---|---|---|
| speech-clean | +0.00 / +0.00 / +0.00 | −0.12 / +0.10 / −0.10 | +0.00 / **+9.63** / **+13.30** |
| speech-noisy | +0.43 / +0.34 / +0.53 | −0.17 / −0.36 / +0.62 | +0.00 / +0.00 / +0.00 |
| music-tonal | +0.00 / +0.00 / +0.00 | **−1.13** / −0.07 / −0.24 | +1.25 / +1.61 / +1.80 |
| percussive | +0.00 / +0.00 / +0.00 | +0.22 / +0.58 / +0.41 | +3.53 / +1.08 / +2.21 |
| noise-like | +0.00 / +0.00 / +0.00 | +0.00 / −0.07 / +0.65 | +0.00 / +0.00 / +0.00 |
| stereo-wide | +0.29 / +0.29 / −0.17 | **−3.94** / −0.70 / +0.12 | +0.00 / +0.00 / +0.00 |
| quiet-dynamic | −0.02 / +0.29 / +0.65 | **−2.81** / +0.48 / −0.60 | +4.51 / +4.80 / +3.22 |
| mixed-speech-music | +0.05 / +0.26 / +0.22 | +0.72 / −0.34 / −0.74 | +0.00 / +6.99 / +8.64 |
| **mean / worst** | **+0.131 / +0.648** | **−0.312 / +0.720** | **+2.607 / +13.301** |

None reaches worst-class ≤ 0, so none is banked. But the three columns fail for
three *different* reasons, and only one of them is "the arm is bad."

#### A1 — ★ **BLOCKED ON A9, not refuted**

A1's column is **+0.00 on five of eight classes** — including `percussive`, the
class it exists for. The arm only reaches frames the encoder codes as short
blocks, and per the A9 finding the transient detector flags the percussive class
**zero times** (`shipping_detector_misses_sparse_attacks`). Short-block psy cannot
help frames that are never short blocks.

Where the detector *does* fire (speech-noisy, stereo-wide) A1 engages and is
roughly neutral-to-slightly-negative. **A1 is unmeasurable until A9 lands** — the
campaign has a hard dependency the original plan did not record. A9 moves ahead of
A1 in the ladder.

One plan correction from reading the emit path: A1 does **not** require window
grouping first. With one group a scalefactor still covers band *b* of all eight
windows, so per-SFB noise shaping is available and is the substantive lever;
grouping adds *temporal* resolution on top. Arm 1a is a genuine follow-up, not a
prerequisite, and the shipped arm here is 1b alone.

#### A2 — the most promising arm, and one refuted "safety" tweak

Best mean of the three (**−0.312**), with real wins where the psy model was
blindest: stereo-wide −3.94 at 64k, quiet-dynamic −2.81, music-tonal −1.13. Its
failures are a clean **per-class sign flip** — wins tonal/stereo/quiet, loses
noisy/percussive — i.e. a dispatch signal, not a mean-loss to discard.

**Refuted:** pinning the noise end to the shipped 18 dB (so A2 could only ever add
protection, never remove it) looked strictly safer and measured **worse**:

| noise end | mean Δ | worst class |
|---|---|---|
| 5.5 dB (textbook) | **−0.312** | +0.720 |
| 18.0 dB ("safe") | −0.006 | **+2.185** |

An SMR pair is a **bit-allocation balance, not two independent protections**.
Raising the tonal end buys precision only because lowering the noise end frees the
bits to pay for it; pin the noise end and the tonal end is pure extra demand
against a fixed budget, so the rate loop lifts the global base and every band gets
coarser. The conservative variant regressed three times as hard as the one it was
meant to protect against. Reverted, with the reasoning recorded at `SMR_NOISE_DB`.

#### A3 — ★ **the metric cannot see this arm**

A3's column is either exactly `+0.00` (the gate declined — it requires
`max_sfb ≥ TNS_MAX_LONG`, so narrow-band and low-rate frames never engage) or
strongly positive. Before reading that as a refutation:

**NMR is structurally blind to what TNS does.** TNS does not reduce
spectral-domain noise — it redistributes quantization noise *in time within the
block*. Our NMR screen scores per-band MDCT noise against a per-band mask, with no
temporal resolution inside a frame. The benefit (pre-echo suppression) is therefore
invisible to it, while the cost (a whitened residual is harder to code) is fully
visible. A metric that can only see one side of a trade cannot judge it.

So A3's number is **not evidence of harm**; it is evidence the instrument is wrong
for this arm. The honest verdict is *unjudged*. It needs PEAQ (which models
temporal masking) or a dedicated pre-echo probe — a time-resolved noise
measurement inside the transient frames. Two things ARE established: the filter
inversion is exactly right (`a3_tns_inverts_exactly`, and
`a3_parcor_quantization_matches_the_decoder` verifies the encoder's quantized
PARCOR against the decoder's dequantizer including its asymmetric negative scale),
and the arm fires (`a3_tns_actually_engages`). The magnitude on speech-clean 128k
(+13.3) is large enough that a genuine rate cost may also be present; the metric
cannot separate the two, which is precisely why it is not banked either way.

### What the three rungs changed about the plan

1. **A9 is now a prerequisite, not cleanup.** It gates A1 entirely and was
   scheduled last (Rung 7). It should run first.
2. **A3 needs a second instrument before it can be judged at all.** NMR-only
   rungs are fine for A1/A2; A3 is not one of them.
3. **A2 is the best candidate for the first real bank** — it already has a
   negative mean and a clean per-class sign flip to route on, and it is the arm
   the Prometheus symbolic leaf was designated for (§4).

All three remain **HYPOTHESES ONLY** under the instrument audit regardless of
sign: no `work` column, no pinned `cpu_ms`, no `--attest-full-stack`.

---

## 9. Second pass (2026-08-08) — the conformance bug, A9, the harvest, A6/A7, LATM

### ★ Multichannel was emitting NON-CONFORMANT bitstreams — FIXED

`init()` had no channel guard. Six channels produced **six SCEs** while the header
declared `channel_configuration = 6`, which ISO Table 1.19 defines as
`SCE, CPE, CPE, LFE`. Our own decoder accepted it because it appends elements in
order — so a self-round-trip looked perfect. **Self-round-trip is not conformance.**

Fixed properly rather than by rejecting: `element_plan()` implements configs 1–6
with the interleave→AAC reordering (AAC carries 5.1 as `C, L, R, Ls, Rs, LFE`;
PCM arrives `L, R, C, LFE, Ls, Rs`, so emitting arrival order would put the centre
channel in the left speaker), `aac_to_interleave_order()` inverts it on decode,
LFE is forced long-only, and 7+ channels are **rejected** pending a
program_config_element. 5 tests, including a channel-identity round-trip that
would have caught the original bug.

### A9 — the level-invariant detector ✅ WORKS, and it REFUTES A1

| arm | mean Δ | percussive 64k/128k | speech-clean 64k/128k |
|---|---|---|---|
| A9 alone | **−0.386** | **−2.81 / −6.10** | +1.56 / +1.18 |
| A9 + A1 | −0.057 | −1.18 / −5.50 | +2.33 / +1.46 |

A9 is a large win on the class that was structurally blind. But A1 **on top of it**
is worse on both. With one window group a single scalefactor governs band *b* of
all eight windows, so the mask comes from their sum — handing the quiet pre-attack
windows a mask sized by the attack, which is precisely the pre-echo the tool was
meant to suppress. A flat scalefactor at least errs uniformly.

**This overturns my own earlier plan correction.** I argued from the emit path that
grouping was a follow-up because per-SFB shaping exists with one group. True, and
irrelevant: that shaping is in the wrong domain. **Arm 1a (window grouping) is a
genuine prerequisite**, exactly as the original plan said. A9 also moves ahead of
A1 in the ladder — it was scheduled last as Rung 7 cleanup.

### aacharvest ✅ — the CSV path is proven end to end

`examples/aacharvest.rs` emits the calculator CSV and **was run through
`_greatgate/gate-calculator`**. 5 of 6 audit boxes check; the sixth
(`--attest-full-stack`) is deliberately not passed while the clock is unpinned.

Two defects the harness caught **in itself**, which is the point of a null arm:

1. **`WORK NULL: 0 vs 0`.** The counters were thread-local, and `encode_stream`
   fans frames across worker threads — so the main thread drained zero. A counter
   that reads flat is a defect report, not reassurance. Made global/atomic; now
   `430 vs 430, DETERMINISTIC`.
2. **Timing null arm: median 7.6%, worst 15.3%.** The unpinned wall clock cannot
   resolve any of these arms. The calculator's own output agrees, flagging
   **1358 counter/clock sign disagreements** — `work` says A2 *costs* ~130 evals
   per frame while the clock claims it is faster. Per the law, the counter rules.

### A6 (PNS) and A7 (intensity stereo) ✅ BUILT

| arm | mean Δ | worst | note |
|---|---|---|---|
| A6 PNS | +0.021 | +0.192 | barely fires; see below |
| **A7 intensity** | **−0.006** | **+0.000** | **first arm to pass worst-class** |

**A7 needed restructuring after a measured refutation.** The first version ran
after `mid_side` and only touched bands M/S declined — and never fired once. The
bands where `R ≈ k·L` are exactly the bands where M/S wins
(`E_M·E_S ≪ E_L·E_R` for a scaled copy), so "M/S declined it" and "intensity wants
it" are near-disjoint by construction. It now decides on the original L/R spectra
and **vetoes** M/S on the bands it claims, making the stereo decision a real 3-arm
dispatch `{L/R, M/S, IS}` instead of two tools overwriting each other.

**A6 needed two guards after firing on a pure 6 kHz tone.** A near-empty band has
a flat spectrum, so SFM reads it as maximally *noise-like* — the measure cannot
distinguish "broadband noise" from "almost nothing", and a band-only gate
substitutes noise into the empty bands surrounding a tone. Fixed with a
frame-level tonality veto (refuse tonal content outright) plus a
population-relative energy floor (refuse empty bands).

### ★ NMR cannot judge A3 or A6 — the instrument, not the arms

A3's column is strongly positive and A6's is mildly positive. Neither is evidence
of harm, because **our screen is blind to what both tools do**:

- **A3/TNS** redistributes quantization noise *in time within the block*. NMR
  scores per-band MDCT noise with no temporal resolution inside a frame.
- **A6/PNS** deliberately randomizes *phase* at matched energy. NMR compares
  coefficients directly, so correct-energy noise reads as large error.

In both cases the benefit is invisible to the metric and the cost is fully
visible. A metric that can only see one side of a trade cannot judge it. Both need
PEAQ (temporal masking) — A3 additionally wants a pre-echo probe. **NMR remains
valid for A1, A2, A7 and A9**, which is where it has been used.

### LATM/LOAS ✅ COMPLETE

`src/latm.rs` — the MPEG-TS / broadcast carriage format, previously absent while
this workspace ships `rff-format-ts`. Writer + stateful `LatmReader` (real streams
send `StreamMuxConfig` once and then set `useSameStreamMux` for long runs, so a
stateless parser fails on the *majority* of frames), sync search, 255-escaped
payload lengths, `Error::Again` on truncation, and explicit refusal of
multi-program/multi-layer/`audioMuxVersion 1` rather than silent mis-parse.
7 tests including **real AAC encoded → LOAS → parsed → decoded**.

### HE-AAC (SBR / PS) — ✅ SIGNALLING + CORE-ONLY DECODE LANDED

`src/sbr.rs`, 9 tests. **What broadcast integration actually needed first was not
the reconstruction — it was the signalling**, because without it an HE-AAC stream
is not "missing its high band", it is **silently played at half speed**: the config
announces a 24 kHz core for 48 kHz output, and a decoder reporting 24 kHz hands the
player half-rate audio.

Implemented exactly:

- **Explicit hierarchical** signalling (`audioObjectType` 5 = SBR, 29 = SBR+PS),
  including the trap that the first sampling-frequency field in that form is the
  **extension (output)** rate, not the core rate — swapping them is the classic
  half-speed/double-speed HE-AAC bug.
- **Explicit backward-compatible** signalling (the `0x2B7` sync extension after
  `GASpecificConfig`, plus the stacked `0x548` PS extension). Real streams use
  both forms, so both are handled.
- **Implicit** signalling — SBR payload detected in the `fill_element`
  (`extension_type` 13/14), which MPEG-TS broadcast genuinely produces with
  nothing in the config to warn you.
- `AacDecoder::with_config_bytes()`, `output_sample_rate()`, `sbr_config()` and
  `sbr_support()`. `rff-codec-aac` now configures through the raw-bytes path, so
  the whole rff pipeline reports HE-AAC rates correctly.

`SbrSupport::CoreOnly` says plainly what the caller gets: the core decoded
correctly at the correct output rate, band-limited to the core's Nyquist. That is
real audio rather than silence or an error, and it is the standard core-only
fallback — **but it is not conformant HE-AAC decoding**, and the API says so
rather than leaving it to be discovered.

### What remains of HE-AAC: the reconstruction, and why it stops here

| piece | what it needs |
|---|---|
| Complex QMF analysis/synthesis | 64-band bank + the **640-tap prototype filter**, ISO/IEC 14496-3 Table 4.A.87 |
| SBR bitstream | envelope + noise-floor grids and their **Huffman codebooks** |
| HF generation | spectral patching + per-band inverse filtering (LPC) |
| Envelope adjustment | gain calculation, limiter bands |
| PS (HE-AAC v2) | decorrelator, IID/ICC parameter bands, a second parameter stream |

**The blocker is normative tables, not design effort.** The QMF prototype and the
SBR Huffman codebooks are large exact tables that must be transcribed from the
specification or a reference implementation. They cannot be derived, and writing
*approximations* of them would produce a decoder that is subtly wrong everywhere
while appearing to work — strictly worse than not having it, and the same reason
`decode` refuses `gain_control_data` outright rather than guessing.

So the reconstruction stops at a clean, honest boundary: everything that can be
built exactly is built and tested; the part that needs the spec tables is named,
scoped, and reported through `SbrSupport` rather than faked. The remaining work is
**decoder-first** (reading broadcast HE-AAC matters far more than emitting it) and
wants its own conformance corpus — the ISO/3GPP HE-AAC reference streams — which
is a campaign of the same shape as this one, not a rung inside it.
