"""Guard-branch census: every bounds check in the emitted assembly, by SOURCE LINE.

A conditional branch that exists only to reach a panic block is pure overhead --
the compiler could not prove an index in range. Counting them is a strictly better
instrument than a whole-function instruction count for this class of change,
because it ATTRIBUTES the win: proving one index removes one guard, and the number
does not move when inlining shifts underneath you.

Finding WHERE each guard is takes a trick. Debug line tables attribute an inlined
bounds check to `core/src/slice/index.rs`, so every guard in the codec reads as the
same useless line. But every panic call passes a `&core::panic::Location`, and
rustc emits that as an `anon.*` rodata object holding {&str file, u32 line, u32
col}. So: in the block that calls the panic, read the `lea anon.N(%rip)`, find
`anon.N:` in the same file, and take its `.long`/`.quad` payload as the line.

    cargo rustc -p <crate> --release --lib -- --emit=asm
    python tools/bench/panic_census.py <file.s> [symbol-filter]

Self-test on every run: a probe whose good news is a zero is indistinguishable
from a broken probe.
"""
import re
import sys
from collections import Counter, defaultdict

SYM = re.compile(r"^([A-Za-z_][A-Za-z0-9_$.]*):\s*$")
PANIC = re.compile(r"call[q]?\s+\S*(panic_bounds_check|slice_index_fail|slice_start_index|"
                   r"slice_end_index|panic_misaligned|unwrap_failed|panic_const_)")
LEA = re.compile(r"le[aq]{1,2}\s+(anon\.[0-9a-f.]+)\(%rip\)")
ANON = re.compile(r"^(anon\.[0-9a-f.]+):\s*$")
QUAD = re.compile(r"\.quad\s+(anon\.[0-9a-f.]+)")
LONG = re.compile(r"\.(long|quad)\s+(\d+)")
ASCIZ = re.compile(r'\.asciz\s+"([^"]*)"')


def self_test():
    assert PANIC.search("\tcallq\t_ZN4core9panicking18panic_bounds_check17hEE"), "PANIC broken"
    assert SYM.match("_ZN9rusty_mp36encode8quantize5loops17hEEE:"), "SYM broken"
    assert LEA.search("\tleaq\tanon.9f3a(%rip), %rdx"), "LEA broken"
    assert ANON.match("anon.9f3a:"), "ANON broken"


def demangle(n):
    return re.sub(r"17h[0-9a-f]{16}E$", "", n).replace("_ZN", "")


def unescape(sq):
    """Decode an `.asciz` payload: octal escapes plus the usual C ones."""
    out = bytearray()
    i = 0
    while i < len(sq):
        c = sq[i]
        if c == "\\" and i + 1 < len(sq):
            nxt = sq[i + 1]
            if nxt.isdigit():
                j = i + 1
                while j < len(sq) and j < i + 4 and sq[j].isdigit():
                    j += 1
                out.append(int(sq[i + 1:j], 8) & 0xFF)
                i = j
                continue
            out.append({"n": 10, "t": 9, "r": 13, "\\": 92, '"': 34}.get(nxt, ord(nxt)))
            i += 2
            continue
        out.append(ord(c) & 0xFF)
        i += 1
    return bytes(out)


def load_anons(lines):
    """anon symbol -> (file, line), decoded from `core::panic::Location` objects.

    The object is `{&str file, u32 line, u32 col}`: the `.quad` is the string
    POINTER (to another anon holding the bytes) and the `.asciz` carries the
    string length in bytes 0..8, then the line in 8..12.
    """
    raw = {}
    cur = None
    for ln in lines:
        m = ANON.match(ln)
        if m:
            cur = m.group(1)
            raw[cur] = {"quad": None, "asciz": []}
            continue
        if cur is None:
            continue
        q = QUAD.search(ln)
        if q and raw[cur]["quad"] is None:
            raw[cur]["quad"] = q.group(1)
        a = ASCIZ.search(ln)
        if a:
            raw[cur]["asciz"].append(a.group(1))
        if ln.startswith("	.section") or SYM.match(ln):
            cur = None

    out = {}
    for name, d in raw.items():
        if d["quad"] is None or not d["asciz"]:
            continue
        payload = unescape(d["asciz"][0])
        if len(payload) < 12:
            continue
        line = int.from_bytes(payload[8:12], "little")
        if not (0 < line < 100000):
            continue
        fname = None
        tgt = raw.get(d["quad"])
        if tgt and tgt["asciz"]:
            fname = tgt["asciz"][0]
        out[name] = (fname, line)
    return out


def main():
    self_test()
    path = sys.argv[1]
    filt = sys.argv[2] if len(sys.argv) > 2 else ""
    lines = open(path, encoding="utf-8", errors="replace").read().split("\n")
    anons = load_anons(lines)

    cur = "<none>"
    per_sym = Counter()
    sites = defaultdict(Counter)
    recent_lea = None
    for ln in lines:
        m = SYM.match(ln)
        if m:
            cur = m.group(1)
            recent_lea = None
            continue
        la = LEA.search(ln)
        if la:
            recent_lea = la.group(1)
        if PANIC.search(ln):
            per_sym[cur] += 1
            loc = anons.get(recent_lea) if recent_lea else None
            sites[cur][loc if loc else ("?", 0)] += 1
            recent_lea = None

    total = sum(per_sym.values())
    print(f"{'guards':>7}  symbol")
    for sym, n in per_sym.most_common():
        d = demangle(sym)
        if filt and filt not in d:
            continue
        if d.startswith(("core", "alloc", "std")) and not filt:
            continue
        print(f"{n:>7}  {d[-62:]}")
        for (f, l), c in sites[sym].most_common(6):
            where = f"{f.split('/')[-1]}:{l}" if f else f"line {l}" if l else "unresolved"
            print(f"{'':>9}  {c:>3} x {where}")
    print(f"\n{total} guard sites total ({len(per_sym)} symbols)")


if __name__ == "__main__":
    main()
