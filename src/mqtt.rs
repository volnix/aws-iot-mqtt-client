// Copyright Amazon.com, Inc. or its affiliates. All Rights Reserved.
// SPDX-License-Identifier: Apache-2.0

//! MQTT 3.1.1 or MQTT 5 connection to AWS IoT Core using mutual TLS (x.509 client certificate).
//!
//! A [`Connection`] owns a background tokio task that drives the rumqttc event loop (`rumqttc`
//! for 3.1.1, `rumqttc::v5` for 5) and reports state changes, packet traffic, and received
//! messages through an [`EventSink`]. Auto-reconnect is intentionally disabled: when the
//! connection drops, the task ends and reports the error.

use std::collections::VecDeque;
use std::sync::atomic::{AtomicBool, Ordering};
use std::sync::{Arc, Mutex};
use std::time::Duration;

use chrono::{DateTime, Local};
use rumqttc::{Outgoing, TlsConfiguration, Transport};
use rustls::{ClientConfig, RootCertStore};
use serde::{Deserialize, Serialize};
use tokio::runtime::Handle;
use tokio::task::JoinHandle;

use crate::error::{categorize_message, AppError, ErrorCategory};
use crate::packet_log::{format_payload, Direction, PacketLogEntry};
use crate::validators::{
    validate_client_id, validate_endpoint, validate_publish_topic, validate_topic_filter,
};

pub const DEFAULT_PORT: u16 = 8883;
pub const CONNECT_TIMEOUT: Duration = Duration::from_secs(30);
pub const DISCONNECT_TIMEOUT: Duration = Duration::from_secs(5);
pub const KEEP_ALIVE: Duration = Duration::from_secs(30);
/// AWS IoT Core's maximum session expiry (7 days); the account default is 1 hour.
pub const MAX_SESSION_EXPIRY_SECS: u32 = 7 * 24 * 60 * 60;
const LOG_PAYLOAD_MAX: usize = 512;

/// Amazon Trust Services root CAs. AWS IoT ATS endpoints chain to one of these.
const AMAZON_ROOT_CAS: &[&str] = &[
    include_str!("../assets/AmazonRootCA1.pem"),
    include_str!("../assets/AmazonRootCA2.pem"),
    include_str!("../assets/AmazonRootCA3.pem"),
    include_str!("../assets/AmazonRootCA4.pem"),
];

#[derive(Debug, Clone, Copy, PartialEq, Eq, Default, Serialize, Deserialize)]
pub enum MqttVersion {
    /// MQTT 3.1.1
    V311,
    #[default]
    V5,
}

impl MqttVersion {
    pub fn label(self) -> &'static str {
        match self {
            MqttVersion::V311 => "MQTT 3.1.1",
            MqttVersion::V5 => "MQTT 5",
        }
    }

    /// "Clean session" in 3.1.1, "Clean start" in 5.
    pub fn clean_label(self) -> &'static str {
        match self {
            MqttVersion::V311 => "Clean session",
            MqttVersion::V5 => "Clean start",
        }
    }
}

#[derive(Debug, Clone, Copy, PartialEq, Eq)]
pub enum ConnectionState {
    Disconnected,
    Connecting,
    Connected,
    Disconnecting,
    Error,
}

impl ConnectionState {
    pub fn label(self) -> &'static str {
        match self {
            ConnectionState::Disconnected => "Disconnected",
            ConnectionState::Connecting => "Connecting…",
            ConnectionState::Connected => "Connected",
            ConnectionState::Disconnecting => "Disconnecting…",
            ConnectionState::Error => "Error",
        }
    }

    /// True while a connection task is (or may be) alive.
    pub fn is_active(self) -> bool {
        matches!(
            self,
            ConnectionState::Connecting
                | ConnectionState::Connected
                | ConnectionState::Disconnecting
        )
    }
}

/// Message the broker publishes on the client's behalf if it disconnects ungracefully.
#[derive(Debug, Clone, PartialEq, Eq)]
pub struct LastWill {
    pub topic: String,
    pub payload: Vec<u8>,
    pub qos: u8,
    pub retain: bool,
}

#[derive(Clone)]
pub struct ConnectionConfig {
    pub endpoint: String,
    pub port: u16,
    pub client_id: String,
    pub certificate_pem: Vec<u8>,
    pub private_key_pem: Vec<u8>,
    pub version: MqttVersion,
    /// Clean session (3.1.1) / clean start (5).
    pub clean_session: bool,
    /// MQTT 5 only: seconds the broker keeps the session after disconnect. `None` = not sent.
    pub session_expiry_secs: Option<u32>,
    pub last_will: Option<LastWill>,
}

impl std::fmt::Debug for ConnectionConfig {
    fn fmt(&self, f: &mut std::fmt::Formatter<'_>) -> std::fmt::Result {
        f.debug_struct("ConnectionConfig")
            .field("endpoint", &self.endpoint)
            .field("port", &self.port)
            .field("client_id", &self.client_id)
            .field("version", &self.version)
            .field("clean_session", &self.clean_session)
            .field("session_expiry_secs", &self.session_expiry_secs)
            .field("last_will", &self.last_will)
            .field("private_key_pem", &"<redacted>")
            .finish_non_exhaustive()
    }
}

