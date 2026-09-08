#!/usr/bin/env python3
"""Guard-branch detection from an emitted .s -- the shared, principled rule.

A GUARD is a conditional branch whose target block is a panic block: walk the
target block to its FIRST control transfer, and it is a panic block if that
transfer is a call to a panic symbol (following unconditional `jmp`s, bounded).

The rule matters. Two earlier versions of this used "does a panic symbol appear
within N lines after the target label", and the answer for one function was 16
at N = nxt+3 and 59 at N = nxt+4 -- because panic blocks share tails and sit
next to each other, so a line window either stops short of the shared `callq` or
falls through into an unrelated neighbour. Sweeping the window found no plateau
(95/115/119/120/125/128/132/143 for budgets 4..64), which is the tell that the
window is measuring layout rather than control flow. The flow rule gives 123 and
has no free parameter.

Also resolves each guard's source line: every panic call passes a
`&core::panic::Location`, emitted as an `anon.*` rodata object holding
{&str file, u32 line, u32 col}. That names the line of OUR code that failed to
prove its index; a debug line table only ever names core/src/slice/index.rs.
"""
import io, os, re

TOP = re.compile(r"^([\w$?@][\w$?@.]*):")
LBL = re.compile(r"^(\.L[\w$.]+):")
CJMP = re.compile(r"^\s+j(?!mp\b)\w+\s+(\.L[\w$.]+)")
JMP = re.compile(r"^\s+jmp\s+(\.L[\w$.]+)")
FLOW = re.compile(r"^\s+(j\w+|call[ql]?|ret[ql]?|ud2|int3)\b")
PANIC = re.compile(r"panic|unwrap_failed|slice_(start|end)_index")
SKIP = re.compile(r"^\s*(\.|#|//|$)")
ANON_DEF = re.compile(r"^(anon\.[0-9a-f]+\.\d+):")
LEA = re.compile(r"lea[ql]?\s+(anon\.[0-9a-f]+\.\d+)\(%rip\)")
QUAD = re.compile(r"^\s+\.quad\s+(anon\.[0-9a-f]+\.\d+)")
ASCIZ = re.compile(r'^\s+\.asci[iz]\s+"(.*)"$')


def _unesc(t):
    out = bytearray(); i = 0
    while i < len(t):
        if t[i] == "\\" and i + 1 < len(t):
            n = t[i + 1]
            if n.isdigit():
                j = i + 1
                while j < len(t) and j < i + 4 and t[j].isdigit():
                    j += 1
                out.append(int(t[i + 1:j], 8)); i = j; continue
            out.append({"n": 10, "t": 9, "r": 13, "\\": 92, '"': 34}.get(n, ord(n)))
            i += 2; continue
        out.append(ord(t[i])); i += 1
    return bytes(out)


def locations(lines):
    """anon.* symbol -> 'file:line' for every panic Location in the file."""
    raw, ptr, cur = {}, {}, None
    for l in lines:
        m = ANON_DEF.match(l)
        if m:
            cur = m.group(1); raw[cur] = bytearray(); continue
        if cur is None:
            continue
        m = QUAD.match(l)
        if m:
            ptr[cur] = m.group(1); continue
        m = ASCIZ.match(l)
        if m:
            raw[cur] += _unesc(m.group(1))
            if l.strip().startswith(".asciz"):
                raw[cur] += b"\x00"
            continue
        if l.strip() and not l.startswith(("\t.", " .")) and not l.startswith("."):
            cur = None
    out = {}
    for sym in raw:
        f = ptr.get(sym); by = bytes(raw[sym])
        if f is None or len(by) < 16:
            continue
        name = bytes(raw.get(f, b"")).decode("utf-8", "replace").rstrip("\x00")
        out[sym] = "%s:%d" % (os.path.basename(name), int.from_bytes(by[8:12], "little"))
    return out


def symbols(lines):
    """(start, name) for every non-local symbol definition."""
    return [(i, m.group(1)) for i, l in enumerate(lines)
            if (m := TOP.match(l)) and not m.group(1).startswith(".L")]


def analyse(b, loc=None):
    """(instruction count, guard count, {source line: count}) for one body."""
    pos = {LBL.match(l).group(1): k for k, l in enumerate(b) if LBL.match(l)}

    def panic_block(t, depth=0):
        if depth > 4:
            return None
        k = t + 1
        while k < len(b):
            l = b[k]
            if not l.strip() or l.startswith((".", "\t.")) or LBL.match(l):
                k += 1; continue
            if not FLOW.match(l):
                k += 1; continue
            if PANIC.search(l):
                # the Location is the last anon.* loaded before the call
                for x in range(k, max(t, k - 12) - 1, -1):
                    m = LEA.search(b[x])
                    if m and loc and m.group(1) in loc:
                        return loc[m.group(1)]
                return "?"
            m = JMP.match(l)
            if m and m.group(1) in pos:
                return panic_block(pos[m.group(1)], depth + 1)
            return None
        return None

    cache, guards, hits = {}, 0, {}
    for l in b:
        m = CJMP.match(l)
        if not m or m.group(1) not in pos:
            continue
        t = m.group(1)
        if t not in cache:
            cache[t] = panic_block(pos[t])
        if cache[t] is not None:
            guards += 1
            hits[cache[t]] = hits.get(cache[t], 0) + 1
    return sum(1 for x in b if not SKIP.match(x)), guards, hits
