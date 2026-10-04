#!/usr/bin/env bash
# Run a crate's tests on aarch64 under qemu-user, from WSL, against this checkout.
#
# A NEON kernel that only COMPILES has proved nothing: its `*_matches_scalar`
# oracle has to execute on an aarch64 core. This runs the real test binaries
# under `qemu-aarch64`, so NEON twins are gated on this x86 box exactly as the
# AVX ones are -- bit-identity asserted, not assumed.
#
#   wsl -d Ubuntu -- bash /mnt/f/coding/remade_ffmpeg_rs/tools/bench/arm_qemu_test.sh [crate] [cargo test args...]
#
# One-time WSL setup: sudo apt-get install gcc-aarch64-linux-gnu libc6-dev-arm64-cross qemu-user
#                     rustup target add aarch64-unknown-linux-gnu
# The target dir lives in the WSL home, not on the Windows checkout.
set -euo pipefail
crate="${1:-rusty_mp3}"
shift || true
here="$(cd "$(dirname "$0")/../.." && pwd)"
export CARGO_TARGET_DIR="${CARGO_TARGET_DIR:-$HOME/rmtarget}"
export CARGO_TARGET_AARCH64_UNKNOWN_LINUX_GNU_LINKER=aarch64-linux-gnu-gcc
export CARGO_TARGET_AARCH64_UNKNOWN_LINUX_GNU_RUNNER="qemu-aarch64 -L /usr/aarch64-linux-gnu"
cd "$here"
cargo test -p "$crate" --release --target aarch64-unknown-linux-gnu "$@"