impl ConnectionConfig {
    pub fn validate(&self) -> Result<(), AppError> {
        validate_endpoint(&self.endpoint).map_err(AppError::configuration)?;
        validate_client_id(&self.client_id).map_err(AppError::configuration)?;
        if self.port == 0 {
            return Err(AppError::configuration("Port must be between 1 and 65535"));
        }
        if let Some(will) = &self.last_will {
            validate_publish_topic(&will.topic)
                .map_err(|e| AppError::configuration(format!("Last will topic: {e}")))?;
        }
        if self
            .session_expiry_secs
            .is_some_and(|s| s > MAX_SESSION_EXPIRY_SECS)
        {
            return Err(AppError::configuration(format!(
                "Session expiry can be at most {MAX_SESSION_EXPIRY_SECS} seconds (7 days)"
            )));
        }
        Ok(())
    }

    /// One-line summary for the CONNECT log entry.
    fn describe(&self) -> String {
        let mut s = format!(
            "mqtts://{}:{} client_id={} {}={} keep_alive={}s",
            self.endpoint.trim(),
            self.port,
            self.client_id,
            match self.version {
                MqttVersion::V311 => "clean_session",
                MqttVersion::V5 => "clean_start",
            },
            self.clean_session,
            KEEP_ALIVE.as_secs()
        );
        if self.version == MqttVersion::V5 {
            if let Some(exp) = self.session_expiry_secs {
                s.push_str(&format!(" session_expiry={exp}s"));
            }
        }
        if let Some(w) = &self.last_will {
            s.push_str(&format!(
                " will(topic={} qos={} retain={} payload={})",
                w.topic,
                w.qos,
                w.retain,
                format_payload(&w.payload, 64)
            ));
        }
        s.push_str(&format!(" ({})", self.version.label()));
        s
    }
}

#[derive(Debug, Clone)]
pub struct ReceivedMessage {
    pub timestamp: DateTime<Local>,
    pub topic: String,
    pub payload: Vec<u8>,
    pub qos: u8,
    pub retain: bool,
}

#[derive(Debug, Clone)]
pub enum MqttEvent {
    State {
        state: ConnectionState,
        error: Option<AppError>,
    },
    Packet(PacketLogEntry),
    Message(ReceivedMessage),
}

/// Receives events tagged with the connection ID that produced them.
pub type EventSink = Arc<dyn Fn(u64, MqttEvent) + Send + Sync>;

pub fn qos_v5(q: u8) -> rumqttc::v5::mqttbytes::QoS {
    use rumqttc::v5::mqttbytes::QoS;
    match q {
        0 => QoS::AtMostOnce,
        1 => QoS::AtLeastOnce,
        _ => QoS::ExactlyOnce,
    }
}

pub fn qos_v4(q: u8) -> rumqttc::QoS {
    use rumqttc::QoS;
    match q {
        0 => QoS::AtMostOnce,
        1 => QoS::AtLeastOnce,
        _ => QoS::ExactlyOnce,
    }
}

/// The protocol-specific client handle.
#[derive(Clone)]
enum Client {
    V4(rumqttc::AsyncClient),
    V5(rumqttc::v5::AsyncClient),
}

impl Client {
    fn try_publish(
        &self,
        topic: &str,
        qos: u8,
        retain: bool,
        payload: Vec<u8>,
    ) -> Result<(), String> {
        match self {
            Client::V4(c) => c
                .try_publish(topic, qos_v4(qos), retain, payload)
                .map_err(|e| e.to_string()),
            Client::V5(c) => c
                .try_publish(topic, qos_v5(qos), retain, payload)
                .map_err(|e| e.to_string()),
        }
    }

    fn try_subscribe(&self, filter: &str, qos: u8) -> Result<(), String> {
        match self {
            Client::V4(c) => c
                .try_subscribe(filter, qos_v4(qos))
                .map_err(|e| e.to_string()),
            Client::V5(c) => c
                .try_subscribe(filter, qos_v5(qos))
                .map_err(|e| e.to_string()),
        }
    }

    fn try_unsubscribe(&self, filter: &str) -> Result<(), String> {
        match self {
            Client::V4(c) => c.try_unsubscribe(filter).map_err(|e| e.to_string()),
            Client::V5(c) => c.try_unsubscribe(filter).map_err(|e| e.to_string()),
        }
    }

    fn try_disconnect(&self) -> Result<(), String> {
        match self {
            Client::V4(c) => c.try_disconnect().map_err(|e| e.to_string()),
            Client::V5(c) => c.try_disconnect().map_err(|e| e.to_string()),
        }
    }
}

