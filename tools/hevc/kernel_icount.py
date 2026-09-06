#!/usr/bin/env python3
"""Deterministic instruction counts for the rusty_h265-accel kernels.

Why this exists
---------------
`codec-measurement` §15: for an effect below ~1% of the pipeline, a
deterministic counter of the work REMOVED is the primary evidence and the clock
is only confirmatory. A kernel micro-optimisation is exactly that size. On a box
whose null arm reads +-3%, no number of paired rounds can resolve "this loop got
two instructions shorter" -- but the assembler can, exactly, every time.

So the instrument is the emitted assembly itself:

  * find every kernel symbol,
  * find its innermost loop (a local label with a backward jump to it),
  * count the real instructions in that loop body,
  * divide by the outputs that body produces.

The result is `instructions per output sample`, reproducible bit-for-bit across
runs and machines with the same toolchain. A win is that number going down while
the `*_matches_scalar` tests and the 147-stream conformance gate stay green.

What this does NOT tell you
---------------------------
Instructions are not cycles. Removing two instructions that were never on the
critical path buys nothing, and a shuffle can cost more than the two moves it
replaced. This counter is a *necessary* condition and an exact one; the clock
still gets the last word on anything large enough for it to see. Where the two
disagree, believe the clock for time and this for work.

Usage
-----
    python tools/hevc/kernel_icount.py                 # build + report
    python tools/hevc/kernel_icount.py --json out.json # machine-readable
    python tools/hevc/kernel_icount.py --baseline b.json  # diff vs a baseline
"""

import argparse
import json
import os
import re
import subprocess
import sys

REPO = os.path.dirname(os.path.dirname(os.path.dirname(os.path.abspath(__file__))))

# The kernels to report on. This used to carry a hand-maintained
# outputs-per-iteration count as well; that is now derived from each loop's
# stores (see `outputs_of`), because the constant went stale the moment a loop
# changed shape and turned a real win into an apparent regression.
#
# Note which names are DISPATCHERS: the `_sse2` bodies are `#[inline]`-able into
# the safe entry point, so LLVM may emit no standalone symbol for them and their
# loop lives inside `fir_h`, `put_uni`, `angular`, ... Those entries are the
# SSE2 path, not a wrapper. Whether that inlining happens flips with unrelated
# edits, which is why the report is keyed on the KERNEL and not the symbol.
OUTPUTS_PER_ITER = {
    # SSE2. These are reported under the `_sse2` name whether LLVM inlined the
    # body into the safe dispatcher (then the loop lives in `fir_h`) or emitted
    # it standalone (`fir_h_sse2`) -- an inlining decision that flips with
    # unrelated edits, and a metric keyed on the SYMBOL rather than the KERNEL
    # reads that flip as an 8-instruction "win".
    "copy_shift_sse2": 8,
    "transform_skip_sse2": 4,
    "fir_h_sse2": 8,
    "fir_v_sse2": 8,
    "put_uni_sse2": 8,
    "put_bi_sse2": 8,
    "add_residual_sse2": 8,
    "band_sse2": 8,
    "edge_sse2": 8,
    "angular_sse2": 8,
    "angular_t_sse2": 64,
    "planar_sse2": 4,
    "dc_fill_sse2": 8,
    "transpose_sse2": 64,
    # AVX2, always a real symbol (a different target_feature set cannot inline
    # into a caller that lacks it)
    "copy_shift_avx2": 16,
    "transform_skip_avx2": 8,
    "weighted_uni_avx2": 16,
    "weighted_bi_avx2": 16,
    "fir_h_avx2": 16,
    "fir_v_avx2": 16,
    "fir_v_avx2_sym": 16,
    "luma_edge_sse2": 4,
    "put_uni_avx2": 16,
    "put_bi_avx2": 16,
    "add_residual_avx2": 16,
    "band_avx2": 16,
    "edge_avx2": 16,
    "angular_avx2": 16,
    "angular_t_avx2": 128,
    "planar_avx2": 8,
    "dc_fill_avx2": 16,
    # aarch64
    "fir_h_neon": 8,
    "fir_v_neon": 8,
    "put_uni_neon": 8,
    "put_bi_neon": 8,
    "add_residual_neon": 8,
    "band_neon": 8,
    "edge_neon": 8,
    "angular_neon": 8,
}

# Directives and label lines are not instructions.
DIRECTIVE = re.compile(r"^\s*\.")
LABEL = re.compile(r"^([\w$.@?]+):\s*$")
LOCAL_LABEL = re.compile(r"^(\.?L[\w$.@]*):\s*$")
JUMP = re.compile(r"^\s*(j\w+)\s+(\.?L[\w$.@]*)")
# Instructions that cost nothing at issue on any modern core; counted separately
# so a "win" that only deletes NOPs is visible as such.
FREE = {"nop", "nopw", "nopl", "ud2", "int3"}
# Any instruction touching a vector register.
SIMD_REG = re.compile(r"[xy]mm[0-9]")
# A vector store: last operand is memory, source is a vector register.
STORE = re.compile(r"^\s*v?(movdqu|movdqa|movups|movaps|movq|movlps|movsd)\s+%([xy])mm[0-9]+,\s*[^%]*\(")


