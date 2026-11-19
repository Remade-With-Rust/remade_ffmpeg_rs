#!/usr/bin/env bash
# Side-by-side speed: every arm pinned to one core, ABBA pairs in one process.
# usage: bench.sh <bigdir> <tiffvs binary> [pairs] [core]
set -eu
DIR=$1; BIN=$2; PAIRS=${3:-20}; CORE=${4:-6}
run() { taskset -c "$CORE" nice -n -5 "$BIN" "$@" 2>&1 | grep -v '^parity' || true; }
echo "== method: taskset -c $CORE, $PAIRS ABBA pairs per row, Instant (CLOCK_MONOTONIC), min of N"
echo "== null arms (same Rust code in both arms)"
run dec "$DIR/rgb8_None.tif" "$PAIRS" null
run dec "$DIR/rgb8_LZW.tif" "$PAIRS" null
run enc "$DIR/rgb8_None.tif" lzw "$PAIRS" null
echo "== decode"
for f in rgb8_None rgb8_LZW rgb8_Zip rgb8_RLE rgb16_None rgb16_LZW rgb16_Zip rgb16_RLE bilevel_G4; do
  "$BIN" dec "$DIR/$f.tif" 1 2>&1 | grep '^parity' || true
  run dec "$DIR/$f.tif" "$PAIRS"
done
echo "== encode (rows_per_strip 16 both, Deflate level 6 both)"
for f in rgb8_None rgb16_None; do
  for c in lzw deflate; do
    "$BIN" enc "$DIR/$f.tif" "$c" 1 2>&1 | grep '^parity' || true
    run enc "$DIR/$f.tif" "$c" "$PAIRS"
  done
done