/// Shared by both protocol drivers: sends events and tracks queued request descriptions.
#[derive(Clone)]
struct Reporter {
    id: u64,
    sink: EventSink,
    /// Descriptions of queued PUBLISH/SUBSCRIBE/UNSUBSCRIBE requests, consumed in FIFO order when
    /// the event loop reports them sent (the request channel preserves ordering).
    pending: Arc<Mutex<VecDeque<String>>>,
    closing: Arc<AtomicBool>,
}

impl Reporter {
    fn state(&self, state: ConnectionState, error: Option<AppError>) {
        (self.sink)(self.id, MqttEvent::State { state, error });
    }

    fn packet(&self, direction: Direction, kind: String, details: String) {
        (self.sink)(
            self.id,
            MqttEvent::Packet(PacketLogEntry::new(direction, kind, details)),
        );
    }

    fn message(&self, topic: String, payload: Vec<u8>, qos: u8, retain: bool) {
        (self.sink)(
            self.id,
            MqttEvent::Message(ReceivedMessage {
                timestamp: Local::now(),
                topic,
                payload,
                qos,
                retain,
            }),
        );
    }

    /// Logs an outgoing packet. Returns true for DISCONNECT, which ends the session: it has
    /// already been flushed to the socket when rumqttc reports it.
    fn outgoing(&self, out: &Outgoing) -> bool {
        let (kind, details) = describe_outgoing(out, &self.pending);
        self.packet(Direction::Sent, kind, details);
        if matches!(out, Outgoing::Disconnect) {
            self.state(ConnectionState::Disconnected, None);
            return true;
        }
        false
    }

    /// Reports the error that ended the event loop.
    fn ended(&self, error: AppError) {
        if self.closing.load(Ordering::SeqCst) {
            self.state(ConnectionState::Disconnected, None);
        } else {
            self.state(ConnectionState::Error, Some(error));
        }
    }
}

/// Live connection handle. Dropping it does not close the connection; call [`Connection::disconnect`].
pub struct Connection {
    pub id: u64,
    pub version: MqttVersion,
    client: Client,
    task: Arc<Mutex<Option<JoinHandle<()>>>>,
    reporter: Reporter,
    rt: Handle,
}

impl Connection {
    /// Validates the config and starts connecting in the background.
    pub fn start(
        rt: &Handle,
        id: u64,
        config: ConnectionConfig,
        sink: EventSink,
    ) -> Result<Self, AppError> {
        config.validate()?;
        let tls = build_tls_config(&config.certificate_pem, &config.private_key_pem)?;
        let reporter = Reporter {
            id,
            sink,
            pending: Arc::default(),
            closing: Arc::new(AtomicBool::new(false)),
        };

        reporter.state(ConnectionState::Connecting, None);
        reporter.packet(Direction::Sent, "CONNECT".into(), config.describe());

        let (client, task) = match config.version {
            MqttVersion::V5 => {
                let (client, eventloop) = v5_client(&config, tls);
                (
                    Client::V5(client),
                    rt.spawn(run_v5(eventloop, reporter.clone())),
                )
            }
            MqttVersion::V311 => {
                let (client, eventloop) = v4_client(&config, tls);
                (
                    Client::V4(client),
                    rt.spawn(run_v4(eventloop, reporter.clone())),
                )
            }
        };

        Ok(Self {
            id,
            version: config.version,
            client,
            task: Arc::new(Mutex::new(Some(task))),
            reporter,
            rt: rt.clone(),
        })
    }

    /// Sends DISCONNECT and waits up to [`DISCONNECT_TIMEOUT`] before force-closing.
    pub fn disconnect(&self) {
        if self.reporter.closing.swap(true, Ordering::SeqCst) {
            return;
        }
        self.reporter.state(ConnectionState::Disconnecting, None);

        let client = self.client.clone();
        let task = self.task.clone();
        let reporter = self.reporter.clone();
        self.rt.spawn(async move {
            // If the request can't be queued (loop already gone), the timeout path below handles it.
            let _ = client.try_disconnect();
            let handle = task.lock().ok().and_then(|mut t| t.take());
            if let Some(mut handle) = handle {
                if tokio::time::timeout(DISCONNECT_TIMEOUT, &mut handle)
                    .await
                    .is_err()
                {
                    handle.abort();
                    reporter.packet(
                        Direction::Sent,
                        "DISCONNECT".into(),
                        format!(
                            "No clean shutdown within {}s; connection closed forcefully",
                            DISCONNECT_TIMEOUT.as_secs()
                        ),
                    );
                    reporter.state(ConnectionState::Disconnected, None);
                }
            }
        });
    }

    pub fn publish(
        &self,
        topic: &str,
        qos: u8,
        retain: bool,
        payload: Vec<u8>,
    ) -> Result<(), AppError> {
        validate_publish_topic(topic).map_err(AppError::configuration)?;
        let description = format!(
            "topic={topic} qos={qos} retain={retain} payload={}",
            format_payload(&payload, LOG_PAYLOAD_MAX)
        );
        self.enqueue(description, || {
            self.client.try_publish(topic, qos, retain, payload)
        })
    }

