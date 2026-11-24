// Copyright 2026 Nick Volgas
// SPDX-License-Identifier: Apache-2.0

//! AWS IoT Device Shadows over MQTT, as the connected device sees them.
//!
//! Shadows are read and updated through the reserved `$aws/things/<thing>/shadow/...` topics,
//! so every request is authorized by the device's own policy, like any other publish. See
//! <https://docs.aws.amazon.com/iot/latest/developerguide/device-shadow-mqtt.html>.
//!
//! This module has no UI or network code: it builds topics and payloads, and folds the
//! service's responses into [`Shadows`].

use std::collections::BTreeMap;
use std::time::{Duration, Instant};

use serde_json::{Map, Value};

/// How long to wait for a response to a `get` before reporting that none came.
pub const RESPONSE_TIMEOUT: Duration = Duration::from_secs(10);

/// A Thing's unnamed ("classic") shadow, or one of its named shadows.
#[derive(Debug, Clone, PartialEq, Eq, Hash, PartialOrd, Ord)]
pub enum ShadowName {
    Classic,
    Named(String),
}

impl ShadowName {
    pub fn label(&self) -> &str {
        match self {
            ShadowName::Classic => "Classic",
            ShadowName::Named(n) => n,
        }
    }

    fn prefix(&self, thing: &str) -> String {
        match self {
            ShadowName::Classic => format!("$aws/things/{thing}/shadow"),
            ShadowName::Named(n) => format!("$aws/things/{thing}/shadow/name/{n}"),
        }
    }
}

/// The topic for a request: `get` or `update`.
pub fn request_topic(thing: &str, name: &ShadowName, op: &str) -> String {
    format!("{}/{op}", name.prefix(thing))
}

/// One filter covering every shadow response for the Thing, classic and named.
pub fn subscription_filter(thing: &str) -> String {
    format!("$aws/things/{thing}/shadow/#")
}

/// Named shadow names: 1-64 characters of letters, digits, `:`, `_`, and `-`.
pub fn validate_shadow_name(name: &str) -> Result<(), String> {
    if name.is_empty() || name.len() > 64 {
        return Err("Shadow names are 1-64 characters".into());
    }
    if !name
        .chars()
        .all(|c| c.is_ascii_alphanumeric() || ":_-".contains(c))
    {
        return Err("Shadow names may only contain letters, digits, and : _ -".into());
    }
    Ok(())
}

/// What a message on a shadow topic is.
#[derive(Debug, Clone, Copy, PartialEq, Eq)]
pub enum Response {
    GetAccepted,
    GetRejected,
    UpdateAccepted,
    UpdateRejected,
    UpdateDelta,
    UpdateDocuments,
    /// A request (ours or another client's) echoed to subscribers, or a topic this module
    /// doesn't use, such as `delete/accepted`.
    Other,
}

/// Splits a shadow topic for `thing` into the shadow and the kind of message. `None` if the
/// topic isn't one of this Thing's shadow topics.
pub fn parse_topic(thing: &str, topic: &str) -> Option<(ShadowName, Response)> {
    let rest = topic.strip_prefix(&format!("$aws/things/{thing}/shadow/"))?;
    let (name, rest) = match rest.strip_prefix("name/") {
        Some(named) => {
            let (name, rest) = named.split_once('/')?;
            (ShadowName::Named(name.to_owned()), rest)
        }
        None => (ShadowName::Classic, rest),
    };
    let response = match rest {
        "get/accepted" => Response::GetAccepted,
        "get/rejected" => Response::GetRejected,
        "update/accepted" => Response::UpdateAccepted,
        "update/rejected" => Response::UpdateRejected,
        "update/delta" => Response::UpdateDelta,
        "update/documents" => Response::UpdateDocuments,
        _ => Response::Other,
    };
    Some((name, response))
}

/// Which part of the state an update writes.
#[derive(Debug, Clone, Copy, PartialEq, Eq)]
pub enum Section {
    Desired,
    Reported,
}

impl Section {
    pub fn key(self) -> &'static str {
        match self {
            Section::Desired => "desired",
            Section::Reported => "reported",
        }
    }
}

