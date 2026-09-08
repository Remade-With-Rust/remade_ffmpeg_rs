#!/usr/bin/env python3
"""Every panic guard in the shipping build, attributed to its exact source line.

usage: panic_census.py <stem> [symbol-substring ...]

A GUARD is a conditional branch whose target block is a panic block, decided by
walking that block to its FIRST control transfer -- see guardlib.py for why a
line-window rule cannot work. Source lines come from each panic call's
`&core::panic::Location`, which names OUR line rather than core/slice/index.rs.
"""
import io, os, re, glob, sys, collections
sys.path.insert(0, os.path.dirname(os.path.abspath(__file__)))
import guardlib as G

REPO = r"F:\codingemade_ffmpeg_rs"
stem = sys.argv[1]
want = sys.argv[2:]
path = max(glob.glob(os.path.join(REPO, "target", "release", "deps", stem + "-*.s")),
           key=os.path.getsize)
lines = io.open(path, encoding="utf-8", errors="replace").read().splitlines()
print(f"# {os.path.basename(path)}  {os.path.getsize(path)//1024} KiB")
loc = G.locations(lines)
syms = G.symbols(lines)
tot = collections.Counter()
for j, (i, n) in enumerate(syms):
    short = re.sub(r"17h\w+E?$", "", n)
    if want and not any(w in short for w in want):
        continue
    end = syms[j + 1][0] if j + 1 < len(syms) else len(lines)
    b = lines[i + 1:end]
    if len(b) < 150:
        continue
    ni, ng, hits = G.analyse(b, loc)
    if ng:
        print(f"
== ...{short[-52:]}   ({ng} guards, {ni} instrs)")
        for l, c in sorted(hits.items(), key=lambda kv: -kv[1])[:16]:
            print(f"   {c:3d}x  {l}")
        tot.update(hits)
if len(want) != 1:
    print(f"
== ALL ({sum(tot.values())} guards)")
    for l, c in tot.most_common(25):
        print(f"   {c:4d}x  {l}")
