#!/usr/bin/env bash
# Sync, build and run the lcms2 accuracy comparison inside WSL.
set -euo pipefail
HERE=$(cd "$(dirname "$0")" && pwd)
bash "$HERE/sync-wsl.sh" "$(dirname "$HERE")"
cd /root/iccbench/rusty_icc/bench-lcms
export PATH=$HOME/.cargo/bin:$PATH
# The repository pins 1.95.0 (rust-toolchain.toml at the repo root, not copied).
export RUSTUP_TOOLCHAIN=${RUSTUP_TOOLCHAIN:-1.95.0}
cargo build --release 2>&1 | grep -E "^(error|warning)" -A6 || true
export RUSTY_ICC_FOGRA39=${RUSTY_ICC_FOGRA39:-/root/iccbench/CoatedFOGRA39.icc}
./target/release/rusty_icc_bench_lcms blackpoints > /root/iccbench/blackpoints.md
./target/release/rusty_icc_bench_lcms accuracy > /root/iccbench/accuracy.md
echo done
