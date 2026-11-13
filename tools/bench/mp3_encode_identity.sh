#!/usr/bin/env bash
# Byte-identity gate for an output-preserving encoder change.
#
#   tools/bench/mp3_encode_identity.sh <baseline encprof.exe> <candidate encprof.exe>
#
# Encodes every corpus file at three CBR rates and one VBR quality with BOTH
# binaries and requires `cmp`-identical output. The corpus spans the content axis
# (real mono + stereo music, the 8 synthetic great-gate classes) and the format
# axis (MPEG-1, MPEG-2 and MPEG-2.5 rates, from tools/bench/mp3_spectrum.py's
# resampled sources when present). Exits non-zero on the first difference.
set -euo pipefail
base="$1"; cand="$2"
root="$(cd "$(dirname "$0")/../.." && pwd)"
tmp="${IDENTITY_TMP:-/f/coding/tmp/mp3identity}"
mkdir -p "$tmp"
files=("$root"/corpus/corp_long_mus_*.wav "$root"/corpus/corp_st_mus_*.wav)
[ -d /f/coding/tmp/classes ] && files+=(/f/coding/tmp/classes/*.wav)
for r in 22050 11025; do
  for c in 1 2; do
    f="/f/coding/tmp/mp3spectrum/src/guitar_${r}_${c}.wav"
    [ -f "$f" ] && files+=("$f")
  done
done
n=0
for w in "${files[@]}"; do
  [ -f "$w" ] || continue
  stem=$(basename "$w" .wav)
  for mode in 96 128 192 vbr; do
    if [ "$mode" = vbr ]; then
      VBR_Q=2 "$base" "$w" 128 "$tmp/a.mp3" >/dev/null 2>&1
      VBR_Q=2 "$cand" "$w" 128 "$tmp/b.mp3" >/dev/null 2>&1
    else
      "$base" "$w" "$mode" "$tmp/a.mp3" >/dev/null 2>&1
      "$cand" "$w" "$mode" "$tmp/b.mp3" >/dev/null 2>&1
    fi
    if ! cmp -s "$tmp/a.mp3" "$tmp/b.mp3"; then
      echo "DIFFERS: $stem @ $mode"
      exit 1
    fi
    n=$((n + 1))
  done
done
echo "$n/$n encodes byte-identical (baseline vs candidate)"
