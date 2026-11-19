#!/usr/bin/env python3
"""Tile real 1080p video frames into one large RGB PPM (real-content source).

usage: mosaic.py out.ppm cols rows frame_step clip1.y4m [clip2.y4m ...]
Takes frames 0, step, 2*step, ... from each clip in turn until cols*rows
tiles are filled. 4:2:0 8-bit y4m only; BT.601 limited-range to RGB.
"""
import sys


def frames(path, step, count):
    with open(path, "rb") as f:
        header = f.readline()
        parts = header.split()
        w = int([p for p in parts if p.startswith(b"W")][0][1:])
        h = int([p for p in parts if p.startswith(b"H")][0][1:])
        fsize = w * h * 3 // 2
        base = len(header)
        for k in range(count):
            f.seek(base + (k * step) * (6 + fsize) + 6)
            data = f.read(fsize)
            if len(data) < fsize:
                return
            yield w, h, data


def to_rgb(w, h, d):
    ys, us, vs = d[: w * h], d[w * h : w * h * 5 // 4], d[w * h * 5 // 4 :]
    out = bytearray(w * h * 3)
    cw = w // 2

    def clip(v):
        return 0 if v < 0 else 255 if v > 255 else v

    for y in range(h):
        row_u = (y // 2) * cw
        for x in range(w):
            c = (ys[y * w + x] - 16) * 298
            u = us[row_u + x // 2] - 128
            v = vs[row_u + x // 2] - 128
            i = (y * w + x) * 3
            out[i] = clip((c + 409 * v + 128) >> 8)
            out[i + 1] = clip((c - 100 * u - 208 * v + 128) >> 8)
            out[i + 2] = clip((c + 516 * u + 128) >> 8)
    return out


def main():
    out, cols, rows, step = sys.argv[1], int(sys.argv[2]), int(sys.argv[3]), int(sys.argv[4])
    clips = sys.argv[5:]
    need = cols * rows
    per_clip = -(-need // len(clips))
    tiles = []
    for c in clips:
        for w, h, d in frames(c, step, per_clip):
            tiles.append((w, h, to_rgb(w, h, d)))
            if len(tiles) == need:
                break
    w, h = tiles[0][0], tiles[0][1]
    W, H = w * cols, h * rows
    img = bytearray(W * H * 3)
    for t, (_, _, rgb) in enumerate(tiles[:need]):
        ox, oy = (t % cols) * w, (t // cols) * h
        for y in range(h):
            img[((oy + y) * W + ox) * 3 : ((oy + y) * W + ox + w) * 3] = rgb[y * w * 3 : (y + 1) * w * 3]
    with open(out, "wb") as f:
        f.write(b"P6\n%d %d\n255\n" % (W, H))
        f.write(img)
    print(f"{out}: {W}x{H} from {need} frames")


main()