    pub fn subscribe(&self, filter: &str, qos: u8) -> Result<(), AppError> {
        validate_topic_filter(filter).map_err(AppError::configuration)?;
        self.enqueue(format!("filter={filter} qos={qos}"), || {
            self.client.try_subscribe(filter, qos)
        })
    }

    pub fn unsubscribe(&self, filter: &str) -> Result<(), AppError> {
        self.enqueue(format!("filter={filter}"), || {
            self.client.try_unsubscribe(filter)
        })
    }

    fn enqueue(
        &self,
        description: String,
        send: impl FnOnce() -> Result<(), String>,
    ) -> Result<(), AppError> {
        // Hold the lock across the send so descriptions stay in the same order as requests.
        let mut pending = self.reporter.pending.lock().expect("pending lock poisoned");
        pending.push_back(description);
        send().map_err(|e| {
            pending.pop_back();
            AppError::network("Could not queue MQTT request").with_details(e)
        })
    }
}

// ---------------------------------------------------------------------------------------------
// MQTT 5
// ---------------------------------------------------------------------------------------------

fn v5_client(
    config: &ConnectionConfig,
    tls: Arc<ClientConfig>,
) -> (rumqttc::v5::AsyncClient, rumqttc::v5::EventLoop) {
    let mut options =
        rumqttc::v5::MqttOptions::new(&config.client_id, config.endpoint.trim(), config.port);
    options
        .set_keep_alive(KEEP_ALIVE)
        .set_clean_start(config.clean_session)
        .set_connection_timeout(CONNECT_TIMEOUT.as_secs())
        .set_session_expiry_interval(config.session_expiry_secs)
        .set_transport(Transport::Tls(TlsConfiguration::Rustls(tls)));
    if let Some(w) = &config.last_will {
        options.set_last_will(rumqttc::v5::mqttbytes::v5::LastWill::new(
            &w.topic,
            w.payload.clone(),
            qos_v5(w.qos),
            w.retain,
            None,
        ));
    }
    rumqttc::v5::AsyncClient::new(options, 64)
}

async fn run_v5(mut eventloop: rumqttc::v5::EventLoop, r: Reporter) {
    use rumqttc::v5::mqttbytes::v5::Packet;
    use rumqttc::v5::{ConnectionError, Event};

    let mut connected = false;
    loop {
        let polled = if connected {
            eventloop.poll().await
        } else {
            // rumqttc applies CONNECT_TIMEOUT internally; this is a backstop.
            match tokio::time::timeout(CONNECT_TIMEOUT + Duration::from_secs(2), eventloop.poll())
                .await
            {
                Ok(res) => res,
                Err(_) => Err(ConnectionError::Timeout(timeout_elapsed().await)),
            }
        };
        match polled {
            Ok(Event::Incoming(packet)) => {
                let (kind, details) = describe_incoming_v5(&packet);
                r.packet(Direction::Received, kind, details);
                match packet {
                    Packet::ConnAck(_) if !connected => {
                        connected = true;
                        r.state(ConnectionState::Connected, None);
                    }
                    Packet::Publish(p) => r.message(
                        String::from_utf8_lossy(&p.topic).into_owned(),
                        p.payload.to_vec(),
                        p.qos as u8,
                        p.retain,
                    ),
                    _ => {}
                }
            }
            Ok(Event::Outgoing(out)) => {
                if r.outgoing(&out) {
                    break;
                }
            }
            Err(e) => {
                r.ended(categorize_connection_error(&e, connected));
                break;
            }
        }
    }
}

fn describe_incoming_v5(packet: &rumqttc::v5::mqttbytes::v5::Packet) -> (String, String) {
    use rumqttc::v5::mqttbytes::v5::Packet;
    match packet {
        Packet::ConnAck(a) => (
            "CONNACK".into(),
            format!("reason={:?} session_present={}", a.code, a.session_present),
        ),
        Packet::Publish(p) => (
            "PUBLISH".into(),
            format!(
                "topic={} qos={} retain={} pkid={} payload={}",
                String::from_utf8_lossy(&p.topic),
                p.qos as u8,
                p.retain,
                p.pkid,
                format_payload(&p.payload, LOG_PAYLOAD_MAX)
            ),
        ),
        Packet::PubAck(a) => (
            "PUBACK".into(),
            format!("pkid={} reason={:?}", a.pkid, a.reason),
        ),
        Packet::PubRec(a) => ("PUBREC".into(), format!("pkid={}", a.pkid)),
        Packet::PubRel(a) => ("PUBREL".into(), format!("pkid={}", a.pkid)),
        Packet::PubComp(a) => ("PUBCOMP".into(), format!("pkid={}", a.pkid)),
        Packet::SubAck(a) => (
            "SUBACK".into(),
            format!("pkid={} codes={:?}", a.pkid, a.return_codes),
        ),
        Packet::UnsubAck(a) => (
            "UNSUBACK".into(),
            format!("pkid={} reasons={:?}", a.pkid, a.reasons),
        ),
        Packet::PingResp(_) => ("PINGRESP".into(), String::new()),
        Packet::PingReq(_) => ("PINGREQ".into(), String::new()),
        Packet::Disconnect(d) => ("DISCONNECT".into(), format!("reason={:?}", d.reason_code)),
        Packet::Auth(_) => ("AUTH".into(), String::new()),
        Packet::Connect(..) => ("CONNECT".into(), String::new()),
        Packet::Subscribe(_) => ("SUBSCRIBE".into(), String::new()),
        Packet::Unsubscribe(_) => ("UNSUBSCRIBE".into(), String::new()),
    }
}

