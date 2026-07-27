# Six-Whys descent — why does our ALT-REF make compression WORSE?

**Unknown.** Our VP9 ARF path measured +21.31% (akiyo), +11.08% (foreman), +37.11% (bus)
BD-rate *worse* than no-ARF, while libvpx runs ARF on by default (`lag_in_frames=25`) and
gains from it. ARF is the single biggest identified component of our ~+24% BD-rate gap to
ffmpeg VP9.

Run 2026-07-27, after the MV-clamp drift fix (`6954dbf`).

---

## D6 — is the measurement sound?  (run FIRST, per the skill)

- **ASKED:** do both arms do identical work; is the BD number trustworthy?
- **MEASURED:** decoded frame counts — source 30, lag0 30, ARF 30. No hidden-frame
  misalignment. Then split the BD by rate region rather than reading one average.
- **ANSWER:** frame counts are sound, but **the single BD number hid a sign flip across
  the rate axis**:

  | region | BD (+ = ARF worse) |
  |---|---:|
  | full range | +21.26% |
  | low quality (34.2–38.8 dB) | **+46.41%** |
  | high quality (38.8–43.5 dB) | **+0.42%** (−7.7% at 43 dB) |

  Rate needed at matched PSNR: 35 dB **+61.4%**, 39 dB +20.2%, 41 dB −2.3%, 43 dB **−7.7%**.
- **CONFIDENCE:** high — deterministic encodes, overlap 9.25 dB, PCHIP over overlap only.
- **SPAWNED:** D5a (what causes the low-rate collapse?)
- **STATUS:** closed. The premise "ARF is worse" was wrong; ARF *wins* at high bitrate and
  collapses at low bitrate. The disabling gate was reacting to an average of a win and a loss.

## D2/D3 — where do the bits go?

- **ASKED:** which frame owns the low-rate loss — the ARF itself or the P frames it serves?
- **MEASURED:** per-frame IVF sizes, akiyo_cif:

  | | total | largest frame | share | median frame |
  |---|---:|---:|---:|---:|
  | crf 16 | 33,865 B | 8,168 B | 24% | 491 B |
  | crf 60 | 7,845 B | **5,481 B** | **70%** | **31 B** |

- **ANSWER:** at low bitrate one hidden ARF consumes 70% of the entire stream while ordinary
  frames cost 31 B. That is the rate floor — the group physically cannot reach low rates.
- **CONFIDENCE:** high.

## D5a — mechanism: the ARF q boost is a MULTIPLIER

- **ASKED:** why does the ARF cost grow as the budget shrinks?
- **MEASURED:** `arf_qscale` (default 0.5) multiplies qindex, so the *absolute* boost grows
  with q — it spends hardest exactly when there is least to spend. Sweep, BD vs no-ARF:

  | arf_qscale | akiyo FULL / low-q | bus FULL / low-q |
  |---|---:|---:|
  | 0.5 (shipped) | +21.31 / +46.52 | +37.11 / +56.91 |
  | 0.7 | +6.53 / +16.41 | +22.90 / +29.74 |
  | 0.85 | +2.72 / +7.64 | +15.88 / +17.14 |
  | 1.0 | +1.15 / +3.46 | +14.31 / +14.54 |

- **ANSWER:** confirmed. The flat multiplier is the rate floor's cause.
- **FIX:** fade the boost out as qindex rises (full `arf_qscale` at q=0, none at q=220).
  Result — BD vs no-ARF:

  | clip | flat 0.5 (was) | rate-aware (new) | high-q half |
  |---|---:|---:|---:|
  | akiyo | +21.31% | **+1.04%** | **−1.69%** |
  | foreman | +11.08% | **+2.13%** | +0.55% |
  | bus | +37.11% | **+15.29%** | +14.90% |

- **STATUS:** closed and fixed.

## D5b — bus keeps a flat ~+15%  (OPEN — independent cause)

- **ASKED:** why does bus_cif stay ~+15% worse at *both* quality halves, regardless of qscale?
- **MEASURED:** at `qscale 1.0` the ARF costs a consistent 10–13% of the stream on every clip.
  akiyo/crf25 nets **−2.9% total bytes** (P-frame savings exceed the ARF's cost — ARF working).
  bus nets **+6.1%** (crf25) and **+10.5%** (crf43) — the P frames recover only ~3% against a
  10–13% cost.
- **ANSWER (partial):** on bus the ARF is coded but barely *used* as a predictor. The cost is
  flat and the benefit is absent, which is a different failure from the rate floor.
- **CONFIDENCE:** medium — the bit accounting is solid, the "barely used" attribution is not
  yet measured directly (needs a per-block reference-selection histogram).
- **STATUS:** OPEN. This is the remaining ARF gap and the reason ARF is still not default-on.

## D6b — the recon oracle does not cover the ARF path  (OPEN)

- **MEASURED:** `VP9_RECON_CHECK=1` prints nothing when ARF is active. `recon_check` is called
  from `code_frame_q`; the ARF path runs through `code_altref_group` / `code_arf_slotted`,
  which bypass it.
- **ANSWER:** **the ARF path has never been verified for encoder/decoder reconstruction
  consistency** — the exact bug class that cost 17 dB in the main path this session
  (`6954dbf`), with ARF's own 3-slot reference management unchecked.
- **STATUS:** OPEN, and it should be closed before ARF is ever considered for default-on.
  Needs the oracle to handle hidden (non-displayed) frames.

---

## Refuted / not causes

- **Frame misalignment from hidden frames** — refuted at D6, counts 30/30/30.
- **"ARF is simply bad for static content"** (the standing code comment) — refuted. akiyo is
  the *best* case once the q boost is fixed (−1.69% at high quality); bus, the high-motion
  clip the comment says ARF suits, is the worst. The comment's polarity is backwards against
  current data, though the comment's own numbers (bus −27%) were taken before the MV-clamp
  fix and are not reproducible now.
