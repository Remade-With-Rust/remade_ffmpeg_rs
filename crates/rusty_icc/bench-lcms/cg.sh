#!/usr/bin/env bash
# Instruction counts (callgrind Ir) for the fixed workloads of
# examples/icc_ir.rs, inside WSL. Invoke it the SAME way every time (the
# environment is part of the count): MSYS_NO_PATHCONV=1 wsl -e bash .../cg.sh [MODES]
# Prints "mode  Ir  anchors"; raw callgrind files land in /root/iccbench/cg/.
# The modes run CG_JOBS at a time (default 8): the count is per process and
# only the `hot_*` functions are collected, so parallel runs give the same
# numbers as serial ones.
set -euo pipefail
HERE=$(cd "$(dirname "$0")" && pwd)
bash "$HERE/sync-wsl.sh" "$(dirname "$HERE")" >/dev/null
cd /root/iccbench/rusty_icc
export PATH=$HOME/.cargo/bin:$PATH
export RUSTUP_TOOLCHAIN=1.95.0
export CARGO_PROFILE_RELEASE_DEBUG=line-tables-only
export RUSTY_ICC_FOGRA39=${RUSTY_ICC_FOGRA39:-/root/iccbench/CoatedFOGRA39.icc}
export RUSTY_ICC_FOGRA39_V4=${RUSTY_ICC_FOGRA39_V4:-/root/iccbench/CoatedFOGRA39-v4.icc}
if ! cargo build --release --example icc_ir >/tmp/icc_ir_build.log 2>&1; then
  grep -E "^(error|warning)" -A12 /tmp/icc_ir_build.log || tail -30 /tmp/icc_ir_build.log
  echo "BUILD FAILED"
  exit 1
fi
grep -E "^warning" -A6 /tmp/icc_ir_build.log || true
BIN=target/release/examples/icc_ir
OUT=/root/iccbench/cg
mkdir -p "$OUT"
# Every mode the binary knows unless some are named.
MODES=${*:-$("$BIN" x 2>&1 | sed -n "s/^modes: //p")}
echo "binary $(stat -c %y $BIN | cut -d. -f1) $(sha256sum $BIN | cut -c1-12)"
JOBS=${CG_JOBS:-8}
one() {
  local m=$1 anchors ir
  anchors=$(valgrind --tool=callgrind --toggle-collect='*hot_*' \
    --callgrind-out-file="$OUT/$m.cg" "$BIN" "$m" 2>/dev/null)
  ir=$(grep -m1 '^summary:' "$OUT/$m.cg" | awk '{print $2}')
  printf "%-10s %14s  %s\n" "$m" "$ir" "$anchors" >"$OUT/$m.line"
}
for m in $MODES; do
  while [ "$(jobs -rp | wc -l)" -ge "$JOBS" ]; do wait -n; done
  one "$m" &
done
wait
for m in $MODES; do cat "$OUT/$m.line"; done
