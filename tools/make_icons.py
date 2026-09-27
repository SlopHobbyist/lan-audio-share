#!/usr/bin/env python3
"""Build the platform icon files from the master artwork.

    python3 tools/make_icons.py

Reads assets/lanaudio.png (2048x2048 RGBA, macOS-style, shadow already baked in)
and writes:

    assets/AppIcon.icns    macOS bundle icon
    assets/AppIcon.ico     Windows executable + taskbar icon
    assets/window-icon.png window icon loaded at runtime

The outputs are committed, so building either platform needs nothing but a
toolchain. Re-run this only when the artwork changes.

Requires Pillow (`pip install Pillow`).
"""

import struct
import sys
from io import BytesIO
from pathlib import Path

try:
    from PIL import Image, ImageChops
except ImportError:
    sys.exit("error: Pillow is required. Install it with: pip install Pillow")

ROOT = Path(__file__).resolve().parent.parent
ASSETS = ROOT / "assets"
SOURCE = ASSETS / "lanaudio.png"

# macOS wants every size the Dock and Finder might ask for; supplying them all
# means the system never has to rescale.
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

ICO_SIZES = [16, 24, 32, 48, 64, 128, 256]

# Size of the icon handed to the window system at runtime.
WINDOW_ICON_SIZE = 256


def load_source():
    image = Image.open(SOURCE).convert("RGBA")
    if image.width != image.height:
        sys.exit(f"error: {SOURCE.name} must be square, got {image.width}x{image.height}")
    return image


def build_unpremultiply_table():
    """table[a][v] recovers a straight-alpha channel value from a premultiplied one."""
    table = [bytes(256)]  # alpha 0: nothing to recover
    for a in range(1, 256):
        table.append(bytes(min(255, (v * 255 + a // 2) // a) for v in range(256)))
    return table


UNPREMULTIPLY = build_unpremultiply_table()


def resize(image, size):
    """Downscale with premultiplied alpha.

    The artwork stores white in its fully transparent pixels. Resizing straight
    RGBA would average that white into the soft shadow and leave a pale halo, so
    the colour channels are weighted by alpha first and restored afterwards.
    """
    if image.width == size:
        return image.copy()

    r, g, b, a = image.split()
    # ImageChops.multiply computes (band * alpha) / 255, which is exactly the
    # premultiplication we want, and does it in C rather than per pixel.
    premultiplied = Image.merge(
        "RGBA",
        (
            ImageChops.multiply(r, a),
            ImageChops.multiply(g, a),
            ImageChops.multiply(b, a),
            a,
        ),
    )

    small = premultiplied.resize((size, size), Image.LANCZOS)

    pr, pg, pb, pa = small.split()
    alpha_bytes = pa.tobytes()

    def restore(band):
        data = band.tobytes()
        out = bytearray(len(data))
        for i, value in enumerate(data):
            out[i] = UNPREMULTIPLY[alpha_bytes[i]][value]
        return Image.frombytes("L", small.size, bytes(out))

    return Image.merge("RGBA", (restore(pr), restore(pg), restore(pb), pa))


def to_png(image):
    buffer = BytesIO()
    image.save(buffer, format="PNG", optimize=True)
    return buffer.getvalue()


def write_icns(renders, path):
    entries = b"".join(
        tag + struct.pack(">I", len(renders[size]) + 8) + renders[size]
        for tag, size in ICNS_TYPES
    )
    data = b"icns" + struct.pack(">I", len(entries) + 8) + entries
    path.write_bytes(data)
    return len(data)


def write_ico(renders, path):
    """Write a PNG-compressed .ico.

    Every entry is a PNG rather than the older BMP layout. Windows has accepted
    that since Vista, and it keeps the 256px entry from bloating the file.
    """
    count = len(ICO_SIZES)
    offset = 6 + 16 * count
    directory = b""
    payload = b""
    for size in ICO_SIZES:
        png = renders[size]
        # 256 is encoded as 0 in the one-byte width/height fields.
        dimension = 0 if size == 256 else size
        directory += struct.pack(
            "<BBBBHHII", dimension, dimension, 0, 0, 1, 32, len(png), offset
        )
        payload += png
        offset += len(png)

    path.write_bytes(struct.pack("<HHH", 0, 1, count) + directory + payload)
    return path.stat().st_size


def main():
    if not SOURCE.exists():
        sys.exit(f"error: missing {SOURCE.relative_to(ROOT)}")

    source = load_source()
    print(f"source: {SOURCE.name} {source.width}x{source.height} {source.mode}")

    sizes = sorted({size for _, size in ICNS_TYPES} | set(ICO_SIZES) | {WINDOW_ICON_SIZE})
    images = {}
    renders = {}
    for size in sizes:
        print(f"  rendering {size}x{size}")
        images[size] = resize(source, size)
        renders[size] = to_png(images[size])

    icns = ASSETS / "AppIcon.icns"
    print(f"wrote {icns.relative_to(ROOT)} ({write_icns(renders, icns):,} bytes)")

    ico = ASSETS / "AppIcon.ico"
    print(f"wrote {ico.relative_to(ROOT)} ({write_ico(renders, ico):,} bytes)")

    window = ASSETS / "window-icon.png"
    window.write_bytes(renders[WINDOW_ICON_SIZE])
    print(f"wrote {window.relative_to(ROOT)} ({len(renders[WINDOW_ICON_SIZE]):,} bytes)")


if __name__ == "__main__":
    main()
