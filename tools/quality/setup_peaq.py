#!/usr/bin/env python3
"""Clone + patch an external PEAQ implementation for use as a calibration oracle.

We do NOT vendor PEAQ_python (external license); this fetches it and applies the
small fixes needed to run under numpy>=2 / Python 3.13. Run once:

    python tools/quality/setup_peaq.py [dest_dir]

Then score a pair (delay-aligned internally):

    python tools/quality/peaq_run.py reference.wav test.wav <dest_dir>

ODG range [-4, 0]: 0 = imperceptible difference, -4 = very annoying.
Validated against the bundled MATLAB reference (ODG = -3.875).
"""
import os
import re
import subprocess
import sys

REPO = "https://github.com/lsg1213/PEAQ_python"


def patch(d: str) -> None:
    f = os.path.join(d, "numpy_PEAQ.py")
    src = open(f, encoding="utf-8").read()
    # numpy>=2 removed the np.<builtin> aliases.
    for name in ("int", "float", "bool", "object", "complex"):
        src = re.sub(r"np\." + name + r"\b", name, src)
    # A stray debugger drop and a bandwidth-MOV zero-size slice on silent frames.
    src = src.replace("import pdb; pdb.set_trace()", "pass  # disabled pdb")
    src = src.replace(
        "X2MatT[...,:int(BWRef-1)]", "X2MatT[...,:max(int(BWRef-1),1)]"
    )
    open(f, "w", encoding="utf-8").write(src)


def main() -> None:
    dest = sys.argv[1] if len(sys.argv) > 1 else "PEAQ_python"
    if not os.path.isdir(dest):
        subprocess.run(["git", "clone", "--depth", "1", REPO, dest], check=True)
    patch(dest)
    print(f"PEAQ ready in {dest}/ — run: python tools/quality/peaq_run.py ref.wav test.wav {dest}")


if __name__ == "__main__":
    main()
