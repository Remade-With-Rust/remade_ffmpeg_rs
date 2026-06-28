# MP3 Quality Harness (NMR) — plan

Close the encoder's biggest blind spot: we've judged quality only by SNR, which
*lies* for perceptual codecs (LAME hides noise under the mask → low SNR, sounds
great). Build a **perceptual** metric — noise-to-mask ratio (NMR) — by reusing our
own psychoacoustic model, validate it, and run **our encoder vs LAME on real
audio**. Light by design (days, not the weeks a full ITU-R BS.1387 PEAQ would cost).

## The metric

For each aligned analysis frame of the original vs a codec's decoded output:
- **noise**(band) = `Σ |X_orig − X_coded|²` over the band's FFT bins (the coding
  error's power spectrum).
- **mask**(band) = the masking threshold of the *original*, straight from our
  psymodel (`psychoacoustic::analyze(orig).thresholds`).
- **NMR**(band) = `noise / mask`. `> 0 dB` = audible noise; `< 0 dB` = inaudible.

Same window (Hann) + FFT size (1024) as the psymodel, so noise and mask live in the
same energy domain. Aggregate over frames/bands → **mean NMR (dB)** (lower = better),
**% (frame,band) audible** (noise over mask), and a **per-band profile** (where we're
weak). Lower NMR than LAME ⇒ we're perceptually competitive *or better*.

## House — floors & bricks

### Floor 1 — NMR core (reuse the psymodel), in `lab::quality`
- **Q1 — frame NMR.** `frame_nmr(orig_frame, coded_frame, sr) -> [f32; bands]`:
  Hann+FFT both, noise = `|Δspectrum|²` per band, mask = `analyze(orig).thresholds`.
- **Q2 — track NMR + align.** `track_nmr(orig, coded, sr) -> NmrReport`: delay-align
  coded to orig (codec delay), hop frames (1024/512), aggregate (mean dB, % audible,
  per-band profile).

### Floor 2 — A/B harness, an `mp3quality` example
- **Q3 — driver.** Loads f32 mono WAVs (`orig`, `coded…`), runs `track_nmr`, prints
  a report. Decoding is done *outside* (system FFmpeg decodes BOTH mp3s — a neutral,
  shared decoder, so the comparison is fair). One command → NMR per codec, head-to-head.

### Floor 3 — Validation (don't trust an unvalidated metric)
- **Q4 — internal sanity.** NMR must fall monotonically with bitrate (128 > 192 >
  320) and approach transparency at 320k. If it doesn't, the metric is wrong.
- **Q5 — external ground truth.** If a PEAQ tool (GstPEAQ / `peaqb` / a Python impl)
  is available, correlate our NMR with its ODG on a few clips. If not, document the
  gap and lean on Q4 + the relative (ours-vs-LAME) framing, which cancels most
  metric bias.

### Floor 4 — Real audio (THE non-negotiable)
- **Q6 — verdict on real audio.** Run the harness on real recorded clips
  (`/c/Windows/Media/Ring*.wav` are real ~6 s recordings — not synthetic) at 128k,
  ours vs LAME. Flag that a *definitive* verdict wants full-length music; these prove
  the harness on real spectra.

## Gate / honesty
- The metric reuses OUR psymodel, so it's biased toward what our encoder optimizes —
  hence the **relative** ours-vs-LAME framing and the external PEAQ check.
- Synthetic signals are banned from the verdict; real audio only.
- Report NMR with its caveats; never claim a perceptual win from one short clip.

## Results (2026-06-28) — built, validated, and what it can/can't tell us

Built in `lab::quality` + the `mp3quality` example. **Validated** by bitrate
monotonicity (our encoder, real clip): mean NMR **−17.3 → −26.9 → −41.2 dB** at
128/192/320k — the metric correctly tracks transparency. Codec delay (1057 samples)
auto-detected. (One bug caught & fixed: the alignment guard skipped short clips,
manufacturing a fake 96%-audible result — alignment is load-bearing.)

**ours vs LAME @128k on real clips** (Ring05/08/09, chimes): our *mean* NMR reads
−17…−25 dB vs LAME's −12…−15 — i.e. we look "better." **This is NOT a real perceptual
win — it's the shared-psymodel bias.** Our distortion loop shapes noise to sit under
*our* mask; grading with *our* mask flatters us by construction. The less-biased
signals say otherwise: on transient `chimes`, **LAME's worst-case is far tighter
(max 2.8 dB / 0.2% audible vs ours 12.7 dB / 3.2%)** — LAME controls peak audible
noise better, the real perceptual risk.

**What this harness IS good for:** a *self-consistent* gauge of OUR encoder across
changes (does a tuning lower our NMR / worst-case?), and catching gross failures.
**What it CANNOT do:** rank us against LAME — that needs an *external* PEAQ
(unavailable here; `peaqb`/GstPEAQ not installed). Use the **max-NMR / % audible**
columns (less biased) over the mean, and treat worst-case noise control as the
target — that's where LAME leads.
