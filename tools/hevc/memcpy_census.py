#!/usr/bin/env python3
"""Every `memcpy`/`memset`/`memmove` CALL the shipping build actually emits.

Why this exists
---------------
`codec-memory-copies` says to grep the source for copies. That finds the ones
written as copies. It does not find:

  * a `[0u16; 64]` stack temporary that LLVM zeroes with a `memset` call,
  * a struct assignment or `Vec` push that moves more bytes than it looks like,
  * a `slice::fill` or `copy_from_slice` whose RUNTIME length makes it a real
    `call memset` instead of the inlined stores a constant length would give,
  * a copy the optimiser INTRODUCED (spilling an argument, materialising a
    temporary for an unaligned load, a by-value struct return),

and it equally cannot tell you which of the copies you DID write survived
inlining. Only the emitted assembly knows. So the instrument is the assembly:
find every symbol, count the `call *memcpy/memset/memmove` inside it, and
resolve each one's length when it is an immediate in the preceding `mov` to the
third argument register.

This found `PicState::fill4` on rusty_h265: a per-4x4 map writer whose own
comment celebrated "(a `memset` for the byte-sized maps)" as the win, filling
rows of TWO to SIXTEEN entries eight to ten times per coding unit. Nine of those
calls were inlined into `coding_quadtree`, the decoder's hottest recursive
function. Dispatching to a fixed-size array reference removed all nine.

Reading the output
------------------
A call with a CONSTANT length is one the compiler chose over inline stores; it
is usually fine and often optimal. A call with an UNKNOWN (runtime) length in a
per-block or per-sample function is the interesting kind: at short lengths the
call overhead is the whole cost, which is exactly what refuted the first SAO
narrowing on this decoder (2.44 M spans of 62 samples measured NO faster than
the whole-plane copy it replaced).

Instructions are not cycles and a call site is not a call COUNT -- a site inside
a cold arm costs nothing. Cross-reference against the census counters and the
stage profiler before acting on anything here.

usage:
    memcpy_census.py                     # build rusty_h265 + accel, report both
    memcpy_census.py --crate rusty_h265  # one crate
    memcpy_census.py --lines --by-line   # name each call's SOURCE LINE
    memcpy_census.py --min 2 --filter mc
"""
import argparse
import collections
import io
import os
import re
import subprocess
import sys

REPO = os.path.dirname(os.path.dirname(os.path.dirname(os.path.abspath(__file__))))

TOP = re.compile(r"^([\w$?@][\w$?@.]*):")
# The MSVC/LLVM x86-64 spelling; `__rust_alloc_zeroed` is here too because a
# zeroed allocation is a memset the allocator does on our behalf.
CALL = re.compile(r"^\s+call[ql]?\s+\*?_?_?(memcpy|memset|memmove|rust_alloc_zeroed|rust_realloc|rust_alloc)")
# A copy the compiler INLINED is still a copy, and none of the above
# find it. `rep movs` is the string-move idiom LLVM picks for a mid-sized
# length; it is unambiguous, so it is reported as its own kind.
REPMOV = re.compile(r"^\s+rep;?\s+movs[bwlq]")
# System V passes the length in %rdx; Windows x64 in %r8. Accept either, and
# accept the 32-bit halves LLVM uses for small constants.
LEN_IMM = re.compile(r"^\s+mov[lq]?\s+\$(\d+),\s*%(rdx|edx|r8|r8d)\b")
CLOBBER = re.compile(r"%(rdx|edx|r8|r8d)\b")
# MSVC targets emit CodeView line tables (`.cv_loc FUNCID FILEID LINE COL`),
# not DWARF `.loc` -- and only when debug info is asked for, which the release
# profile does not do. `--lines` re-emits with `-Cdebuginfo=1`, which adds
# metadata without changing codegen decisions, so the sites it names are the
# ones the shipping build has. Verify that by diffing the call COUNTS against a
# run without `--lines`: if they differ, the debug build is not the same code.
CV_FILE = re.compile(r'^\s+\.cv_file\s+(\d+)\s+"(.*?)"')
# A `.cv_loc` names the INNERMOST inline frame, which for a copy is always the
# std function that emitted it -- `slice::fill` resolves to specialize.rs,
# `copy_from_slice` to core's mod.rs. Useless on its own. So the scanner also
# carries the most recent location in a file that is OURS, which is the calling
# statement: std frames never switch back to our file until the inlined body is
# done. Anything under the toolchain or the registry is not ours.
FOREIGN = ("/library/", "/rustlib/", ".cargo", "/rustc/")
CV_LOC = re.compile(r"^\s+\.cv_loc\s+\d+\s+(\d+)\s+(\d+)")


