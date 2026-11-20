# AWS IoT MQTT Client

A native desktop MQTT client (3.1.1 and 5) for AWS IoT Core. It's written in Rust with
[egui](https://github.com/emilk/egui) and builds into a single binary for macOS, Windows, and Linux.

1. Pick an AWS profile and region, then click **Load from AWS**. The app resolves your
   credentials, shows the account you're signed in to (`sts:GetCallerIdentity`), lists the
   Things, and discovers the data endpoints you can connect to.
2. Choose who to **connect as**:
   - **AWS IoT Thing:** pick a Thing from the account. The client ID is always the Thing name,
     so thing policy variables like `${iot:Connection.Thing.ThingName}` resolve. To keep large
     accounts fast, only the first 100 Things are listed (one `ListThings` call). For any other
     Thing, type its exact name and press Enter (`DescribeThing`). Selecting a Thing makes no
     AWS calls.
   - **Custom client ID:** type any client ID. No Thing is involved.
3. Pick the **endpoint**. **Default** is the account's `iot:Data-ATS` endpoint. Custom domain
   configurations (`ListDomainConfigurations`) appear by name, and **Custom…** lets you type a
   hostname.
4. Choose the certificate and private key files, **or** click **➕ Create certificate…** to
   have the app create them (see below).
5. Choose **MQTT 5** or **MQTT 3.1.1** and the connection options: clean session (3.1.1) or
   clean start (5), session expiry (MQTT 5 only, up to 7 days), and an optional last will
   (topic, payload, QoS, retain).
6. Connect over mutual TLS on port 8883, then subscribe and publish.

After a successful connect, the left pane shows the broker's view of the connection, using the
AWS IoT data plane through the account's default endpoint. These calls are made only while
connected:

- **Connectivity** (`GetConnection`): connected since, clean session, keep alive, session
  expiry, Thing name, and optionally client and endpoint IP addresses (**Include socket info**,
  which needs separate authorization).
- **Active subscriptions** (`ListSubscriptions`): every subscription the broker holds for the
  client, including ones kept from a persistent session. It refreshes automatically after each
  SUBACK or UNSUBACK, and each entry has an unsubscribe button.

The interface uses frosted-glass panels over a soft background and only shows what matters for
the current step:

- **Before connecting:** the account card, the identity picker, and a short connection form
  (endpoint, protocol, certificate, key). Session and last-will settings sit under **Advanced**,
  and any non-default values are summarized next to it.
- **While connected:** the form collapses into a one-line bar with **Disconnect**, the message
  stream with a subscribe field and a publish composer takes the space, and the sidebar shows
  live connectivity and subscriptions.
- **Logs** open in a drawer from the top bar.

Text uses the system UI font: San Francisco on macOS, Segoe UI on Windows, and the fontconfig
default on Linux. It's loaded from the system at runtime, so nothing is redistributed. The
top-bar theme button cycles **Auto → Light → Dark**; Auto follows the desktop setting.

The bottom panel has two logs: **MQTT packets**, and **AWS API calls**. The API log shows every
AWS request and response with its parameters, duration, request ID, and result. Secrets are
never logged; access key IDs are masked, and private keys are never written to the log.

Apart from **Create certificate**, the app only reads from your AWS account. Only file *paths*
are saved between runs, never key contents.

## Download

Builds for macOS (one app for Apple silicon and Intel), Windows (x86_64), and Linux (x86_64 and
arm64) are on the [Releases page](https://github.com/aws-solutions/aws-iot-mqtt-client/releases).
Each release lists SHA-256 checksums in `SHA256SUMS`.

The builds aren't code-signed with a paid developer certificate yet, so the first launch needs
one extra step:

- **macOS:** unzip, move **AWS IoT MQTT Client** to Applications, then right-click it and choose
  **Open** (once). Or run `xattr -dr com.apple.quarantine "/Applications/AWS IoT MQTT Client.app"`.
- **Windows:** unzip and run `aws-iot-mqtt-client.exe`. If SmartScreen appears, choose
  **More info → Run anyway**.
- **Linux:** extract and run `./aws-iot-mqtt-client`. It needs a desktop session (X11 or
  Wayland) with OpenGL, which standard desktop installs include.

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
for per-platform details. On Windows x86_64 the repository's `.cargo/config.toml` opts in to
AWS-LC's prebuilt assembly, so NASM isn't needed either.

### 2. Build and run

```sh
git clone https://github.com/aws-solutions/aws-iot-mqtt-client.git
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
  radius, spacing value, and font size lives in [`assets/theme.json`](assets/theme.json). Save
  the file and the running app restyles itself within half a second, without rebuilding or
  losing state. A **Live theme** badge in the top bar shows this is active. If the JSON has a
  mistake, the app keeps the last valid theme and shows the error until you fix it.
- **Code changes rebuild and relaunch.** Saving a file under `src/`, or changing `Cargo.toml`,
  triggers an incremental rebuild (usually a few seconds), and the app restarts. If the build
  fails, the compiler error is printed and the previous app keeps running, so fix it and save
  again.

Options: `python3 scripts/dev.py --release` for optimized builds. To live-reload a different
token file, including in a release build, set `AWS_IOT_MQTT_THEME_FILE=/path/to/theme.json`.

Rust code itself isn't hot-patched in place. Tools that do that, such as Dioxus `subsecond`,
can only patch the binary crate, and this app's UI lives in its library crate.

## Policy simulator

The **Policy simulator** tab checks whether an AWS IoT Core policy would accept or reject an MQTT
operation. It runs entirely on your machine and re-evaluates as you type. The only AWS calls are
the optional `ListPolicies` and `GetPolicy` reads behind **Load from account**.

1. Paste a policy document, or load one from your account.
2. Pick the operation. The choices follow the MQTT specification:

   | Operation | Checked as | Resource |
   | --- | --- | --- |
   | CONNECT | `iot:Connect` | `client/<client ID>` |
   | PUBLISH | `iot:Publish`, plus `iot:RetainPublish` when retained | `topic/<topic>` |
   | SUBSCRIBE | `iot:Subscribe` | `topicfilter/<filter>` |
   | Receive (a PUBLISH the broker delivers) | `iot:Receive` | `topic/<topic>` |

   UNSUBSCRIBE isn't listed because IoT policies have no action for it.
3. Set the connection context: client ID, and optionally the Thing (attached or not, exclusive
   attachment, type, attributes), other variables such as `aws:SourceIp`, and the region and
   account used in the resource ARNs. **Use the Client tab's identity** fills these in.

The result is **Accept** or **Reject**, with the statement that decided it. **How this was
decided** shows every check and why each statement did or didn't apply.

Evaluation follows AWS IoT's rules:

- An explicit `Deny` wins. Otherwise an `Allow` is required; with neither, the request is
  implicitly denied.
- `*` and `?` are wildcards in `Action` and `Resource`. MQTT's `+` and `#` are literal
  characters in policies.
- Actions are case-insensitive; resources are case-sensitive.
- `NotAction` and `NotResource` are supported.
- **Policy variables:** `${iot:ClientId}`, the thing variables, and any you add are
  substituted literally, and `${*}`, `${?}` and `${$}` escapes work.
  - Thing variables only resolve when the certificate is attached and the client ID equals the
    Thing name, unless the attachment is exclusive.
  - A variable with no value makes its statement not match, as in IAM. The result lists which
    variables were missing.
- **Supported conditions:** `String*`, `Arn*`, `Bool`, `Null`, `IpAddress` and `NotIpAddress`,
  including `…IfExists`.
- **Other condition operators** (for example `Date*`, `Numeric*`, and `ForAnyValue:`/`ForAllValues:`)
  aren't evaluated. If one could change the outcome, the result is marked **Not certain**
  instead of guessing.

## Creating a certificate

Click **➕ Create certificate…**: under the selected Thing in Thing mode, or under the client
ID in custom client ID mode. In the dialog you can either:

- **Create a new policy.** The name is prefilled as `<thing or client ID>-policy`, and you can
  edit the document before it's created (it must be valid JSON). The ARNs use your account,
  partition, and region.
  - **Thing mode** follows the AWS
    [thing policy best practice](https://docs.aws.amazon.com/iot/latest/developerguide/thing-policy-examples.html):
    `iot:Connect` is allowed only on `client/${iot:Connection.Thing.ThingName}`, and only when
    `iot:Connection.Thing.IsAttached` is `true`.
  - **Custom client ID mode** allows `iot:Connect` only on that literal client ID.
  - Both allow `iot:Publish`/`iot:Receive` on `topic/*` and `iot:Subscribe` on `topicfilter/*`.
- **Attach an existing policy** from the account's policy list.

Before anything runs, the dialog lists the exact changes. Clicking **Create certificate** then:

1. Creates the policy, if you chose a new one. This runs first, so a bad document or a name
   that's already taken fails before any certificate exists.
2. Creates a key pair and an `ACTIVE` certificate (`CreateKeysAndCertificate`).
3. Saves `certificate.pem.crt`, `private.pem.key`, and `public.pem.key` to the app's cache
   folder: `<cache>/aws-iot-mqtt-client/certificates/<thing or client-ID>/<certificate id>/`.
   On Unix the folder is `0700` and the key file is `0600`. `<cache>` is `~/Library/Caches` on
   macOS, `~/.cache` on Linux, and `%LOCALAPPDATA%` on Windows.
4. Attaches the policy to the certificate, and (Thing mode only) the certificate to the Thing.

The new certificate and key are loaded automatically, so you can connect right away. If a
later step fails, the dialog shows which steps finished. Nothing is rolled back or deleted, and
the saved key is still loaded, because AWS returns a private key only once.

## Cost

The app itself is free. You are responsible for the cost of the AWS services you use with it,
mainly AWS IoT Core connectivity and messaging. See [AWS IoT Core pricing](https://aws.amazon.com/iot-core/pricing/).
Certificates and policies created by the app don't incur charges on their own, but they remain
in your account until you remove them.

## AWS credentials

Credentials come from the standard AWS chain: environment variables, `~/.aws/config` and
`~/.aws/credentials` profiles, SSO, `credential_process`, and container/instance metadata. The
profile picker lists every profile in your config files. Leave it on "(default chain)" to use
`AWS_PROFILE` or the default profile.

For SSO profiles, run `aws sso login --profile <name>` first.

Minimum IAM permissions for browsing (read-only). `sts:GetCallerIdentity` needs no permission.

```
iot:DescribeEndpoint, iot:ListThings, iot:DescribeThing,
iot:ListDomainConfigurations, iot:DescribeDomainConfiguration,
iot:GetConnection, iot:ListSubscriptions
```

`iot:GetConnection` and `iot:ListSubscriptions` are used only after a successful connect, for
the Connectivity and Active subscriptions views.

Without the two domain configuration permissions, only the Default endpoint is listed.

Additional permissions for **Create certificate**:

```
iot:ListPolicies, iot:GetPolicy (simulator "Load from account"), iot:CreateKeysAndCertificate, iot:AttachPolicy,
iot:AttachThingPrincipal, iot:CreatePolicy (only when creating a new policy)
```

The AWS step is optional. You can also type an endpoint and client ID by hand.

## Device permissions

AWS IoT closes the connection without giving a reason if the certificate isn't `ACTIVE`, or if
its policy doesn't allow `iot:Connect` for the client ID you use. A typical device policy
scopes access to the Thing name:

```json
{
  "Effect": "Allow",
  "Action": ["iot:Connect"],
  "Resource": "arn:aws:iot:REGION:ACCOUNT:client/${iot:Connection.Thing.ThingName}",
  "Condition": { "Bool": { "iot:Connection.Thing.IsAttached": "true" } }
}
```

Variables such as `${iot:Connection.Thing.ThingName}` only resolve when the client ID matches
the Thing name **and** the certificate is attached to that Thing. The app checks both.

## Development

```sh
cargo test                                   # unit and property-based tests (proptest)
cargo clippy --all-targets -- -D warnings
cargo fmt --check
cargo deny check                             # licenses, advisories, sources (cargo install cargo-deny)
python3 scripts/generate_notice.py           # regenerate NOTICE after dependency changes
```

CI (`.github/workflows/ci.yml`) runs these checks on macOS, Windows, and Linux, plus a build with
the minimum supported Rust version from `Cargo.toml`.

| Module | Responsibility |
| --- | --- |
| `src/aws.rs` | Profile discovery, credential resolution, logged read-only IoT/STS calls, endpoint discovery |
| `src/policy_sim.rs` | Offline policy evaluation for MQTT operations (no UI or AWS dependencies) |
| `src/app/simulator.rs` | The Policy simulator tab |
| `src/provision.rs` | Certificate creation workflow, default policy template, local credential storage |
| `src/certs.rs` | PEM certificate/key parsing, certificate ID (SHA-256 of DER) |
| `src/mqtt.rs` | MQTT 3.1.1 and 5 connections over mTLS (rumqttc + rustls), state machine, packet descriptions |
| `src/icons.rs` | Icon glyphs, with a test that every UI glyph exists in the bundled fonts |
| `src/theme.rs` | Glass design tokens (from `assets/theme.json`), live reload, painted background, shared widgets, contrast tests |
| `src/fonts.rs` | Loads the system UI font at runtime, with egui's fonts as fallbacks |
| `src/packet_log.rs` | Bounded packet log and payload formatting |
| `src/validators.rs` | Client ID, endpoint, and topic validation |
| `src/error.rs` | Categorized errors with details and suggested actions |
| `src/paths.rs` | Platform home and cache directories |
| `src/app.rs` | egui UI; background work runs on a tokio runtime and reports back over a channel |
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
   parallel, then publishes a GitHub release with the archives, `SHA256SUMS`, and release notes
   generated from the merged pull requests. Follow its progress in the **Actions** tab.

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

## Security

See [SECURITY.md](SECURITY.md) for how to report a security issue. Please don't open a public
issue for security problems.

The app keeps private keys in memory, stores only file paths in its settings, and writes keys it
creates to a user-only cache folder. Treat those files like any other credential.

## Contributing

See [CONTRIBUTING.md](CONTRIBUTING.md) and the [Code of Conduct](CODE_OF_CONDUCT.md).

## License

This project is licensed under the Apache License 2.0; see [LICENSE](LICENSE). Third-party
components and their licenses are listed in [NOTICE](NOTICE).
