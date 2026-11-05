#!/usr/bin/env python3
# Makes the desktop's animations (player.c plays them; Epicardium's decoder,
# ../epicardium-anim.patch, decodes them on core 0) as .rla files: 96 x 42
# pixels, a 256-colour RGB565 palette, each frame PackBits-compressed (the
# format: epicardium.h, "Animation decoder").
#
#   python3 tools/a64-site/card10/ctk/gen-anims.py OUT-DIR
#
# Writes plasma.rla (a plasma in the desktop's colours), cube.rla (a
# shaded cube turning) and chip.rla (the RetroCPUGeek avatar's chip, its
# owner's, bouncing over a circuit board with signals running along its
# traces). Needs Pillow; ../build.sh runs it and puts them in /apps/ctk/anims.
import math
import os
import struct
import sys

from PIL import Image, ImageDraw

HERE = os.path.dirname(os.path.abspath(__file__))
W, H = 96, 42


def rgb565(c):
    r, g, b = c[:3]
    return ((r >> 3) << 11) | ((g >> 2) << 5) | (b >> 3)


def packbits(data):
    out = bytearray()
    i, n = 0, len(data)
    while i < n:
        run = 1
        while i + run < n and run < 129 and data[i + run] == data[i]:
            run += 1
        if run >= 2:
            out += bytes((run + 126, data[i]))
            i += run
            continue
        j = i
        while j < n and j - i < 128 and not (j + 1 < n and data[j + 1] == data[j]):
            j += 1
        out += bytes((j - i - 1,)) + bytes(data[i:j])
        i = j
    return bytes(out)


def unpackbits(enc, count):
    out, i = [], 0
    while i < len(enc) and len(out) < count:
        c = enc[i]
        i += 1
        if c < 128:
            out += enc[i:i + c + 1]
            i += c + 1
        else:
            out += [enc[i]] * (c - 126)
            i += 1
    return bytes(out)


def write(path, palette, frames, fps):
    """palette: 256 (r, g, b); frames: W*H index bytes each"""
    with open(path, "wb") as f:
        f.write(b"RLA1" + struct.pack("<HHHHI", W, H, len(frames), fps, 0))
        f.write(struct.pack("<256H", *[rgb565(c) for c in palette]))
        total = 0
        for fr in frames:
            enc = packbits(fr)
            assert unpackbits(enc, W * H) == fr
            f.write(struct.pack("<H", len(enc)) + enc)
            total += len(enc)
    print(f"{os.path.basename(path)}: {len(frames)} frames, {total} bytes of frames "
          f"({total * 100 // (len(frames) * W * H)}% of their pixels)")


def quantized(images):
    """One palette for all the frames (so the player can keep one)"""
    sheet = Image.new("RGB", (W, H * len(images)))
    for i, im in enumerate(images):
        sheet.paste(im, (0, i * H))
    pal = sheet.quantize(256, method=Image.Quantize.MEDIANCUT, dither=Image.Dither.NONE)
    flat = pal.getpalette()[:768]
    flat += [0] * (768 - len(flat))
    palette = [tuple(flat[i:i + 3]) for i in range(0, 768, 3)]
    frames = [bytes(im.quantize(palette=pal, dither=Image.Dither.NONE).tobytes()) for im in images]
    return palette, frames


def plasma(n=48):
    # the desktop's colours, round a circle: purple, gold, cream, green
    stops = [(92, 48, 104), (214, 168, 64), (248, 233, 190), (18, 94, 52), (92, 48, 104)]
    palette = []
    for i in range(256):
        t = i / 256 * (len(stops) - 1)
        a, b = stops[int(t)], stops[int(t) + 1]
        f = t - int(t)
        palette.append(tuple(int(a[k] + (b[k] - a[k]) * f) for k in range(3)))
    frames = []
    for k in range(n):
        p = 2 * math.pi * k / n
        fr = bytearray()
        for y in range(H):
            for x in range(W):
                v = (math.sin(x / 9 + p) + math.sin((y / 6 + p) * 1.3) +
                     math.sin((x + y) / 14 - 2 * p) +
                     math.sin(math.hypot(x - 48 - 20 * math.cos(p), y - 21 - 8 * math.sin(p)) / 5))
                fr.append(int((v + 4) / 8 * 255 + k * 256 / n) & 255)
        frames.append(bytes(fr))
    return palette, frames


