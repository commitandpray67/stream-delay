"""Draws the stream-delay icons (no image libraries needed).

Writes src-tauri/icons/app-source.png (1024x1024, fed to `tauri icon`) and
the tray icons tray-{live,delayed,busy,offline}.png (64x64).
"""
import math
import struct
import zlib
from pathlib import Path

OUT = Path(__file__).parent / "src-tauri" / "icons"


def png(path, size, pixel):
    rows = []
    for y in range(size):
        row = bytearray([0])
        for x in range(size):
            row += bytes(pixel(x + 0.5, y + 0.5, size))
        rows.append(bytes(row))
    raw = b"".join(rows)

    def chunk(tag, data):
        c = struct.pack(">I", len(data)) + tag + data
        return c + struct.pack(">I", zlib.crc32(tag + data) & 0xFFFFFFFF)

    data = b"\x89PNG\r\n\x1a\n"
    data += chunk(b"IHDR", struct.pack(">IIBBBBB", size, size, 8, 6, 0, 0, 0))
    data += chunk(b"IDAT", zlib.compress(raw, 9))
    data += chunk(b"IEND", b"")
    path.write_bytes(data)


def clamp01(v):
    return max(0.0, min(1.0, v))


def seg_dist(px, py, ax, ay, bx, by):
    dx, dy = bx - ax, by - ay
    t = clamp01(((px - ax) * dx + (py - ay) * dy) / (dx * dx + dy * dy))
    return math.hypot(px - (ax + t * dx), py - (ay + t * dy))


def clock(color):
    """A filled circle with a partial ring and clock hands, anti-aliased."""

    def pixel(x, y, size):
        s = size / 64.0
        cx = cy = size / 2
        r = math.hypot(x - cx, y - cy)
        disc = clamp01(30 * s - r + 0.5)
        if disc <= 0:
            return (0, 0, 0, 0)
        # White ring (270 degrees) and hands.
        ang = math.degrees(math.atan2(y - cy, x - cx))  # 0 = east, 90 = south
        ring_r, ring_w = 18 * s, 3 * s
        on_arc = not (-90 < ang < 0)  # gap in the top-right quadrant
        ring = clamp01(ring_w - abs(r - ring_r) + 0.5) if on_arc else 0.0
        hand1 = clamp01(2.5 * s - seg_dist(x, y, cx, cy, cx, cy - 10 * s) + 0.5)
        hand2 = clamp01(2.5 * s - seg_dist(x, y, cx, cy, cx + 8 * s, cy + 5 * s) + 0.5)
        white = max(ring, hand1, hand2)
        rgb = [round(c * (1 - white) + 255 * white) for c in color]
        return (*rgb, round(255 * disc))

    return pixel


OUT.mkdir(parents=True, exist_ok=True)
png(OUT / "app-source.png", 1024, clock((0x91, 0x47, 0xFF)))
for name, color in {
    "live": (0x00, 0xB0, 0x5E),
    "delayed": (0xF0, 0x9A, 0x10),
    "busy": (0x3E, 0xA6, 0xFF),
    "offline": (0x80, 0x80, 0x8A),
}.items():
    png(OUT / f"tray-{name}.png", 64, clock(color))
print("icons written to", OUT)