/// `{"state": {"desired": …}}` from the editor text. The value must be a JSON object, or
/// `null` to clear that section.
pub fn update_payload(section: Section, text: &str) -> Result<Vec<u8>, String> {
    let value: Value = serde_json::from_str(text).map_err(|e| format!("Invalid JSON: {e}"))?;
    if !(value.is_object() || value.is_null()) {
        return Err(format!(
            "The {} state must be a JSON object, or null to clear it",
            section.key()
        ));
    }
    let mut state = Map::new();
    state.insert(section.key().to_owned(), value);
    Ok(serde_json::to_vec(&serde_json::json!({ "state": state })).unwrap_or_default())
}

/// Attributes of `desired` that `reported` doesn't match, compared the way the Device Shadow
/// service builds its delta: objects recursively, everything else (arrays included) by value.
pub fn delta(desired: Option<&Value>, reported: Option<&Value>) -> Option<Value> {
    fn diff(
        desired: &Map<String, Value>,
        reported: Option<&Map<String, Value>>,
    ) -> Map<String, Value> {
        let mut out = Map::new();
        for (key, want) in desired {
            let have = reported.and_then(|r| r.get(key));
            match (want, have) {
                (Value::Object(w), Some(Value::Object(h))) => {
                    let nested = diff(w, Some(h));
                    if !nested.is_empty() {
                        out.insert(key.clone(), Value::Object(nested));
                    }
                }
                (want, Some(have)) if want == have => {}
                (want, _) => {
                    out.insert(key.clone(), want.clone());
                }
            }
        }
        out
    }
    let desired = desired?.as_object()?;
    let out = diff(desired, reported.and_then(Value::as_object));
    (!out.is_empty()).then_some(Value::Object(out))
}

#[derive(Debug, Clone, PartialEq)]
pub enum Status {
    /// A `get` was sent at this time and hasn't been answered.
    Loading(Instant),
    Loaded,
    /// The service says this shadow doesn't exist (yet). An update creates it.
    Missing,
    Error(String),
}

/// One shadow's last known document.
#[derive(Debug, Clone, PartialEq)]
pub struct Shadow {
    pub status: Status,
    pub desired: Option<Value>,
    pub reported: Option<Value>,
    pub delta: Option<Value>,
    pub version: Option<u64>,
    /// Seconds since the Unix epoch, from the last document received.
    pub timestamp: Option<i64>,
    /// Set while an update is waiting for `update/accepted` or `update/rejected`.
    pub updating: bool,
    /// When the pending update was sent, for its timeout.
    pub update_sent: Option<Instant>,
    pub update_error: Option<String>,
}

impl Shadow {
    fn new() -> Self {
        Self {
            status: Status::Missing,
            desired: None,
            reported: None,
            delta: None,
            version: None,
            timestamp: None,
            updating: false,
            update_sent: None,
            update_error: None,
        }
    }

    /// Records that an update was just sent.
    pub fn start_update(&mut self, now: Instant) {
        self.updating = true;
        self.update_sent = Some(now);
        self.update_error = None;
    }

    fn finish_update(&mut self, error: Option<String>) {
        self.updating = false;
        self.update_sent = None;
        self.update_error = error;
    }

    pub fn section(&self, section: Section) -> Option<&Value> {
        match section {
            Section::Desired => self.desired.as_ref(),
            Section::Reported => self.reported.as_ref(),
        }
    }

    /// Takes `desired`/`reported`/`delta`, `version`, and `timestamp` from a full document.
    fn load(&mut self, doc: &Value, delta_from_service: bool) {
        let state = doc.get("state");
        let part = |k: &str| {
            state
                .and_then(|s| s.get(k))
                .filter(|v| !v.is_null())
                .cloned()
        };
        self.desired = part("desired");
        self.reported = part("reported");
        self.delta = if delta_from_service {
            part("delta")
        } else {
            delta(self.desired.as_ref(), self.reported.as_ref())
        };
        self.version = doc.get("version").and_then(Value::as_u64).or(self.version);
        self.status = Status::Loaded;
    }
}

/// What the app should do after a message.
#[derive(Debug, Clone, PartialEq, Eq)]
pub enum Applied {
    /// Not one of this Thing's shadow topics.
    NotShadow,
    Handled,
    /// The shadow changed: send a `get` for its full document.
    Refresh(ShadowName),
}

/// Every shadow of one Thing that the app knows about.
#[derive(Debug, Clone)]
pub struct Shadows {
    pub thing: String,
    pub items: BTreeMap<ShadowName, Shadow>,
    /// Why named shadows couldn't be listed, if they couldn't.
    pub list_error: Option<String>,
}

