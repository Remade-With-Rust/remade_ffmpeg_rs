#!/usr/bin/env bash
# Byte-identity gate for an output-preserving change ANYWHERE on the CLI's MP3
# paths -- the adapter, the demuxers/muxers, the transcode conform chain -- which
# the codec-level gates (mp3_encode_identity.sh, decode hashes) never execute.
#
#   tools/bench/mp3_cli_identity.sh <baseline rff.exe> <candidate rff.exe>
#
# Encode: WAV -> MP3 over s16 and f32 input, stereo and mono, MPEG-1/2/2.5
# rates, CBR 128/192, CBR 320 (no reservoir), VBR, and the lookahead reservoir.
# Conform: WAV s16 <-> f32 (the shared sample-format conversion).
# Decode: MP3 -> WAV (s16 and f32 output) on every MP3 the encode half wrote,
# plus any LAME/shine streams in corpus/dl. Every pair must `cmp` identical.
set -euo pipefail
base="$1"; cand="$2"
root="$(cd "$(dirname "$0")/../.." && pwd)"
tmp="${IDENTITY_TMP:-/f/coding/tmp/mp3cli_identity}"
mkdir -p "$tmp"

inputs=("$root"/corpus/corp_st_mus_guitar.wav "$root"/corpus/corp_long_mus_piano.wav)
for r in 22050 11025; do
  f="/f/coding/tmp/mp3spectrum/src/guitar_${r}_2.wav"
  [ -f "$f" ] && inputs+=("$f")
done
# An f32 twin of the stereo clip, made by the BASELINE so both arms read it.
"$base" -y -i "$root/corpus/corp_st_mus_guitar.wav" -c:a pcm_f32le "$tmp/st_f32.wav" >/dev/null 2>&1
inputs+=("$tmp/st_f32.wav")

n=0
run_pair() { # <tag> <env> <args...>
  local tag="$1" env="$2"; shift 2
  env $env "$base" -y "$@" "$tmp/a.out" >/dev/null 2>&1
  env $env "$cand" -y "$@" "$tmp/b.out" >/dev/null 2>&1
  [ -s "$tmp/a.out" ] || { echo "EMPTY baseline output: $tag"; exit 1; }
  if ! cmp -s "$tmp/a.out" "$tmp/b.out"; then echo "DIFFERS: $tag"; exit 1; fi
  n=$((n + 1))
}

for w in "${inputs[@]}"; do
  stem=$(basename "$w" .wav)
  for mode in "-b:a 128k" "-b:a 192k" "-b:a 320k" "-q:a 2"; do
    run_pair "$stem enc $mode" "" -i "$w" -c:a mp3 $mode -f mp3
    cp "$tmp/a.out" "$tmp/${stem}_$(echo "$mode" | tr -d " :-").mp3"
  done
  run_pair "$stem enc lookahead" "MP3_RESV_LOOKAHEAD=1" -i "$w" -c:a mp3 -b:a 128k -f mp3
done

# The shared sample-format conform, both directions (s16 -> f32, f32 -> s16).
run_pair "conform s16->f32" "" -i "$root/corpus/corp_st_mus_guitar.wav" -c:a pcm_f32le -f wav
run_pair "conform f32->s16" "" -i "$tmp/st_f32.wav" -c:a pcm_s16le -f wav

for m in "$tmp"/*.mp3 "$root"/corpus/dl/*.mp3; do
  [ -f "$m" ] || continue
  run_pair "$(basename "$m") dec s16" "" -i "$m" -c:a pcm_s16le -f wav
  run_pair "$(basename "$m") dec f32" "" -i "$m" -c:a pcm_f32le -f wav
done
echo "$n/$n CLI runs byte-identical (baseline vs candidate)"
