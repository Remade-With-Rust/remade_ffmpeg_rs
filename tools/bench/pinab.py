"""The one compliant timing harness: pinned, High priority, ABBA-interleaved.

`codec-measurement` section 13 says there must be ONE of these and everything else
calls it, because a second implementation is a second place for the discipline to
rot. It also says every benchmark PRINTS its method line, so this one does.

What it enforces:

* **Pinning + High priority.** An unpinned run on this box shows a ~2x spread
  within a single arm -- scheduler migration, not heat -- which destroys paired
  A/B even though the minima are sound.
* **ABBA with the LEADING arm alternated.** `A B A B` is not alternating; B is
  always second and pays the cold-cache cost.
* **A null arm first**, same binary in both slots, printed as the floor. A delta
  smaller than the floor is not a result.
* **The arm's own internal duration**, parsed from its output, so process launch
  is excluded (it inflates the shorter arm by a larger fraction).

    python tools/bench/pinab.py <armA> <armB> <input.wav> [kbps] [rounds]

An arm is an executable path, or `KEY=VAL[,KEY=VAL]:path` to run the same
binary with an env knob set -- e.g. `MP3_SHORT_SHAPE=0:target/.../encprof.exe`.

Any other command line (the rff CLI, say) goes after `--`, verbatim, with the
round count from `ROUNDS` (default 12). The arm still has to print
`total <ms> ms`; `RFF_PHASES=1` makes the CLI do that:

    ROUNDS=16 python tools/bench/pinab.py RFF_PHASES=1:base/rff.exe         RFF_PHASES=1:target/release/rff.exe -- -y -i long.wav -c:a mp3 out.mp3
"""
import os
import re
import statistics
import subprocess
import sys

import psutil

# Avoid CPU 0: it takes the interrupt load.
PIN_CPU = int(os.environ.get("PIN_CPU", "4"))
# The arm's duration. `PINAB_DUR` overrides the pattern to A/B ONE phase of a
# multi-phase report -- e.g. `PINAB_DUR='conform +([\d.]+) ms'` against the
# rff CLI's RFF_PHASES table -- when a change touches only that phase and the
# others would just add their noise to the total.
DUR = re.compile(os.environ.get("PINAB_DUR", r"total ([\d.]+) ms"))


def split_arm(arm):
    """An arm is `path` or `KEY=VAL[,KEY=VAL...]:path` -- the env form lets two
    arms be the SAME binary with one knob flipped, which is the cleanest A/B there
    is (nothing but the knob differs). Windows drive letters (`F:\`) are not
    mistaken for the separator: the env part must contain `=`."""
    head, sep, tail = arm.partition(":")
    if sep and "=" in head and not os.path.exists(arm):
        env = dict(kv.split("=", 1) for kv in head.split(","))
        return tail, env
    return arm, {}


def run(arm, args):
    """One pinned, High-priority run; returns the arm's self-reported ms."""
    exe, extra = split_arm(arm)
    env = dict(os.environ)
    env.update(extra)
    p = subprocess.Popen([exe] + args, stdout=subprocess.PIPE,
                         stderr=subprocess.STDOUT, text=True, env=env)
    try:
        h = psutil.Process(p.pid)
        h.cpu_affinity([PIN_CPU])
        h.nice(psutil.HIGH_PRIORITY_CLASS)
    except Exception:
        pass  # a run that could not be pinned still counts; the null arm shows it
    out, _ = p.communicate()
    m = DUR.search(out)
    if not m:
        raise SystemExit(f"no duration in output of {exe}:\n{out[:400]}")
    return float(m.group(1))


def series(a, b, args, rounds):
    res = {a: [], b: []}
    for i in range(rounds):
        order = (a, b) if i % 2 == 0 else (b, a)   # alternate the LEADING arm
        for k in order:
            res[k].append(run(k, args))
    return res


def report(name_a, name_b, res, label):
    a, b = sorted(res[name_a]), sorted(res[name_b])
    pairs = list(zip(res[name_a], res[name_b]))
    wins = sum(1 for x, y in pairs if y < x)
    n = len(pairs)
    z = (wins - n / 2) / (0.5 * n ** 0.5)
    print(f"  {label:<22} A min={a[0]:7.1f} med={statistics.median(a):7.1f} | "
          f"B min={b[0]:7.1f} med={statistics.median(b):7.1f} | "
          f"min {a[0]/b[0]:.3f}x med {statistics.median(a)/statistics.median(b):.3f}x | "
          f"B faster {wins}/{n}, z={z:+.2f}")
    return a[0] / b[0], statistics.median(a) / statistics.median(b), z


def main():
    arm_a, arm_b = sys.argv[1], sys.argv[2]
    if len(sys.argv) > 3 and sys.argv[3] == "--":
        args = sys.argv[4:]
        rounds = int(os.environ.get("ROUNDS", "12"))
    else:
        inp = sys.argv[3]
        kbps = sys.argv[4] if len(sys.argv) > 4 else "192"
        rounds = int(sys.argv[5]) if len(sys.argv) > 5 else 12
        args = [os.path.abspath(inp), kbps]

    print(f"method: pinned to CPU {PIN_CPU}, High priority, arms ABBA with the "
          f"leading arm alternated, {rounds} pairs, each arm's own internal "
          f"stage time (process launch excluded), null arm first")
    nul = series(arm_a, arm_a, args, max(6, rounds // 2))
    # the null arm's two slots are the same list object key, so re-run explicitly
    nres = {"n1": [], "n2": []}
    for i in range(max(6, rounds // 2)):
        for k in (("n1", "n2") if i % 2 == 0 else ("n2", "n1")):
            nres[k].append(run(arm_a, args))
    n1, n2 = sorted(nres["n1"]), sorted(nres["n2"])
    floor_min = max(n1[0] / n2[0], n2[0] / n1[0])
    floor_med = max(statistics.median(n1) / statistics.median(n2),
                    statistics.median(n2) / statistics.median(n1))
    spread = n1[-1] / n1[0]
    print(f"  {'NULL ARM':<22} floor min {floor_min:.3f}x  med {floor_med:.3f}x  "
          f"(within-arm spread {spread:.2f}x)")
    del nul

    res = series(arm_a, arm_b, args, rounds)
    rmin, rmed, z = report(arm_a, arm_b, res, "A vs B")
    effect = max(abs(rmin - 1), abs(rmed - 1))
    if effect < (floor_min - 1) or abs(z) < 2:
        print(f"\n  VERDICT: NOT ADMISSIBLE -- effect {effect*100:.1f}% vs floor "
              f"{(floor_min-1)*100:.1f}%, |z|={abs(z):.2f}. Decide on a counter.")
    else:
        print(f"\n  VERDICT: {rmin:.3f}x min / {rmed:.3f}x median, z={z:+.2f}, "
              f"above a {(floor_min-1)*100:.1f}% floor.")


if __name__ == "__main__":
    main()