impl Shadows {
    pub fn new(thing: impl Into<String>) -> Self {
        Self {
            thing: thing.into(),
            items: BTreeMap::new(),
            list_error: None,
        }
    }

    pub fn get_or_insert(&mut self, name: ShadowName) -> &mut Shadow {
        self.items.entry(name).or_insert_with(Shadow::new)
    }

    /// Records that a `get` was just sent.
    pub fn requested(&mut self, name: ShadowName, now: Instant) {
        self.get_or_insert(name).status = Status::Loading(now);
    }

    /// Shadows that exist in the account (loaded, or still being fetched).
    pub fn existing(&self) -> usize {
        self.items
            .values()
            .filter(|s| matches!(s.status, Status::Loaded | Status::Loading(_)))
            .count()
    }

    /// Folds one message into the state.
    pub fn apply(&mut self, topic: &str, payload: &[u8]) -> Applied {
        let Some((name, response)) = parse_topic(&self.thing, topic) else {
            return Applied::NotShadow;
        };
        if response == Response::Other {
            return Applied::Handled;
        }
        let doc: Value = serde_json::from_slice(payload).unwrap_or(Value::Null);
        let timestamp = doc.get("timestamp").and_then(Value::as_i64);
        let shadow = self.get_or_insert(name.clone());
        if timestamp.is_some() {
            shadow.timestamp = timestamp;
        }
        match response {
            Response::GetAccepted => shadow.load(&doc, true),
            Response::GetRejected => {
                let (code, message) = error_of(&doc);
                shadow.status = if code == Some(404) {
                    Status::Missing
                } else {
                    Status::Error(message)
                };
                shadow.desired = None;
                shadow.reported = None;
                shadow.delta = None;
            }
            Response::UpdateAccepted => {
                shadow.finish_update(None);
                return Applied::Refresh(name);
            }
            Response::UpdateRejected => shadow.finish_update(Some(error_of(&doc).1)),
            Response::UpdateDocuments => {
                if let Some(current) = doc.get("current") {
                    shadow.load(current, false);
                }
            }
            Response::UpdateDelta => {
                shadow.delta = doc.get("state").filter(|s| !s.is_null()).cloned();
                if let Some(v) = doc.get("version").and_then(Value::as_u64) {
                    shadow.version = Some(v);
                }
            }
            Response::Other => {}
        }
        Applied::Handled
    }

    /// Fails `get`s and updates unanswered for longer than `timeout`. Returns how many
    /// failed, so the caller can re-establish the shadow subscription.
    pub fn expire(&mut self, now: Instant, timeout: Duration) -> usize {
        const NO_RESPONSE: &str = "No response from the Device Shadow service. Check that the \
            device's policy allows receiving its $aws/things/…/shadow/ topics.";
        let mut expired = 0;
        for shadow in self.items.values_mut() {
            if let Status::Loading(since) = shadow.status {
                if now.duration_since(since) >= timeout {
                    shadow.status = Status::Error(NO_RESPONSE.into());
                    expired += 1;
                }
            }
            if let Some(sent) = shadow.update_sent {
                if now.duration_since(sent) >= timeout {
                    shadow.finish_update(Some(format!(
                        "{NO_RESPONSE} The update may still have been applied."
                    )));
                    expired += 1;
                }
            }
        }
        expired
    }
}

/// `(code, message)` from a rejected response: `{"code": 404, "message": "…"}`.
fn error_of(doc: &Value) -> (Option<i64>, String) {
    let code = doc.get("code").and_then(Value::as_i64);
    let message = doc
        .get("message")
        .and_then(Value::as_str)
        .unwrap_or("Rejected without a message");
    let text = match code {
        Some(c) => format!("{c}: {message}"),
        None => message.to_owned(),
    };
    (code, text)
}

#[cfg(test)]
mod tests {
    use super::*;
    use serde_json::json;

    #[test]
    fn builds_classic_and_named_topics() {
        assert_eq!(
            request_topic("t1", &ShadowName::Classic, "get"),
            "$aws/things/t1/shadow/get"
        );
        assert_eq!(
            request_topic("t1", &ShadowName::Named("config".into()), "update"),
            "$aws/things/t1/shadow/name/config/update"
        );
        assert_eq!(subscription_filter("t1"), "$aws/things/t1/shadow/#");
    }