// ---------------------------------------------------------------------------------------------
// MQTT 3.1.1
// ---------------------------------------------------------------------------------------------

fn v4_client(
    config: &ConnectionConfig,
    tls: Arc<ClientConfig>,
) -> (rumqttc::AsyncClient, rumqttc::EventLoop) {
    let mut options =
        rumqttc::MqttOptions::new(&config.client_id, config.endpoint.trim(), config.port);
    options
        .set_keep_alive(KEEP_ALIVE)
        .set_clean_session(config.clean_session)
        .set_transport(Transport::Tls(TlsConfiguration::Rustls(tls)));
    if let Some(w) = &config.last_will {
        options.set_last_will(rumqttc::LastWill::new(
            &w.topic,
            w.payload.clone(),
            qos_v4(w.qos),
            w.retain,
        ));
    }
    let (client, mut eventloop) = rumqttc::AsyncClient::new(options, 64);
    eventloop
        .network_options
        .set_connection_timeout(CONNECT_TIMEOUT.as_secs());
    (client, eventloop)
}

async fn run_v4(mut eventloop: rumqttc::EventLoop, r: Reporter) {
    use rumqttc::{ConnectionError, Event, Packet};

    let mut connected = false;
    loop {
        let polled = if connected {
            eventloop.poll().await
        } else {
            // rumqttc applies CONNECT_TIMEOUT internally; this is a backstop.
            match tokio::time::timeout(CONNECT_TIMEOUT + Duration::from_secs(2), eventloop.poll())
                .await
            {
                Ok(res) => res,
                Err(_) => Err(ConnectionError::NetworkTimeout),
            }
        };
        match polled {
            Ok(Event::Incoming(packet)) => {
                let (kind, details) = describe_incoming_v4(&packet);
                r.packet(Direction::Received, kind, details);
                match packet {
                    Packet::ConnAck(_) if !connected => {
                        connected = true;
                        r.state(ConnectionState::Connected, None);
                    }
                    Packet::Publish(p) => {
                        r.message(p.topic.clone(), p.payload.to_vec(), p.qos as u8, p.retain)
                    }
                    _ => {}
                }
            }
            Ok(Event::Outgoing(out)) => {
                if r.outgoing(&out) {
                    break;
                }
            }
            Err(e) => {
                r.ended(categorize_v4_error(&e, connected));
                break;
            }
        }
    }
}

fn describe_incoming_v4(packet: &rumqttc::Packet) -> (String, String) {
    use rumqttc::Packet;
    match packet {
        Packet::ConnAck(a) => (
            "CONNACK".into(),
            format!("code={:?} session_present={}", a.code, a.session_present),
        ),
        Packet::Publish(p) => (
            "PUBLISH".into(),
            format!(
                "topic={} qos={} retain={} pkid={} payload={}",
                p.topic,
                p.qos as u8,
                p.retain,
                p.pkid,
                format_payload(&p.payload, LOG_PAYLOAD_MAX)
            ),
        ),
        Packet::PubAck(a) => ("PUBACK".into(), format!("pkid={}", a.pkid)),
        Packet::PubRec(a) => ("PUBREC".into(), format!("pkid={}", a.pkid)),
        Packet::PubRel(a) => ("PUBREL".into(), format!("pkid={}", a.pkid)),
        Packet::PubComp(a) => ("PUBCOMP".into(), format!("pkid={}", a.pkid)),
        Packet::SubAck(a) => (
            "SUBACK".into(),
            format!("pkid={} codes={:?}", a.pkid, a.return_codes),
        ),
        Packet::UnsubAck(a) => ("UNSUBACK".into(), format!("pkid={}", a.pkid)),
        Packet::PingResp => ("PINGRESP".into(), String::new()),
        Packet::PingReq => ("PINGREQ".into(), String::new()),
        Packet::Disconnect => ("DISCONNECT".into(), String::new()),
        Packet::Connect(_) => ("CONNECT".into(), String::new()),
        Packet::Subscribe(_) => ("SUBSCRIBE".into(), String::new()),
        Packet::Unsubscribe(_) => ("UNSUBSCRIBE".into(), String::new()),
    }
}

// ---------------------------------------------------------------------------------------------
// Shared helpers
// ---------------------------------------------------------------------------------------------

