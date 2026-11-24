# Changelog

All notable changes to this project will be documented in this file.

The format is based on [Keep a Changelog](https://keepachangelog.com/en/1.1.0/), and this project
adheres to [Semantic Versioning](https://semver.org/spec/v2.0.0.html).

## [1.2.0] - 2026-11-24

MQTT 5 User Properties, a form editor for shadows, command-line export, shareable traces, an
app icon, and fixes for copying and for shadow updates that never finished.

### Added

- MQTT 5 User Properties.
  - **Sent:** add them to CONNECT and the last will (under **Advanced**), to PUBLISH (under
    the composer), and to SUBSCRIBE and UNSUBSCRIBE (under the subscribe field).
  - **Shown:** on received messages, and in the MQTT log for every broker packet that carries
    them (CONNACK, RECEIVE, PUBACK, PUBREC, PUBREL, PUBCOMP, SUBACK, UNSUBACK, DISCONNECT,
    AUTH).
  - **Copied:** they're included in copied `mosquitto_pub`/`mosquitto_sub` commands as `-D`
    options.
  - **Not supported:** DISCONNECT and the acknowledgements the client sends, because the MQTT
    library doesn't allow setting their properties.
- Sent PUBLISH and SUBSCRIBE rows in the MQTT log have a copy button that shows the
  equivalent `mosquitto_pub` or `mosquitto_sub` command, with the full topic, QoS, retain flag,
  and payload, for macOS/Linux or PowerShell.
- **Export…** in the MQTT log saves the log as a trace, to share with someone. It's saved as
  JSON (full topics, payloads, and User Properties) or as text that reads like the log, and
  includes only what hasn't been cleared.
- On received messages, click the topic badge to copy it. The payload has a copy button.
- Copying anything shows a short "copied to the clipboard" confirmation.
- An app icon, shown in the Dock, taskbar, and window, on the Windows `.exe`, and on the
  macOS disk image.

### Changed

- The shadow editor has a **Fields** mode, now the default. Each field has a type (text,
  number, true/false, group, list, or null) and a matching input, and groups nest. Removing
  a field deletes it from the shadow when you send, with undo. Problems such as a bad number
  or a duplicate name are shown inline. **JSON** mode is still one click away, and edits
  carry over in both directions. True/false fields use **True** and **False** radio buttons.
- When you're not connected, the **Shadows** tab offers a short connect form (account, Thing,
  and certificate), and you can reconnect from the shadow list after disconnecting.
- Received messages show the topic as a badge, then the payload, then a **User properties**
  toggle (only when the message has any).
- Messages the broker delivers are logged as **RECEIVE**, so they're easy to tell apart from
  the PUBLISH packets the app sends.
- Copy buttons in the MQTT log are outlined clipboard buttons, so they read as buttons.
- Counts appear under the title as "2 active" (Subscriptions, Shadows) instead of beside it,
  and the Shadows tab no longer shows a count.
- Shorter error messages. A profile that doesn't exist, missing credentials, and other
  credential failures each get a one-line message with a specific next step. Long technical
  details are behind a **Details** toggle.

### Fixed

- Copying didn't reach the system clipboard (including ⌘C/Ctrl+C in text fields), because
  clipboard support wasn't built in.
- A shadow update could spin forever when no response came back. Updates now time out after
  10 seconds, and the app re-subscribes to the shadow topics and reloads the shadows once.
- The shadow-topic subscription is marked **Used by Shadows** in the Subscriptions list and
  can't be unsubscribed there, since that silently broke the Shadows tab.

## [1.1.0] - 2026-11-22

### Fixed

- Clicking the text of a collapsible section (such as **Advanced** or **More**) now opens it,
  not just clicking its arrow.

### Changed

- Icon-only buttons (refresh, unsubscribe, theme, close) now have names for screen readers.
- Updated the file dialog library (`rfd`) to 0.18.
- Updated `url` to 2.5.9 for a denial-of-service fix ([RUSTSEC-2026-0592](https://rustsec.org/advisories/RUSTSEC-2026-0592)).
- The project is maintained by Nick Volgas. Copyright notices, the Code of Conduct, and
  security reporting were updated to match.

### Added

- Device Shadow support. Connecting as a Thing loads its classic and named shadows, and a new
  **Shadows** tab shows their desired, reported, and delta state. You can update either state,
  or create a named shadow, over MQTT as the device. Listing named shadows needs
  `iot:ListNamedShadowsForThing`.
- A macOS disk image (`.dmg`) to drag the app into Applications.
- macOS releases are signed with a Developer ID and notarized by Apple, so the app opens
  without Gatekeeper warnings.
- Offline end-to-end tests: a fake of the AWS endpoints, a local TLS MQTT test broker, and UI
  tests that drive the real window. Line coverage went from about 60% to 92%.

## [1.0.0] - 2026-11-20

The initial release of AWS IoT MQTT Client: a desktop MQTT client for AWS IoT Core with a
built-in IoT policy simulator, for macOS, Windows, and Linux.

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
- Security: the AWS SDK now uses its current rustls 0.23 HTTPS client instead of the legacy
  rustls 0.21 stack, which removes several RustSec advisories. `chrono` (unmaintained) was
  replaced with `jiff`, and the app now uses rustls' own PEM parser.
- CI measures test coverage (`cargo-llvm-cov`, uploaded to Codecov).
- Release automation: pushing a `v*` tag publishes macOS (universal `.app`), Linux (x86_64,
  arm64), and Windows (x86_64) builds with SHA-256 checksums. `scripts/package.py` builds the
  same archive locally.
- Policy simulator tab: checks whether a policy accepts or rejects CONNECT, PUBLISH (including
  retained), SUBSCRIBE, or message delivery, with a statement-by-statement explanation.
  Evaluation is local; existing policies can be loaded from the account.