    #[test]
    fn parses_response_topics() {
        use Response::*;
        let classic = |r| Some((ShadowName::Classic, r));
        assert_eq!(
            parse_topic("t1", "$aws/things/t1/shadow/get/accepted"),
            classic(GetAccepted)
        );
        assert_eq!(
            parse_topic("t1", "$aws/things/t1/shadow/get/rejected"),
            classic(GetRejected)
        );
        assert_eq!(
            parse_topic("t1", "$aws/things/t1/shadow/update/delta"),
            classic(UpdateDelta)
        );
        assert_eq!(
            parse_topic("t1", "$aws/things/t1/shadow/get"),
            classic(Other)
        );
        assert_eq!(
            parse_topic("t1", "$aws/things/t1/shadow/name/cfg/update/documents"),
            Some((ShadowName::Named("cfg".into()), UpdateDocuments))
        );
        assert_eq!(
            parse_topic("t1", "$aws/things/t2/shadow/get/accepted"),
            None
        );
        assert_eq!(parse_topic("t1", "sensors/1"), None);
        assert_eq!(parse_topic("t1", "$aws/things/t1/shadow/name/"), None);
    }

    #[test]
    fn shadow_names_are_validated() {
        assert!(validate_shadow_name("config-1:v2_a").is_ok());
        assert!(validate_shadow_name("").is_err());
        assert!(validate_shadow_name("has space").is_err());
        assert!(validate_shadow_name(&"x".repeat(65)).is_err());
    }

