#!/usr/bin/env python3
# Copyright Amazon.com, Inc. or its affiliates. All Rights Reserved.
# SPDX-License-Identifier: Apache-2.0
"""Builds a release archive of the app for one platform, ready to upload or share.

    python3 scripts/package.py                    # this machine's platform
    python3 scripts/package.py --macos-universal  # macOS: one app for Apple silicon and Intel
    python3 scripts/package.py --target x86_64-unknown-linux-gnu

Output goes to dist/:
  macOS    aws-iot-mqtt-client-<version>-macos-<arch>.zip   (an .app bundle)
  Linux    aws-iot-mqtt-client-<version>-linux-<arch>.tar.gz
  Windows  aws-iot-mqtt-client-<version>-windows-<arch>.zip
Each archive includes LICENSE, NOTICE, and README.md, and gets a matching .sha256 file.

Standard library only. The release workflow (.github/workflows/release.yml) runs this script,
so local packages match the published ones.
"""
import argparse
import hashlib
import os
import re
import shutil
import subprocess
import sys
import tarfile
import zipfile
from pathlib import Path

ROOT = Path(__file__).resolve().parent.parent
BINARY = "aws-iot-mqtt-client"
APP_NAME = "AWS IoT MQTT Client"
BUNDLE_ID = "io.github.volnix.aws-iot-mqtt-client"
MACOS_MIN = "11.0"
DOCS = ["LICENSE", "NOTICE", "README.md"]


def log(message: str) -> None:
    print(f"[package] {message}", flush=True)


def run(cmd, env=None) -> None:
    log(" ".join(str(c) for c in cmd))
    subprocess.run(cmd, cwd=ROOT, check=True, env=env)


def crate_version() -> str:
    text = (ROOT / "Cargo.toml").read_text(encoding="utf-8")
    package = text.split("[package]", 1)[1].split("\n[", 1)[0]
    match = re.search(r'^version\s*=\s*"([^"]+)"', package, re.M)
    if not match:
        sys.exit("Couldn't find the package version in Cargo.toml")
    return match.group(1)


def normalize(version: str) -> str:
    """Pads a short version to three parts, so a `v1.0` tag matches version 1.0.0."""
    core, sep, rest = version.partition("-")
    parts = core.split(".")
    while len(parts) < 3:
        parts.append("0")
    return ".".join(parts) + sep + rest


def host_target() -> str:
    out = subprocess.run(["rustc", "-vV"], capture_output=True, text=True, check=True).stdout
    for line in out.splitlines():
        if line.startswith("host:"):
            return line.split(":", 1)[1].strip()
    sys.exit("Couldn't determine the host target from `rustc -vV`")


def platform_of(target: str):
    """Maps a Rust target triple to (os, arch) labels used in file names."""
    arch = {"x86_64": "x86_64", "aarch64": "arm64", "i686": "x86", "armv7": "armv7"}.get(
        target.split("-", 1)[0], target.split("-", 1)[0]
    )
    if "apple-darwin" in target:
        return "macos", arch
    if "windows" in target:
        return "windows", arch
    if "linux" in target:
        return "linux", arch
    # Other Rust platforms (FreeBSD, ...) are packaged like Linux.
    return target.split("-")[2] if target.count("-") >= 2 else "unknown", arch


def build(target: str) -> Path:
    env = dict(os.environ)
    if "apple-darwin" in target:
        env.setdefault("MACOSX_DEPLOYMENT_TARGET", MACOS_MIN)
    run(["cargo", "build", "--release", "--locked", "--bin", BINARY, "--target", target], env=env)
    exe = BINARY + (".exe" if "windows" in target else "")
    path = ROOT / "target" / target / "release" / exe
    if not path.is_file():
        sys.exit(f"Build finished but {path} is missing")
    return path


def sha256_file(path: Path) -> Path:
    digest = hashlib.sha256(path.read_bytes()).hexdigest()
    out = path.with_name(path.name + ".sha256")
    # Same format as `shasum -a 256` / `sha256sum`, so users can verify with either.
    out.write_text(f"{digest}  {path.name}\n", encoding="utf-8")
    return out


def info_plist(version: str) -> str:
    return f"""<?xml version="1.0" encoding="UTF-8"?>
<!DOCTYPE plist PUBLIC "-//Apple//DTD PLIST 1.0//EN" "http://www.apple.com/DTDs/PropertyList-1.0.dtd">
<plist version="1.0">
<dict>
  <key>CFBundleName</key><string>{APP_NAME}</string>
  <key>CFBundleDisplayName</key><string>{APP_NAME}</string>
  <key>CFBundleIdentifier</key><string>{BUNDLE_ID}</string>
  <key>CFBundleExecutable</key><string>{BINARY}</string>
  <key>CFBundlePackageType</key><string>APPL</string>
  <key>CFBundleShortVersionString</key><string>{version}</string>
  <key>CFBundleVersion</key><string>{version}</string>
  <key>LSMinimumSystemVersion</key><string>{MACOS_MIN}</string>
  <key>NSHighResolutionCapable</key><true/>
  <key>LSApplicationCategoryType</key><string>public.app-category.developer-tools</string>
</dict>
</plist>
"""


