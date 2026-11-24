// Copyright 2026 Nick Volgas
// SPDX-License-Identifier: Apache-2.0

//! `mosquitto_pub` / `mosquitto_sub` commands equivalent to a publish or subscribe in the app,
//! so the same action can be repeated from a terminal or a script.
//!
//! The commands use the same endpoint, client ID, protocol version, session settings, last
//! will, and certificate and key *files* as the app's connection. Key contents are never
//! included. See <https://mosquitto.org/man/mosquitto_pub-1.html>.

use crate::mqtt::{LastWill, MqttVersion};

/// Amazon Root CA 1, which AWS IoT Core's ATS endpoints chain to. The command expects it next
/// to where it's run; the first line of the snippet downloads it.
pub const CA_FILE: &str = "AmazonRootCA1.pem";
pub const CA_URL: &str = "https://www.amazontrust.com/repository/AmazonRootCA1.pem";

/// Which shell the command is quoted for.
#[derive(Debug, Clone, Copy, PartialEq, Eq, Default)]
pub enum Shell {
    /// bash and zsh on macOS and Linux.
    #[default]
    Posix,
    /// PowerShell 7.3 or later on Windows (earlier versions mangle quotes in arguments).
    PowerShell,
}

/// The connection settings the command reproduces.
#[derive(Debug, Clone)]
pub struct Connection {
    pub endpoint: String,
    pub port: u16,
    pub client_id: String,
    pub cert_path: String,
    pub key_path: String,
    pub version: MqttVersion,
    pub clean_session: bool,
    /// MQTT 5 only.
    pub session_expiry_secs: Option<u32>,
    pub last_will: Option<LastWill>,
    /// MQTT 5 CONNECT User Properties.
    pub user_properties: Vec<(String, String)>,
}

/// A command line and the shell it's for; rendered with one option per line.
struct Command {
    shell: Shell,
    program: &'static str,
    args: Vec<String>,
}

impl Command {
    fn new(shell: Shell, program: &'static str) -> Self {
        Self {
            shell,
            program,
            args: Vec::new(),
        }
    }

    fn flag(&mut self, flag: &str) {
        self.args.push(flag.to_owned());
    }

    fn opt(&mut self, flag: &str, value: &str) {
        self.args
            .push(format!("{flag} {}", quote(self.shell, value)));
    }

    /// `-D <packet> user-property <name> <value>` for each pair (MQTT 5 only).
    fn user_properties(&mut self, packet: &str, props: &[(String, String)]) {
        for (name, value) in props {
            self.args.push(format!(
                "-D {packet} user-property {} {}",
                quote(self.shell, name),
                quote(self.shell, value)
            ));
        }
    }

    fn render(&self) -> String {
        let continuation = match self.shell {
            Shell::Posix => " \\",
            Shell::PowerShell => " `",
        };
        let mut lines = vec![self.program.to_owned()];
        lines.extend(self.args.iter().map(|a| format!("  {a}")));
        lines.join(&format!("{continuation}\n"))
    }
}

/// Quotes `value` for the shell, leaving simple words bare for readability.
pub fn quote(shell: Shell, value: &str) -> String {
    let plain = !value.is_empty()
        && value
            .chars()
            .all(|c| c.is_ascii_alphanumeric() || "-_./:=@%+,".contains(c));
    if plain {
        return value.to_owned();
    }
    match shell {
        Shell::Posix => format!("'{}'", value.replace('\'', r"'\''")),
        Shell::PowerShell => format!("'{}'", value.replace('\'', "''")),
    }
}

fn connection_options(cmd: &mut Command, c: &Connection) {
    cmd.opt("-h", c.endpoint.trim());
    cmd.opt("-p", &c.port.to_string());
    cmd.opt("-i", &c.client_id);
    cmd.opt(
        "-V",
        match c.version {
            MqttVersion::V5 => "mqttv5",
            MqttVersion::V311 => "mqttv311",
        },
    );
    cmd.opt("--cafile", CA_FILE);
    cmd.opt("--cert", &c.cert_path);
    cmd.opt("--key", &c.key_path);
    if c.port == 443 {
        // AWS IoT Core accepts MQTT over TLS on 443 only with this ALPN protocol name.
        cmd.opt("--tls-alpn", "x-amzn-mqtt-ca");
    }
    if !c.clean_session {
        cmd.flag("-c");
    }
    let v5 = c.version == MqttVersion::V5;
    if let (true, Some(secs)) = (v5, c.session_expiry_secs) {
        cmd.opt("-x", &secs.to_string());
    }
    if v5 {
        cmd.user_properties("connect", &c.user_properties);
    }
    if let Some(w) = &c.last_will {
        cmd.opt("--will-topic", &w.topic);
        cmd.opt("--will-payload", &String::from_utf8_lossy(&w.payload));
        cmd.opt("--will-qos", &w.qos.to_string());
        if w.retain {
            cmd.flag("--will-retain");
        }
        if v5 {
            cmd.user_properties("will", &w.user_properties);
        }
    }
}

