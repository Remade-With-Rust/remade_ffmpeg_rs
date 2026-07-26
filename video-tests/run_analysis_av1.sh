#!/usr/bin/env bash
# Drive the full AV1 function-level analysis: rusty_av1e / rusty_av1d vs libaom,
# dav1d and ffmpeg, encoder + decoder, at default settings, on the fixed corpus.
#
#   bash video-tests/run_analysis_av1.sh                          # whole corpus
#   CLIPS=akiyo_cif,mobile_cif bash video-tests/run_analysis_av1.sh  # a subset
#   FRAMES=0 bash video-tests/run_analysis_av1.sh                 # whole clips (slow)
#
# Three passes. Unlike the VP9 driver, the analyzer is built TWICE: the AV1 forks
# gate their stage profilers with a cargo feature (not at runtime), so the
# throughput binary must carry no instrumentation at all.
#   1. speed   (built WITHOUT `profile`) -> results/av1/speed.tsv
#   2. stages  (built WITH    `profile`) -> results/av1/stages.tsv
#   3. report  (merge)                   -> results/av1/REPORT.md
set -eu
cd "$(dirname "$0")/analyzer-av1"

# The C references are optional: ours-vs-ffmpeg is a complete measurement without
# them, they only add per-function attribution on the reference side.
AOM_DIR="${AOM_DIR:-../../../_ref_aom}"
DAV1D_DIR="${DAV1D_DIR:-../../../_ref_dav1d}"
missing=0
for p in "$AOM_DIR/av1enc.exe" "$AOM_DIR/av1enc-prof.exe" \
         "$DAV1D_DIR/av1dec.exe" "$DAV1D_DIR/av1dec-prof.exe"; do
  [ -x "$p" ] || missing=1
done
if [ "$missing" = 1 ]; then
  echo "!! C references not fully built — the run will use the ffmpeg arm only."
  echo "   To get per-function numbers on the reference side:"
  echo "     cd $AOM_DIR   && python instrument.py && bash build.sh && bash build.sh prof"
  echo "     cd $DAV1D_DIR && python instrument.py && bash build.sh && bash build.sh prof"
  echo
fi

if [ -z "${CLIPS_DIR:-}" ] && [ ! -d ../clips ] && [ ! -d ../../../rs_h264/video-tests/clips ]; then
  echo "!! no corpus found — run video-tests/fetch_clips.sh, or set CLIPS_DIR"
  exit 1
fi

echo "### 1/3  throughput (profiler OFF)"
cargo build --release -q
./target/release/analyzer-av1 speed

echo
echo "### 2/3  per-function breakdown (profiler ON)"
cargo build --release -q --features profile
./target/release/analyzer-av1 stages

echo
echo "### 3/3  report"
cargo build --release -q
./target/release/analyzer-av1 report

echo
echo "results in video-tests/results/av1/ — REPORT.md, speed.tsv, stages.tsv"
