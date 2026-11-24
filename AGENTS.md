# AGENTS.md

Guidance for AI coding agents working in this repository. Read [README.md](README.md) for what
the app does, and [DEVELOPMENT.md](DEVELOPMENT.md) for building, hot reload, the module map, and
releasing.

## Before you finish any change

Run what CI runs. All four must pass:

```sh
cargo fmt --check
cargo clippy --all-targets --locked -- -D warnings
cargo test --locked
python3 scripts/generate_notice.py --check   # only fails if dependencies changed
```

- To see a UI change in a running app, use `python3 scripts/dev.py`. It rebuilds and relaunches
  on save, and `assets/theme.json` edits apply live without a rebuild.
- Agents usually can't screenshot the window. Say so instead of claiming a visual change looks
  right, and rely on the headless render test (below).

## Project rules

- **Public repo:** never add internal hostnames, account IDs, credentials, personal paths, or
  internal tool names. Use `123456789012` as the example account ID.
- **New source files:** start with the copyright and SPDX header used in every existing `.rs`
  and `.py` file.
- **Dependencies:**
  - crates.io only, under a license allowed in `deny.toml`. Permissive only: an MPL crate
    (`dirs`) was removed for this reason.
  - After any dependency change, run `python3 scripts/generate_notice.py` and commit `NOTICE`.
- **MSRV** is `rust-version` in `Cargo.toml` (set by egui). Don't use newer std APIs.
- **eframe features:** default features are off, so list what's needed. `clipboard` is
  required for copy and paste; without it `ctx.copy_text` silently does nothing.
- **AWS SDK features:** keep `default-features = false` on the `aws-sdk-*` crates. Their
  default `rustls` feature brings back a legacy TLS stack with security advisories.
- **Advisories:** `cargo deny check` must pass. Only add a `deny.toml` ignore after confirming
  the vulnerable code isn't reachable, and record why in its `reason`.
- **Time:** use `jiff` (`ZonedDateTime`, `strftime`). `chrono` was removed as unmaintained.
- **AWS calls:**
  - Every call goes through `Session::call` in `src/aws.rs`, which handles the timeout,
    request/response logging, and request IDs.
  - Never put secrets, PEMs, or private keys in its request string or summary closure.
- **Shadows** are read and updated over MQTT as the device (`src/shadow.rs`), never through
  the `UpdateThingShadow`/`DeleteThingShadow` APIs. The only shadow API call is the read-only
  `ListNamedShadowsForThing`. The app doesn't delete shadows.
- **Writes to AWS** exist only in `src/provision.rs`, behind the user's confirmation in the
  Create certificate dialog. Don't add others. Never delete or roll back AWS resources.
- **Persisted `Settings`** (in `src/app.rs`) use `#[serde(default)]`. New fields need a
  `Default` value. Never persist certificate or key contents, only paths.
- **User-visible changes** get a line in `CHANGELOG.md`.
- **Versioning:** a release tag must match the `Cargo.toml` version, and `scripts/package.py`
  enforces it.

## UI conventions (egui 0.37)

- **egui 0.37 API:** it differs from most online examples:
  - `eframe::App::ui` (not `update`).
  - `egui::Panel::left/top/bottom(..).show(ui, ..)`.
  - `egui::Id::unique` (`Id::new` is deprecated).
  - When unsure, read the source under `~/.cargo/registry/src/*/egui-0.37.*`.
- **Colors:**
  - Never hard-code colors. Use `theme::Palette::of(ui)` and `theme::metrics(ui)`.
  - New colors go in `assets/theme.json` for both `light` and `dark`.
  - `theme::tests::text_is_readable_on_glass_over_any_part_of_the_background` enforces contrast.
- **Shared widgets:** reuse the ones in `src/theme.rs` (`card`, `inset`, `banner`, `pill`,
  `disclosure`, `segmented`, buttons) instead of styling widgets ad hoc.
- **Icons:**
  - Only use glyphs from `src/icons.rs`. egui's fonts lack many symbols, which render as `[]`.
  - The direction arrows exist only in the monospace font.
  - For chevrons, use `theme::paint_chevron` rather than a glyph.
  - `icons::tests::app_strings_only_use_renderable_glyphs` scans the UI sources. Add any new
    UI source file to that test.
