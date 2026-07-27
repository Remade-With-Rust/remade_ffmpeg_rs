#!/usr/bin/env bash
# ABBA-interleaved A/B for one env knob, on a machine that will not hold still.
#
# Sequential "measure A, then measure B" is worthless here: the null arm reads
# +/-10% between two identical encoders, and a run taken minutes later can drift
# 40%. Alternating A and B inside a tight loop makes both arms experience the
# same drift, and the statistic that survives is the PAIRED WIN RATE: in how many
# of N head-to-head rounds did A beat B. Under the null that is a fair coin, so
#   z = (wins - N/2) / (0.5*sqrt(N))
# and |z| > 2 is a verdict regardless of how far the medians wandered.
# The order is swapped every round so a warm-second-run effect cancels.
#
#   bash abba.sh VP9_TX_MEMSET akiyo_cif 10
#     -> arm A = knob unset (the candidate), arm B = knob=1 (the oracle)
set -eu
cd "$(dirname "$0")/analyzer"

KNOB="${1:-VP9_TX_MEMSET}"
CLIP="${2:-akiyo_cif}"
ROUNDS="${3:-10}"
export CLIPS="$CLIP"
export FRAMES="${FRAMES:-20}"
export PARETO_CRF="${PARETO_CRF:-32}"
export PARETO_SPEED="${PARETO_SPEED:-0}"
export PARETO_LAG="${PARETO_LAG:-0}"
export PARETO_NOREF=1

run() { # $1 = "A" | "B"
  if [ "$1" = "A" ]; then env -u "$KNOB" ./target/release/analyzer pareto 2>&1
  else env "$KNOB=1" ./target/release/analyzer pareto 2>&1
  fi | awk '/^  ours/{print $3}'
}

wins=0; n=0
as=""; bs=""
for i in $(seq 1 "$ROUNDS"); do
  if [ $((i % 2)) -eq 1 ]; then a=$(run A); b=$(run B); else b=$(run B); a=$(run A); fi
  [ -z "$a" ] || [ -z "$b" ] && { echo "!! empty sample in round $i"; continue; }
  as="$as $a"; bs="$bs $b"; n=$((n+1))
  awk -v a="$a" -v b="$b" 'BEGIN{exit !(a<b)}' && wins=$((wins+1))
  printf "  round %2d  A %8.1f ms   B %8.1f ms   %s\n" "$i" "$a" "$b" \
    "$(awk -v a="$a" -v b="$b" 'BEGIN{printf "%+.2f%%", (a/b-1)*100}')"
done

echo
echo "$CLIP  ($KNOB unset = A = candidate,  $KNOB=1 = B = oracle)"
echo " A wins $wins / $n rounds"
awk -v w="$wins" -v n="$n" 'BEGIN{
  if (n==0) exit
  z=(w-n/2)/(0.5*sqrt(n))
  printf "  z = %+.2f  ->  %s\n", z, (z>2?"A FASTER (real)":(z<-2?"B FASTER (real)":"INSIDE THE NOISE - not a result"))
}'
echo -n " A best/median:"; echo "$as" | tr ' ' '\n' | grep -v '^$' | sort -n | awk '{v[NR]=$1} END{printf " best %.1f  median %.1f ms\n", v[1], v[int((NR+1)/2)]}'
echo -n " B best/median:"; echo "$bs" | tr ' ' '\n' | grep -v '^$' | sort -n | awk '{v[NR]=$1} END{printf " best %.1f  median %.1f ms\n", v[1], v[int((NR+1)/2)]}'
