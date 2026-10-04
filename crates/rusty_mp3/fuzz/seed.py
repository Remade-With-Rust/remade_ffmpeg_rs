"""Regenerate the fuzz seed corpora from real streams (reproducible; small files).

    python seed.py <mp3spectrum dir> <minimp3 vectors dir> <wav file>

Seeds are excerpts -- a few KB -- so libFuzzer starts from real frame syntax
(every MPEG version, mono/stereo, LAME/ours/shine, mixed blocks, both intensity
stereo forms) instead of rediscovering the sync word.
"""
import os
import struct
import sys

spectrum, vectors, wav = sys.argv[1:4]
here = os.path.dirname(os.path.abspath(__file__))


def put(target, name, data):
    d = os.path.join(here, "corpus", target)
    os.makedirs(d, exist_ok=True)
    open(os.path.join(d, name), "wb").write(data)


streams = []
for enc in ("lame", "ours", "shine"):
    for name in ("guitar_44100_st_cbr128.mp3", "guitar_22050_mo_cbr64.mp3",
                 "guitar_11025_st_cbr32.mp3", "piano_48000_mo_cbr192.mp3"):
        p = os.path.join(spectrum, enc, name)
        if os.path.exists(p) and os.path.getsize(p) > 0:
            streams.append((f"{enc}_{name[:-4]}", open(p, "rb").read()[:3072]))
for name in ("l3-compl", "l3-si_block", "l3-he_mode", "l3-test45", "l3-test46", "l3-he_48khz"):
    p = os.path.join(vectors, name + ".bit")
    if os.path.exists(p):
        streams.append((name, open(p, "rb").read()[:4096]))

for i, (name, b) in enumerate(streams):
    put("decode", name, bytes([i * 37 % 256]) + b)
    put("pipelined", name, b)


def frames(b, limit=4):
    """Split a stream into (header, side info, main data) by walking real syncs."""
    out, pos = [], 0
    while pos + 4 <= len(b) and len(out) < limit:
        if b[pos] != 0xFF or b[pos + 1] & 0xE0 != 0xE0:
            pos += 1
            continue
        h = b[pos:pos + 4]
        ver = (h[1] >> 3) & 3
        br_i, sr_i = h[2] >> 4, (h[2] >> 2) & 3
        if br_i in (0, 15) or sr_i == 3 or ver == 1 or (h[1] >> 1) & 3 != 1:
            pos += 1
            continue
        v1 = ver == 3
        br = ([0, 32, 40, 48, 56, 64, 80, 96, 112, 128, 160, 192, 224, 256, 320] if v1 else
              [0, 8, 16, 24, 32, 40, 48, 56, 64, 80, 96, 112, 128, 144, 160])[br_i]
        sr = [44100, 48000, 32000][sr_i] // (1 if v1 else (2 if ver == 2 else 4))
        size = (144 if v1 else 72) * br * 1000 // sr + ((h[2] >> 1) & 1)
        mono = (h[3] >> 6) == 3
        si = (17 if mono else 32) if v1 else (9 if mono else 17)
        crc = 0 if h[1] & 1 else 2
        start = pos + 4 + crc
        out.append((h, b[start:start + si], b[start + si:pos + size]))
        pos += size
    return out


for name, b in streams:
    rec = b""
    for h, si, md in frames(b):
        md = md[:255 * 7]
        rec += h + bytes([len(si), (len(md) + 6) // 7]) + si + md
    if rec:
        put("decode_frame", name, rec)

pcm = open(wav, "rb").read()[44:44 + 8192]
for i, (rate, ch, kbps, mode) in enumerate([(7, 2, 4, 0x01), (6, 1, 5, 0x41), (4, 2, 3, 0x41),
                                             (1, 1, 1, 0x41), (8, 2, 6, 0x81), (2, 3, 0, 0x01)]):
    put("encode", f"seed{i}", bytes([rate, (mode & 0xC0) | ch, kbps, mode & 0x3F]) + pcm)
print({t: len(os.listdir(os.path.join(here, "corpus", t))) for t in os.listdir(os.path.join(here, "corpus"))})
