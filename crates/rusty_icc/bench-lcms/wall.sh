#!/usr/bin/env bash
# Pinned, interleaved wall check of two icc_ir builds (arms saved with
# `save-arm.sh NAME`), inside WSL. Each run is one process pinned to one CPU
# that times the hot call REPS times in-process (monotonic clock) and reports
# its minimum; the arms alternate ABBA (the leading arm flips every pair).
# Prints per mode: each arm's floor (min of mins), the median of the per-pair
# B/A ratios and in how many pairs B was faster, with z.
# Usage: wall.sh ARM_A ARM_B CPU PAIRS REPS MODES...   (ARM_A == ARM_B: null arm)
set -euo pipefail
A=/root/iccbench/arms/$1; B=/root/iccbench/arms/$2; CPU=$3; PAIRS=$4; REPS=$5; shift 5
export RUSTY_ICC_FOGRA39=${RUSTY_ICC_FOGRA39:-/root/iccbench/CoatedFOGRA39.icc}
echo "method: taskset -c $CPU; $PAIRS ABBA pairs; $REPS in-process reps per run, min; A=$(basename "$A") B=$(basename "$B"); load $(cut -d' ' -f1-3 /proc/loadavg)"
run() { taskset -c "$CPU" "$1" "$2" "$REPS" | awk '/^time_ns/ {print $3}'; }
for m in "$@"; do
  fa=$("$A" "$m" | head -1); fb=$("$B" "$m" | head -1)
  [ "$fa" = "$fb" ] || { echo "$m ANCHOR MISMATCH: $fa / $fb"; continue; }
  as=(); bs=()
  for ((i = 0; i < PAIRS; i++)); do
    if ((i % 2 == 0)); then a=$(run "$A" "$m"); b=$(run "$B" "$m")
    else b=$(run "$B" "$m"); a=$(run "$A" "$m"); fi
    as+=("$a"); bs+=("$b")
  done
  python3 - "$m" "${as[*]}" "${bs[*]}" <<'PY'
import sys, statistics, math
m = sys.argv[1]
a = [int(x) for x in sys.argv[2].split()]
b = [int(x) for x in sys.argv[3].split()]
r = sorted(y / x for x, y in zip(a, b))
wins = sum(1 for x, y in zip(a, b) if y < x)
n = len(a)
z = (wins - n / 2) / (0.5 * math.sqrt(n))
print(f"{m:11s} A {min(a)/1e6:9.3f} ms  B {min(b)/1e6:9.3f} ms  floor B/A {min(b)/min(a):.4f}  "
      f"median pair B/A {statistics.median(r):.4f}  B faster {wins}/{n} (z {z:+.2f})")
PY
done
