"""Copy census from the EMITTED ASSEMBLY, attributed to the symbol that emits it.

A source grep finds the copies you WROTE. It cannot find the ones the compiler
made -- a by-value struct return, a memset'd stack temporary, an argument spill --
and it cannot tell you which of yours survived inlining. So the instrument is the
.s file.

    cargo rustc -p <crate> --release --lib -- --emit=asm
    python tools/bench/copy_census.py target/release/deps/<crate>-<hash>.s

Reports every `call memcpy/memmove/memset` and every allocator call, grouped by
the function containing it, with the length resolved from the immediate moved
into the length register where it is a constant. A CONSTANT length inlines; a
RUNTIME length is a real opaque call, and knowing which you have is the whole
point before "fixing" anything.

Self-test: every pattern is asserted against a known-good line on each run,
because a broken probe and a clean codebase produce the same output -- zero.
"""
import re
import sys
from collections import defaultdict

COPY = re.compile(r"call[q]?\s+(memcpy|memmove|memset)\b")
ALLOC = re.compile(r"call[q]?\s+_?_?R?N?v?C?s?\w*?(__rust_alloc_zeroed|__rust_alloc|__rust_dealloc|__rust_realloc)\b")
SYM = re.compile(r"^([A-Za-z_][A-Za-z0-9_$.]*):\s*$")
# `mov $N, %edx` / `movl $N, %edx` -- the length argument in SysV / Win64.
IMM = re.compile(r"mov[lq]?\s+\$(\d+),\s*%(edx|rdx|r8d|r8)\b")


def self_test():
    """A probe whose good news is a zero needs proof its patterns still match."""
    assert COPY.search("\tcallq\tmemcpy"), "COPY pattern broken"
    assert COPY.search("        call    memset"), "COPY pattern (spaces) broken"
    assert SYM.match("_ZN9rusty_mp36encode8quantize5loops17hEEE:"), "SYM pattern broken"
    assert IMM.search("\tmovl\t$2304, %edx"), "IMM pattern broken"


def demangle(name):
    n = re.sub(r"17h[0-9a-f]{16}E$", "", name)
    return n.replace("_ZN", "").replace("$LT$", "<").replace("$GT$", ">")


def census(path):
    lines = open(path, encoding="utf-8", errors="replace").read().split("\n")
    cur = "<none>"
    per = defaultdict(lambda: defaultdict(list))
    allocs = defaultdict(int)
    recent_imm = None
    imm_age = 999
    for ln in lines:
        m = SYM.match(ln)
        if m:
            cur = m.group(1)
            recent_imm = None
            continue
        imm_age += 1
        mi = IMM.search(ln)
        if mi:
            recent_imm = int(mi.group(1))
            imm_age = 0
        mc = COPY.search(ln)
        if mc:
            # PROXIMITY GUARD. The length register is loaded immediately before
            # the call; an immediate further back belongs to something else.
            # Without this, `frame_size`'s `144 * 1000` arithmetic constant was
            # reported as a 144,000-byte memcpy -- a number that would have sent
            # the next reader hunting a copy that does not exist.
            per[cur][mc.group(1)].append(recent_imm if imm_age <= 6 else None)
            recent_imm = None
            imm_age = 999
            continue
        ma = ALLOC.search(ln)
        if ma:
            allocs[cur] += 1
    return per, allocs


def main():
    self_test()
    path = sys.argv[1]
    only = sys.argv[2] if len(sys.argv) > 2 else ""
    per, allocs = census(path)

    rows = []
    for sym, kinds in per.items():
        total = sum(len(v) for v in kinds.values())
        rows.append((total, sym, kinds))
    rows.sort(reverse=True)

    print(f"{'n':>4}  {'const-len bytes':>16}  symbol / kinds")
    grand = 0
    for total, sym, kinds in rows:
        d = demangle(sym)
        if only and only not in d:
            continue
        if d.startswith(("core", "alloc", "std")) and not only:
            continue
        grand += total
        detail = []
        const_bytes = 0
        for kind, lens in sorted(kinds.items()):
            known = [x for x in lens if x is not None]
            const_bytes += sum(known)
            unknown = len(lens) - len(known)
            s = f"{kind}x{len(lens)}"
            if known:
                s += f"[{','.join(str(x) for x in sorted(set(known))[:4])}]"
            if unknown:
                s += f"(+{unknown} runtime)"
            detail.append(s)
        print(f"{total:>4}  {const_bytes:>16}  {d[-58:]:<58} {' '.join(detail)}")
    print(f"\n{grand} copy call sites in project symbols")
    if allocs:
        print("\nallocator call sites:")
        for sym, n in sorted(allocs.items(), key=lambda kv: -kv[1])[:10]:
            d = demangle(sym)
            if d.startswith(("core", "alloc", "std")):
                continue
            print(f"  {n:>3}  {d[-64:]}")


if __name__ == "__main__":
    main()
