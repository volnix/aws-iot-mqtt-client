#!/usr/bin/env python3
# Copyright Amazon.com, Inc. or its affiliates. All Rights Reserved.
# SPDX-License-Identifier: Apache-2.0
"""Development loop: rebuilds and relaunches the app whenever Rust sources change.

    python3 scripts/dev.py            # debug build, relaunch on save
    python3 scripts/dev.py --release  # same, with optimizations

Design-token changes (assets/theme.json) don't trigger a rebuild: the running debug app
re-applies them live. If a rebuild fails, the previous app keeps running and the compiler
error is printed, so you can fix it and save again.

Standard library only; works on macOS, Linux, and Windows.
"""
import argparse
import os
import signal
import subprocess
import sys
import time
from pathlib import Path

ROOT = Path(__file__).resolve().parent.parent
BINARY = "aws-iot-mqtt-client"
# Paths whose changes need a rebuild. assets/theme.json is reloaded live instead.
WATCHED = [ROOT / "src", ROOT / "Cargo.toml", ROOT / "Cargo.lock", ROOT / "assets"]
IGNORED = {ROOT / "assets" / "theme.json"}
POLL_SECONDS = 0.4
# Wait for a burst of saves (format-on-save, multi-file edits) to settle.
DEBOUNCE_SECONDS = 0.35


def snapshot() -> dict:
    """mtime of every watched file."""
    files = {}
    for path in WATCHED:
        candidates = path.rglob("*") if path.is_dir() else [path]
        for f in candidates:
            if f.is_file() and f not in IGNORED:
                try:
                    files[f] = f.stat().st_mtime_ns
                except OSError:
                    pass
    return files


def changed(before: dict, after: dict) -> list:
    keys = set(before) | set(after)
    return sorted(str(k.relative_to(ROOT)) for k in keys if before.get(k) != after.get(k))


def build(release: bool) -> bool:
    cmd = ["cargo", "build", "--bin", BINARY] + (["--release"] if release else [])
    started = time.monotonic()
    result = subprocess.run(cmd, cwd=ROOT)
    elapsed = time.monotonic() - started
    if result.returncode == 0:
        log(f"built in {elapsed:.1f}s")
        return True
    log("build failed; keeping the previous app running")
    return False


def launch(release: bool, app_args: list) -> subprocess.Popen:
    exe = ROOT / "target" / ("release" if release else "debug") / BINARY
    if os.name == "nt":
        exe = exe.with_suffix(".exe")
    log(f"launching {exe.relative_to(ROOT)}")
    return subprocess.Popen([str(exe), *app_args], cwd=ROOT)


def stop(proc) -> None:
    if proc is None or proc.poll() is not None:
        return
    proc.terminate()
    try:
        proc.wait(timeout=5)
    except subprocess.TimeoutExpired:
        proc.kill()
        proc.wait()


def log(message: str) -> None:
    print(f"\033[36m[dev]\033[0m {message}", flush=True)


def main() -> int:
    parser = argparse.ArgumentParser(description=__doc__.splitlines()[0])
    parser.add_argument("--release", action="store_true", help="build with optimizations")
    parser.add_argument(
        "--once",
        action="store_true",
        help="exit after the first relaunch caused by a change (used by tests)",
    )
    parser.add_argument("app_args", nargs="*", help="arguments passed to the app")
    args = parser.parse_args()

    signal.signal(signal.SIGINT, lambda *_: (_ for _ in ()).throw(KeyboardInterrupt))
    app = None
    try:
        if build(args.release):
            app = launch(args.release, args.app_args)
        state = snapshot()
        log("watching src/, assets/, Cargo.toml (Ctrl+C to stop)")
        while True:
            time.sleep(POLL_SECONDS)
            current = snapshot()
            if current == state:
                if app is not None and app.poll() is not None:
                    log(f"app exited with status {app.returncode}; save a file to relaunch")
                    app = None
                continue
            time.sleep(DEBOUNCE_SECONDS)
            current = snapshot()
            log("changed: " + ", ".join(changed(state, current)[:5]))
            state = current
            if build(args.release):
                stop(app)
                app = launch(args.release, args.app_args)
                if args.once:
                    return 0
    except KeyboardInterrupt:
        log("stopping")
    finally:
        stop(app)
    return 0


if __name__ == "__main__":
    sys.exit(main())
