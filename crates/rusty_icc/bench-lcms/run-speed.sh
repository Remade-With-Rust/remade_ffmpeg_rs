#!/usr/bin/env bash
# Side-by-side speed, rusty_icc vs lcms2, inside WSL. One process per case,
# pinned to one CPU at raised priority; the binary interleaves the arms
# (order reversed every round), times each run with a monotonic in-process
# clock, and reports min/median, Mpx/s, cpu/wall per arm, paired win rates
# with z, and a null arm (a second, identical rusty_icc transform).
# Usage: run-speed.sh [CPU] [PAIRS] [SIDE] [CASES...]
set -euo pipefail
HERE=$(cd "$(dirname "$0")" && pwd)
CPU=${1:-4}; PAIRS=${2:-21}; SIDE=${3:-4096}; shift 3 || true
CASES=${*:-srgb-p3-u8 srgb-p3-u16 srgb-fogra-u8 fogra-srgb-u16 srgb-lab-f32 lab-srgb-f32}
bash "$HERE/sync-wsl.sh" "$(dirname "$HERE")" >/dev/null
cd /root/iccbench/rusty_icc/bench-lcms
export PATH=$HOME/.cargo/bin:$PATH
# The repository pins 1.95.0 (rust-toolchain.toml at the repo root, not copied).
export RUSTUP_TOOLCHAIN=${RUSTUP_TOOLCHAIN:-1.95.0}
cargo build --release 2>&1 | grep -E "^(error|warning)" -A6 || true
export RUSTY_ICC_FOGRA39=${RUSTY_ICC_FOGRA39:-/root/iccbench/CoatedFOGRA39.icc}
BIN=./target/release/rusty_icc_bench_lcms
echo "method: $(grep -m1 'model name' /proc/cpuinfo | cut -d: -f2 | xargs); $(uname -r);" \
     "lcms2 $(dpkg-query -W -f='${Version}' liblcms2-2); $(rustc --version);" \
     "taskset -c $CPU nice -n -5; ${PAIRS} interleaved rounds; ${SIDE}x${SIDE};" \
     "binary $(stat -c %y $BIN | cut -d. -f1); load $(cut -d' ' -f1-3 /proc/loadavg)"
for c in $CASES; do
  taskset -c "$CPU" nice -n -5 "$BIN" speed "$c" "$PAIRS" "$SIDE"
  echo
done