def outputs_of(seq):
    """Samples one trip of this loop writes, derived from its STORES.

    The denominator used to be a hand-maintained constant per kernel, and it
    lied the moment a loop changed shape: the two-row vertical FIR emits 32
    samples per trip, the table still said 16, and a real 18% win displayed as
    a 64% regression. Counting the stores cannot drift out of date -- a 256-bit
    store is 16 `u16`, a 128-bit store is 8, a 64-bit store is 4.
    """
    n = 0
    for line in seq:
        m = STORE.match(line)
        if not m:
            continue
        op, width = m.group(1), m.group(2)
        if op in ("movq", "movlps", "movsd"):
            n += 4
        else:
            n += 16 if width == "y" else 8
    return n


def demangle(sym):
    """Map a mangled symbol to a kernel name: the LONGEST key it contains, so
    `fir_h_avx2` never gets filed under `fir_h_sse2`. A bare dispatcher name
    (`fir_h`, `angular`) maps to the `_sse2` kernel, which is the body LLVM
    inlines into it."""
    best = None
    for name in OUTPUTS_PER_ITER:
        stem = name[:-5] if name.endswith("_sse2") else name
        for cand in ({name, stem} if stem != name else {name}):
            if re.search(r"\d" + re.escape(cand) + r"17h", sym) and (best is None or len(cand) > len(best[1])):
                best = (name, cand)
    return best[0] if best else None


def parse(path):
    """Symbol -> {total, loop, loop_label, free} instruction counts."""
    with open(path, encoding="utf-8", errors="replace") as fh:
        lines = fh.read().split("\n")

    # Split into symbol bodies.
    bodies, cur, name = {}, [], None
    for line in lines:
        m = LABEL.match(line)
        if m and not LOCAL_LABEL.match(line):
            if name:
                bodies[name] = cur
            name, cur = m.group(1), []
            continue
        if name is not None:
            cur.append(line)
    if name:
        bodies[name] = cur

    out = {}
    for sym, body in bodies.items():
        short = demangle(sym)
        if short is None:
            continue
        # Index every local label, then look for a backward jump to it.
        label_at = {}
        for i, line in enumerate(body):
            m = LOCAL_LABEL.match(line)
            if m:
                label_at[m.group(1)] = i
        def count(seq):
            """(real instructions, free instructions, SIMD instructions)."""
            n = f = v = 0
            for line in seq:
                s = line.strip()
                if not s or DIRECTIVE.match(line) or LOCAL_LABEL.match(line) or s.startswith("#"):
                    continue
                op = s.split()[0]
                if op in FREE:
                    f += 1
                    continue
                n += 1
                if SIMD_REG.search(line):
                    v += 1
            return n, f, v

        # Every back-edge in the symbol: (label line, jump line).
        spans = []
        for i, line in enumerate(body):
            j = JUMP.match(line)
            if not j:
                continue
            start = label_at.get(j.group(2))
            if start is None or start >= i:
                continue  # forward jump: not a loop back-edge
            # A backward jump is not always a loop: LLVM lays the epilogue out
            # early, so `je .LBB11_11` to a block above it looked like a
            # 51-instruction "loop" that was really the function's exit. No
            # loop body contains a return.
            if any(l.strip().startswith(("retq", "ret ", "ret\t")) for l in body[start : i + 1]):
                continue
            spans.append((start, i))

        # Rank spans by SIMD DENSITY, then by size.
        #
        # Two rules were tried and both broke. "Smallest span" picks the scalar
        # tail, and flatters every kernel to about 5 instructions. "Innermost by
        # line containment, then most SIMD" works until LLVM's block layout puts
        # a scalar loop inside the vector loop's line range -- then the real
        # kernel is filtered out as an outer loop and `add_residual` reports a
        # 5-instruction bounds check with ZERO SIMD instructions, which is not a
        # thing a vector kernel can be.
        #
        # Density is immune to both: a tight vector body scores ~0.8, an
        # enclosing loop scores slightly less because it adds bookkeeping, and a
        # scalar loop scores 0.
        # Report EVERY vector loop in the symbol, not just the "best" one.
        #
        # Picking one is wrong whenever a kernel is specialised: `fir_h_avx2`
        # holds a shifted and an unshifted loop, and any single-winner rule has
        # to rank them. Density ranks them BACKWARDS -- deleting two SIMD
        # instructions from a 23-instruction loop lowers simd/total, so the
        # slower loop wins and the optimisation reads as no change at all.
        #
        # Qualifying = enough vector work to be a kernel and not mostly
        # bookkeeping; then keep only the innermost such spans.
        qual = []
        for s in spans:
            n, _, simd = count(body[s[0] : s[1] + 1])
            if n and simd >= 3 and simd / n >= 0.5:
                qual.append(s)
        qual = [s for s in qual if not any(o != s and o[0] >= s[0] and o[1] <= s[1] for o in qual)]
        total, total_free, total_simd = count(body)
        for s in qual:
            loop, loop_free, loop_simd = count(body[s[0] : s[1] + 1])
            label = [k for k, v in label_at.items() if v == s[0]][0]
            outs = outputs_of(body[s[0] : s[1] + 1])
            out.setdefault(short, []).append({
                "symbol": sym,
                "body": list(body[s[0] : s[1] + 1]),
                "total": total,
                "loop": loop,
                "loop_free": loop_free,
                "loop_simd": loop_simd,
                "total_simd": total_simd,
                "label": label,
                "per_output": round(loop / outs, 3) if outs else 0.0,
                "outputs": outs,
            })

    # A generic kernel is monomorphised once per tap count (`N = 8` luma,
    # `N = 4` chroma). Both are real code on the hot path, so both are reported:
    # the largest keeps the plain name, the rest get `~2`, `~3`. Folding them
    # into one number would hide a win in the chroma path entirely.
    flat = {}
    for short, recs in out.items():
        # A symbol with no SIMD loop is a wrapper that called the kernel rather
        # than inlining it; it carries no information about the kernel's cost.
        withv = [r for r in recs if r["loop_simd"] > 0]
        recs = withv or recs
        recs.sort(key=lambda r: -r["loop"])
        for i, rec in enumerate(recs):
            flat[short if i == 0 else f"{short}~{i + 1}"] = rec
    return flat


