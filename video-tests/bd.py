#!/usr/bin/env python3
"""BD-rate between two ladders of our own encoder — the gate for a bitstream change.

Reads two `pareto.tsv` files (anchor, candidate) and reports Bjontegaard-Delta rate
per clip. Negative = the candidate needs fewer bits at equal quality = a win.

Deliberately conservative, following the traps this repo has already paid for:

  * PCHIP (shape-preserving) interpolation over the PSNR OVERLAP ONLY. A global
    cubic fit extrapolates outside the overlap and has swung 8pp between identical
    runs.
  * Every curve is checked for RATE MONOTONICITY in the quality parameter. Rate is
    read straight from the file and cannot lie; if rate is ragged the encode or the
    metric is broken, not the codec.
  * The overlap width and how many ladder points fall inside it are REPORTED, so a
    BD computed from one usable point announces itself instead of being believed.

  usage: bd.py anchor.tsv candidate.tsv
"""
import sys, collections, math

def load(path):
    rows = [l.rstrip("\n").split("\t") for l in open(path)]
    hdr, rows = rows[0], rows[1:]
    ix = {k: i for i, k in enumerate(hdr)}
    out = collections.defaultdict(list)
    for r in rows:
        if r[ix["kind"]] != "encode" or r[ix["codec"]] != "ours":
            continue
        p = float(r[ix["psnr"]])
        k = float(r[ix["kbps"]])
        if p != p or k <= 0:
            continue
        out[r[ix["clip"]]].append((p, k, float(r[ix["wall_ms"]])))
    return {c: sorted(v) for c, v in out.items()}


def pchip_slopes(x, y):
    """Fritsch-Carlson slopes: monotone, no overshoot between data points."""
    n = len(x)
    h = [x[i + 1] - x[i] for i in range(n - 1)]
    d = [(y[i + 1] - y[i]) / h[i] for i in range(n - 1)]
    m = [0.0] * n
    m[0], m[-1] = d[0], d[-1]
    for i in range(1, n - 1):
        if d[i - 1] * d[i] <= 0:
            m[i] = 0.0
        else:
            w1, w2 = 2 * h[i] + h[i - 1], h[i] + 2 * h[i - 1]
            m[i] = (w1 + w2) / (w1 / d[i - 1] + w2 / d[i])
    return m


def pchip_eval(x, y, m, t):
    for i in range(len(x) - 1):
        if x[i] <= t <= x[i + 1]:
            h = x[i + 1] - x[i]
            s = (t - x[i]) / h
            h00 = 2 * s**3 - 3 * s**2 + 1
            h10 = s**3 - 2 * s**2 + s
            h01 = -2 * s**3 + 3 * s**2
            h11 = s**3 - s**2
            return h00 * y[i] + h10 * h * m[i] + h01 * y[i + 1] + h11 * h * m[i + 1]
    return None


def bd_rate(anchor, cand):
    """Mean log-rate difference over the shared PSNR range, as a percentage."""
    lo = max(anchor[0][0], cand[0][0])
    hi = min(anchor[-1][0], cand[-1][0])
    if hi - lo < 0.5:
        return None, lo, hi, 0
    inside = sum(1 for p, _, _ in anchor if lo <= p <= hi) + \
             sum(1 for p, _, _ in cand if lo <= p <= hi)

    def curve(pts):
        x = [p for p, _, _ in pts]
        y = [math.log10(k) for _, k, _ in pts]
        return x, y, pchip_slopes(x, y)

    ax, ay, am = curve(anchor)
    cx, cy, cm = curve(cand)
    N = 200
    acc = 0.0
    for i in range(N + 1):
        t = lo + (hi - lo) * i / N
        a, c = pchip_eval(ax, ay, am, t), pchip_eval(cx, cy, cm, t)
        if a is None or c is None:
            return None, lo, hi, inside
        acc += c - a
    return (10 ** (acc / (N + 1)) - 1) * 100.0, lo, hi, inside


def monotone(pts):
    """Rate must fall as PSNR falls. Read from the file, so it cannot lie."""
    return all(pts[i][1] < pts[i + 1][1] for i in range(len(pts) - 1))


a, c = load(sys.argv[1]), load(sys.argv[2])
print(f"{'clip':<16}{'BD-rate':>10}{'speed':>9}{'overlap dB':>13}{'pts':>5}  trust")
print("-" * 68)
vals, spds = [], []
for clip in sorted(set(a) & set(c)):
    bd, lo, hi, inside = bd_rate(a[clip], c[clip])
    ta = sum(t for _, _, t in a[clip])
    tc = sum(t for _, _, t in c[clip])
    warn = []
    if not monotone(a[clip]):
        warn.append("ANCHOR RATE NON-MONOTONE")
    if not monotone(c[clip]):
        warn.append("CAND RATE NON-MONOTONE")
    if inside < 4:
        warn.append(f"only {inside} pts in overlap")
    if bd is None:
        print(f"{clip:<16}{'n/a':>10}{'':>9}{hi-lo:12.2f}{inside:5d}  NO USABLE OVERLAP")
        continue
    vals.append(bd)
    spds.append(ta / tc)
    print(f"{clip:<16}{bd:+9.2f}%{ta/tc:8.2f}x{hi-lo:12.2f}{inside:5d}  "
          f"{'; '.join(warn) if warn else 'ok'}")
print("-" * 68)
if vals:
    gm = math.exp(sum(math.log(s) for s in spds) / len(spds))
    print(f"{'mean':<16}{sum(vals)/len(vals):+9.2f}%{gm:8.2f}x")
    print(f"{'worst clip':<16}{max(vals):+9.2f}%")
    print("\n(negative BD-rate = candidate wins; speed = anchor_time/cand_time, >1 = faster)")
