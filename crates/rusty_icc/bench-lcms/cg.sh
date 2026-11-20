#!/usr/bin/env bash
# Instruction counts (callgrind Ir) for the fixed workloads of
# examples/icc_ir.rs, inside WSL. Invoke it the SAME way every time (the
# environment is part of the count): MSYS_NO_PATHCONV=1 wsl -e bash .../cg.sh [MODES]
# Prints "mode  Ir  anchors"; raw callgrind files land in /root/iccbench/cg/.
set -euo pipefail
HERE=$(cd "$(dirname "$0")" && pwd)
bash "$HERE/sync-wsl.sh" "$(dirname "$HERE")" >/dev/null
cd /root/iccbench/rusty_icc
export PATH=$HOME/.cargo/bin:$PATH
export RUSTUP_TOOLCHAIN=1.95.0
export CARGO_PROFILE_RELEASE_DEBUG=line-tables-only
export RUSTY_ICC_FOGRA39=${RUSTY_ICC_FOGRA39:-/root/iccbench/CoatedFOGRA39.icc}
cargo build --release --example icc_ir 2>&1 | grep -E "^(error|warning)" -A6 || true
BIN=target/release/examples/icc_ir
OUT=/root/iccbench/cg
mkdir -p "$OUT"
MODES=${*:-p3u8 p3u16 fogu8 fog16 labf32 labinf32 build3 build4 parse}
echo "binary $(stat -c %y $BIN | cut -d. -f1) $(sha256sum $BIN | cut -c1-12)"
for m in $MODES; do
  anchors=$(valgrind --tool=callgrind --toggle-collect='*hot_*' \
    --callgrind-out-file="$OUT/$m.cg" "$BIN" "$m" 2>/dev/null)
  ir=$(grep -m1 '^summary:' "$OUT/$m.cg" | awk '{print $2}')
  printf "%-9s %14s  %s\n" "$m" "$ir" "$anchors"
done