def build():
    env = dict(os.environ)
    env["RUSTFLAGS"] = env.get("RUSTFLAGS", "")
    cmd = [
        "cargo", "rustc", "--release", "-p", "rusty_h265-accel", "--lib",
        "--", "--emit", "asm",
    ]
    r = subprocess.run(cmd, cwd=REPO, capture_output=True, text=True, env=env)
    if r.returncode != 0:
        sys.stderr.write(r.stderr)
        raise SystemExit("build failed")
    deps = os.path.join(REPO, "target", "release", "deps")
    cands = [
        os.path.join(deps, f)
        for f in os.listdir(deps)
        if f.startswith("rusty_h265_accel-") and f.endswith(".s")
    ]
    if not cands:
        raise SystemExit("no .s emitted")
    return max(cands, key=os.path.getmtime)


def main():
    ap = argparse.ArgumentParser()
    ap.add_argument("--json")
    ap.add_argument("--baseline")
    ap.add_argument("--asm", help="use an existing .s instead of building")
    ap.add_argument("--show", help="print the innermost loop body of one kernel")
    args = ap.parse_args()

    path = args.asm or build()
    data = parse(path)

    if args.show:
        d = data.get(args.show)
        if d is None:
            raise SystemExit(f"no kernel {args.show!r}; have: {', '.join(sorted(data))}")
        print(f"{args.show}: {d['loop']} instructions ({d['loop_simd']} SIMD) "
              f"for {d['outputs']} outputs = {d['per_output']:.3f}/output  [{d['label']}]")
        for line in d["body"]:
            if line.strip():
                print("   " + line.rstrip())
        return

    base = {}
    if args.baseline and os.path.exists(args.baseline):
        with open(args.baseline, encoding="utf-8") as fh:
            base = json.load(fh)

    print(f"asm: {os.path.relpath(path, REPO)}")
    print("metric: instructions in the innermost loop body, and per output sample")
    print("deterministic: same toolchain + same source => same numbers, every run\n")
    hdr = f"{'kernel':22} {'loop':>6} {'simd':>5} {'out':>5} {'/out':>7} {'total':>7}"
    if base:
        hdr += f" {'was':>6} {'delta':>7}"
    print(hdr)
    print("-" * len(hdr))
    tot_now = tot_was = 0
    for name in sorted(data):
        d = data[name]
        row = f"{name:22} {d['loop']:6} {d['loop_simd']:5} {d['outputs']:5} {d['per_output']:7.3f} {d['total']:7}"
        if base:
            b = base.get(name)
            if b:
                delta = d["loop"] - b["loop"]
                tot_now += d["loop"]
                tot_was += b["loop"]
                mark = "" if delta == 0 else (f"  {delta:+d}" if delta > 0 else f"  {delta:+d} <-")
                row += f" {b['loop']:6} {mark:>7}"
            else:
                row += f" {'new':>6} {'':>7}"
        print(row)
    if base and tot_was:
        print(f"\ntotal inner-loop instructions: {tot_was} -> {tot_now} ({tot_now - tot_was:+d})")

    if args.json:
        with open(args.json, "w", encoding="utf-8") as fh:
            json.dump({k: {kk: vv for kk, vv in v.items() if kk != "body"} for k, v in data.items()}, fh, indent=1, sort_keys=True)
        print(f"\nwrote {args.json}")


if __name__ == "__main__":
    main()
