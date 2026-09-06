#!/usr/bin/env python3
"""HEVC conformance harness (docs/plans/rusty_hevc.md, H0.2) — decoder-agnostic.

For every JCT-VC HEVC_v1 stream in --vectors (fetched by
scripts/fetch-hevc-vectors.sh) it runs a decoder to raw planar YUV in output
order and reports, per stream:

  md5   the MD5 of the whole decoded YUV vs the published <name>.yuv.md5
  sei   the decoded-picture-hash SEI (payload 132: MD5 / CRC / checksum per
        plane) vs the same hashes computed from the decoder's pictures, compared
        as a multiset (the SEI is in decode order, the output in display order);
        skipped when the stream is cropped (the SEI covers the full coded
        picture and external decoders emit the cropped one) or carries no hash

Decoders:
  --decoder ffmpeg              ffmpeg's native hevc decoder (the C north star)
  --decoder "<cmd> <name>"      any command taking `<in.bit> <out.yuv>` and
                                writing 8-bit as u8 / >8-bit as u16 LE, e.g.
                                "../_hevc_score/target/release/hevc_score rust_h265"

Self-test (run it before believing any table):
  --self-test   a known-good stream must PASS and a byte-corrupted copy must
                FAIL; exits non-zero otherwise.

Usage:
  python tools/hevc/conform.py --vectors hevc-vectors --decoder ffmpeg --json out/ffmpeg.json
  python tools/hevc/conform.py --vectors hevc-vectors --decoder "hevc_score rust_h265" \
      --json out/rust_h265.json --ref-json out/ffmpeg.json
"""
import argparse, hashlib, json, os, re, shutil, subprocess, sys, tempfile, time

# ---------------------------------------------------------------- bitstream

def annexb_nals(data: bytes):
    """Yield NAL payloads (without start code, emulation prevention removed)."""
    i, n = 0, len(data)
    starts = []
    while i + 3 <= n:
        if data[i] == 0 and data[i + 1] == 0 and data[i + 2] == 1:
            starts.append(i + 3)
            i += 3
        else:
            i += 1
    for k, s in enumerate(starts):
        e = starts[k + 1] - 3 if k + 1 < len(starts) else n
        while e > s and data[e - 1] == 0:  # trailing zero bytes belong to the next start code
            e -= 1
        raw = data[s:e]
        out = bytearray()
        zeros = 0
        for b in raw:
            if zeros >= 2 and b == 3:
                zeros = 0
                continue
            out.append(b)
            zeros = zeros + 1 if b == 0 else 0
        yield bytes(out)


def sei_picture_hashes(data: bytes):
    """All decoded-picture-hash SEI payloads: list of (hash_type, [plane bytes...])."""
    found = []
    for nal in annexb_nals(data):
        if len(nal) < 3:
            continue
        nut = (nal[0] >> 1) & 0x3F
        if nut not in (39, 40):  # prefix / suffix SEI
            continue
        p = 2
        while p + 1 < len(nal):
            ptype = 0
            while p < len(nal) and nal[p] == 0xFF:
                ptype += 255
                p += 1
            if p >= len(nal):
                break
            ptype += nal[p]; p += 1
            psize = 0
            while p < len(nal) and nal[p] == 0xFF:
                psize += 255
                p += 1
            if p >= len(nal):
                break
            psize += nal[p]; p += 1
            payload = nal[p:p + psize]
            p += psize
            if ptype == 132 and len(payload) >= 1:
                ht = payload[0]
                per = {0: 16, 1: 2, 2: 4}.get(ht)
                if per is None:
                    continue
                planes = [payload[1 + k * per:1 + (k + 1) * per] for k in range(3)]
                if all(len(x) == per for x in planes):
                    found.append((ht, planes))
            if p < len(nal) and nal[p] == 0x80:  # rbsp trailing bits
                break
    return found


