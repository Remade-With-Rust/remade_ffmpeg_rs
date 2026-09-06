#!/usr/bin/env bash
# Fetch the JCT-VC HEVC_v1 conformance bitstreams for the HEVC bit-exactness
# gate (docs/plans/rusty_hevc.md, H0.1). Then:
#
#   python tools/hevc/conform.py --vectors hevc-vectors --decoder ffmpeg
#
# Usage: scripts/fetch-hevc-vectors.sh [dest-dir] [NAME ...]
#   dest-dir  default ./hevc-vectors
#   NAME ...  optional subset of stream names (zip basenames without .zip);
#             default = every zip listed in the ITU directory (147, ~130 MB).
#
# Layout produced (flat, one stream = one basename):
#   <name>.bit      the Annex-B bitstream (renamed from .bin where needed)
#   <name>.yuv.md5  the published MD5 of the whole decoded YUV, bare 32-hex
#   <name>.txt      the submitter's description
#   <name>.sha256   sha256 of the bitstream as fetched (provenance)
# The .yuv / .trc / trace files inside the zips are NOT extracted (they are
# hundreds of MB and the md5 replaces them); zips are deleted after extraction.
# Idempotent: a stream whose .bit and .yuv.md5 already exist is skipped.
set -euo pipefail
DIR="${1:-hevc-vectors}"
shift || true
BASE="https://www.itu.int/wftp3/av-arch/jctvc-site/bitstream_exchange/draft_conformance/HEVC_v1"
mkdir -p "$DIR"
DIR="$(cd "$DIR" && pwd)"

if [ $# -gt 0 ]; then
  NAMES=("$@")
else
  # Directory listing -> zip basenames.
  mapfile -t NAMES < <(curl -sS -m 120 "$BASE/" | grep -o 'HEVC_v1/[^"]*\.zip' | sed 's#HEVC_v1/##; s#\.zip$##' | sort -u)
  [ "${#NAMES[@]}" -gt 0 ] || { echo "listing returned no zips" >&2; exit 1; }
fi
echo "fetching ${#NAMES[@]} streams into $DIR"

ok=0; skipped=0; failed=()
for name in "${NAMES[@]}"; do
  if [ -f "$DIR/$name.bit" ] && [ -f "$DIR/$name.yuv.md5" ]; then
    skipped=$((skipped+1)); continue
  fi
  tmp="$(mktemp -d)"
  if ! curl -sS -m 600 -f -o "$tmp/$name.zip" "$BASE/$name.zip"; then
    echo "  FAIL download $name" >&2; failed+=("$name"); rm -rf "$tmp"; continue
  fi
  # -j flattens subfolders; skip the big yuv / trace payloads.
  unzip -o -j -q "$tmp/$name.zip" -d "$tmp/x" -x '*.yuv' '*.trc' '*trace*' '*Trace*' '*.log' 2>/dev/null || true
  bit="$(ls "$tmp/x"/*.bit "$tmp/x"/*.bin 2>/dev/null | head -1 || true)"
  if [ -z "$bit" ]; then
    echo "  FAIL no bitstream in $name.zip: $(ls "$tmp/x" | tr '\n' ' ')" >&2; failed+=("$name"); rm -rf "$tmp"; continue
  fi
  # The yuv md5: prefer *_yuv.md5 / *yuv*.md5, else a .md5 that does not
  # name the bitstream itself (those are md5s OF the .bit, not of the output).
  # (MainConcept ships it as <name>_md5.txt.)
  md5file=""
  for cand in "$tmp/x"/*yuv*.md5 "$tmp/x"/*.md5 "$tmp/x"/*md5*.txt; do
    [ -f "$cand" ] || continue
    if grep -qiE '\.(bit|bin)\b' "$cand"; then continue; fi
    md5file="$cand"; break
  done
  if [ -z "$md5file" ]; then
    echo "  FAIL no yuv md5 in $name.zip: $(ls "$tmp/x" | tr '\n' ' ')" >&2; failed+=("$name"); rm -rf "$tmp"; continue
  fi
  hex="$(grep -oiE '[0-9a-f]{32}' "$md5file" | head -1 | tr 'A-F' 'a-f')"
  [ -n "$hex" ] || { echo "  FAIL unparsable md5 in $name.zip" >&2; failed+=("$name"); rm -rf "$tmp"; continue; }
  cp "$bit" "$DIR/$name.bit"
  printf '%s\n' "$hex" > "$DIR/$name.yuv.md5"
  txt="$(ls "$tmp/x"/*.txt 2>/dev/null | head -1 || true)"
  [ -n "$txt" ] && cp "$txt" "$DIR/$name.txt"
  sha256sum "$DIR/$name.bit" | cut -d' ' -f1 > "$DIR/$name.sha256"
  rm -rf "$tmp"
  ok=$((ok+1)); echo "  ok $name"
done
echo "done: $ok fetched, $skipped already present, ${#failed[@]} failed${failed[*]:+: ${failed[*]}}"
[ "${#failed[@]}" -eq 0 ]
