#!/usr/bin/env bash
# Drive the full VP9 function-level analysis: rff-vp9 vs libvpx vs ffmpeg,
# encoder + decoder, everything at default settings, on the fixed corpus.
#
#   bash video-tests/run_analysis.sh                        # whole corpus
#   CLIPS=akiyo_cif,mobile_cif bash video-tests/run_analysis.sh   # a subset
#   FRAMES=0 bash video-tests/run_analysis.sh               # whole clips (slow)
#
# Three passes, because throughput and per-function breakdown cannot come from
# the same run — the rdtsc scopes inflate wall time on both sides:
#   1. speed   (profiler OFF) -> results/speed.tsv
#   2. stages  (profiler ON)  -> results/stages.tsv
#   3. report  (merge)        -> results/REPORT.md
set -eu
cd "$(dirname "$0")/analyzer"

LIBVPX_DIR="${LIBVPX_DIR:-../../../_ref_libvpx}"
for exe in vp9enc.exe vp9dec.exe vp9enc-prof.exe vp9dec-prof.exe; do
  if [ ! -x "$LIBVPX_DIR/$exe" ]; then
    echo "!! libvpx reference not built ($exe missing). Run:"
    echo "     cd $LIBVPX_DIR && python instrument.py && bash build.sh && bash build.sh prof"
    exit 1
  fi
done

# The corpus is shared with the rs_h264 checkout so both codecs are measured on
# byte-identical pixels. CLIPS_DIR overrides; video-tests/clips wins if present.
CLIPS_DIR="${CLIPS_DIR:-}"
if [ -z "$CLIPS_DIR" ] && [ ! -d ../clips ] && [ ! -d ../../../rs_h264/video-tests/clips ]; then
  echo "!! no corpus found — run video-tests/fetch_clips.sh, or set CLIPS_DIR"
  exit 1
fi

echo "### 1/3  throughput (profiler OFF)"
cargo build --release -q
./target/release/analyzer speed

echo
echo "### 2/3  per-function breakdown (profiler ON)"
./target/release/analyzer stages

echo
echo "### 3/3  report"
./target/release/analyzer report

echo
echo "results in video-tests/results/ — REPORT.md, speed.tsv, stages.tsv"