def _crc16_table():
    t = []
    for b in range(256):
        c = b << 8
        for _ in range(8):
            c = ((c << 1) ^ 0x1021) & 0xFFFF if c & 0x8000 else (c << 1) & 0xFFFF
        t.append(c)
    return t

_CRC16 = _crc16_table()


def plane_crc(buf: bytes) -> bytes:
    """H.265 D.3.19 CRC. The spec's loop is the AUGMENTED form (init 0xFFFF, the data
    bit enters at the LSB, 16 zero bits pushed through at the end); that is identical to
    the plain table update with init 0x1D0F and no augmentation (CRC-16/AUG-CCITT) —
    proven equal to the bit loop on random buffers. Table form is ~40x the bit loop,
    which stalled a 1080p x 96-frame stream for hours."""
    crc = 0x1D0F
    for b in buf:
        crc = ((crc << 8) & 0xFFFF) ^ _CRC16[((crc >> 8) ^ b) & 0xFF]
    return crc.to_bytes(2, "big")


def plane_checksum(buf: bytes, w: int, h: int, bps: int) -> bytes:
    """H.265 D.3.19 checksum: sum over samples of (xorMask ^ byte), where
    xorMask = (x & 0xFF) ^ (y & 0xFF) ^ (x >> 8) ^ (y >> 8); for >8-bit both bytes
    (low then high) use the same mask. Row masks are built once and applied with a
    C-level map, not a Python loop per sample."""
    import operator
    s = 0
    xmask = bytes(((x & 0xFF) ^ (x >> 8)) for x in range(w))
    if bps == 2:
        xmask = bytes(v for v in xmask for _ in (0, 1))
    rowlen = w * bps
    for y in range(h):
        ym = (y & 0xFF) ^ (y >> 8)
        mask = bytes(v ^ ym for v in xmask) if ym else xmask
        row = buf[y * rowlen:(y + 1) * rowlen]
        s += sum(map(operator.xor, row, mask))
    return (s & 0xFFFFFFFF).to_bytes(4, "big")


def frame_hashes(frame: bytes, w: int, h: int, bps: int, ht: int):
    cw, ch = (w + 1) // 2, (h + 1) // 2
    sizes = [(w, h), (cw, ch), (cw, ch)]
    out, off = [], 0
    for pw, ph in sizes:
        n = pw * ph * bps
        pl = frame[off:off + n]
        off += n
        if ht == 0:
            out.append(hashlib.md5(pl).digest())
        elif ht == 1:
            out.append(plane_crc(pl))
        else:
            out.append(plane_checksum(pl, pw, ph, bps))
    return out

# ---------------------------------------------------------------- probing

def ffprobe(path):
    cmd = ["ffprobe", "-v", "error", "-f", "hevc", "-select_streams", "v:0", "-show_entries",
           "stream=width,height,coded_width,coded_height,pix_fmt,profile", "-of", "json", path]
    try:
        js = json.loads(subprocess.run(cmd, capture_output=True, text=True, timeout=120).stdout)
        return js["streams"][0]
    except Exception:
        return None


def bps_of(pix_fmt: str) -> int:
    return 2 if re.search(r"p1[0-6]", pix_fmt or "") else 1

# ---------------------------------------------------------------- decoding

def run_decoder(decoder: str, bit: str, yuv: str):
    t0 = time.perf_counter()
    if decoder == "ffmpeg":
        # `-f hevc`: the .bit extension is not a registered demuxer name.
        # (`-fps_mode` is an OUTPUT option: after `-i`, or ffmpeg refuses the input.)
        cmd = ["ffmpeg", "-v", "error", "-y", "-threads", "1", "-f", "hevc", "-i", bit,
               "-fps_mode", "passthrough", "-f", "rawvideo", yuv]
    else:
        cmd = decoder.split() + [bit, yuv]
    try:
        r = subprocess.run(cmd, capture_output=True, text=True, timeout=1800)
        rc, err = r.returncode, (r.stderr or "").strip()
    except subprocess.TimeoutExpired:
        rc, err = -9, "timeout"
    return rc, err, time.perf_counter() - t0


