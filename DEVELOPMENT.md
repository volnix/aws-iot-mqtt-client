# Developing AWS IoT MQTT Client

How to build the app from source, iterate on it with hot reload, run the checks, and publish a
release. To use the app, download it from the
[Releases page](https://github.com/volnix/aws-iot-mqtt-client/releases) instead; see the
[README](README.md).

## Build from source

The app builds anywhere Rust and a desktop windowing system are available. macOS, Windows, and
Linux are tested in CI. Other Unix desktops with X11 or Wayland should also work.

### 1. Install the prerequisites

| Platform | Install |
| --- | --- |
| All | Rust stable 1.95 or newer, from [rustup.rs](https://rustup.rs) |
| macOS | Xcode Command Line Tools: `xcode-select --install` |
| Windows | [Visual Studio Build Tools](https://visualstudio.microsoft.com/visual-cpp-build-tools/) with the **Desktop development with C++** workload |
| Debian / Ubuntu | `sudo apt install build-essential pkg-config libxkbcommon-dev libwayland-dev libx11-dev libxcursor-dev libxrandr-dev libxi-dev libgl1-mesa-dev` |
| Fedora | `sudo dnf install gcc pkgconf-pkg-config libxkbcommon-devel wayland-devel libX11-devel libXcursor-devel libXrandr-devel libXi-devel mesa-libGL-devel` |

A C compiler is needed because the TLS library (`aws-lc-sys`) includes C code; CMake isn't
required. See the [AWS-LC requirements](https://aws.github.io/aws-lc-rs/requirements/index.html)
for per-platform details. On Windows x86_64, the repository's `.cargo/config.toml` opts in to
AWS-LC's prebuilt assembly, so NASM isn't needed either.

### 2. Build and run

```sh
git clone https://github.com/volnix/aws-iot-mqtt-client.git
cd aws-iot-mqtt-client
cargo run --release
```

`cargo build --release` builds without launching. The app is written to
`target/release/aws-iot-mqtt-client` (`aws-iot-mqtt-client.exe` on Windows), and it's a single
file you can copy anywhere.

### 3. Package it (optional)

To produce the same kind of archive the Releases page has, for the machine you're on:

```sh
python3 scripts/package.py                    # macOS, Linux, or Windows (use `python` on Windows)
python3 scripts/package.py --macos-universal  # macOS: one app for Apple silicon and Intel
```

The archive and its `.sha256` file go to `dist/`. On macOS it's a zipped `.app`. A universal
build needs both targets: `rustup target add aarch64-apple-darwin x86_64-apple-darwin`.

## Develop with hot reload

Run the development loop from the repository root:

```sh
python3 scripts/dev.py
```

It builds a debug build, launches it, and then watches the source tree:

- **Design changes apply instantly.** Every color, glass opacity, background light, corner
  radius, spacing value, and font size lives in [`assets/theme.json`](assets/theme.json).
  - Save the file and the running app restyles itself within half a second, without
    rebuilding or losing state.
  - A **Live theme** badge in the top bar shows this is active.
  - If the JSON has a mistake, the app keeps the last valid theme and shows the error until you
    fix it.
- **Code changes rebuild and relaunch.** Saving a file under `src/`, or changing `Cargo.toml`,
  triggers an incremental rebuild (usually a few seconds), and the app restarts. If the build
  fails, the compiler error is printed and the previous app keeps running, so fix it and save
  again.

Options:

- `python3 scripts/dev.py --release` uses optimized builds.
- To live-reload a different token file, including in a release build, set
  `AWS_IOT_MQTT_THEME_FILE=/path/to/theme.json`.

Rust code itself isn't hot-patched in place. Tools that do that, such as Dioxus `subsecond`,
can only patch the binary crate, and this app's UI lives in its library crate.

## Checks

CI ([`.github/workflows/ci.yml`](.github/workflows/ci.yml)) runs these on macOS, Windows, and
Linux, plus a build with the minimum supported Rust version from `Cargo.toml`:

```sh
cargo fmt --check
cargo clippy --all-targets --locked -- -D warnings
cargo test --locked                          # unit, property-based, and headless UI render tests
cargo deny check                             # licenses, advisories, sources (cargo install cargo-deny)
python3 scripts/generate_notice.py --check   # NOTICE matches the dependencies
```

After changing dependencies, run `python3 scripts/generate_notice.py` and commit `NOTICE`.

### Test coverage

CI also measures coverage with [`cargo-llvm-cov`](https://github.com/taiki-e/cargo-llvm-cov) and
uploads it to [Codecov](https://about.codecov.io), which powers the README badge. Codecov is free for public repos. To enable it once:

1. Sign in at [codecov.io](https://codecov.io) with GitHub and add this repository.
2. Copy the repository's upload token from Codecov.
3. In GitHub, open **Settings → Secrets and variables → Actions** and add a secret named
   `CODECOV_TOKEN` with that value.

Until the secret exists, the upload step is skipped without failing CI. To see coverage locally:
`cargo install cargo-llvm-cov`, then `cargo llvm-cov --open`.

### Security advisories

`cargo deny check` fails CI on any RustSec advisory. The few ignored in [`deny.toml`](deny.toml)
each record why they don't affect this app; remove an ignore once its cause is gone (cargo-deny
warns when an ignore no longer matches anything).

If you use an AI coding agent, [AGENTS.md](AGENTS.md) has the repository's conventions and known
pitfalls.

## Project layout

| Path | Responsibility |
| --- | --- |
| `src/app.rs` | egui UI; background work runs on a tokio runtime and reports back over a channel |
| `src/app/simulator.rs` | The Policy simulator tab |
| `src/policy_sim.rs` | Offline policy evaluation for MQTT operations (no UI or AWS dependencies) |
| `src/aws.rs` | Profile discovery, credential resolution, logged IoT/STS/data-plane calls, endpoint discovery |
| `src/provision.rs` | Certificate creation workflow, default policy template, local credential storage |
| `src/mqtt.rs` | MQTT 3.1.1 and 5 connections over mTLS (rumqttc + rustls), state machine, packet descriptions |
| `src/certs.rs` | PEM certificate/key parsing, certificate ID (SHA-256 of DER) |
| `src/theme.rs` | Glass design tokens (from `assets/theme.json`), live reload, painted background, shared widgets, contrast tests |
| `src/fonts.rs` | Loads the system UI font at runtime, with egui's fonts as fallbacks |
| `src/icons.rs` | Icon glyphs, with a test that every UI glyph exists in the bundled fonts |
| `src/packet_log.rs` | Bounded packet log and payload formatting |
| `src/validators.rs` | Client ID, endpoint, and topic validation |
| `src/error.rs` | Categorized errors with details and suggested actions |
| `src/paths.rs` | Platform home and cache directories |
| `scripts/` | `dev.py` (rebuild and relaunch on save), `package.py` (release archives), `generate_notice.py` |
| `assets/` | Amazon Root CA 1–4 (TLS trust anchors, plus the Mozilla root set for custom domains) and `theme.json` design tokens |

## Releasing

Releases are built and published by
[`.github/workflows/release.yml`](.github/workflows/release.yml) when a version tag is pushed.

1. Set `version` in `Cargo.toml`, add a section for it to `CHANGELOG.md`, and commit.
2. Tag the commit and push the tag:
   ```sh
   git tag v0.1.0
   git push origin v0.1.0
   ```
3. The workflow builds macOS (universal), Linux (x86_64 and arm64), and Windows (x86_64) in
   parallel. It then publishes a GitHub release with the archives, `SHA256SUMS`, and release
   notes generated from the merged pull requests. Follow its progress in the **Actions** tab.

Notes:

- **Version check:** the tag must match `Cargo.toml`. Tagging `v0.2.0` while `Cargo.toml` says
  `0.1.0` fails the build before anything is published.
- **Pre-releases:** a tag with a hyphen, such as `v0.2.0-beta.1`, is published as a
  pre-release.
- **Dry run:** to check the builds without publishing, open **Actions → Release → Run
  workflow**. The archives are attached to that run as artifacts.
- **Re-running:** if publishing fails partway, delete the release and the tag on GitHub, fix the
  problem, and push the tag again.
- **Code signing:** builds aren't signed yet. macOS signing and notarization need an Apple
  Developer ID, and Windows signing needs a code-signing certificate. Both can be added to the
  workflow later as repository secrets.
