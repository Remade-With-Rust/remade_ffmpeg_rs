#!/bin/bash
# Fetch the complex-music quality corpus: strictly non-copyrighted (CC0 / Public
# Domain) clips from Wikimedia Commons, trimmed to short 44.1 kHz mono references.
#
# The notification-sound corpus (/c/Windows/Media/Ring*.wav) is tonal/sustained —
# it fires <10% short blocks, so it cannot stress transient/short-block or bit-
# allocation work. These music clips fire ~27% short blocks (piano/guitar) and are
# spectrally dense, the right content for Floor 2 (short blocks) and the reservoir RD.
#
# Usage:  bash tools/quality/fetch_corpus.sh [OUT_DIR]   (default ./corpus)
#   FFMPEG=/path/to/ffmpeg.exe to override the binary.
set -e
OUT="${1:-./corpus}"; mkdir -p "$OUT/dl"
FF="${FFMPEG:-ffmpeg}"
UA="rff-corpus/1.0 (quality-eval; non-commercial)"
WC="https://upload.wikimedia.org/wikipedia/commons"

# name | license | source URL | trim-start(s) | trim-len(s)
CLIPS=(
  "mus_piano|CC0|$WC/d/d8/Kimiko_Ishizaka_-_J.S._Bach-_-Open-_Goldberg_Variations%2C_BWV_988_%28Piano%29_-_09_Variatio_8_a_2_Clav.mp3|14|6"
  "mus_guitar|Public domain|$WC/2/2f/Legend_%28Leyenda%29_performed_by_Michael_Laucke.flac|42|6"
  "mus_vocal|Public domain|$WC/5/5d/Wolfgang_Amadeus_Mozart_-_cosi_fan_tutte_act_ii_-_no._19_aria_-_una_donna_a_quindici_anni.ogg|16|6"
)

for row in "${CLIPS[@]}"; do
  IFS='|' read -r name lic url ss t <<<"$row"
  ext="${url##*.}"
  echo ">> $name ($lic)"
  curl -sL --max-time 180 -A "$UA" -o "$OUT/dl/$name.$ext" "$url"
  # f32 reference for PEAQ + s16 sibling for the encoder/NMR harness
  "$FF" -y -loglevel error -ss "$ss" -t "$t" -i "$OUT/dl/$name.$ext" -ac 1 -ar 44100 -c:a pcm_f32le "$OUT/corp_$name.wav"
  "$FF" -y -loglevel error -ss "$ss" -t "$t" -i "$OUT/dl/$name.$ext" -ac 1 -ar 44100 -c:a pcm_s16le "$OUT/$name.wav"
done
echo "corpus ready in $OUT (CC0/PD only — safe to encode/redistribute results)"
