#!/usr/bin/env bash
# Fetch libvpx VP9 conformance vectors for the bit-exactness gate
# (crates/rff/tests/vp9_conformance.rs). Then:
#
#   VP9_VECTORS_DIR=<dir> cargo test -p rff --test vp9_conformance --release \
#       -- --ignored --nocapture
#
# Usage: scripts/fetch-vp9-vectors.sh [dest-dir]   (default: ./vp9-vectors)
set -euo pipefail
DIR="${1:-vp9-vectors}"
BASE="https://storage.googleapis.com/downloads.webmproject.org/test_data/libvpx"
mkdir -p "$DIR"
cd "$DIR"

# Tile vectors (the parallelism-critical set) plus a feature sample. The full
# ~315-vector list lives in libvpx's test/test_vectors.cc — add names here to
# widen coverage; the harness runs whatever .webm + .webm.md5 pairs it finds.
VECTORS=(
  vp90-2-08-tile_1x2 vp90-2-08-tile_1x4 vp90-2-08-tile_1x8
  vp90-2-08-tile_1x2_frame_parallel vp90-2-08-tile_1x4_frame_parallel
  vp90-2-08-tile_1x8_frame_parallel
  vp90-2-08-tile-4x1 vp90-2-08-tile-4x4
  vp90-2-00-quantizer-00 vp90-2-00-quantizer-32 vp90-2-00-quantizer-63
  vp90-2-01-sharpness-1 vp90-2-01-sharpness-7
  vp90-2-02-size-130x132 vp90-2-03-size-196x196
  vp90-2-09-lf_deltas vp90-2-06-bilinear
)
for v in "${VECTORS[@]}"; do
  for ext in webm webm.md5; do
    if curl -fsS -A rff --max-time 120 -o "$v.$ext" "$BASE/$v.$ext"; then
      echo "got  $v.$ext"
    else
      echo "skip $v.$ext (not found)"
      rm -f "$v.$ext"
    fi
  done
done
echo "vectors in $DIR: $(ls ./*.webm 2>/dev/null | wc -l)"
