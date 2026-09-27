#!/usr/bin/env python3
"""Generate the macOS app icon (assets/AppIcon.icns) and a reference PNG.

Run this only when changing the icon design; the generated files are committed so
that building the .app needs nothing but a shell.

    python3 tools/make_icon.py

Uses only the standard library. Each icon size is drawn at its native resolution
rather than downscaled from one big image, which is what keeps the 16px version
from turning to mush. Shapes are anti-aliased from a signed distance field.
"""

import math
import struct
import zlib
from pathlib import Path

ROOT = Path(__file__).resolve().parent.parent
ASSETS = ROOT / "assets"

# Squircle gradient, top to bottom. Saturated enough to stay legible against
# both light and dark Docks.
TOP = (0x63, 0x66, 0xF1)
BOTTOM = (0x06, 0xB6, 0xD4)
BAR = (0xFF, 0xFF, 0xFF)

# Relative bar heights, as a fraction of the squircle's inner height. Symmetric,
# so it reads as a waveform rather than a random bar chart.
BARS_FULL = [0.34, 0.62, 1.0, 0.62, 0.34]
BARS_COARSE = [0.5, 1.0, 0.5]


def rounded_rect_sdf(px, py, cx, cy, hx, hy, r):
    """Signed distance from (px, py) to a rounded rectangle. Negative inside."""
    qx = abs(px - cx) - (hx - r)
    qy = abs(py - cy) - (hy - r)
    outside = math.hypot(max(qx, 0.0), max(qy, 0.0))
    inside = min(max(qx, qy), 0.0)
    return outside + inside - r


def coverage(distance):
    """Anti-aliased coverage for a signed distance, over roughly one pixel."""
    return min(max(0.5 - distance, 0.0), 1.0)


def blend(dst, src, alpha):
    """Composite a solid colour onto a straight-alpha RGBA pixel."""
    dr, dg, db, da = dst
    sr, sg, sb = src
    out_a = alpha + da * (1.0 - alpha)
    if out_a <= 0.0:
        return (0, 0, 0, 0.0)
    # Un-premultiplied compositing, so edges do not darken against transparency.
    r = (sr * alpha + dr * da * (1.0 - alpha)) / out_a
    g = (sg * alpha + dg * da * (1.0 - alpha)) / out_a
    b = (sb * alpha + db * da * (1.0 - alpha)) / out_a
    return (r, g, b, out_a)


def render(size):
    """Draw the icon at `size` x `size`, returning straight-alpha RGBA bytes."""
    # macOS icons sit inside a small transparent margin rather than bleeding to
    # the edge, so they line up with every other icon in the Dock.
    margin = size * 0.085
    inner = size - 2 * margin
    cx = cy = size / 2.0
    half = inner / 2.0
    radius = inner * 0.225  # close to the macOS superellipse at these sizes

    pixels = [[(0, 0, 0, 0.0)] * size for _ in range(size)]

    # Background squircle, with a vertical gradient.
    top_edge = margin
    for y in range(size):
        py = y + 0.5
        t = min(max((py - top_edge) / inner, 0.0), 1.0)
        colour = tuple(TOP[i] + (BOTTOM[i] - TOP[i]) * t for i in range(3))
        row = pixels[y]
        for x in range(size):
            px = x + 0.5
            a = coverage(rounded_rect_sdf(px, py, cx, cy, half, half, radius))
            if a > 0.0:
                row[x] = blend(row[x], colour, a)

    # Equalizer bars. Fewer of them at small sizes, where five would smear into
    # an unreadable grey band.
    heights = BARS_COARSE if size <= 32 else BARS_FULL
    count = len(heights)
    span = inner * 0.60
    bar_w = span / (count * 2 - 1)
    gap = bar_w
    max_h = inner * 0.52
    start = cx - span / 2.0

    for i, rel in enumerate(heights):
        bx = start + i * (bar_w + gap) + bar_w / 2.0
        bh = max(max_h * rel, bar_w)
        bhx = bar_w / 2.0
        bhy = bh / 2.0
        br = min(bhx, bhy)
        # Only touch the pixels this bar can actually cover.
        x0 = max(int(bx - bhx - 2), 0)
        x1 = min(int(math.ceil(bx + bhx + 2)), size)
        y0 = max(int(cy - bhy - 2), 0)
        y1 = min(int(math.ceil(cy + bhy + 2)), size)
        for y in range(y0, y1):
            py = y + 0.5
            row = pixels[y]
            for x in range(x0, x1):
                px = x + 0.5
                a = coverage(rounded_rect_sdf(px, py, bx, cy, bhx, bhy, br))
                if a > 0.0:
                    row[x] = blend(row[x], BAR, a)

    out = bytearray()
    for y in range(size):
        for r, g, b, a in pixels[y]:
            out += bytes(
                (
                    int(round(min(max(r, 0.0), 255.0))),
                    int(round(min(max(g, 0.0), 255.0))),
                    int(round(min(max(b, 0.0), 255.0))),
                    int(round(min(max(a, 0.0), 1.0) * 255.0)),
                )
            )
    return bytes(out)


def png_chunk(tag, data):
    body = tag + data
    return struct.pack(">I", len(data)) + body + struct.pack(">I", zlib.crc32(body))


def encode_png(size, rgba):
    """Minimal RGBA PNG encoder (colour type 6, filter 0)."""
    raw = bytearray()
    stride = size * 4
    for y in range(size):
        raw.append(0)  # filter: none
        raw += rgba[y * stride : (y + 1) * stride]

    header = struct.pack(">IIBBBBB", size, size, 8, 6, 0, 0, 0)
    return (
        b"\x89PNG\r\n\x1a\n"
        + png_chunk(b"IHDR", header)
        + png_chunk(b"IDAT", zlib.compress(bytes(raw), 9))
        + png_chunk(b"IEND", b"")
    )


# OSType -> pixel size. Both the plain and @2x families are included so macOS
# always has an exact match and never has to rescale.
ICNS_TYPES = [
    (b"icp4", 16),
    (b"icp5", 32),
    (b"icp6", 64),
    (b"ic07", 128),
    (b"ic08", 256),
    (b"ic09", 512),
    (b"ic10", 1024),
    (b"ic11", 32),
    (b"ic12", 64),
    (b"ic13", 256),
    (b"ic14", 512),
]


def main():
    ASSETS.mkdir(exist_ok=True)

    sizes = sorted({size for _, size in ICNS_TYPES})
    pngs = {}
    for size in sizes:
        print(f"  rendering {size}x{size}")
        pngs[size] = encode_png(size, render(size))

    entries = b"".join(
        tag + struct.pack(">I", len(pngs[size]) + 8) + pngs[size]
        for tag, size in ICNS_TYPES
    )
    icns = b"icns" + struct.pack(">I", len(entries) + 8) + entries

    icns_path = ASSETS / "AppIcon.icns"
    icns_path.write_bytes(icns)
    print(f"wrote {icns_path.relative_to(ROOT)} ({len(icns):,} bytes)")

    # A plain PNG too, for READMEs and anything that cannot read .icns.
    png_path = ASSETS / "icon.png"
    png_path.write_bytes(pngs[1024])
    print(f"wrote {png_path.relative_to(ROOT)} ({len(pngs[1024]):,} bytes)")


if __name__ == "__main__":
    main()
