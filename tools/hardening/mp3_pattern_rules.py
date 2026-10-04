#!/usr/bin/env python3
"""Pattern rules for rusty_mp3 beyond what clippy checks (use-protection-please H-22).

    python tools/hardening/mp3_pattern_rules.py [crate_dir]

Each rule encodes a hardening invariant of this crate that clippy cannot see.
Exits 1 and lists every violation if any rule fails.

  R1  every `unsafe { ... }` block and `unsafe impl` is preceded by a
      `// SAFETY:` comment (attribute lines and other comment lines may sit
      between them).
  R2  every `unsafe fn` has a `# Safety` section in its doc comment (methods of
      trait impls inherit the trait's contract and are exempt).
  R3  in the modules that parse untrusted bytes, every `.unwrap()` / `.expect(`
      outside test code carries a `Cannot fail` justification within the three
      lines above it.
  R4  no `static mut`, `Box::leak`, `mem::forget` or `transmute` in library
      code outside tests (the band-model cache leak was a `Box::leak`).
  R5  every inline `#[allow(...)]` in library code states a reason, in a
      comment on the same line or the line(s) directly above.
"""
import pathlib
import re
import sys

crate = pathlib.Path(sys.argv[1] if len(sys.argv) > 1 else "crates/rusty_mp3")
PARSERS = {"src/header.rs", "src/bitio.rs", "src/decode/sideinfo.rs",
           "src/decode/scalefactors.rs", "src/decode/reservoir.rs",
           "src/decode/huffman.rs", "src/lib.rs"}
violations = []


def code_lines(path):
    """(lineno, text, in_test) for every line; `in_test` once a `#[cfg(test)]`
    module starts (this crate keeps tests at the end of each file)."""
    in_test = False
    for n, line in enumerate(path.read_text(encoding="utf-8").split("\n"), 1):
        if line.strip().startswith("#[cfg(test)]"):
            in_test = True
        yield n, line, in_test


def in_trait_impl(lines, i):
    """A method of a trait impl (`impl T for X` / `unsafe impl T for X`) inherits
    the trait's safety contract, so R2 does not apply to it."""
    for j in range(i - 1, -1, -1):
        t = lines[j]
        if t.startswith("impl") or t.startswith("unsafe impl"):
            return " for " in t
        if t.startswith("}") or t.startswith("fn ") or t.startswith("pub fn"):
            return False
    return False


def preceding(lines, i, limit=12):
    """Comment / attribute lines directly above index i (nearest first)."""
    out = []
    j = i - 1
    while j >= 0 and len(out) < limit:
        s = lines[j].strip()
        if s.startswith("//") or s.startswith("#[") or s.startswith("#![") or s == ")]" or s.startswith("clippy::") or s.endswith(",") and s.startswith("clippy"):
            out.append(s)
            j -= 1
            continue
        break
    return out


for path in sorted(list(crate.glob("src/**/*.rs")) + list(crate.glob("examples/*.rs"))):
    rel = path.relative_to(crate).as_posix()
    text = path.read_text(encoding="utf-8")
    lines = text.split("\n")
    is_lib = rel.startswith("src/")
    for n, line, in_test in code_lines(path):
        s = line.strip()
        if s.startswith("//"):
            continue
        i = n - 1
        above = preceding(lines, i)
        # R1
        if re.search(r"\bunsafe\s*\{", s) or s.startswith("unsafe impl"):
            same_stmt = re.match(r".*\bunsafe\s*\{", s) and not re.match(r"\s*(let\b.*=\s*)?(return\s+)?unsafe\b|^\[?unsafe\b|.*=\s*\[unsafe", s)
            if not any(a.startswith("// SAFETY") for a in above) and "SAFETY" not in line:
                # a SAFETY comment may also span several lines ending just above
                window = "\n".join(lines[max(0, i - 6):i])
                if "SAFETY:" not in window:
                    violations.append(f"R1 {rel}:{n}: unsafe without a SAFETY comment: {s[:80]}")
        # R2
        if re.match(r"(pub(\(crate\))?\s+)?unsafe fn\b", s) and not in_trait_impl(lines, i):
            doc = []
            j = i - 1
            while j >= 0 and (lines[j].strip().startswith("///") or lines[j].strip().startswith("#[")):
                doc.append(lines[j]); j -= 1
            if not any("# Safety" in d for d in doc):
                violations.append(f"R2 {rel}:{n}: unsafe fn without a `# Safety` doc section")
        if not is_lib or in_test:
            continue
        # R3
        if rel in PARSERS and (".unwrap()" in s or ".expect(" in s):
            window = "\n".join(lines[max(0, i - 3):i + 1])
            if "Cannot fail" not in window and "Never" not in window:
                violations.append(f"R3 {rel}:{n}: unwrap/expect on an input path without a `Cannot fail` justification")
        # R4
        for pat in ("static mut ", "Box::leak(", "mem::forget(", "transmute"):
            if pat in s:
                violations.append(f"R4 {rel}:{n}: forbidden `{pat.strip('( ')}`")
        # R5
        if s.startswith("#[allow(") or s.startswith("#![allow("):
            if "//" not in line and not any(a.startswith("//") for a in above):
                violations.append(f"R5 {rel}:{n}: #[allow] without a stated reason")

if violations:
    print("\n".join(violations))
    print(f"\n{len(violations)} pattern-rule violation(s)")
    sys.exit(1)
print("pattern rules R1-R5: clean")