/// Produces a real `Elapsed` value for the backstop timeout path.
async fn timeout_elapsed() -> tokio::time::error::Elapsed {
    tokio::time::timeout(Duration::ZERO, std::future::pending::<()>())
        .await
        .unwrap_err()
}

fn describe_outgoing(out: &Outgoing, pending: &Mutex<VecDeque<String>>) -> (String, String) {
    let take = || {
        pending
            .lock()
            .ok()
            .and_then(|mut p| p.pop_front())
            .unwrap_or_default()
    };
    match out {
        Outgoing::Publish(pkid) => ("PUBLISH".into(), format!("pkid={pkid} {}", take())),
        Outgoing::Subscribe(pkid) => ("SUBSCRIBE".into(), format!("pkid={pkid} {}", take())),
        Outgoing::Unsubscribe(pkid) => ("UNSUBSCRIBE".into(), format!("pkid={pkid} {}", take())),
        Outgoing::PubAck(pkid) => ("PUBACK".into(), format!("pkid={pkid}")),
        Outgoing::PubRec(pkid) => ("PUBREC".into(), format!("pkid={pkid}")),
        Outgoing::PubRel(pkid) => ("PUBREL".into(), format!("pkid={pkid}")),
        Outgoing::PubComp(pkid) => ("PUBCOMP".into(), format!("pkid={pkid}")),
        Outgoing::PingReq => ("PINGREQ".into(), String::new()),
        Outgoing::PingResp => ("PINGRESP".into(), String::new()),
        Outgoing::Disconnect => ("DISCONNECT".into(), "normal disconnection".into()),
        Outgoing::AwaitAck(pkid) => ("AWAITACK".into(), format!("pkid={pkid}")),
    }
}

const POLICY_HINT: &str =
    "AWS IoT closes the connection without a reason when the certificate isn't \
ACTIVE, isn't attached to a policy that allows iot:Connect for this client ID, or another client \
connected with the same client ID.";

/// Trust anchors: the Amazon Root CAs (used by every AWS IoT endpoint) plus the Mozilla root
/// set, so custom-domain endpoints whose server certificate comes from another public CA work.
fn root_store() -> RootCertStore {
    let mut roots = RootCertStore::empty();
    let pem = AMAZON_ROOT_CAS.concat();
    for cert in rustls_pemfile::certs(&mut pem.as_bytes()).flatten() {
        // Bundled certificates are verified by a unit test; a failure here can't happen.
        let _ = roots.add(cert);
    }
    roots.extend(webpki_roots::TLS_SERVER_ROOTS.iter().cloned());
    roots
}

/// Builds the mutual-TLS client config. Fails early (before any network I/O) if the PEMs are
/// unreadable or the private key doesn't belong to the certificate.
pub fn build_tls_config(
    certificate_pem: &[u8],
    private_key_pem: &[u8],
) -> Result<Arc<ClientConfig>, AppError> {
    let certs = rustls_pemfile::certs(&mut &certificate_pem[..])
        .collect::<Result<Vec<_>, _>>()
        .map_err(|e| {
            AppError::authentication("Could not read the certificate").with_details(e.to_string())
        })?;
    if certs.is_empty() {
        return Err(AppError::authentication(
            "The certificate file has no CERTIFICATE block",
        ));
    }
    let key = rustls_pemfile::private_key(&mut &private_key_pem[..])
        .map_err(|e| {
            AppError::authentication("Could not read the private key").with_details(e.to_string())
        })?
        .ok_or_else(|| AppError::authentication("The private key file has no PRIVATE KEY block"))?;

    let config = ClientConfig::builder()
        .with_root_certificates(root_store())
        .with_client_auth_cert(certs, key)
        .map_err(|e| {
            AppError::authentication("The certificate and private key can't be used together")
                .with_details(e.to_string())
                .with_action(
                    "Make sure the private key is the one generated with this certificate.",
                )
        })?;
    Ok(Arc::new(config))
}

/// Protocol-independent shape of a connection failure.
enum Failure {
    Refused {
        category: ErrorCategory,
        code: String,
    },
    Tls,
    Timeout,
    ServerDisconnect(String),
    Other,
}

fn failure_to_error(failure: Failure, details: String, was_connected: bool) -> AppError {
    match failure {
        Failure::Refused { category, code } => {
            AppError::new(category, format!("Broker refused the connection: {code}"))
                .with_details(details)
        }
        Failure::Tls => AppError::authentication("TLS setup or handshake failed")
            .with_details(details)
            .with_action("Verify the certificate and private key are a matching pair and the certificate is registered with AWS IoT."),
        Failure::Timeout => AppError::network(format!(
            "Connection timed out after {} seconds",
            CONNECT_TIMEOUT.as_secs()
        ))
        .with_details(details),
        Failure::ServerDisconnect(reason) => {
            AppError::network(format!("Broker closed the connection: {reason}"))
                .with_details(details)
                .with_action(POLICY_HINT)
        }
        Failure::Other => {
            let category = match categorize_message(&details) {
                ErrorCategory::Authentication => ErrorCategory::Authentication,
                _ => ErrorCategory::Network,
            };
            let message = if was_connected { "Connection lost" } else { "Connection failed" };
            let e = AppError::new(category, message).with_details(details);
            if category == ErrorCategory::Network {
                e.with_action(format!("Check your network connection. {POLICY_HINT}"))
            } else {
                e
            }
        }
    }
}