- **Bold text:** use `theme::title(..)`, which selects the `fonts::SEMIBOLD` family. That
  family must be installed: a bare `egui::Context::default()` panics on it.
- **Layout:** inside a `ScrollArea` the available height is unbounded. Never size widgets from
  `ui.available_height()` there.
- **Visibility:** sections show or hide by context (setup vs. connected). Keep advanced options
  behind `theme::disclosure`, and summarize any non-default hidden values next to it.

## Testing gotchas

- **Headless rendering:** `ctx.run_ui(..)` output must be dropped with
  `out.textures_delta.clear()` first, or it panics.
- **Building a test app:** use `MqttApp::with_settings(&ctx, rt, settings)`. It installs fonts
  and theme and makes no AWS calls; the initial AWS load only runs in `App::ui`.
- **New views:** when you add a view or tab, extend `app::tests::every_view_renders_without_panicking`.
- **Tests never touch real AWS or the network:**
  - AWS: build sessions with `test_support::FakeAws`, which routes by method and path to canned
    JSON. Find an operation's path in the SDK's `schema_serde/shape_<op>_input_input.rs`.
  - MQTT: use `test_support::broker::start`, a TLS broker on `127.0.0.1`.
  - App: set `test_sdk_config` and `test_tls_roots`, and keep `started = true` so the real AWS
    load never runs.
- **UI tests (`src/app/ui_tests.rs`):**
  - They use `egui_kittest` and find widgets by accessible label.
  - Custom-painted widgets need `response.widget_info(..)` to be findable. `WidgetType` is now
    `egui::Role`.
  - Use `wait_until` for background results, never fixed sleeps.
  - Keep clicked widgets inside the harness window (1400×1000). Off-screen clicks silently miss.
  - Widgets scrolled out of a `ScrollArea` still report a rect. Call `node.scroll_to_me()`
    and step before clicking them.
  - A modal needs a few `run_steps` after opening before its contents are in the tree.
- **MQTT protocol versions:** `src/mqtt.rs` has separate MQTT 3.1.1 (`rumqttc`) and MQTT 5
  (`rumqttc::v5`) drivers. Their `QoS`, `Packet`, and error types are distinct. Change both,
  and keep `unreachable_endpoint_reports_error` covering both.
- **Policy simulator (`src/policy_sim.rs`):**
  - Its semantics follow the AWS IoT policy docs, and many tests mirror AWS doc examples.
    Don't change evaluation rules without citing the docs.
  - A variable with no value means the statement doesn't match.
  - Anything unsupported must yield `Outcome::Unknown` (shown as "Not certain"), never a guess.

## Shell and tooling gotchas

- **zsh word-splitting:** the default macOS shell is zsh, where an unquoted `$VAR` holding a
  file list is *not* split into words. Searches like `grep pattern $FILES` then silently match
  nothing. Use Python, `git ls-files -z | xargs -0`, or zsh's `${=VAR}`.
- **Never commit** `.claude/`, `target/`, or `dist/`. All are in `.gitignore`.
- **`.cargo/config.toml`:** it sets `AWS_LC_SYS_PREBUILT_NASM=1` so Windows builds don't need
  NASM. Changing it rebuilds `aws-lc-sys`, which takes over a minute.
- **Release packaging:** `.github/workflows/release.yml` packages through `scripts/package.py`.
  Change packaging in the script, not the workflow, and test it locally with
  `python3 scripts/package.py`.
- **App icon:** `assets/icon/icon.png` is the source. After changing it, run
  `python3 scripts/make_icons.py` (macOS) and commit `assets/icon/icon.ico` too. `build.rs`
  embeds the `.ico` on Windows only, and can't be tested on macOS: watch the Windows CI job.
- **Signing secrets:** never expose them to a step that compiles. Dependency build scripts
  run during `cargo build`. The release workflow builds first, then signs in a separate step
  with `package.py --skip-build`.
