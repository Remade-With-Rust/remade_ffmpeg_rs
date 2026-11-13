"""Vectorization census per symbol, from the emitted assembly.

    cargo rustc -p <crate> --release --lib -- --emit=asm
    python tools/bench/vec_census.py <file.s> [filter ...]

For each symbol whose demangled name contains any filter (all symbols if none),
counts what the compiler ACTUALLY emitted:

  packed_f   packed float arithmetic   (addps/mulps/... and their v- forms, pd too)
  scalar_f   scalar float arithmetic   (addss/mulss/sqrtsd/cvt*ss/...)
  packed_i   packed integer arithmetic (padd*/pmul*/psub*/pmadd*/...)
  ymm        instructions touching a 256-bit register
  calls      `call` instructions (a libm call per element vetoes vectorization)

"Did it auto-vectorize?" is an empirical claim (codec-vectorize-kernel): a loop
that looks textbook-vectorizable can emit 0 packed ops. This answers it for every
symbol at once, deterministically -- same toolchain + source, same numbers.
"""
import re
import sys
from collections import defaultdict

SYM = re.compile(r"^([A-Za-z_][A-Za-z0-9_$.]*):\s*$")
INSN = re.compile(r"^\s+([a-z][a-z0-9.]{1,15})\b(.*)$")
FOPS = r"(add|sub|mul|div|min|max|sqrt|and|andn|or|xor|cmp|round|hadd|fmadd\d*|fmsub\d*|fnmadd\d*)"
PACKED_F = re.compile(rf"^v?{FOPS}p[sd]$")
SCALAR_F = re.compile(rf"^v?({FOPS}s[sd]|cvt\w*s[sd]\w*|cvtt\w*s[sd]\w*)$")
PACKED_I = re.compile(r"^v?p(add|sub|mul|madd|max|min|abs|sra|srl|sll|shuf|and|or|xor|cmp|unpck|ack)\w*$")


def demangle(n):
    m = re.search(r"_ZN(.*)E$", n) or re.search(r"_ZN(.*?)17h[0-9a-f]{16}E", n)
    if not m:
        return n
    parts, s = [], m.group(1)
    while s and s[0].isdigit():
        k = re.match(r"(\d+)", s)
        ln = int(k.group(1))
        s = s[len(k.group(1)):]
        parts.append(s[:ln])
        s = s[ln:]
    parts = [p for p in parts if not re.fullmatch(r"h[0-9a-f]{16}", p)]
    return "::".join(parts)


def main():
    path, filters = sys.argv[1], [f.lower() for f in sys.argv[2:]]
    stats = defaultdict(lambda: defaultdict(int))
    cur = None
    for ln in open(path, encoding="utf-8", errors="replace"):
        m = SYM.match(ln)
        if m:
            cur = demangle(m.group(1))
            continue
        if cur is None:
            continue
        mi = INSN.match(ln)
        if not mi or ln.lstrip().startswith("."):
            continue
        op, rest = mi.group(1), mi.group(2)
        st = stats[cur]
        st["insns"] += 1
        if PACKED_F.match(op):
            st["packed_f"] += 1
        elif SCALAR_F.match(op):
            st["scalar_f"] += 1
        elif PACKED_I.match(op):
            st["packed_i"] += 1
        if "%ymm" in rest:
            st["ymm"] += 1
        if op.startswith("call"):
            st["calls"] += 1
    rows = [(n, s) for n, s in stats.items()
            if not filters or any(f in n.lower() for f in filters)]
    rows.sort(key=lambda r: -r[1]["insns"])
    print(f"{'symbol':<64}{'insns':>7}{'packed_f':>9}{'scalar_f':>9}{'packed_i':>9}{'ymm':>6}{'calls':>6}")
    for n, s in rows:
        print(f"{n[-64:]:<64}{s['insns']:>7}{s['packed_f']:>9}{s['scalar_f']:>9}"
              f"{s['packed_i']:>9}{s['ymm']:>6}{s['calls']:>6}")


if __name__ == "__main__":
    main()
