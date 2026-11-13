#!/usr/bin/env bash
# Sweep stale Rust build artifacts out of remade_ffmpeg_rs and the sibling
# codec repos checked out next to it (../rusty_*, ../rusty-*): anything in a
# target/ dir that no build has used for SWEEP_DAYS days (default 14) is
# deleted by cargo-sweep. Fresh artifacts are kept, so incremental builds stay
# warm.
#
# Runs from the Claude Code SessionEnd hook in .claude/settings.json. In that
# mode it detaches itself (session exit never waits on it), runs at most once a
# day (stamp in target/), and is a silent no-op if cargo-sweep isn't installed.
#
#   scripts/sweep-targets.sh          # hook mode: detach, sweep, log to target/sweep.log
#   scripts/sweep-targets.sh --now    # sweep in the foreground and print what was removed
#   SWEEP_DRY_RUN=1 scripts/sweep-targets.sh --now   # only report what would go
#
# One-time setup: cargo install cargo-sweep
set -u

repo=$(cd "$(dirname "$0")/.." && pwd)
days=${SWEEP_DAYS:-14}
dry=()
[ -n "${SWEEP_DRY_RUN:-}" ] && dry=(--dry-run)
stamp="$repo/target/.last-sweep"

if [ "${1:-}" != "--now" ]; then
  command -v cargo-sweep >/dev/null 2>&1 || exit 0
  # At most once a day: skip while the stamp is younger than 24 h.
  if [ -f "$stamp" ] && [ -z "$(find "$stamp" -mmin +1440 2>/dev/null)" ]; then
    exit 0
  fi
  mkdir -p "$repo/target"
  nohup bash "$0" --now >"$repo/target/sweep.log" 2>&1 </dev/null &
  exit 0
fi

if ! command -v cargo-sweep >/dev/null 2>&1; then
  echo "cargo-sweep is not installed: cargo install cargo-sweep" >&2
  exit 1
fi
mkdir -p "$repo/target"
[ ${#dry[@]} -eq 0 ] && touch "$stamp"
echo "cargo sweep${dry[*]:+ (dry run)}: artifacts unused for ${days} days ($(date))"

parent=$(dirname "$repo")
for dir in "$repo" "$parent"/rusty_* "$parent"/rusty-*; do
  [ -d "$dir/target" ] || continue
  echo "== $dir"
  if [ -f "$dir/Cargo.toml" ]; then
    cargo sweep "${dry[@]}" --time "$days" "$dir"
  else
    # A repo of several workspaces (e.g. rusty-av1-toolkit): find each one.
    cargo sweep "${dry[@]}" --recursive --time "$days" "$dir"
  fi
done
echo "done ($(date))"
