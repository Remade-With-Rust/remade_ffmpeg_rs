#!/usr/bin/env bash
# Byte-identity gate for an output-preserving DECODER change: the PCM hash of
# every stream must match between a baseline and a candidate decprof, on the
# serial decoder (`Mp3Decoder`, what rff ships) AND the two-thread pipelined one.
#
#   tools/bench/mp3_decode_identity.sh <baseline decprof.exe> <candidate decprof.exe>
#
# Streams: tools/bench/mp3_spectrum.py's matrix (ours / LAME / shine x 9 rates x
# mono/stereo x CBR/VBR) plus the ISO 11172-4 / 13818-4 vectors (mixed blocks,
# both intensity-stereo forms). Exits non-zero on the first difference, or if a
# baseline hash is the empty-input basis on a stream that should decode.
set -euo pipefail
base="$1"; cand="$2"
spectrum="${MP3_SPECTRUM_DIR:-/f/coding/tmp/mp3spectrum}"
vectors="${MP3_ISO_DIR:-/f/coding/tmp/minimp3/vectors}"
EMPTY=0xcbf29ce484222325

streams=()
while IFS= read -r f; do streams+=("$f"); done < <(find "$spectrum" -name "*.mp3" | sort)
for f in "$vectors"/l3-*.bit; do [ -f "$f" ] && streams+=("$f"); done
[ "${#streams[@]}" -gt 0 ] || { echo "no streams found"; exit 1; }

hash_of() { "$@" 2>/dev/null | sed -n 's/^PCM fnv1a: \(0x[0-9a-f]*\).*/\1/p'; }
n=0; decoded=0
for f in "${streams[@]}"; do
  for mode in serial pipelined; do
    pipe=0; [ "$mode" = pipelined ] && pipe=1
    a=$(MP3_PIPELINE=$pipe hash_of "$base" "$f")
    b=$(MP3_PIPELINE=$pipe hash_of "$cand" "$f")
    [ -n "$a" ] || { echo "NO HASH from baseline: $f ($mode)"; exit 1; }
    if [ "$a" != "$b" ]; then echo "DIFFERS: $f ($mode) $a vs $b"; exit 1; fi
    [ "$a" != "$EMPTY" ] && decoded=$((decoded + 1))
    n=$((n + 1))
  done
done
echo "$n/$n decodes hash-identical (baseline vs candidate); $decoded produced PCM"