def md5_file(p):
    m = hashlib.md5()
    with open(p, "rb") as f:
        for chunk in iter(lambda: f.read(1 << 20), b""):
            m.update(chunk)
    return m.hexdigest()

# ---------------------------------------------------------------- one stream

def check_stream(name, vdir, decoder, tmp, want_sei=True):
    bit = os.path.join(vdir, name + ".bit")
    want = open(os.path.join(vdir, name + ".yuv.md5")).read().strip().lower()
    yuv = os.path.join(tmp, name + ".yuv")
    rc, err, secs = run_decoder(decoder, bit, yuv)
    res = {"name": name, "class": name.split("_")[0].upper(), "rc": rc, "secs": round(secs, 3),
           "stderr": err[-400:], "md5": "fail", "sei": "n/a", "frames": None, "out_md5": None}
    if not os.path.exists(yuv) or os.path.getsize(yuv) == 0:
        res["md5"] = "fail(no output)"
        return res
    got = md5_file(yuv)
    res["out_md5"] = got
    res["md5"] = "pass" if got == want else "fail"
    info = ffprobe(bit)
    if info:
        w, h = int(info["width"]), int(info["height"])
        cw, ch = int(info.get("coded_width") or w), int(info.get("coded_height") or h)
        bps = bps_of(info.get("pix_fmt", ""))
        fsz = (w * h + 2 * ((w + 1) // 2) * ((h + 1) // 2)) * bps
        size = os.path.getsize(yuv)
        res["frames"] = size // fsz if fsz else None
        if fsz == 0:
            # ffprobe could not read the geometry (e.g. TSUNEQBD's unequal luma/chroma
            # bit depth, which ffmpeg's hevc decoder refuses) -- the md5 verdict stands alone.
            res["sei"] = "n/a(no probe geometry)"
        elif want_sei:
            with open(bit, "rb") as f:
                data = f.read()
            seis = sei_picture_hashes(data)
            if not seis:
                res["sei"] = "n/a(no hash sei)"
            elif (cw, ch) != (w, h):
                res["sei"] = "n/a(cropped)"
            elif size % fsz != 0:
                res["sei"] = "fail(size)"
            else:
                # Every OUTPUT picture must carry a hash that some SEI declared.
                # The SEI set may be larger: pictures with pic_output_flag=0 or
                # dropped RASL pictures are decoded (hashed) but never output.
                ht = seis[0][0]
                from collections import Counter
                want = Counter(b"".join(p) for _, p in seis)
                got, missing = 0, 0
                with open(yuv, "rb") as f:
                    while True:
                        fr = f.read(fsz)
                        if len(fr) < fsz:
                            break
                        got += 1
                        key = b"".join(frame_hashes(fr, w, h, bps, ht))
                        if want[key] > 0:
                            want[key] -= 1
                        else:
                            missing += 1
                res["sei"] = (f"pass({got}/{len(seis)})" if got == len(seis) else f"pass({got} of {len(seis)} sei)") \
                    if missing == 0 and got > 0 else f"fail({missing} of {got} pics unmatched)"
    try:
        os.remove(yuv)
    except OSError:
        pass
    return res

# ---------------------------------------------------------------- main

def main():
    ap = argparse.ArgumentParser()
    ap.add_argument("--vectors", default="hevc-vectors")
    ap.add_argument("--decoder", default="ffmpeg")
    ap.add_argument("--filter", default=None, help="regex on stream names")
    ap.add_argument("--json", default=None)
    ap.add_argument("--ref-json", default=None, help="another run's json; report md5 agreement with it")
    ap.add_argument("--no-sei", action="store_true")
    ap.add_argument("--self-test", action="store_true")
    a = ap.parse_args()

    names = sorted(f[:-4] for f in os.listdir(a.vectors) if f.endswith(".bit")
                   and os.path.exists(os.path.join(a.vectors, f[:-4] + ".yuv.md5")))
    if a.filter:
        names = [n for n in names if re.search(a.filter, n)]
    # Scratch YUVs are hundreds of MB; keep them beside the vectors (on the big
    # drive), never in the system temp dir -- C: filling mid-run turned a whole
    # table into "fail(size)" once.
    scratch_root = os.path.join(a.vectors, ".tmp")
    os.makedirs(scratch_root, exist_ok=True)
    tmp = tempfile.mkdtemp(prefix="hevc_conform_", dir=scratch_root)

    if a.self_test:
        good = next((n for n in names if n.startswith("IPRED_A")), names[0])
        r = check_stream(good, a.vectors, a.decoder, tmp)
        bad_dir = os.path.join(tmp, "bad"); os.makedirs(bad_dir, exist_ok=True)
        data = bytearray(open(os.path.join(a.vectors, good + ".bit"), "rb").read())
        mid = len(data) // 2
        for k in range(64):
            data[mid + k] ^= 0x5A
        open(os.path.join(bad_dir, good + ".bit"), "wb").write(data)
        shutil.copy(os.path.join(a.vectors, good + ".yuv.md5"), os.path.join(bad_dir, good + ".yuv.md5"))
        rb = check_stream(good, bad_dir, a.decoder, tmp)
        ok = r["md5"] == "pass" and rb["md5"] != "pass"
        print(f"self-test [{a.decoder}] good={r['md5']}/{r['sei']} corrupted={rb['md5']}/{rb['sei']} -> {'OK' if ok else 'BROKEN'}")
        shutil.rmtree(tmp, ignore_errors=True)
        sys.exit(0 if ok else 1)

    ref = {}
    if a.ref_json:
        ref = {r["name"]: r for r in json.load(open(a.ref_json))["results"]}

    results = []
    print(f"decoder: {a.decoder}   streams: {len(names)}")
    print(f"{'stream':38} {'class':9} {'md5':6} {'sei':18} {'frames':>6} {'secs':>7}  note")
    for n in names:
        r = check_stream(n, a.vectors, a.decoder, tmp, want_sei=not a.no_sei)
        note = ""
        if ref and n in ref and ref[n].get("out_md5"):
            r["vs_ref"] = "same" if r["out_md5"] == ref[n]["out_md5"] else "differs"
            if r["md5"] != "pass":
                note = "matches ref output" if r["vs_ref"] == "same" else ("ref also fails" if ref[n]["md5"] != "pass" else "")
        if r["md5"] != "pass" and r["stderr"]:
            note = (note + " | " if note else "") + r["stderr"].splitlines()[-1][:80]
        results.append(r)
        print(f"{n:38} {r['class']:9} {r['md5']:6} {r['sei']:18} {str(r['frames']):>6} {r['secs']:7.2f}  {note}")
        sys.stdout.flush()

    # roll-up
    by = {}
    for r in results:
        d = by.setdefault(r["class"], [0, 0])
        d[1] += 1
        d[0] += r["md5"] == "pass"
    total = sum(1 for r in results if r["md5"] == "pass")
    print("\nper class (md5 pass/total):")
    print("  " + "  ".join(f"{c} {p}/{t}" for c, (p, t) in sorted(by.items())))
    sei_p = sum(1 for r in results if r["sei"].startswith("pass"))
    sei_t = sum(1 for r in results if r["sei"].startswith(("pass", "fail")))
    print(f"TOTAL md5 {total}/{len(results)} ({100.0 * total / max(1, len(results)):.1f}%)   sei {sei_p}/{sei_t}")
    if a.json:
        os.makedirs(os.path.dirname(a.json) or ".", exist_ok=True)
        json.dump({"decoder": a.decoder, "results": results, "total": total, "n": len(results)},
                  open(a.json, "w"), indent=1)
    shutil.rmtree(tmp, ignore_errors=True)


if __name__ == "__main__":
    main()
