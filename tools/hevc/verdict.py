#!/usr/bin/env python3
"""Fold conform.py JSON runs into the H0.4 verdict table.

  python tools/hevc/verdict.py --ref hevc-vectors/results/ffmpeg.json \
      hevc-vectors/results/rust_h265.json hevc-vectors/results/hpvcd.json

Several JSON files may describe the same decoder (e.g. a main run plus an
`_extra` run over streams fetched later): pass them all; later files override
earlier rows with the same stream name. The reference run supplies the
"agrees with ffmpeg output" column (candidate output md5 == ffmpeg output md5),
which separates "wrong vs the published md5 but identical to ffmpeg" (a
harness/reference question) from a real decoder difference.
"""
import argparse, collections, glob, json, os


def load(paths):
    rows, decoder = {}, None
    for p in paths:
        js = json.load(open(p))
        decoder = decoder or js["decoder"]
        for r in js["results"]:
            rows[r["name"]] = r
    return decoder, rows


def main():
    ap = argparse.ArgumentParser()
    ap.add_argument("--ref", required=True, help="reference (ffmpeg) json file(s), comma-separated")
    ap.add_argument("cands", nargs="*", help="candidate json files; group by decoder automatically")
    ap.add_argument("--markdown", action="store_true")
    a = ap.parse_args()

    _, ref = load(a.ref.split(","))
    groups = collections.OrderedDict()
    for p in a.cands:
        d = json.load(open(p))["decoder"].split()[-1]
        groups.setdefault(d, []).append(p)
    cands = [(name, load(ps)[1]) for name, ps in groups.items()]

    names = sorted(ref)
    classes = sorted({n.split("_")[0].upper() for n in names})
    cols = ["ffmpeg"] + [c[0] for c in cands]

    def cls_of(n):
        return n.split("_")[0].upper()

    sep = "|" if a.markdown else " "
    hdr = f"{'class':11}" + "".join(f"{sep}{c:>16}" for c in cols) + ("|" if a.markdown else "")
    if a.markdown:
        print("| class | " + " | ".join(cols) + " |")
        print("|---|" + "---:|" * len(cols))
    else:
        print(hdr)
    tot = {c: [0, 0] for c in cols}
    agree = {c[0]: [0, 0] for c in cands}
    for cl in classes:
        ns = [n for n in names if cls_of(n) == cl]
        cells = []
        for c in cols:
            rows = ref if c == "ffmpeg" else dict(cands)[c]
            p = sum(1 for n in ns if rows.get(n, {}).get("md5") == "pass")
            tot[c][0] += p
            tot[c][1] += len(ns)
            cells.append(f"{p}/{len(ns)}")
        if a.markdown:
            print(f"| {cl} | " + " | ".join(cells) + " |")
        else:
            print(f"{cl:11}" + "".join(f" {x:>16}" for x in cells))
    cells = [f"{tot[c][0]}/{tot[c][1]} ({100.0 * tot[c][0] / max(1, tot[c][1]):.1f}%)" for c in cols]
    if a.markdown:
        print("| **TOTAL md5** | " + " | ".join(f"**{x}**" for x in cells) + " |")
    else:
        print(f"{'TOTAL md5':11}" + "".join(f" {x:>16}" for x in cells))

    # agreement + failure anatomy per candidate
    for name, rows in cands:
        same = sum(1 for n in names if rows.get(n, {}).get("out_md5") and rows[n]["out_md5"] == ref.get(n, {}).get("out_md5"))
        fails = [n for n in names if rows.get(n, {}).get("md5") != "pass"]
        no_out = sum(1 for n in fails if str(rows.get(n, {}).get("md5", "")).startswith("fail(no output)"))
        errs = sum(1 for n in fails if "errors=" in rows.get(n, {}).get("stderr", "") and "errors=0" not in rows[n]["stderr"])
        sei_pass = sum(1 for n in names if str(rows.get(n, {}).get("sei", "")).startswith("pass"))
        sei_judged = sum(1 for n in names if str(rows.get(n, {}).get("sei", "")).startswith(("pass", "fail")))
        print(f"\n{name}: output byte-identical to ffmpeg on {same}/{len(names)}; "
              f"{len(fails)} md5 failures = {no_out} no output + {errs} with decode errors + "
              f"{len(fails) - no_out - errs} silent mismatches; SEI per-picture pass {sei_pass}/{sei_judged}")
    rf = [n for n in names if ref[n].get("md5") != "pass"]
    print(f"\nffmpeg (reference) fails {len(rf)}: {', '.join(rf)}")


if __name__ == "__main__":
    main()
