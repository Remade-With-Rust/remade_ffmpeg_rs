# Quality calibration tools (PEAQ ↔ NMR)

External-PEAQ oracle to calibrate the in-house NMR metric (`lab::quality` +
the `mp3quality` example). PEAQ_python is **not vendored** (external license);
`setup_peaq.py` fetches + patches it.

```sh
# 1. one-time setup (clones + patches PEAQ_python for numpy>=2 / py3.13)
python tools/quality/setup_peaq.py PEAQ_python

# 2. score one (reference, decoded) pair — ODG in [-4,0], delay-aligned internally
python tools/quality/peaq_run.py orig.wav coded.wav PEAQ_python      # -> "ODG= -0.65 delay= 1057"

# 3. calibrate: build a CSV (clip,br,enc,odg,nmr_mean,nmr_max,pct) over a bitrate
#    sweep of ours vs LAME, then correlate ODG against our NMR
python tools/quality/analyze_calib.py calib.csv
```

Decode BOTH candidates with the same neutral decoder (e.g. system ffmpeg) before
scoring, so the comparison is fair. Keep clips short (~5 s) — pure-Python PEAQ is
~2 s/s of audio.

## Calibration result (2026-06-28) — see `docs/mp3-quality-harness-plan.md`
- Our **mean NMR is a valid self-metric** (within-encoder Spearman −0.81/−0.88 vs ODG).
- **`% audible` is the best cross-encoder ODG predictor (−0.90)**; `max NMR` is broken
  (silent-band artifact). → headline on `% audible`.
- **PEAQ verdict:** LAME ahead on average (7/10 wins), but we genuinely beat it on some
  content/bitrates. Use PEAQ for real rankings; NMR for self-tracking.
