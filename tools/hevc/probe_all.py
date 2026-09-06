#!/usr/bin/env python3
"""Record ffprobe's view of every conformance stream (the H1.1 parse oracle).

  python tools/hevc/probe_all.py [hevc-vectors] > hevc-vectors/probe.json

Per stream: width/height (cropped), pix_fmt, profile, level, and the output
picture count from the 100 % decoder's run (hpvcd.json) when present, so a
Rust test can check parameter-set and picture-count parity without ffmpeg.
"""
import glob, json, os, subprocess, sys

vec = sys.argv[1] if len(sys.argv) > 1 else "hevc-vectors"
frames = {}
for js in ("hpvcd.json", "hpvcd_extra.json"):
    p = os.path.join(vec, "results", js)
    if os.path.exists(p):
        for r in json.load(open(p))["results"]:
            frames[r["name"]] = r.get("frames")
out = {}
for bit in sorted(glob.glob(os.path.join(vec, "*.bit"))):
    name = os.path.basename(bit)[:-4]
    try:
        js = subprocess.run(
            ["ffprobe", "-v", "error", "-f", "hevc", "-show_streams", "-of", "json", bit],
            capture_output=True, text=True, timeout=120).stdout
        st = json.loads(js)["streams"][0]
    except Exception as e:  # noqa
        st = {}
    out[name] = {
        "width": st.get("width", 0), "height": st.get("height", 0),
        "pix_fmt": st.get("pix_fmt", ""), "profile": st.get("profile", ""),
        "level": st.get("level", 0), "frames": frames.get(name),
    }
json.dump(out, sys.stdout, indent=1, sort_keys=True)
