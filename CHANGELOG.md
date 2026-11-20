# Changelog

All notable changes to this project will be documented in this file.

The format is based on [Keep a Changelog](https://keepachangelog.com/en/1.1.0/), and this project
adheres to [Semantic Versioning](https://semver.org/spec/v2.0.0.html).

## [0.1.0] - Unreleased

### Added

- Native desktop MQTT 3.1.1 and MQTT 5 client for AWS IoT Core (macOS, Windows, Linux), built with egui.
- Connection options: clean session / clean start, session expiry (MQTT 5), and last will.
- AWS credential discovery through the standard credential chain, with a profile and region picker.
- Connect as an AWS IoT Thing (client ID equals the Thing name) or as a custom client ID.
- Things list loads only the first page; other Things are looked up by exact name.
- After connecting: broker-side connectivity details (`GetConnection`) and active subscriptions
  (`ListSubscriptions`) with unsubscribe.
- Endpoint selection: the default `iot:Data-ATS` endpoint plus custom domain configurations.
- Certificate creation with a new (best-practice, editable) or existing IoT policy, attached
  automatically, with credentials saved to the local cache folder and loaded automatically.
- Publish, subscribe, received-message view, and an MQTT packet log.
- AWS API call log with parameters, timing, and request IDs (secrets are never logged).
- Glass interface with light and dark themes and an Auto/Light/Dark switch. Sections show and
  hide by context (setup vs. connected), with advanced options and logs one click away.
- System UI font (San Francisco, Segoe UI, or the fontconfig default), loaded at runtime.
- Design tokens in `assets/theme.json`, reloaded live in debug builds, with automated contrast
  checks against the glass surfaces.
- `scripts/dev.py`: rebuild and relaunch on every source change.
- Release automation: pushing a `v*` tag publishes macOS (universal `.app`), Linux (x86_64,
  arm64), and Windows (x86_64) builds with SHA-256 checksums. `scripts/package.py` builds the
  same archive locally.
- Policy simulator tab: checks whether a policy accepts or rejects CONNECT, PUBLISH (including
  retained), SUBSCRIBE, or message delivery, with a statement-by-statement explanation.
  Evaluation is local; existing policies can be loaded from the account.