def build(crate, lib=True, lines=False, bin_name=None):
    env = dict(os.environ)
    env["RUSTFLAGS"] = env.get("RUSTFLAGS", "")
    cmd = ["cargo", "rustc", "--release", "-p", crate]
    if bin_name:
        cmd += ["--bin", bin_name]
    elif lib:
        cmd.append("--lib")
    cmd += ["--", "--emit", "asm"]
    if lines:
        cmd += ["-Cdebuginfo=1"]
    # Deleting the .s does NOT make cargo re-emit it: the fingerprint is
    # unchanged, so the build is a no-op and the file simply stays missing.
    # Touch the crate root, the same rule as the stale-binary checklist.
    for rel in ("lib.rs", os.path.join("bin", (bin_name or "") + ".rs")):
        root = os.path.join(REPO, "crates", crate, "src", rel)
        if os.path.exists(root):
            os.utime(root, None)
    r = subprocess.run(cmd, cwd=REPO, capture_output=True, text=True, env=env)
    if r.returncode != 0:
        sys.stderr.write(r.stderr[-3000:])
        raise SystemExit("build failed: " + crate)
    deps = os.path.join(REPO, "target", "release", "deps")
    stem = (bin_name or crate).replace("-", "_") + "-"
    cands = [os.path.join(deps, f) for f in os.listdir(deps)
             if f.startswith(stem) and f.endswith(".s")]
    if not cands:
        raise SystemExit("no .s emitted for " + crate)
    return max(cands, key=os.path.getmtime)


def scan(path):
    """[(symbol, [(kind, length-or-None, 'file:line'-or-None), ...]), ...]."""
    lines = io.open(path, encoding="utf-8", errors="replace").read().splitlines()
    out, cur, hits = [], None, []
    files, loc, own = {}, None, None
    # Track the last immediate written to the length register; any other write
    # to it invalidates the guess, so a reported length is never a stale one.
    pending = None
    for l in lines:
        m = CV_FILE.match(l)
        if m:
            full = m.group(2).replace("\\", "/")
            files[m.group(1)] = (os.path.basename(full),
                                 not any(f.replace("\\", "/") in full for f in FOREIGN))
            continue
        m = CV_LOC.match(l)
        if m:
            name, ours = files.get(m.group(1), ("?", False))
            loc = name + ":" + m.group(2)
            if ours:
                own = loc
            continue
        m = TOP.match(l)
        if m:
            if cur and hits:
                out.append((cur, hits))
            cur, hits, pending, loc, own = m.group(1), [], None, None, None
            continue
        if cur is None:
            continue
        m = REPMOV.match(l)
        if m:
            hits.append(("rep movs", pending, own or loc))
            pending = None
            continue
        m = CALL.match(l)
        if m:
            # Prefer OUR statement; fall back to the std frame when the whole
            # symbol is std (a monomorphised `Vec::extend`, say).
            hits.append((m.group(1), pending, own or loc))
            pending = None
            continue
        m = LEN_IMM.match(l)
        if m:
            pending = int(m.group(1))
            continue
        if CLOBBER.search(l):
            pending = None
    if cur and hits:
        out.append((cur, hits))
    return out


