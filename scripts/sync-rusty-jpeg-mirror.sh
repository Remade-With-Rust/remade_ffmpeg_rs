#!/usr/bin/env bash
#
# Push crates/rusty_jpeg to its dedicated public repo.
#
# rusty_jpeg is published from TWO places on purpose:
#
#   crates/rusty_jpeg/                      <- source of truth, workspace member
#   github.com/Remade-With-Rust/rusty_jpeg  <- public home, for visibility
#
# The monorepo copy is the one that builds, tests and publishes to crates.io.
# The dedicated repo is a MIRROR: never edit it directly, or the next sync will
# silently discard the change. This script is the only supported way to update
# it.
#
# Usage:
#   scripts/sync-rusty-jpeg-mirror.sh [--push] [mirror-dir]
#
# Without --push it stages and shows the diff, and touches no remote.
set -euo pipefail

REPO_ROOT="$(cd "$(dirname "${BASH_SOURCE[0]}")/.." && pwd)"
SRC="$REPO_ROOT/crates/rusty_jpeg"
PUSH=0
MIRROR=""

for arg in "$@"; do
  case "$arg" in
    --push) PUSH=1 ;;
    *) MIRROR="$arg" ;;
  esac
done
MIRROR="${MIRROR:-$REPO_ROOT/../rusty_jpeg_mirror}"

[ -d "$SRC" ] || { echo "no such crate: $SRC" >&2; exit 1; }

if [ ! -d "$MIRROR/.git" ]; then
  echo "mirror not found at $MIRROR — clone it first:" >&2
  echo "  git clone https://github.com/Remade-With-Rust/rusty_jpeg \"$MIRROR\"" >&2
  exit 1
fi

# Exactly the crate content. Repo-only furniture (.github/, .gitignore) lives in
# the mirror and is deliberately NOT touched.
ITEMS=(Cargo.toml README.md NOTICE.md CHANGES.md WHYS.md LICENSE-APACHE LICENSE-MIT src tests examples fuzz)
for item in "${ITEMS[@]}"; do
  [ -e "$SRC/$item" ] || { echo "missing from crate: $item" >&2; exit 1; }
  rm -rf "${MIRROR:?}/$item"
  cp -r "$SRC/$item" "$MIRROR/"
done

VERSION="$(grep -m1 '^version = ' "$SRC/Cargo.toml" | cut -d'"' -f2)"
echo "synced rusty_jpeg $VERSION -> $MIRROR"

# The mirror has to stand on its own: it is not in the workspace, so a manifest
# that leans on workspace inheritance would only fail once it is public.
( cd "$MIRROR" && cargo test --release --quiet ) || {
  echo "mirror does not build/test standalone — not committing" >&2
  exit 1
}

cd "$MIRROR"
git add -A
if git diff --cached --quiet; then
  echo "mirror already up to date"
  exit 0
fi
git --no-pager diff --cached --stat

if [ "$PUSH" -eq 1 ]; then
  git commit -q -m "Sync rusty_jpeg $VERSION from remade_ffmpeg_rs"
  git push -q origin main
  echo "pushed"
else
  echo
  echo "staged only. re-run with --push to commit and push."
fi
