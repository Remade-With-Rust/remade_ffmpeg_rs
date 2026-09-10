#!/usr/bin/env bash
# Does rusty_mp3 work on wasm -- and produce the SAME BYTES as the host?
#
# Compiling for wasm proves almost nothing. This crate compiled cleanly for
# `wasm32-unknown-unknown` while trapping on its first decoded frame, because
# `Instant::now()` is unsupported there and the stage profiler wrapped every
# stage in it. So the gate is a hash comparison, on both wasm targets:
#
#   wasm32-wasip1          has a clock and threads   -> run under wasmtime
#   wasm32-unknown-unknown has NEITHER (the browser) -> run under node, no WASI
#
# The second one is the one that matters and the one every "it builds" check
# misses. It also runs a CONTROL export that calls the clock directly: that must
# TRAP, otherwise the target gained a clock and the cfg needs revisiting -- a
# probe whose good news is "no trap" has to prove it can still trap.
#
#   bash tools/bench/wasm_check.sh
set -uo pipefail
ROOT="$(cd "$(dirname "$0")/../.." && pwd)"
cd "$ROOT"
# Cargo is a native Windows binary under Git Bash, so a POSIX path in a manifest
# resolves to nonsense. `pwd -W` hands over the native form.
ROOT_WIN="$(cd "$ROOT" && pwd -W 2>/dev/null || printf '%s' "$ROOT")"
TMP="${WASM_CHECK_TMP:-/f/coding/tmp/mp3q/wasmprobe}"
mkdir -p "$TMP"
# node is a native binary too -- same POSIX-path trap as cargo, one layer along.
TMP_WIN="$(cd "$TMP" && pwd -W 2>/dev/null || printf '%s' "$TMP")"
fail=0

say() { printf '%s\n' "$*"; }
need() { command -v "$1" >/dev/null 2>&1 || { say "SKIP: $1 not installed"; return 1; }; }

say "== 1. host baseline =="
host="$(cargo run -q -p rusty_mp3 --release --example wasmcheck 2>/dev/null)" || { say "host run failed"; exit 1; }
host_dec=$(echo "$host" | grep -oE 'decode fnv1a: 0x[0-9a-f]+' | grep -oE '0x[0-9a-f]+')
host_enc=$(echo "$host" | grep -oE 'encode fnv1a: 0x[0-9a-f]+' | grep -oE '0x[0-9a-f]+')
say "   decode $host_dec   encode $host_enc"

say "== 2. wasm32-wasip1 under wasmtime =="
if need wasmtime && rustup target list --installed | grep -q wasm32-wasip1; then
  cargo build -q -p rusty_mp3 --release --target wasm32-wasip1 --example wasmcheck 2>/dev/null
  w="$(wasmtime target/wasm32-wasip1/release/examples/wasmcheck.wasm 2>&1)"
  w_dec=$(echo "$w" | grep -oE 'decode fnv1a: 0x[0-9a-f]+' | grep -oE '0x[0-9a-f]+')
  w_enc=$(echo "$w" | grep -oE 'encode fnv1a: 0x[0-9a-f]+' | grep -oE '0x[0-9a-f]+')
  [ "$w_dec" = "$host_dec" ] && say "   decode MATCHES host" || { say "   decode MISMATCH: $w_dec vs $host_dec"; fail=1; }
  [ "$w_enc" = "$host_enc" ] && say "   encode MATCHES host" || { say "   encode MISMATCH: $w_enc vs $host_enc"; fail=1; }
else
  say "   SKIP"
fi

say "== 3. wasm32-unknown-unknown under node (no WASI: the browser shape) =="
if need node && rustup target list --installed | grep -q wasm32-unknown-unknown; then
  mkdir -p "$TMP/src"
  cat > "$TMP/Cargo.toml" <<EOF
[package]
name = "wasmprobe"
version = "0.0.0"
edition = "2021"
[lib]
crate-type = ["cdylib"]
[dependencies]
rusty_mp3 = { path = "$ROOT_WIN/crates/rusty_mp3" }
[profile.release]
opt-level = 2
panic = "abort"
[workspace]
EOF
  sed -e "s#@FIXTURE@#$ROOT_WIN/crates/rusty_mp3/examples/wasmcheck_fixture.mp3#" \
      "$ROOT/tools/bench/wasm_probe.rs.in" > "$TMP/src/lib.rs"
  if ! (cd "$TMP" && cargo build --release --target wasm32-unknown-unknown >/dev/null 2>"$TMP/err.txt"); then
    # A build failure must not read as a codec finding: without this the empty
    # hashes print as mismatches and the control prints as "gained a clock".
    say "   BUILD FAILED -- not a codec result:"; sed -n '1,6p' "$TMP/err.txt" | sed 's/^/     /'
    exit 1
  fi
  cat > "$TMP/run.mjs" <<EOF
import { readFileSync } from 'node:fs';
const b = readFileSync('$TMP_WIN/target/wasm32-unknown-unknown/release/wasmprobe.wasm');
const { instance } = await WebAssembly.instantiate(b, {});
const hex = v => '0x' + (v & 0xffffffffffffffffn).toString(16).padStart(16, '0');
const out = {};
for (const n of ['probe_clock', 'decode_hash', 'encode_hash', 'pipelined_hash']) {
  try { out[n] = hex(instance.exports[n]()); } catch { out[n] = 'TRAP'; }
}
console.log(JSON.stringify(out));
EOF
  j="$(node "$TMP/run.mjs" 2>&1 | tail -1)"
  case "$j" in
    *decode_hash*) ;;
    *) say "   PROBE DID NOT RUN -- not a codec result: $j"; exit 1 ;;
  esac
  get() { echo "$j" | grep -oE "\"$1\":\"[^\"]+\"" | sed 's/.*:"//;s/"//'; }
  for k in decode_hash pipelined_hash; do
    v=$(get $k); [ "$v" = "$host_dec" ] && say "   $k MATCHES host" || { say "   $k MISMATCH: $v vs $host_dec"; fail=1; }
  done
  v=$(get encode_hash); [ "$v" = "$host_enc" ] && say "   encode_hash MATCHES host" || { say "   encode_hash MISMATCH: $v vs $host_enc"; fail=1; }
  v=$(get probe_clock)
  [ "$v" = "TRAP" ] && say "   probe_clock TRAPS (control: the target still has no clock)" \
                    || { say "   probe_clock did NOT trap ($v) -- target gained a clock, revisit the cfg"; fail=1; }
else
  say "   SKIP"
fi

say ""
[ $fail -eq 0 ] && say "WASM CHECK PASSED -- bit-exact with the host on every target" || say "WASM CHECK FAILED"
exit $fail