/// Maps an MQTT 5 event-loop error to a user-facing error.
pub fn categorize_connection_error(
    err: &rumqttc::v5::ConnectionError,
    was_connected: bool,
) -> AppError {
    use rumqttc::v5::mqttbytes::v5::ConnectReturnCode as Code;
    use rumqttc::v5::ConnectionError as E;
    let failure = match err {
        E::ConnectionRefused(code) => Failure::Refused {
            category: match code {
                Code::NotAuthorized
                | Code::BadUserNamePassword
                | Code::BadAuthenticationMethod
                | Code::Banned => ErrorCategory::Authentication,
                Code::ClientIdentifierNotValid => ErrorCategory::Configuration,
                _ => ErrorCategory::Network,
            },
            code: format!("{code:?}"),
        },
        E::Tls(_) => Failure::Tls,
        E::Timeout(_) => Failure::Timeout,
        E::MqttState(rumqttc::v5::StateError::ServerDisconnect { reason_code, .. }) => {
            Failure::ServerDisconnect(format!("{reason_code:?}"))
        }
        _ => Failure::Other,
    };
    failure_to_error(failure, err.to_string(), was_connected)
}

/// Maps an MQTT 3.1.1 event-loop error to a user-facing error.
pub fn categorize_v4_error(err: &rumqttc::ConnectionError, was_connected: bool) -> AppError {
    use rumqttc::ConnectReturnCode as Code;
    use rumqttc::ConnectionError as E;
    let failure = match err {
        E::ConnectionRefused(code) => Failure::Refused {
            category: match code {
                Code::NotAuthorized | Code::BadUserNamePassword => ErrorCategory::Authentication,
                Code::BadClientId => ErrorCategory::Configuration,
                _ => ErrorCategory::Network,
            },
            code: format!("{code:?}"),
        },
        E::Tls(_) => Failure::Tls,
        E::NetworkTimeout | E::FlushTimeout => Failure::Timeout,
        _ => Failure::Other,
    };
    failure_to_error(failure, err.to_string(), was_connected)
}

#[cfg(test)]
mod tests {
    use super::*;

    fn config() -> ConnectionConfig {
        ConnectionConfig {
            endpoint: "abc-ats.iot.us-east-1.amazonaws.com".into(),
            port: DEFAULT_PORT,
            client_id: "thing-1".into(),
            certificate_pem: vec![],
            private_key_pem: b"SECRET".to_vec(),
            version: MqttVersion::V5,
            clean_session: true,
            session_expiry_secs: None,
            last_will: None,
        }
    }

    #[test]
    fn config_validation() {
        assert!(config().validate().is_ok());
        let mut c = config();
        c.client_id = "bad id".into();
        assert_eq!(
            c.validate().unwrap_err().category,
            ErrorCategory::Configuration
        );
        let mut c = config();
        c.endpoint = "mqtts://x".into();
        assert!(c.validate().is_err());
        let mut c = config();
        c.port = 0;
        assert!(c.validate().is_err());
    }

    #[test]
    fn last_will_and_expiry_validation() {
        let mut c = config();
        c.last_will = Some(LastWill {
            topic: "status/+".into(),
            payload: vec![],
            qos: 1,
            retain: false,
        });
        assert!(c.validate().unwrap_err().message.contains("Last will"));
        c.last_will.as_mut().unwrap().topic = "status/offline".into();
        assert!(c.validate().is_ok());
        c.session_expiry_secs = Some(MAX_SESSION_EXPIRY_SECS + 1);
        assert!(c.validate().is_err());
    }

    #[test]
    fn connect_description_reflects_options() {
        let mut c = config();
        c.session_expiry_secs = Some(3600);
        c.clean_session = false;
        c.last_will = Some(LastWill {
            topic: "lwt".into(),
            payload: b"bye".to_vec(),
            qos: 0,
            retain: true,
        });
        let d = c.describe();
        assert!(d.contains("clean_start=false"));
        assert!(d.contains("session_expiry=3600s"));
        assert!(d.contains("will(topic=lwt qos=0 retain=true payload=bye)"));
        assert!(d.ends_with("(MQTT 5)"));

        c.version = MqttVersion::V311;
        let d = c.describe();
        assert!(d.contains("clean_session=false"));
        assert!(!d.contains("session_expiry"), "expiry is MQTT 5 only");
        assert!(d.ends_with("(MQTT 3.1.1)"));
    }

    #[test]
    fn config_debug_redacts_key() {
        assert!(!format!("{:?}", config()).contains("SECRET"));
    }

