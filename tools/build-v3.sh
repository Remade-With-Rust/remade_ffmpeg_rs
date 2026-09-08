#!/bin/sh
# Build the `x86-64-v3` (AVX2) artifact alongside the portable baseline one.
#
# Measured 1.066x faster than the default build, bit-exact, on both an
# x265-encoded clip and a JCT-VC conformance stream (12/15 z=2.32 and 14/15
# z=3.36 on tools/bench/codec-bench.ps1).
#
# The SIMD kernels are already AVX2 wherever the CPU allows -- they carry
# `#[target_feature]` and are chosen at runtime. Everything else, which is
# roughly 87% of the time, compiles for the portable x86-64 baseline (SSE2)
# because the binary has to run anywhere. This build lets that majority use VEX
# encoding, three-operand forms and sixteen ymm registers as well.
#
# x86-64-v3 = AVX2 + BMI1/2 + FMA + LZCNT/MOVBE: Haswell (2013) and later,
# Excavator (2015) and later. It SIGILLs on anything older, which is why it is a
# separate artifact rather than a change to the default one.
#
#   ./build-v3.sh              -> target-v3/release/rusty_h265
#   CPU=native ./build-v3.sh   -> tuned for this machine, not distributable
set -eu

root=$(cd "$(dirname "$0")/.." && pwd)
cpu=${CPU:-x86-64-v3}
dir=${TARGET_DIR:-target-v3}

echo "building with -C target-cpu=$cpu -> $dir"
RUSTFLAGS="-C target-cpu=$cpu" \
  cargo build --release --manifest-path "$root/Cargo.toml" --target-dir "$root/$dir"

cat <<EOF

  $root/$dir/release/rusty_h265

  This binary requires $cpu. Verify it against the portable build before
  shipping -- the two must be bit-identical on every stream:

    ./target/release/rusty_h265 IN.hevc --pipe x | md5sum
    ./$dir/release/rusty_h265 IN.hevc --pipe x | md5sum
EOF
