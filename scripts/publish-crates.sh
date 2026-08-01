#!/usr/bin/env bash
# Publish the remade_ffmpeg_rs crate family to crates.io, one crate at a time,
# respecting the PublishNew rate limit (burst 5, then 1 per 10 minutes).
#
# This is "Path B" from docs/publishing-plan.md — the fallback for when a rate
# limit increase has NOT been granted. If it has, you don't need this script:
#
#     cargo publish --workspace --exclude rff-ui
#
# does the whole thing in one shot, resolving order itself.
#
# Usage:
#   scripts/publish-crates.sh --dry-run     # print the plan, upload nothing
#   scripts/publish-crates.sh               # do it (~7 hours, resumable)
#
# Resumable: a crate+version already on crates.io is treated as SUCCESS, not an
# error, so re-running after any interruption picks up where it left off.
# Progress is appended to .publish-progress.log.

set -uo pipefail

DRY_RUN=0
[ "${1:-}" = "--dry-run" ] && DRY_RUN=1

SLEEP_SECONDS="${SLEEP_SECONDS:-620}"   # 10 min + margin
BURST="${BURST:-5}"                     # free uploads before throttling kicks in
LOG=".publish-progress.log"

# Publish order: strictly by dependency wave. Within a wave order is free.
# Derived from the manifests; see docs/publishing-plan.md §3.
CRATES=(
  # wave 0 — no local dependencies (rusty_jpeg is a prerequisite of
  # rff-codec-jpeg, so it belongs here, not off to one side)
  rusty_jpeg
  rff-core rff-auth rff-resample rff-subtitle
  # wave 1 — abstractions over rff-core
  rff-codec rff-format rff-filter rff-io
  # wave 2 — leaf codecs
  rff-codec-aac rff-codec-avif rff-codec-flac rff-codec-gif rff-codec-h264
  rff-codec-jpeg rff-codec-jxl rff-codec-mp3 rff-codec-openh264 rff-codec-opus
  rff-codec-pcm rff-codec-png rff-codec-rawvideo rff-codec-vorbis
  rff-codec-vp9 rff-codec-webp
  # wave 2 — leaf containers
  rff-format-avi rff-format-avif rff-format-flac rff-format-flv rff-format-gif
  rff-format-ivf rff-format-jpeg rff-format-jxl rff-format-mkv rff-format-mp3
  rff-format-mp4 rff-format-ogg rff-format-png rff-format-srt rff-format-ts
  rff-format-wav rff-format-webp rff-format-webvtt rff-format-y4m
  # wave 3 — depends on rff-format-ts
  rff-format-hls
  # wave 4 — the engine facade, depends on everything above
  rff
  # wave 5 — front-ends
  rff-cli rff-server
)

# rff-ui is deliberately absent: MPL-2.0 webview deps, `publish = false`.

echo "crates to publish: ${#CRATES[@]}"
if [ "$DRY_RUN" = 1 ]; then
  printf '%s\n' "${CRATES[@]}" | nl
  after_burst=$(( ${#CRATES[@]} - BURST ))
  [ "$after_burst" -lt 0 ] && after_burst=0
  mins=$(( after_burst * SLEEP_SECONDS / 60 ))
  echo
  echo "estimated wall time: ~${mins} minutes ($(( mins / 60 ))h $(( mins % 60 ))m)"
  echo "dry run — nothing uploaded."
  exit 0
fi

command -v cargo >/dev/null || { echo "cargo not on PATH"; exit 1; }

seen=0        # crates processed (including skips)
uploaded=0    # actual uploads — only these consume rate-limit tokens
for crate in "${CRATES[@]}"; do
  seen=$((seen + 1))
  echo
  echo "=== [$seen/${#CRATES[@]}] $crate ==="

  # Pace only AFTER real uploads. Sleeping before the attempt would burn 10
  # minutes on every already-published crate during a resume, which for this
  # workspace is over an hour of dead time.
  if [ "$uploaded" -ge "$BURST" ] && [ "$uploaded" -gt 0 ]; then
    echo "    rate limit: sleeping ${SLEEP_SECONDS}s (bucket spent)"
    sleep "$SLEEP_SECONDS"
  fi

  output=$(cargo publish -p "$crate" --allow-dirty 2>&1)
  status=$?

  if [ "$status" -eq 0 ]; then
    echo "    OK — uploaded"
    echo "$(date -u +%FT%TZ) OK $crate" >> "$LOG"
    uploaded=$((uploaded + 1))
  elif echo "$output" | grep -qi "already uploaded\|already exists"; then
    # Resume case: this version is already on crates.io. Not an error, and it
    # cost no rate-limit token — so do NOT pace after it.
    echo "    already published — skipping (no token spent)"
    echo "$(date -u +%FT%TZ) SKIP $crate (already published)" >> "$LOG"
  elif echo "$output" | grep -qi "too many"; then
    echo "    RATE LIMITED. crates.io said:"
    echo "$output" | sed 's/^/      /'
    echo "$(date -u +%FT%TZ) RATELIMIT $crate" >> "$LOG"
    echo
    echo "Re-run this script to resume from here; already-published crates are skipped."
    exit 2
  else
    echo "    FAILED:"
    echo "$output" | sed 's/^/      /'
    echo "$(date -u +%FT%TZ) FAIL $crate" >> "$LOG"
    echo
    echo "Stopping. Fix the problem, then re-run to resume."
    exit 1
  fi

done

echo
echo "=== done: $seen crates processed, $uploaded newly uploaded ==="
echo "Verify from OUTSIDE the workspace — the local build hides missing version pins:"
echo "    cargo new /tmp/rff-smoke && cd /tmp/rff-smoke"
echo "    cargo add rff && cargo build"
echo "    cargo install rff-cli && rff -codecs"
