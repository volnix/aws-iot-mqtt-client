// Copyright 2026 Nick Volgas
// SPDX-License-Identifier: Apache-2.0

//! Exports the MQTT log as a trace to share: JSON for tools, or text that reads like the log.
//!
//! Only what's in the log is exported (cleared entries are gone). PUBLISH and RECEIVE packets
//! include the full topic, payload, and User Properties when the log kept them (text payloads
//! up to 64 KB); otherwise only the shortened log line. No keys, certificate paths, or account
//! details are included.

use jiff::ZonedDateTime;
use serde_json::{json, Map, Value};

use crate::packet_log::{Direction, PacketLogEntry, Replay};

/// Identifies the format, so a reader can tell what it's looking at.
pub const FORMAT: &str = "aws-iot-mqtt-client-trace";
pub const FORMAT_VERSION: u32 = 1;

/// About the connection the trace came from.
#[derive(Debug, Clone)]
pub struct TraceInfo {
    pub exported_at: ZonedDateTime,
    pub endpoint: Option<String>,
    pub client_id: Option<String>,
    /// "MQTT 5" or "MQTT 3.1.1".
    pub protocol: String,
}

fn time(t: &ZonedDateTime) -> String {
    t.strftime("%Y-%m-%dT%H:%M:%S%.3f%:z").to_string()
}

fn direction(d: Direction) -> &'static str {
    match d {
        Direction::Sent => "sent",
        Direction::Received => "received",
    }
}

fn properties_json(props: &[(String, String)]) -> Value {
    Value::Array(
        props
            .iter()
            .map(|(name, value)| json!({ "name": name, "value": value }))
            .collect(),
    )
}

/// The trace as pretty-printed JSON.
pub fn to_json(info: &TraceInfo, entries: &[&PacketLogEntry]) -> String {
    let packets: Vec<Value> = entries
        .iter()
        .map(|e| {
            let mut packet = Map::new();
            packet.insert("time".into(), time(&e.timestamp).into());
            packet.insert("direction".into(), direction(e.direction).into());
            packet.insert("type".into(), e.kind.clone().into());
            packet.insert("details".into(), e.details.clone().into());
            match &e.replay {
                Some(Replay::Publish {
                    topic,
                    qos,
                    retain,
                    payload,
                    user_properties,
                }) => {
                    packet.insert("topic".into(), topic.clone().into());
                    packet.insert("qos".into(), (*qos).into());
                    packet.insert("retain".into(), (*retain).into());
                    packet.insert("payload".into(), payload.clone().into());
                    if !user_properties.is_empty() {
                        packet.insert("user_properties".into(), properties_json(user_properties));
                    }
                }
                Some(Replay::Subscribe {
                    filter,
                    qos,
                    user_properties,
                }) => {
                    packet.insert("topic_filter".into(), filter.clone().into());
                    packet.insert("qos".into(), (*qos).into());
                    if !user_properties.is_empty() {
                        packet.insert("user_properties".into(), properties_json(user_properties));
                    }
                }
                None => {}
            }
            Value::Object(packet)
        })
        .collect();
    let doc = json!({
        "format": FORMAT,
        "format_version": FORMAT_VERSION,
        "app_version": env!("CARGO_PKG_VERSION"),
        "exported_at": time(&info.exported_at),
        "endpoint": info.endpoint,
        "client_id": info.client_id,
        "protocol": info.protocol,
        "packet_count": packets.len(),
        "packets": packets,
    });
    serde_json::to_string_pretty(&doc).unwrap_or_default() + "\n"
}

/// The trace as text: a short header, then one line per packet like the MQTT log, with the
/// full payload and User Properties under each PUBLISH and RECEIVE.
pub fn to_text(info: &TraceInfo, entries: &[&PacketLogEntry]) -> String {
    let mut out = format!(
        "AWS IoT MQTT Client trace (app {})\nExported:  {}\nEndpoint:  {}\nClient ID: {}\nProtocol:  {}\nPackets:   {}\n\n",
        env!("CARGO_PKG_VERSION"),
        time(&info.exported_at),
        info.endpoint.as_deref().unwrap_or("-"),
        info.client_id.as_deref().unwrap_or("-"),
        info.protocol,
        entries.len(),
    );
    for e in entries {
        let arrow = match e.direction {
            Direction::Sent => "->",
            Direction::Received => "<-",
        };
        out.push_str(&format!(
            "{} {arrow} {:<11} {}\n",
            e.timestamp.strftime("%H:%M:%S%.3f"),
            e.kind,
            e.details
        ));
        if let Some(Replay::Publish {
            payload,
            user_properties,
            ..
        }) = &e.replay
        {
            for (name, value) in user_properties {
                out.push_str(&format!("    property {name}: {value}\n"));
            }
            for line in payload.lines() {
                out.push_str(&format!("    | {line}\n"));
            }
        }
    }
    out
}