def demangle(n):
    return re.sub(r"17h[0-9a-f]{16}E?$", "", n).replace("_ZN", "").lstrip("_")


def self_test():
    """A pattern that matches nothing looks exactly like a codec with no copies.

    This fired for real: a `\b` written into the source as a literal backspace
    left CALL unable to match anything, and the census reported ZERO call sites
    across four crates -- a clean bill of health for code that has 125 of them in
    one .s alone. `codec-measurement` section 10: a probe reading zero for work
    that must be happening is a broken probe, not free work.
    """
    assert CALL.match("	callq	memcpy"), "CALL cannot match a plain memcpy call"
    assert CALL.match("        call    __rust_alloc"), "CALL cannot match an allocation"
    assert REPMOV.match("	rep;	movsq"), "REPMOV cannot match a string move"
    assert LEN_IMM.match("	movl	$1536, %r8d"), "LEN_IMM cannot read a length"
    assert TOP.match("_RNvNtCs_5alloc14box_new_uninit:"), "TOP cannot match a symbol"
    assert CV_LOC.match("	.cv_loc	802 24 280 0"), "CV_LOC cannot match a line record"


def main():
    ap = argparse.ArgumentParser()
    ap.add_argument("--crate", action="append",
                    help="crate to scan (default: rusty_h265 and rusty_h265-accel)")
    ap.add_argument("--bin", action="append", default=[],
                    help="also scan a BINARY target, as crate:bin (the lib-only "
                         "default hides every copy in the CLI itself)")
    ap.add_argument("--min", type=int, default=1, help="only symbols with >= this many calls")
    ap.add_argument("--filter", default="", help="substring the symbol must contain")
    ap.add_argument("--lines", action="store_true",
                    help="rebuild with -Cdebuginfo=1 and name each call's source line")
    ap.add_argument("--by-line", action="store_true",
                    help="with --lines: rank RUNTIME-length calls by source line")
    a = ap.parse_args()
    self_test()
    crates = a.crate or ["rusty_h265", "rusty_h265-accel"]

    grand = collections.Counter()
    byline = collections.Counter()
    targets = [(c, None) for c in crates]
    targets += [(t.split(":", 1)[0], t.split(":", 1)[1]) for t in a.bin]
    for c, b in targets:
        path = build(c, lines=a.lines, bin_name=b)
        print("\n#### %s  (%s, %d KiB)" % (c, os.path.basename(path), os.path.getsize(path) // 1024))
        rows = []
        for sym, hits in scan(path):
            short = demangle(sym)
            if a.filter and a.filter not in short:
                continue
            if len(hits) < a.min:
                continue
            rows.append((len(hits), short, hits))
        rows.sort(key=lambda r: -r[0])
        for n, short, hits in rows:
            kinds = collections.Counter(k for k, _, _ in hits)
            unknown = sum(1 for _, ln, _ in hits if ln is None)
            sizes = sorted({ln for _, ln, _ in hits if ln is not None})
            desc = " ".join("%sx%d" % (k, v) for k, v in sorted(kinds.items()))
            tail = ("  const=%s" % sizes[:8]) if sizes else ""
            print("  %3d  %-70s  %s  runtime=%d%s" % (n, short[-70:], desc, unknown, tail))
            if a.lines:
                per = collections.Counter()
                for k, ln, src in hits:
                    per[(src or "?", k, "const" if ln is not None else "RUNTIME")] += 1
                for (src, k, kind), cnt in per.most_common():
                    print("          %3dx  %-8s %-7s  %s" % (cnt, k, kind, src))
                    if kind == "RUNTIME":
                        byline[(src, k)] += cnt
            grand[short] += n
    if a.by_line and byline:
        print("\n#### RUNTIME-length calls by source line -- the dangerous kind")
        for (src, k), cnt in byline.most_common(30):
            print("  %4dx  %-8s  %s" % (cnt, k, src))
    print("\n#### %d call sites in %d symbols" % (sum(grand.values()), len(grand)))


main()
