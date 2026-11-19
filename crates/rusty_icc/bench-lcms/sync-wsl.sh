#!/usr/bin/env bash
# Copies the crate from the Windows checkout into the WSL-native filesystem
# (cargo over the 9p /mnt mount hangs) and makes the copy standalone.
# Usage (from Git Bash): MSYS_NO_PATHCONV=1 wsl -e bash /mnt/e/.../bench-lcms/sync-wsl.sh /mnt/e/.../crates/rusty_icc
set -euo pipefail
SRC=${1:?path to crates/rusty_icc under /mnt}
DST=${2:-/root/iccbench/rusty_icc}
mkdir -p "$DST"
# --checksum without -t: changed files get a fresh mtime, so cargo never
# misses an edit because of clock skew between Windows and WSL.
rsync -rl --checksum --delete --exclude target --exclude 'bench-lcms/target' "$SRC"/ "$DST"/
# Outside the repo workspace the inherited dev-dependency cannot resolve.
sed -i 's/^rusty_alloc-api.workspace = true/rusty_alloc-api = "2.5.0"/' "$DST/Cargo.toml"
echo "synced to $DST"