    #[test]
    fn update_payload_wraps_one_section() {
        let p = update_payload(Section::Desired, r#"{"led": "on"}"#).unwrap();
        let v: Value = serde_json::from_slice(&p).unwrap();
        assert_eq!(v, json!({ "state": { "desired": { "led": "on" } } }));
        let p = update_payload(Section::Reported, "null").unwrap();
        let v: Value = serde_json::from_slice(&p).unwrap();
        assert_eq!(v, json!({ "state": { "reported": null } }));
        assert!(update_payload(Section::Desired, "[1]")
            .unwrap_err()
            .contains("object"));
        assert!(update_payload(Section::Desired, "{")
            .unwrap_err()
            .contains("Invalid JSON"));
    }

    #[test]
    fn delta_follows_the_service_rules() {
        let desired =
            json!({ "led": "on", "fan": 2, "cfg": { "a": 1, "b": [1, 2] }, "same": true });
        let reported =
            json!({ "led": "off", "cfg": { "a": 1, "b": [2, 1] }, "same": true, "extra": 1 });
        assert_eq!(
            delta(Some(&desired), Some(&reported)),
            Some(json!({ "led": "on", "fan": 2, "cfg": { "b": [1, 2] } }))
        );
        assert_eq!(delta(Some(&desired), Some(&desired)), None);
        assert_eq!(delta(None, Some(&reported)), None);
        assert_eq!(
            delta(Some(&json!({ "a": 1 })), None),
            Some(json!({ "a": 1 }))
        );
    }

    fn applied(shadows: &mut Shadows, suffix: &str, payload: Value) -> Applied {
        let topic = format!("$aws/things/t1/shadow/{suffix}");
        shadows.apply(&topic, payload.to_string().as_bytes())
    }

    #[test]
    fn get_accepted_loads_the_document() {
        let mut s = Shadows::new("t1");
        s.requested(ShadowName::Classic, Instant::now());
        assert_eq!(s.existing(), 1);
        let doc = json!({
            "state": { "desired": { "led": "on" }, "reported": { "led": "off" }, "delta": { "led": "on" } },
            "version": 7, "timestamp": 1_700_000_000,
        });
        assert_eq!(applied(&mut s, "get/accepted", doc), Applied::Handled);
        let c = &s.items[&ShadowName::Classic];
        assert_eq!(c.status, Status::Loaded);
        assert_eq!(c.version, Some(7));
        assert_eq!(c.timestamp, Some(1_700_000_000));
        assert_eq!(c.delta, Some(json!({ "led": "on" })));
        assert_eq!(c.section(Section::Reported), Some(&json!({ "led": "off" })));
    }

    #[test]
    fn missing_and_failed_gets() {
        let mut s = Shadows::new("t1");
        applied(
            &mut s,
            "get/rejected",
            json!({ "code": 404, "message": "No shadow exists" }),
        );
        assert_eq!(s.items[&ShadowName::Classic].status, Status::Missing);
        assert_eq!(s.existing(), 0);
        applied(
            &mut s,
            "name/cfg/get/rejected",
            json!({ "code": 403, "message": "Forbidden" }),
        );
        assert_eq!(
            s.items[&ShadowName::Named("cfg".into())].status,
            Status::Error("403: Forbidden".into())
        );
    }

    #[test]
    fn updates_report_results_and_ask_for_a_refresh() {
        let mut s = Shadows::new("t1");
        s.get_or_insert(ShadowName::Classic).updating = true;
        assert_eq!(
            applied(
                &mut s,
                "update/accepted",
                json!({ "state": { "desired": { "a": 1 } }, "version": 2 })
            ),
            Applied::Refresh(ShadowName::Classic)
        );
        assert!(!s.items[&ShadowName::Classic].updating);

        s.get_or_insert(ShadowName::Classic).updating = true;
        applied(
            &mut s,
            "update/rejected",
            json!({ "code": 400, "message": "Missing required node: state" }),
        );
        let c = &s.items[&ShadowName::Classic];
        assert!(!c.updating);
        assert_eq!(
            c.update_error.as_deref(),
            Some("400: Missing required node: state")
        );
    }

    #[test]
    fn documents_and_delta_messages_update_the_state() {
        let mut s = Shadows::new("t1");
        let docs = json!({
            "previous": { "state": {}, "version": 1 },
            "current": { "state": { "desired": { "a": 2 }, "reported": { "a": 1 } }, "version": 2 },
            "timestamp": 1_700_000_100,
        });
        applied(&mut s, "name/cfg/update/documents", docs);
        let cfg = &s.items[&ShadowName::Named("cfg".into())];
        assert_eq!(cfg.version, Some(2));
        assert_eq!(cfg.delta, Some(json!({ "a": 2 })), "computed locally");

        applied(
            &mut s,
            "name/cfg/update/delta",
            json!({ "state": { "a": 3 }, "version": 3 }),
        );
        let cfg = &s.items[&ShadowName::Named("cfg".into())];
        assert_eq!(cfg.delta, Some(json!({ "a": 3 })));
        assert_eq!(cfg.version, Some(3));
    }

    #[test]
    fn echoed_requests_and_other_topics_are_ignored() {
        let mut s = Shadows::new("t1");
        assert_eq!(applied(&mut s, "get", json!({})), Applied::Handled);
        assert_eq!(s.apply("sensors/1", b"{}"), Applied::NotShadow);
        assert_eq!(
            s.apply("$aws/things/t1/shadow/get/accepted", b"not json"),
            Applied::Handled
        );
    }

    #[test]
    fn unanswered_updates_time_out() {
        let mut s = Shadows::new("t1");
        let start = Instant::now();
        s.get_or_insert(ShadowName::Classic).start_update(start);
        assert_eq!(
            s.expire(start + Duration::from_secs(1), RESPONSE_TIMEOUT),
            0
        );
        assert!(s.items[&ShadowName::Classic].updating);
        assert_eq!(s.expire(start + RESPONSE_TIMEOUT, RESPONSE_TIMEOUT), 1);
        let c = &s.items[&ShadowName::Classic];
        assert!(!c.updating, "Send update is usable again");
        assert!(c
            .update_error
            .as_deref()
            .unwrap()
            .contains("may still have been applied"));
        assert_eq!(
            s.expire(start + RESPONSE_TIMEOUT * 2, RESPONSE_TIMEOUT),
            0,
            "only once"
        );
    }

    #[test]
    fn unanswered_gets_time_out() {
        let mut s = Shadows::new("t1");
        let start = Instant::now();
        s.requested(ShadowName::Classic, start);
        s.expire(start + Duration::from_secs(1), RESPONSE_TIMEOUT);
        assert!(matches!(
            s.items[&ShadowName::Classic].status,
            Status::Loading(_)
        ));
        s.expire(start + RESPONSE_TIMEOUT, RESPONSE_TIMEOUT);
        assert!(
            matches!(&s.items[&ShadowName::Classic].status, Status::Error(e) if e.contains("policy"))
        );
    }
}