/// A file name such as `mqtt-trace-sensor-1-20261124-153000.json`.
pub fn file_name(info: &TraceInfo, extension: &str) -> String {
    let client: String = info
        .client_id
        .as_deref()
        .unwrap_or("client")
        .chars()
        .map(|c| {
            if c.is_ascii_alphanumeric() || "-_".contains(c) {
                c
            } else {
                '_'
            }
        })
        .collect();
    format!(
        "mqtt-trace-{client}-{}.{extension}",
        info.exported_at.strftime("%Y%m%d-%H%M%S")
    )
}

#[cfg(test)]
mod tests {
    use super::*;

    fn info() -> TraceInfo {
        TraceInfo {
            exported_at: ZonedDateTime::now(),
            endpoint: Some("abc-ats.iot.us-east-1.amazonaws.com".into()),
            client_id: Some("sensor/1".into()),
            protocol: "MQTT 5".into(),
        }
    }

    fn entries() -> Vec<PacketLogEntry> {
        let props = vec![("source".to_owned(), "test".to_owned())];
        vec![
            PacketLogEntry::new(Direction::Sent, "CONNECT", "client_id=sensor/1"),
            PacketLogEntry::new(Direction::Sent, "SUBSCRIBE", "pkid=1 filter=a/#").with_replay(
                Some(Replay::Subscribe {
                    filter: "a/#".into(),
                    qos: 1,
                    user_properties: vec![],
                }),
            ),
            PacketLogEntry::new(Direction::Received, "RECEIVE", "topic=a/b payload={\"t\":…")
                .with_replay(Replay::publish(
                    "a/b",
                    1,
                    false,
                    b"{\"t\":21,\n\"u\":1}",
                    &props,
                )),
            PacketLogEntry::new(Direction::Received, "PUBACK", "pkid=2"),
        ]
    }

    #[test]
    fn json_trace_has_metadata_and_full_payloads() {
        let entries = entries();
        let refs: Vec<_> = entries.iter().collect();
        let doc: Value = serde_json::from_str(&to_json(&info(), &refs)).unwrap();
        assert_eq!(doc["format"], FORMAT);
        assert_eq!(doc["packet_count"], 4);
        assert_eq!(doc["client_id"], "sensor/1");
        let packets = doc["packets"].as_array().unwrap();
        assert_eq!(packets[0]["direction"], "sent");
        assert_eq!(packets[1]["topic_filter"], "a/#");
        let receive = &packets[2];
        assert_eq!(receive["type"], "RECEIVE");
        assert_eq!(
            receive["payload"], "{\"t\":21,\n\"u\":1}",
            "full, not the log's short form"
        );
        assert_eq!(receive["user_properties"][0]["name"], "source");
        assert!(packets[3].get("payload").is_none());
        assert!(receive["time"].as_str().unwrap().contains('T'));
    }

    #[test]
    fn text_trace_reads_like_the_log() {
        let entries = entries();
        let refs: Vec<_> = entries.iter().collect();
        let text = to_text(&info(), &refs);
        assert!(text.starts_with("AWS IoT MQTT Client trace"), "{text}");
        assert!(text.contains("Packets:   4"), "{text}");
        assert!(text.contains("-> CONNECT"), "{text}");
        assert!(text.contains("<- RECEIVE"), "{text}");
        assert!(text.contains("    property source: test\n"), "{text}");
        assert!(
            text.contains("    | {\"t\":21,\n    | \"u\":1}\n"),
            "{text}"
        );
    }

    #[test]
    fn file_names_are_safe() {
        let name = file_name(&info(), "json");
        assert!(name.starts_with("mqtt-trace-sensor_1-"), "{name}");
        assert!(name.ends_with(".json"));
    }

    #[test]
    fn empty_traces_are_valid() {
        let doc: Value = serde_json::from_str(&to_json(&info(), &[])).unwrap();
        assert_eq!(doc["packet_count"], 0);
    }
}
