#!/usr/bin/env python3
"""Mutation-test the RExt guards: remove each, confirm a test goes red."""
import io
import os
import shutil
import subprocess
import sys
import time


def write_and_touch(path, text):
    """Write, then bump mtime forward.

    cargo fingerprints by mtime. Swapping a file back and forth fast enough can
    leave the timestamp looking unchanged, so the crate is NOT rebuilt -- which
    in a mutation harness means the mutated code never ran and the guard reads
    as SURVIVED when it was simply never tested. Never let a mutation harness
    decide 'not a gate' on a build it cannot prove happened.
    """
    io.open(path, "w", encoding="utf-8").write(text)
    t = time.time() + 1
    os.utime(path, (t, t))

SAO = "crates/rusty_h265-accel/src/sao.rs"
INTRA = "crates/rusty_h265-accel/src/intra.rs"

MUTATIONS = [
    ("SAO band offset guard", SAO, "offsets_fit_lut(band) && ", ""),
    ("SAO edge offset guard", SAO, "offsets_fit_lut(offs) && ", ""),
    ("angular i16 guard", INTRA, " && angular_i16_is_exact(max)", ""),
]


def run_tests():
    r = subprocess.run(
        ["cargo", "test", "--release", "-p", "rusty_h265-accel", "--lib"],
        capture_output=True, text=True,
    )
    return r.returncode == 0, r.stdout + r.stderr


ok, out = run_tests()
if not ok:
    print("BASELINE IS ALREADY RED -- fix before mutating")
    sys.exit(1)
print("baseline: green\n")

for name, path, find, repl in MUTATIONS:
    shutil.copy(path, path + ".bak")
    s = io.open(path, encoding="utf-8").read()
    n = s.count(find)
    if n == 0:
        print(f"{name:<26} SKIP (pattern absent)")
        shutil.move(path + ".bak", path)
        continue
    write_and_touch(path, s.replace(find, repl))
    green, out = run_tests()
    write_and_touch(path, io.open(path + ".bak", encoding="utf-8").read())
    os.remove(path + ".bak")
    if green:
        print(f"{name:<26} *** SURVIVED ({n} site(s) removed, tests still green) -- NOT A GATE")
    else:
        failed = [l.strip() for l in out.splitlines() if "FAILED" in l and "test " in l]
        print(f"{name:<26} killed by: {failed[0] if failed else 'a failing test'}")

green, _ = run_tests()
print("\nrestored:", "green" if green else "RED -- restore failed")
