"""Static instruction count per symbol, from the emitted assembly.

The deterministic counter for a straight-line code change: one run, no pinning, no
noise floor, and it sizes the effect while proving it. `codec-measurement` §15.

    cargo rustc -p <crate> --release --lib -- --emit=asm
    python tools/bench/icount.py <file.s> [--save name] [--diff name] [filter]

`--save` writes a baseline; `--diff` compares against one and prints only the
symbols that moved. Read the LIMITS before acting on a number:

* It cannot price work moved BETWEEN BRANCHES or between loop levels -- hoisting a
  bounds check out of an inner loop leaves the count flat while removing nearly all
  of its executions. Hoists must be judged on where the guard sits, not the total.
* It scores every FAST PATH as a loss: new arm plus the old one retained.
* The register allocator is its noise floor -- an unrelated edit to the same
  function moves paths by a few instructions. Treat |delta| <= 3 as noise.
"""
import json
import os
import re
import sys
from collections import Counter

SYM = re.compile(r"^([A-Za-z_][A-Za-z0-9_$.]*):\s*$")
# An instruction line starts with whitespace and a mnemonic, and is not a directive.
INSN = re.compile(r"^\s+([a-z][a-z0-9.]{1,14})\b")
SKIP = {"nop"}


def count(path):
    per = Counter()
    cur = None
    for ln in open(path, encoding="utf-8", errors="replace"):
        m = SYM.match(ln)
        if m:
            cur = m.group(1)
            continue
        if cur is None:
            continue
        if ln.lstrip().startswith("."):
            continue
        mi = INSN.match(ln)
        if mi and mi.group(1) not in SKIP:
            per[cur] += 1
    return per


def demangle(n):
    return re.sub(r"17h[0-9a-f]{16}E$", "", n).replace("_ZN", "")


def main():
    args = [a for a in sys.argv[1:]]
    path = args.pop(0)
    save = diff = None
    filt = ""
    while args:
        a = args.pop(0)
        if a == "--save":
            save = args.pop(0)
        elif a == "--diff":
            diff = args.pop(0)
        else:
            filt = a
    per = count(path)
    assert per, "no instructions counted -- the .s layout changed"

    if save:
        json.dump({k: v for k, v in per.items()}, open(save, "w"))
        print(f"baseline saved: {len(per)} symbols -> {save}")

    if diff and os.path.exists(diff):
        base = json.load(open(diff))
        rows = []
        for k, v in per.items():
            b = base.get(k)
            if b is not None and b != v:
                rows.append((v - b, b, v, demangle(k)))
        # symbols that vanished or appeared (inlining moved) -- reported separately
        gone = sum(base[k] for k in base if k not in per)
        new = sum(v for k, v in per.items() if k not in base)
        rows.sort()
        print(f"{'delta':>7}{'before':>9}{'after':>8}  symbol")
        for d, b, v, k in rows:
            if filt and filt not in k:
                continue
            print(f"{d:>+7}{b:>9}{v:>8}  {k[-58:]}")
        tot_b = sum(base.values())
        tot_a = sum(per.values())
        print(f"\ntotal {tot_b} -> {tot_a} ({tot_a - tot_b:+})"
              f"   [symbols gone {gone}, new {new} -- inlining moved, not a win]")
    elif not save:
        for k, v in per.most_common(30):
            d = demangle(k)
            if filt and filt not in d:
                continue
            if d.startswith(("core", "alloc", "std")) and not filt:
                continue
            print(f"{v:>7}  {d[-62:]}")


if __name__ == "__main__":
    main()
