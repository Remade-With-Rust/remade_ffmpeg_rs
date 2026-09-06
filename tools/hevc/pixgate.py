#!/usr/bin/env python3
"""Phase-2 pixel gate: run rusty_h265 --verify-sei over the corpus and
tabulate per-stream SEI picture-hash results (first picture and all).

  python tools/hevc/pixgate.py [hevc-vectors] [target/release/rusty_h265.exe] [NAME_PREFIX...]
"""
import glob, os, subprocess, sys, time

vec = sys.argv[1] if len(sys.argv) > 1 else "hevc-vectors"
exe = sys.argv[2] if len(sys.argv) > 2 else "target/release/rusty_h265.exe"
prefixes = sys.argv[3:]
rows = []
for bit in sorted(glob.glob(os.path.join(vec, "*.bit"))):
    name = os.path.basename(bit)[:-4]
    if prefixes and not any(name.startswith(p) for p in prefixes):
        continue
    out = os.path.join(vec, ".tmp", "pix.yuv")
    t0 = time.time()
    try:
        r = subprocess.run([exe, bit, out, "--verify-sei"], capture_output=True, text=True, timeout=600)
        line = r.stdout.strip().splitlines()[-1] if r.stdout.strip() else ""
        kv = dict(x.split("=", 1) for x in line.split() if "=" in x)
        err = r.stderr.strip().splitlines()[-1] if r.stderr.strip() else ""
        if r.returncode != 0:
            kv = {"first_sei": "CRASH", "sei_checked": "0", "sei_mismatch": "0", "errors": "?"}
            err = (r.stderr.strip().splitlines() or ["crash"])[-1][:120]
    except subprocess.TimeoutExpired:
        kv = {"first_sei": "TIMEOUT", "sei_checked": "0", "sei_mismatch": "0", "errors": "?"}
        err = "timeout"
    rows.append((name, kv.get("first_sei", "?"), int(kv.get("sei_checked", 0)), int(kv.get("sei_checked", 0)) - int(kv.get("sei_mismatch", 0)), kv.get("errors", "?"), round(time.time() - t0, 1), err[:100]))
    print(f"{name:40} first={rows[-1][1]:5} sei={rows[-1][3]}/{rows[-1][2]} errors={rows[-1][4]} {rows[-1][5]}s {rows[-1][6]}", flush=True)
first_ok = sum(1 for r in rows if r[1] == "ok")
first_judged = sum(1 for r in rows if r[1] in ("ok", "bad"))
all_ok = sum(1 for r in rows if r[2] > 0 and r[3] == r[2])
print(f"\nfirst picture SEI ok: {first_ok}/{first_judged} (of {len(rows)} streams); whole-stream SEI ok: {all_ok}/{len(rows)}")
