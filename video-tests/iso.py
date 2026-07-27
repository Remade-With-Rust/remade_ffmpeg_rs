#!/usr/bin/env python3
"""Iso-quality Pareto from results/pareto.tsv.

Both arms were run at a matched operating point, but a matched cq-level does not
land them at the same PSNR — so comparing time at the same crf still compares two
different amounts of work. The only honest reading is: at the SAME reconstructed
quality, how long does each take and how many bits does each spend.

Interpolation is linear on the (psnr -> ms) and (psnr -> kbps) curves and is
restricted to the PSNR overlap of the two arms; nothing is extrapolated.
"""
import sys, collections

path = sys.argv[1] if len(sys.argv) > 1 else "results/pareto.tsv"
rows = [l.rstrip("\n").split("\t") for l in open(path)]
hdr, rows = rows[0], rows[1:]
ix = {k: i for i, k in enumerate(hdr)}

# clip -> codec -> [(psnr, ms, kbps)]
data = collections.defaultdict(lambda: collections.defaultdict(list))
for r in rows:
    if r[ix["kind"]] != "encode":
        continue
    psnr = float(r[ix["psnr"]])
    if psnr != psnr:      # NaN
        continue
    data[r[ix["clip"]]][r[ix["codec"]]].append(
        (psnr, float(r[ix["wall_ms"]]), float(r[ix["kbps"]]))
    )


def interp(curve, q):
    """Linear interpolation of (ms, kbps) at psnr q. None outside the range."""
    c = sorted(curve)
    if q < c[0][0] or q > c[-1][0]:
        return None
    for (q0, m0, k0), (q1, m1, k1) in zip(c, c[1:]):
        if q0 <= q <= q1:
            f = 0.0 if q1 == q0 else (q - q0) / (q1 - q0)
            return (m0 + f * (m1 - m0), k0 + f * (k1 - k0))
    return None


print(f"{'clip':<16}{'PSNR':>7}{'ours ms':>10}{'libvpx ms':>11}"
      f"{'slower':>9}{'ours kb/s':>11}{'libvpx':>9}{'bits':>8}")
print("-" * 81)
agg_t, agg_b = [], []
for clip, arms in data.items():
    if "ours" not in arms or "libvpx" not in arms:
        continue
    o, l = sorted(arms["ours"]), sorted(arms["libvpx"])
    lo = max(o[0][0], l[0][0])
    hi = min(o[-1][0], l[-1][0])
    if hi <= lo:
        print(f"{clip:<16}  no PSNR overlap ({o[0][0]:.1f}-{o[-1][0]:.1f} vs "
              f"{l[0][0]:.1f}-{l[-1][0]:.1f})")
        continue
    # Sample the overlap at its midpoint and quartiles rather than one point, so
    # a single interpolation artifact cannot carry the verdict.
    for frac, label in ((0.5, ""),):
        q = lo + frac * (hi - lo)
        a, b = interp(o, q), interp(l, q)
        if not a or not b:
            continue
        tr, br = a[0] / b[0], a[1] / b[1]
        agg_t.append(tr)
        agg_b.append(br)
        print(f"{clip:<16}{q:7.2f}{a[0]:10.0f}{b[0]:11.0f}{tr:8.2f}x"
              f"{a[1]:11.0f}{b[1]:9.0f}{(br-1)*100:+7.1f}%")
print("-" * 81)
if agg_t:
    gm = lambda v: (lambda p: p ** (1.0 / len(v)))(
        __import__("functools").reduce(lambda x, y: x * y, v, 1.0))
    print(f"{'geomean':<16}{'':>7}{'':>10}{'':>11}{gm(agg_t):8.2f}x"
          f"{'':>11}{'':>9}{(gm(agg_b)-1)*100:+7.1f}%")
