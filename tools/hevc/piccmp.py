#!/usr/bin/env python3
"""Per-picture comparison of our output vs ffmpeg (both fully filtered):
for each output picture, count differing luma samples and locate the first
differing 8x8 block.  python tools/hevc/piccmp.py VEC EXE NAME [maxpics]"""
import json, os, subprocess, sys
vec, exe, name = sys.argv[1], sys.argv[2], sys.argv[3]
maxp = int(sys.argv[4]) if len(sys.argv) > 4 else 6
p = json.load(open(os.path.join(vec, "probe.json")))[name]
w, h = p["width"], p["height"]
bps = 2 if "10" in p["pix_fmt"] else 1
fs = w * h * 3 // 2 * bps
bit = os.path.join(vec, name + ".bit")
ours = os.path.join(vec, ".tmp", "cmp_ours.yuv"); ref = os.path.join(vec, ".tmp", "cmp_ref.yuv")
r = subprocess.run([exe, bit, ours], capture_output=True, text=True); print(r.stdout.strip()[:200])
nolf = ["-skip_loop_filter", "all"] if os.environ.get("RH265_NO_LF") else []
subprocess.run(["ffmpeg", "-v", "error", "-y", "-threads", "1"] + nolf + ["-f", "hevc", "-i", bit, "-fps_mode", "passthrough", "-f", "rawvideo", ref], capture_output=True)
a = open(ours, "rb").read(); b = open(ref, "rb").read()
n = min(len(a), len(b)) // fs
print(f"{name} {w}x{h} pictures ours={len(a)//fs} ref={len(b)//fs}")
for k in range(min(n, maxp)):
    fa = a[k*fs:(k+1)*fs]; fb = b[k*fs:(k+1)*fs]
    if fa == fb:
        print(f"  pic {k}: identical"); continue
    ya = fa[:w*h*bps]; yb = fb[:w*h*bps]
    nd = sum(1 for i in range(0, len(ya), bps) if ya[i:i+bps] != yb[i:i+bps])
    first = None
    for by in range(0, h, 8):
        for bx in range(0, w, 8):
            bad = False
            for yy in range(by, min(by+8, h)):
                o = (yy*w+bx)*bps
                if ya[o:o+8*bps] != yb[o:o+8*bps]:
                    bad = True; break
            if bad:
                first = (bx, by); break
        if first: break
    # first differing 8x8 blocks list (up to 8)
    blocks = []
    for by in range(0, h, 8):
        for bx in range(0, w, 8):
            for yy in range(by, min(by+8, h)):
                o = (yy*w+bx)*bps
                if ya[o:o+8*bps] != yb[o:o+8*bps]:
                    blocks.append((bx, by)); break
            if len(blocks) >= 12: break
        if len(blocks) >= 12: break
    print(f"  pic {k}: {nd} luma samples differ; first 8x8 blocks: {blocks}")
    if first:
        bx, by = first
        for yy in range(by, min(by+4, h)):
            o = (yy*w+bx)*bps
            print("     ours", list(ya[o:o+8*bps][::bps]), " ref", list(yb[o:o+8*bps][::bps]))
