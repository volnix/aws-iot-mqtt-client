#!/usr/bin/env python3
# Copyright Amazon.com, Inc. or its affiliates. All Rights Reserved.
# SPDX-License-Identifier: Apache-2.0
"""Regenerates NOTICE from `cargo metadata`.

Lists every crate that can end up in the shipped binary on any platform (normal and build
dependencies; dev-dependencies are excluded), plus bundled non-crate assets.

Usage:
    python3 scripts/generate_notice.py          # rewrite NOTICE
    python3 scripts/generate_notice.py --check  # exit 1 if NOTICE is out of date (CI)
"""
import json
import pathlib
import subprocess
import sys

ROOT = pathlib.Path(__file__).resolve().parent.parent
NOTICE = ROOT / "NOTICE"

HEADER = """AWS IoT MQTT Client
Copyright Amazon.com, Inc. or its affiliates. All Rights Reserved.

**********************
THIRD PARTY COMPONENTS
**********************

This software includes third party software subject to the following copyrights and licenses.
License texts are available in each component's source distribution on https://crates.io.

Bundled assets:

- Amazon Root CA 1-4 certificates (assets/), from https://www.amazontrust.com/repository/
- Mozilla root certificates, via webpki-roots, under the CDLA-Permissive-2.0 license
- Fonts bundled by egui (epaint_default_fonts):
  - Ubuntu Light, under the Ubuntu Font Licence 1.0
  - Hack, under the MIT License (includes Bitstream Vera, under the Bitstream Vera License)
  - Noto Emoji, under the SIL Open Font License 1.1
  - emoji-icon-font, under the MIT License

Rust crates:

"""


def runtime_packages():
    meta = json.loads(
        subprocess.check_output(
            ["cargo", "metadata", "--format-version", "1", "--locked"], cwd=ROOT
        )
    )
    packages = {p["id"]: p for p in meta["packages"]}
    nodes = {n["id"]: n for n in meta["resolve"]["nodes"]}
    root = meta["resolve"]["root"]
    seen, stack = set(), [root]
    while stack:
        pkg_id = stack.pop()
        if pkg_id in seen:
            continue
        seen.add(pkg_id)
        for dep in nodes[pkg_id]["deps"]:
            if any(k["kind"] in (None, "build") for k in dep["dep_kinds"]):
                stack.append(dep["pkg"])
    seen.discard(root)
    return sorted(
        (packages[i] for i in seen), key=lambda p: (p["name"].lower(), p["version"])
    )


def render():
    lines = []
    for p in runtime_packages():
        license_ = p.get("license") or "see license file"
        url = p.get("repository") or f"https://crates.io/crates/{p['name']}"
        lines.append(f"{p['name']} {p['version']} under {license_} ({url})")
    return HEADER + "\n".join(lines) + "\n"


def main():
    content = render()
    if "--check" in sys.argv:
        if not NOTICE.exists() or NOTICE.read_text() != content:
            print("NOTICE is out of date. Run: python3 scripts/generate_notice.py", file=sys.stderr)
            return 1
        return 0
    NOTICE.write_text(content)
    print(f"Wrote {NOTICE} ({content.count(chr(10))} lines)")
    return 0


if __name__ == "__main__":
    sys.exit(main())