/// JSON payloads are sent compact, which is easier to read on one line. Anything else is sent
/// as typed.
fn payload_text(payload: &str) -> String {
    serde_json::from_str::<serde_json::Value>(payload)
        .map(|v| v.to_string())
        .unwrap_or_else(|_| payload.to_owned())
}

fn download_ca(shell: Shell) -> String {
    match shell {
        Shell::Posix => format!("curl -sSO {CA_URL}"),
        Shell::PowerShell => format!("Invoke-WebRequest {CA_URL} -OutFile {CA_FILE}"),
    }
}

/// A `mosquitto_pub` command that publishes one message, preceded by the CA download.
pub fn mosquitto_pub(
    c: &Connection,
    topic: &str,
    qos: u8,
    retain: bool,
    payload: &str,
    user_properties: &[(String, String)],
    shell: Shell,
) -> String {
    let mut cmd = Command::new(shell, "mosquitto_pub");
    connection_options(&mut cmd, c);
    cmd.opt("-t", topic);
    cmd.opt("-q", &qos.to_string());
    if retain {
        cmd.flag("-r");
    }
    if c.version == MqttVersion::V5 {
        cmd.user_properties("publish", user_properties);
    }
    let payload = payload_text(payload);
    if payload.is_empty() {
        cmd.flag("-n");
    } else {
        cmd.opt("-m", &payload);
    }
    format!("{}\n{}", download_ca(shell), cmd.render())
}

/// A `mosquitto_sub` command that subscribes and prints each message with its topic.
pub fn mosquitto_sub(
    c: &Connection,
    filter: &str,
    qos: u8,
    user_properties: &[(String, String)],
    shell: Shell,
) -> String {
    let mut cmd = Command::new(shell, "mosquitto_sub");
    connection_options(&mut cmd, c);
    cmd.opt("-t", filter);
    cmd.opt("-q", &qos.to_string());
    if c.version == MqttVersion::V5 {
        cmd.user_properties("subscribe", user_properties);
    }
    cmd.flag("-v");
    format!("{}\n{}", download_ca(shell), cmd.render())
}

#[cfg(test)]
mod tests {
    use super::*;

    fn conn() -> Connection {
        Connection {
            endpoint: "abc-ats.iot.us-east-1.amazonaws.com".into(),
            port: 8883,
            client_id: "sensor-1".into(),
            cert_path: "/Users/me/Library/Caches/certs/certificate.pem.crt".into(),
            key_path: "/Users/me/My Keys/private.pem.key".into(),
            version: MqttVersion::V5,
            clean_session: true,
            session_expiry_secs: None,
            last_will: None,
            user_properties: Vec::new(),
        }
    }

    #[test]
    fn publish_command_for_posix() {
        let cmd = mosquitto_pub(
            &conn(),
            "sensors/1",
            1,
            false,
            "{\n  \"t\": 21\n}",
            &[],
            Shell::Posix,
        );
        assert_eq!(
            cmd,
            "curl -sSO https://www.amazontrust.com/repository/AmazonRootCA1.pem\n\
             mosquitto_pub \\\n  \
             -h abc-ats.iot.us-east-1.amazonaws.com \\\n  \
             -p 8883 \\\n  \
             -i sensor-1 \\\n  \
             -V mqttv5 \\\n  \
             --cafile AmazonRootCA1.pem \\\n  \
             --cert /Users/me/Library/Caches/certs/certificate.pem.crt \\\n  \
             --key '/Users/me/My Keys/private.pem.key' \\\n  \
             -t sensors/1 \\\n  \
             -q 1 \\\n  \
             -m '{\"t\":21}'"
        );
    }

