#!/usr/bin/env python3
# Copyright 2026 Nick Volgas
# SPDX-License-Identifier: Apache-2.0
"""Draws the app icon to assets/icon/icon.png (1024x1024, standard library only).

    python3 scripts/draw_icon.py

The design: a rounded square in the app's accent greens with a white "broadcast" mark, a dot
with signal arcs on both sides, for publish and subscribe. It follows the macOS icon grid
(an 824 px body inside a 1024 px canvas, with a soft shadow).

To use your own artwork instead, replace assets/icon/icon.png with a 1024x1024 PNG and run
`python3 scripts/make_icons.py` to regenerate the Windows icon. The macOS icon is built from
icon.png when packaging.
"""
import math
import struct
import zlib
from pathlib import Path

ROOT = Path(__file__).resolve().parent.parent
OUT = ROOT / "assets" / "icon" / "icon.png"
SIZE = 1024

# Body: the macOS grid's 824 px rounded square.
BODY_MIN, BODY_MAX, BODY_RADIUS = 100.0, 924.0, 185.0
TOP_LEFT = (0x3B, 0xE3, 0xB0)  # lighter accent
BOTTOM_RIGHT = (0x05, 0x69, 0x4A)  # deeper accent

# Mark, centered: a dot and two arcs to each side.
CENTER = (512.0, 512.0)
DOT_RADIUS = 64.0
ARC_RADII = (170.0, 280.0)
STROKE_HALF = 27.0
ARC_HALF_SPAN = math.radians(42)


def rounded_rect(px, py, lo, hi, r):
    c = (lo + hi) / 2
    h = (hi - lo) / 2 - r
    qx, qy = abs(px - c) - h, abs(py - c) - h
    outside = math.hypot(max(qx, 0.0), max(qy, 0.0))
    return outside + min(max(qx, qy), 0.0) - r


def arc(px, py, radius, direction):
    """Distance to an arc of `radius` around CENTER, centered on `direction` (0 = right,
    pi = left), with round caps."""
    dx, dy = px - CENTER[0], py - CENTER[1]
    theta = math.atan2(dy, dx) - direction
    theta = (theta + math.pi) % (2 * math.pi) - math.pi
    if abs(theta) <= ARC_HALF_SPAN:
        return abs(math.hypot(dx, dy) - radius) - STROKE_HALF
    end = direction + math.copysign(ARC_HALF_SPAN, theta)
    ex = CENTER[0] + radius * math.cos(end)
    ey = CENTER[1] + radius * math.sin(end)
    return math.hypot(px - ex, py - ey) - STROKE_HALF


def coverage(d):
    """Anti-aliased coverage for a signed distance in pixels."""
    return min(max(0.5 - d, 0.0), 1.0)


def over(dst, src, alpha):
    """Composites `src` color with `alpha` over `dst` (r, g, b, a), straight alpha."""
    r, g, b, a = dst
    out_a = alpha + a * (1 - alpha)
    if out_a == 0:
        return (0.0, 0.0, 0.0, 0.0)
    mix = lambda s, d: (s * alpha + d * a * (1 - alpha)) / out_a  # noqa: E731
    return (mix(src[0], r), mix(src[1], g), mix(src[2], b), out_a)


def pixel(x, y):
    px, py = x + 0.5, y + 0.5
    color = (0.0, 0.0, 0.0, 0.0)

    # Soft shadow under the body.
    shadow_d = rounded_rect(px, py - 14, BODY_MIN, BODY_MAX, BODY_RADIUS)
    shadow = 0.30 * min(max(1 - (shadow_d + 6) / 34, 0.0), 1.0)
    if shadow > 0:
        color = over(color, (0, 0, 0), shadow)

    body = coverage(rounded_rect(px, py, BODY_MIN, BODY_MAX, BODY_RADIUS))
    if body > 0:
        t = min(max(((px - BODY_MIN) + (py - BODY_MIN)) / (2 * (BODY_MAX - BODY_MIN)), 0.0), 1.0)
        fill = [TOP_LEFT[i] + (BOTTOM_RIGHT[i] - TOP_LEFT[i]) * t for i in range(3)]
        # A faint glassy highlight across the top.
        glow = max(0.0, 1 - (py - BODY_MIN) / 360) * 0.16
        fill = [c + (255 - c) * glow for c in fill]
        color = over(color, fill, body)

        d = math.hypot(px - CENTER[0], py - CENTER[1]) - DOT_RADIUS
        for radius in ARC_RADII:
            d = min(d, arc(px, py, radius, 0.0), arc(px, py, radius, math.pi))
        mark = coverage(d) * body
        if mark > 0:
            color = over(color, (255, 255, 255), mark)
    return color


def png(width, height, rows):
    def chunk(kind, data):
        body = kind + data
        return struct.pack(">I", len(data)) + body + struct.pack(">I", zlib.crc32(body))

    raw = b"".join(b"\x00" + bytes(row) for row in rows)
    return (
        b"\x89PNG\r\n\x1a\n"
        + chunk(b"IHDR", struct.pack(">IIBBBBB", width, height, 8, 6, 0, 0, 0))
        + chunk(b"IDAT", zlib.compress(raw, 9))
        + chunk(b"IEND", b"")
    )


def main() -> None:
    rows = []
    for y in range(SIZE):
        row = bytearray()
        for x in range(SIZE):
            r, g, b, a = pixel(x, y)
            row += bytes((round(r), round(g), round(b), round(a * 255)))
        rows.append(row)
    OUT.parent.mkdir(parents=True, exist_ok=True)
    OUT.write_bytes(png(SIZE, SIZE, rows))
    print(f"Wrote {OUT.relative_to(ROOT)}")


if __name__ == "__main__":
    main()
