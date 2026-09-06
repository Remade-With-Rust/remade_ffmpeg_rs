#!/usr/bin/env python3
"""Pre-loop-filter first-picture gate: our first output picture vs ffmpeg's
with `-skip_loop_filter all` (deblocking + SAO off). Isolates the
intra/residual/transform path from the in-loop filters.

  python tools/hevc/prefilter_gate.py F:/.../hevc-vectors F:/.../rusty_h265.exe [PREFIX...]
"""
import glob, json, os, subprocess, sys, time

vec, exe = sys.argv[1], sys.argv[2]
prefixes = sys.argv[3:]
probe = json.load(open(os.path.join(vec, "probe.json")))
ok = bad = skipped = 0
for bit in sorted(glob.glob(os.path.join(vec, "*.bit"))):
    name = os.path.basename(bit)[:-4]
    if prefixes and not any(name.startswith(p) for p in prefixes):
        continue
    p = probe.get(name, {})
    w, h = p.get("width", 0), p.get("height", 0)
    if not w or name.startswith("VPSSPSPPS"):
        skipped += 1
        continue
    bps = 2 if "10" in p.get("pix_fmt", "") else 1
    fs = w * h * 3 // 2 * bps
    ours = os.path.join(vec, ".tmp", "pf_ours.yuv")
    ref = os.path.join(vec, ".tmp", "pf_ref.yuv")
    t0 = time.time()
    r = subprocess.run([exe, bit, ours], capture_output=True, text=True, timeout=900)
    subprocess.run(["ffmpeg", "-v", "error", "-y", "-threads", "1", "-skip_loop_filter", "all", "-f", "hevc", "-i", bit,
                    "-fps_mode", "passthrough", "-frames:v", "1", "-f", "rawvideo", ref], capture_output=True, timeout=900)
    try:
        a = open(ours, "rb").read(fs)
        b = open(ref, "rb").read(fs)
    except OSError:
        a, b = b"", b""
    err = (r.stderr.strip().splitlines() or [""])[-1][:80]
    if len(a) == fs and len(b) == fs and a == b:
        ok += 1
        print(f"{name:40} SAME  {time.time()-t0:.1f}s", flush=True)
    else:
        bad += 1
        planes = [("Y", 0, w * h * bps, w * bps), ("U", w * h * bps, w * h // 4 * bps, w // 2 * bps), ("V", w * h * bps * 5 // 4, w * h // 4 * bps, w // 2 * bps)]
        detail = []
        for pn, off, n, pw in planes:
            pa, pb = a[off:off + n], b[off:off + n]
            if len(pa) != n or len(pb) != n:
                detail.append(f"{pn}:short")
                continue
            nd = sum(1 for i in range(n) if pa[i] != pb[i])
            if nd:
                i = next(i for i in range(n) if pa[i] != pb[i])
                detail.append(f"{pn}:{nd}diff@({i % pw // bps},{i // pw})")
        print(f"{name:40} DIFF  {' '.join(detail)} {err} {time.time()-t0:.1f}s", flush=True)
print(f"\npre-filter first picture identical to ffmpeg: {ok}/{ok + bad} (skipped {skipped})")