    #[test]
    fn subscribe_command_carries_session_and_will_settings() {
        let mut c = conn();
        c.version = MqttVersion::V5;
        c.clean_session = false;
        c.session_expiry_secs = Some(3600);
        c.port = 443;
        c.last_will = Some(LastWill {
            topic: "status/sensor-1".into(),
            payload: b"offline".to_vec(),
            qos: 1,
            retain: true,
            user_properties: vec![("reason".into(), "lost power".into())],
        });
        c.user_properties = vec![("app".into(), "aws-iot-mqtt-client".into())];
        let cmd = mosquitto_sub(&c, "sensors/#", 0, &[], Shell::Posix);
        for part in [
            "--tls-alpn x-amzn-mqtt-ca",
            "  -c \\",
            "-x 3600",
            "--will-topic status/sensor-1",
            "--will-payload offline",
            "--will-qos 1",
            "--will-retain",
            "-t 'sensors/#'",
            "-q 0",
            "  -v",
            "-D connect user-property app aws-iot-mqtt-client",
            "-D will user-property reason 'lost power'",
        ] {
            assert!(cmd.contains(part), "missing {part:?} in\n{cmd}");
        }
    }

    #[test]
    fn publish_and_subscribe_user_properties_are_mqtt5_only() {
        let props = vec![("source".to_owned(), "it's me".to_owned())];
        let cmd = mosquitto_pub(&conn(), "t", 0, false, "x", &props, Shell::Posix);
        assert!(
            cmd.contains(r"-D publish user-property source 'it'\''s me'"),
            "{cmd}"
        );
        let cmd = mosquitto_sub(&conn(), "t/#", 0, &props, Shell::PowerShell);
        assert!(
            cmd.contains("-D subscribe user-property source 'it''s me'"),
            "{cmd}"
        );
        let mut v311 = conn();
        v311.version = MqttVersion::V311;
        v311.user_properties = props.clone();
        let cmd = mosquitto_pub(&v311, "t", 0, false, "x", &props, Shell::Posix);
        assert!(!cmd.contains("-D "), "MQTT 3.1.1 has no properties: {cmd}");
    }

    #[test]
    fn mqtt_311_has_no_session_expiry() {
        let mut c = conn();
        c.version = MqttVersion::V311;
        c.session_expiry_secs = Some(60);
        let cmd = mosquitto_sub(&c, "a", 1, &[], Shell::Posix);
        assert!(cmd.contains("-V mqttv311"));
        assert!(!cmd.contains("-x "));
    }

    #[test]
    fn powershell_quoting_and_continuations() {
        let cmd = mosquitto_pub(
            &conn(),
            "it's/here",
            0,
            true,
            "it's",
            &[],
            Shell::PowerShell,
        );
        assert!(cmd.starts_with("Invoke-WebRequest https://"), "{cmd}");
        assert!(cmd.contains("mosquitto_pub `\n"), "{cmd}");
        assert!(cmd.contains("-t 'it''s/here'"), "{cmd}");
        assert!(cmd.contains("-m 'it''s'"), "{cmd}");
        assert!(cmd.contains("  -r `"), "{cmd}");
    }

    #[test]
    fn posix_quoting_handles_single_quotes() {
        assert_eq!(quote(Shell::Posix, "it's"), r"'it'\''s'");
        assert_eq!(quote(Shell::Posix, "plain-word_1.2"), "plain-word_1.2");
        assert_eq!(quote(Shell::Posix, ""), "''");
        assert_eq!(
            quote(Shell::Posix, "$HOME"),
            "'$HOME'",
            "no shell expansion"
        );
    }

    #[test]
    fn empty_payload_sends_a_null_message() {
        let cmd = mosquitto_pub(&conn(), "t", 0, false, "", &[], Shell::Posix);
        assert!(cmd.ends_with("  -n"), "{cmd}");
    }

    #[test]
    fn non_json_payloads_are_kept_as_typed() {
        let cmd = mosquitto_pub(&conn(), "t", 0, false, "hello world", &[], Shell::Posix);
        assert!(cmd.ends_with("-m 'hello world'"), "{cmd}");
    }

    #[test]
    fn private_key_contents_never_appear() {
        let cmd = mosquitto_pub(&conn(), "t", 0, false, "x", &[], Shell::Posix);
        assert!(!cmd.contains("BEGIN"), "only file paths are used");
    }
}
