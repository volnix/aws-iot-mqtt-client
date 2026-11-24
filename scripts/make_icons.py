#!/usr/bin/env python3
# Copyright 2026 Nick Volgas
# SPDX-License-Identifier: Apache-2.0
"""Builds the platform icon files from assets/icon/icon.png (1024x1024).

    python3 scripts/make_icons.py            # writes assets/icon/icon.ico (commit it)

The Windows icon (.ico, embedded in the .exe by build.rs) is cropped to the icon's body,
since Windows icons fill their square. Needs macOS (it uses `sips`).

The macOS icon (.icns) isn't committed: scripts/package.py builds it from icon.png with
`macos_icns()` below whenever it packages the app.
"""
import shutil
import struct
import subprocess
import sys
import tempfile
from pathlib import Path

ROOT = Path(__file__).resolve().parent.parent
SOURCE = ROOT / "assets" / "icon" / "icon.png"
ICO = ROOT / "assets" / "icon" / "icon.ico"
ICO_SIZES = [16, 24, 32, 48, 64, 128, 256]
# The rounded square inside the 1024 px macOS canvas (see scripts/draw_icon.py).
BODY = 824


def sips(*args: str) -> None:
    subprocess.run(["sips", *args], check=True, capture_output=True)


def resized(source: Path, size: int, out: Path) -> bytes:
    sips("-z", str(size), str(size), str(source), "--out", str(out))
    return out.read_bytes()


def write_ico(pngs: dict, out: Path) -> None:
    """An .ico with one PNG image per size (supported since Windows Vista)."""
    header = struct.pack("<HHH", 0, 1, len(pngs))
    offset = 6 + 16 * len(pngs)
    entries, data = b"", b""
    for size, blob in sorted(pngs.items()):
        dim = 0 if size >= 256 else size  # 0 means 256
        entries += struct.pack("<BBBBHHII", dim, dim, 0, 0, 1, 32, len(blob), offset)
        offset += len(blob)
        data += blob
    out.write_bytes(header + entries + data)


def macos_icns(source: Path, out: Path) -> None:
    """Builds AppIcon.icns with `iconutil` (macOS only)."""
    with tempfile.TemporaryDirectory() as tmp:
        iconset = Path(tmp) / "AppIcon.iconset"
        iconset.mkdir()
        for size in (16, 32, 128, 256, 512):
            resized(source, size, iconset / f"icon_{size}x{size}.png")
            resized(source, size * 2, iconset / f"icon_{size}x{size}@2x.png")
        subprocess.run(["iconutil", "-c", "icns", str(iconset), "-o", str(out)], check=True)


def main() -> int:
    if not shutil.which("sips"):
        sys.exit("This script needs macOS (sips).")
    with tempfile.TemporaryDirectory() as tmp:
        body = Path(tmp) / "body.png"
        sips("-c", str(BODY), str(BODY), str(SOURCE), "--out", str(body))
        pngs = {s: resized(body, s, Path(tmp) / f"{s}.png") for s in ICO_SIZES}
    write_ico(pngs, ICO)
    print(f"Wrote {ICO.relative_to(ROOT)}")
    return 0


if __name__ == "__main__":
    sys.exit(main())
