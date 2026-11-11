"""Renders assets/swiftfind.ico (a magnifying glass) at the sizes Windows uses.

Same drawing as the old runtime icon in tray.rs, scaled per size.
Run: python3 scripts/make_icon.py
"""
import math
import struct
import zlib
from pathlib import Path

SIZES = [16, 20, 24, 32, 40, 48, 64, 128, 256]
RING = (0x1F, 0x6F, 0xD6)
GLASS = (0x8C, 0xC4, 0xFF)


def render(n):
    s = n / 32.0
    cx = cy = 13.0 * s
    r = 8.5 * s
    ring_w = max(2.0 * s, 1.1)  # half-width of the ring stroke
    handle_w = max(3.0 * s, 1.4)
    ax = ay = 19.5 * s
    bx = by = 28.0 * s
    rows = []
    for y in range(n):
        row = bytearray([0])  # PNG filter: none
        for x in range(n):
            fx, fy = x + 0.5, y + 0.5
            d = math.hypot(fx - cx, fy - cy)
            ring = min(max(ring_w - abs(d - r), 0.0), 1.0)
            glass = min(max(r - ring_w / 2 - d, 0.0), 1.0)
            t = ((fx - ax) * (bx - ax) + (fy - ay) * (by - ay)) / ((bx - ax) ** 2 + (by - ay) ** 2)
            t = min(max(t, 0.0), 1.0)
            dh = math.hypot(fx - (ax + t * (bx - ax)), fy - (ay + t * (by - ay)))
            handle = min(max(handle_w - dh, 0.0), 1.0)
            solid = max(ring, handle)
            if solid > 0:
                a, rgb = solid, RING
            else:
                a, rgb = glass * 0.35, GLASS
            row += bytes(rgb) + bytes([round(a * 255)])
        rows.append(bytes(row))
    return png(n, b"".join(rows))


def png(n, raw):
    def chunk(kind, data):
        return struct.pack(">I", len(data)) + kind + data + struct.pack(">I", zlib.crc32(kind + data))

    return (
        b"\x89PNG\r\n\x1a\n"
        + chunk(b"IHDR", struct.pack(">IIBBBBB", n, n, 8, 6, 0, 0, 0))
        + chunk(b"IDAT", zlib.compress(raw, 9))
        + chunk(b"IEND", b"")
    )


def main():
    images = [render(n) for n in SIZES]
    out = struct.pack("<HHH", 0, 1, len(images))
    offset = 6 + 16 * len(images)
    for n, data in zip(SIZES, images):
        out += struct.pack("<BBBBHHII", n % 256, n % 256, 0, 0, 1, 32, len(data), offset)
        offset += len(data)
    out += b"".join(images)
    path = Path(__file__).resolve().parent.parent / "assets" / "swiftfind.ico"
    path.write_bytes(out)
    print(f"wrote {path} ({len(out)} bytes)")


if __name__ == "__main__":
    main()