    #[test]
    fn bundled_cas_parse() {
        let pem = AMAZON_ROOT_CAS.concat();
        let certs: Vec<_> = rustls_pemfile::certs(&mut pem.as_bytes())
            .collect::<Result<_, _>>()
            .unwrap();
        assert_eq!(certs.len(), 4);
    }

    /// A self-signed certificate and its private key (PEM).
    fn test_identity() -> (Vec<u8>, Vec<u8>) {
        let ck = rcgen::generate_simple_self_signed(vec!["device".into()]).unwrap();
        (
            ck.cert.pem().into_bytes(),
            ck.signing_key.serialize_pem().into_bytes(),
        )
    }

    #[test]
    fn tls_config_accepts_matching_pair() {
        let (cert, key) = test_identity();
        build_tls_config(&cert, &key).unwrap();
    }

    #[test]
    fn tls_config_rejects_mismatched_key() {
        let (cert, _) = test_identity();
        let (_, other_key) = test_identity();
        let err = build_tls_config(&cert, &other_key).unwrap_err();
        assert_eq!(err.category, ErrorCategory::Authentication);
    }

    #[test]
    fn root_store_has_amazon_and_public_roots() {
        assert!(root_store().len() > AMAZON_ROOT_CAS.len());
    }

    #[test]
    fn refused_not_authorized_is_auth_error() {
        let e = categorize_connection_error(
            &rumqttc::v5::ConnectionError::ConnectionRefused(
                rumqttc::v5::mqttbytes::v5::ConnectReturnCode::NotAuthorized,
            ),
            false,
        );
        assert_eq!(e.category, ErrorCategory::Authentication);
        let e = categorize_v4_error(
            &rumqttc::ConnectionError::ConnectionRefused(rumqttc::ConnectReturnCode::NotAuthorized),
            false,
        );
        assert_eq!(e.category, ErrorCategory::Authentication);
        let e = categorize_v4_error(
            &rumqttc::ConnectionError::ConnectionRefused(rumqttc::ConnectReturnCode::BadClientId),
            false,
        );
        assert_eq!(e.category, ErrorCategory::Configuration);
    }

    #[test]
    fn io_error_after_connect_is_connection_lost() {
        let io = std::io::Error::new(std::io::ErrorKind::ConnectionReset, "reset by peer");
        let e = categorize_connection_error(&rumqttc::v5::ConnectionError::Io(io), true);
        assert_eq!(e.category, ErrorCategory::Network);
        assert_eq!(e.message, "Connection lost");
        assert!(e.action.unwrap().contains("iot:Connect"));
        let e = categorize_v4_error(&rumqttc::ConnectionError::NetworkTimeout, false);
        assert!(e.message.contains("timed out"));
    }

    #[test]
    fn outgoing_descriptions_consume_pending_in_order() {
        let pending = Mutex::new(VecDeque::from(["first".to_string(), "second".to_string()]));
        assert!(describe_outgoing(&Outgoing::Publish(1), &pending)
            .1
            .ends_with("first"));
        assert!(describe_outgoing(&Outgoing::Subscribe(2), &pending)
            .1
            .ends_with("second"));
        assert_eq!(describe_outgoing(&Outgoing::PingReq, &pending).0, "PINGREQ");
    }

    #[test]
    fn qos_mapping() {
        assert_eq!(qos_v5(0), rumqttc::v5::mqttbytes::QoS::AtMostOnce);
        assert_eq!(qos_v5(1), rumqttc::v5::mqttbytes::QoS::AtLeastOnce);
        assert_eq!(qos_v4(1), rumqttc::QoS::AtLeastOnce);
        assert_eq!(qos_v4(2), rumqttc::QoS::ExactlyOnce);
    }

    /// Connecting to an unresolvable host must end in an Error state (not hang or panic),
    /// for both protocol versions.
    #[test]
    fn unreachable_endpoint_reports_error() {
        for version in [MqttVersion::V5, MqttVersion::V311] {
            let rt = tokio::runtime::Runtime::new().unwrap();
            let (tx, rx) = std::sync::mpsc::channel();
            let sink: EventSink = Arc::new(move |id, ev| {
                let _ = tx.send((id, ev));
            });
            let (cert, key) = test_identity();
            let mut c = config();
            c.endpoint = "nonexistent.invalid".into();
            c.certificate_pem = cert;
            c.private_key_pem = key;
            c.version = version;
            c.last_will = Some(LastWill {
                topic: "lwt".into(),
                payload: b"gone".to_vec(),
                qos: 1,
                retain: false,
            });
            let _conn = Connection::start(rt.handle(), 7, c, sink).unwrap();

            let deadline = std::time::Instant::now() + Duration::from_secs(40);
            loop {
                let (id, ev) = rx
                    .recv_timeout(deadline - std::time::Instant::now())
                    .expect("no terminal event");
                assert_eq!(id, 7);
                if let MqttEvent::State {
                    state: ConnectionState::Error,
                    error,
                } = ev
                {
                    assert!(error.is_some(), "{version:?}");
                    break;
                }
            }
        }
    }
}
