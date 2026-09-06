#!/usr/bin/env python3
"""The load-folding lens, applied to every kernel.

A symmetry/reassociation win depends on whether the compiler already folded the
kernel's loads into memory operands. Where it did, the loads are not
instructions and cannot be saved -- and a fold would ADD one, since two memory
operands cannot share an instruction. Where it did not (values reused across
several ops), the loads are real and a fold removes them.

So: per kernel, count memory-operand vector ops vs explicit vector loads inside
the hot loop, and rank the fold candidates.
"""
import re
import subprocess
import sys
import glob
import os

# ALU op whose SOURCE is memory: the load is folded, costs no instruction.
MEMOP = re.compile(r"^\s+v(?!mov)[a-z0-9]+\s+[^,]*\(%[^)]*\),")
# standalone load: memory source, vector-register destination.
VLOAD = re.compile(r"^\s+vmov[a-z]+\s+[^,]*\(%[^)]*\),\s*%[xy]mm")
VOP = re.compile(r"^\s+v[a-z0-9]+")
JUMP = re.compile(r"^\s+j\w+\s+(\.?L[\w$.@]*)")
LOCAL = re.compile(r"^(\.?L[\w$.@]*):\s*$")
SYM = re.compile(r"^(_ZN[\w$.@]*?)([\w]+)17h[0-9a-f]{16}E:\s*$")


def kernels(path):
    """Yield (name, body-lines) for every mangled kernel symbol in the .s."""
    lines = open(path, encoding="utf-8", errors="replace").read().splitlines()
    starts = []
    for i, ln in enumerate(lines):
        m = SYM.match(ln)
        if m:
            starts.append((i, m.group(2)))
    for idx, (i, name) in enumerate(starts):
        end = starts[idx + 1][0] if idx + 1 < len(starts) else len(lines)
        yield name, lines[i:end]


def hot_loop(body):
    """The largest backward-jump span -- the kernel's main loop."""
    labels = {}
    for i, ln in enumerate(body):
        m = LOCAL.match(ln)
        if m:
            labels[m.group(1)] = i
    best = None
    for i, ln in enumerate(body):
        m = JUMP.match(ln)
        if m and m.group(1) in labels:
            j = labels[m.group(1)]
            if j < i:
                span = body[j:i + 1]
                if any("retq" in x for x in span):
                    continue
                n = sum(1 for x in span if VOP.match(x))
                if best is None or n > best[0]:
                    best = (n, span)
    return best[1] if best else []


def main():
    files = sorted(glob.glob("target/release/deps/rusty_h265_accel-*.s"),
                   key=os.path.getmtime, reverse=True)
    if not files:
        print("no .s found -- run tools/hevc/kernel_icount.py first", file=sys.stderr)
        return 1
    rows = []
    seen = set()
    for name, body in kernels(files[0]):
        if not any(k in name for k in ("fir_", "put_", "add_residual", "band", "edge",
                                       "angular", "planar", "dc_fill", "copy_shift",
                                       "weighted", "transform_skip", "transpose", "avg_")):
            continue
        loop = hot_loop(body)
        if not loop:
            continue
        vops = sum(1 for x in loop if VOP.match(x))
        if vops < 6:
            continue
        memop = sum(1 for x in loop if MEMOP.match(x))
        vld = sum(1 for x in loop if VLOAD.match(x))
        key = (name, vops, memop, vld)
        if key in seen:
            continue
        seen.add(key)
        rows.append((name, vops, memop, vld))

    rows.sort(key=lambda r: -r[3])
    print(f"{'kernel':<26}{'vecops':>7}{'memop':>7}{'loads':>7}{'load%':>8}  verdict")
    print("-" * 78)
    for name, vops, memop, vld in rows:
        pct = 100.0 * vld / vops if vops else 0
        if vld == 0:
            v = "loads fully folded - fold would ADD"
        elif pct >= 25:
            v = "** reuse-heavy: fold candidate"
        elif pct >= 12:
            v = "some real loads"
        else:
            v = "mostly folded"
        print(f"{name:<26}{vops:>7}{memop:>7}{vld:>7}{pct:>7.0f}%  {v}")
    return 0


sys.exit(main())