def cube(n=40):
    # a quarter turn about two axes: the cube looks the same after it, so
    # it loops
    verts = [(x, y, z) for x in (-1, 1) for y in (-1, 1) for z in (-1, 1)]
    faces = [(0, 1, 3, 2), (4, 6, 7, 5), (0, 4, 5, 1), (2, 3, 7, 6), (0, 2, 6, 4), (1, 5, 7, 3)]
    # (one colour: the quarter turns swap the faces round, so they look alike)
    colours = [(214, 168, 64)] * 6
    light = (-0.45, -0.55, -0.7)
    images = []
    for k in range(n):
        a = math.pi / 2 * k / n
        b = math.pi / 2 * k / n + 0.5
        ca, sa, cb, sb = math.cos(a), math.sin(a), math.cos(b), math.sin(b)

        def rot(v):
            x, y, z = v
            x, z = x * ca + z * sa, -x * sa + z * ca
            y, z = y * cb - z * sb, y * sb + z * cb
            return x, y, z
        r = [rot(v) for v in verts]
        im = Image.new("RGB", (W, H), (12, 62, 34))
        d = ImageDraw.Draw(im)
        for x in range(0, W, 8):
            d.line((x, 0, x, H), fill=(18, 80, 46))
        for y in range(0, H, 8):
            d.line((0, y, W, y), fill=(18, 80, 46))

        def proj(v):
            s = 52 / (v[2] + 5)
            return (48 + v[0] * s, 21 + v[1] * s)
        drawn = []
        for fi, f in enumerate(faces):
            p = [r[i] for i in f]
            # (its outward normal: from the cube's centre to the face's)
            nx, ny, nz = (sum(q[i] for q in p) / 4 for i in range(3))
            if nz < 0:
                shade = 0.3 + 0.7 * max(0, nx * light[0] + ny * light[1] + nz * light[2])
                drawn.append((nz, [proj(q) for q in p], tuple(int(c * shade) for c in colours[fi])))
        for _, pts, col in sorted(drawn, reverse=True):
            d.polygon(pts, fill=col, outline=(248, 233, 190))
        images.append(im)
    return quantized(images)


def chip(n=48):
    board = Image.new("RGB", (W, H), (18, 94, 52))
    d = ImageDraw.Draw(board)
    traces = [[(0, 8), (30, 8), (36, 2), (96, 2)], [(0, 21), (96, 21)], [(0, 34), (40, 34), (46, 40), (96, 40)],
              [(60, 2), (60, 14), (96, 14)], [(20, 21), (20, 34)], [(76, 21), (76, 30), (96, 30)]]
    for t in traces:
        d.line(t, fill=(150, 118, 48))
    avatar = Image.open(os.path.join(HERE, "retrocpugeek.png")).convert("RGBA")
    c = avatar.crop((92, 92, 308, 308))
    bg = Image.new("RGBA", c.size, (92, 48, 104, 255))
    bg.alpha_composite(c)
    chip_im = bg.convert("RGB").resize((22, 22), Image.LANCZOS)

    def along(t, f):
        # a point a fraction f of the way along trace t
        segs = list(zip(t, t[1:]))
        lens = [math.dist(a, b) for a, b in segs]
        s = f * sum(lens)
        for (a, b), ln in zip(segs, lens):
            if s <= ln:
                return (a[0] + (b[0] - a[0]) * s / ln, a[1] + (b[1] - a[1]) * s / ln)
            s -= ln
        return t[-1]
    images = []
    for k in range(n):
        im = board.copy()
        d = ImageDraw.Draw(im)
        for i, t in enumerate(traces):
            for j in range(2):
                x, y = along(t, ((k / n) + i * 0.17 + j * 0.5) % 1)
                d.rectangle((x - 1, y - 1, x + 1, y + 1), fill=(255, 236, 140))
        # a bounce: across and back, up and down twice
        p = k / n
        x = int(4 + (W - 30) * (1 - abs(1 - 2 * p)))
        y = int(H - 23 - abs(math.sin(2 * math.pi * p)) * 16)
        im.paste(chip_im, (x, y))
        images.append(im)
    return quantized(images)


def main():
    out = sys.argv[1] if len(sys.argv) > 1 else "."
    os.makedirs(out, exist_ok=True)
    write(os.path.join(out, "plasma.rla"), *plasma(), 15)
    write(os.path.join(out, "cube.rla"), *cube(), 15)
    write(os.path.join(out, "chip.rla"), *chip(), 12)


main()