def package_macos(binary: Path, version: str, arch: str, dist: Path) -> Path:
    stage = dist / "stage"
    app = stage / f"{APP_NAME}.app"
    (app / "Contents" / "MacOS").mkdir(parents=True)
    (app / "Contents" / "Resources").mkdir()
    shutil.copy2(binary, app / "Contents" / "MacOS" / BINARY)
    (app / "Contents" / "Info.plist").write_text(info_plist(version), encoding="utf-8")
    for doc in DOCS:
        shutil.copy2(ROOT / doc, app / "Contents" / "Resources" / doc)
    if shutil.which("codesign"):
        # Ad-hoc signature: required for arm64 executables and keeps the bundle consistent.
        # It is not a Developer ID signature, so Gatekeeper still asks on first launch.
        run(["codesign", "--force", "--deep", "--sign", "-", str(app)])
    archive = dist / f"{BINARY}-{version}-macos-{arch}.zip"
    if shutil.which("ditto"):
        # ditto keeps the bundle's permissions and extended attributes intact.
        run(["ditto", "-c", "-k", "--keepParent", str(app), str(archive)])
    else:
        zip_dir(stage, archive)
    return archive


def package_tar(binary: Path, name: str, archive: Path) -> Path:
    with tarfile.open(archive, "w:gz") as tar:
        info = tar.gettarinfo(str(binary), arcname=f"{name}/{binary.name}")
        info.mode = 0o755
        with open(binary, "rb") as f:
            tar.addfile(info, f)
        for doc in DOCS:
            tar.add(ROOT / doc, arcname=f"{name}/{doc}")
    return archive


def package_zip(binary: Path, name: str, archive: Path) -> Path:
    with zipfile.ZipFile(archive, "w", zipfile.ZIP_DEFLATED) as z:
        z.write(binary, f"{name}/{binary.name}")
        for doc in DOCS:
            z.write(ROOT / doc, f"{name}/{doc}")
    return archive


def zip_dir(directory: Path, archive: Path) -> None:
    with zipfile.ZipFile(archive, "w", zipfile.ZIP_DEFLATED) as z:
        for path in sorted(directory.rglob("*")):
            z.write(path, path.relative_to(directory))


def main() -> int:
    parser = argparse.ArgumentParser(description=__doc__.splitlines()[0])
    parser.add_argument("--target", help="Rust target triple (default: this machine)")
    parser.add_argument(
        "--macos-universal",
        action="store_true",
        help="build Apple silicon and Intel slices and combine them (macOS only)",
    )
    parser.add_argument("--out", default="dist", help="output directory (default: dist)")
    parser.add_argument(
        "--expect-version",
        help="fail unless Cargo.toml has this version (used by the release workflow)",
    )
    args = parser.parse_args()

    version = crate_version()
    if args.expect_version and normalize(args.expect_version) != normalize(version):
        sys.exit(
            f"Tag version {args.expect_version!r} doesn't match Cargo.toml version {version!r}. "
            "Update Cargo.toml (and CHANGELOG.md), commit, and tag again."
        )

    dist = (ROOT / args.out).resolve()
    stage = dist / "stage"
    if stage.exists():
        shutil.rmtree(stage)
    dist.mkdir(parents=True, exist_ok=True)

    if args.macos_universal:
        if sys.platform != "darwin":
            sys.exit("--macos-universal needs macOS (it uses lipo)")
        slices = [build(t) for t in ("aarch64-apple-darwin", "x86_64-apple-darwin")]
        stage.mkdir(parents=True)
        binary = stage / BINARY
        run(["lipo", "-create", "-output", str(binary), *map(str, slices)])
        archive = package_macos(binary, version, "universal", dist)
    else:
        target = args.target or host_target()
        system, arch = platform_of(target)
        binary = build(target)
        name = f"{BINARY}-{version}-{system}-{arch}"
        if system == "macos":
            archive = package_macos(binary, version, arch, dist)
        elif system == "windows":
            archive = package_zip(binary, name, dist / f"{name}.zip")
        else:
            archive = package_tar(binary, name, dist / f"{name}.tar.gz")

    if stage.exists():
        shutil.rmtree(stage)
    checksum = sha256_file(archive)
    log(f"created {archive.relative_to(ROOT)}")
    log(f"checksum {checksum.relative_to(ROOT)}")
    return 0


if __name__ == "__main__":
    sys.exit(main())
