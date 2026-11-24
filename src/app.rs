// Copyright 2026 Nick Volgas
// SPDX-License-Identifier: Apache-2.0

//! egui front end. All network work runs on the tokio runtime; results come back over a channel.

use std::collections::VecDeque;
use std::path::{Path, PathBuf};
use std::sync::mpsc::{self, Receiver, Sender};
use std::sync::Arc;
use std::time::Duration;

use egui::{Color32, Margin, RichText};
use serde::{Deserialize, Serialize};
use tokio::runtime::Runtime;

use crate::aws::{
    self, ApiLogSink, ConnectionInfo, DataEndpoint, EndpointDiscovery, Session, SubscriptionInfo,
    ThingPage, ThingSummary,
};
use crate::certs::{self, LocalCertificate, LocalPrivateKey};
use crate::error::AppError;
use crate::fonts;
use crate::icons;
use crate::mqtt::{
    self, Connection, ConnectionConfig, ConnectionState, EventSink, LastWill, MqttEvent,
    MqttVersion, ReceivedMessage,
};
use crate::packet_log::{format_payload, Direction, PacketLog, PacketLogEntry, Replay};
use crate::provision::{
    self, ArnScope, Identity, PolicyChoice, ProvisionOutcome, ProvisionRequest,
};
use crate::theme::{self, Palette, Severity, ThemeWatcher};
use crate::trace_export;
use crate::validators::{topic_matches, validate_client_id};

mod cli_dialog;
mod shadows;
mod simulator;
use cli_dialog::CliDialog;
use shadows::ShadowState;
use simulator::{SimSettings, SimState};

const MAX_MESSAGES: usize = 500;
/// Payload bytes shown per message before truncating.
const MESSAGE_PAYLOAD_MAX: usize = 8 * 1024;

/// The top-level tabs.
#[derive(Debug, Clone, Copy, PartialEq, Eq, Default, Serialize, Deserialize)]
enum View {
    #[default]
    Client,
    Shadows,
    Simulator,
}

/// Which theme to use. `System` follows the desktop's light/dark setting.
#[derive(Debug, Clone, Copy, PartialEq, Eq, Default, Serialize, Deserialize)]
enum ThemeChoice {
    #[default]
    System,
    Light,
    Dark,
}

impl ThemeChoice {
    fn label(self) -> &'static str {
        match self {
            ThemeChoice::System => "Auto",
            ThemeChoice::Light => "Light",
            ThemeChoice::Dark => "Dark",
        }
    }

    fn icon(self) -> &'static str {
        match self {
            ThemeChoice::System => icons::THEME_AUTO,
            ThemeChoice::Light => icons::THEME_LIGHT,
            ThemeChoice::Dark => icons::THEME_DARK,
        }
    }

    /// The next choice when cycling with the top-bar button.
    fn next(self) -> Self {
        match self {
            ThemeChoice::System => ThemeChoice::Light,
            ThemeChoice::Light => ThemeChoice::Dark,
            ThemeChoice::Dark => ThemeChoice::System,
        }
    }

    fn preference(self) -> egui::ThemePreference {
        match self {
            ThemeChoice::System => egui::ThemePreference::System,
            ThemeChoice::Light => egui::ThemePreference::Light,
            ThemeChoice::Dark => egui::ThemePreference::Dark,
        }
    }
}

/// Who the client connects as.
#[derive(Debug, Clone, Copy, PartialEq, Eq, Default, Serialize, Deserialize)]
enum IdentityMode {
    /// An AWS IoT Thing from the account; the client ID is the Thing name.
    #[default]
    Thing,
    /// A client ID typed by the user, with no Thing involved.
    ClientId,
}

/// Persisted between runs. Holds file *paths* only — never certificate or key contents.
#[derive(Serialize, Deserialize)]
#[serde(default)]
struct Settings {
    profile: String,
    region: String,
    mode: IdentityMode,
    thing: Option<String>,
    custom_client_id: String,
    endpoint: String,
    /// True when the user typed an endpoint instead of picking a discovered one.
    endpoint_custom: bool,
    port: u16,
    cert_path: Option<PathBuf>,
    key_path: Option<PathBuf>,
    subscribe_filter: String,
    publish_topic: String,
    mqtt_version: MqttVersion,
    /// Clean session (3.1.1) / clean start (5).
    clean_session: bool,
    /// MQTT 5 only. 0 = not sent (the session ends when the connection closes).
    session_expiry_secs: u32,
    will_enabled: bool,
    will_topic: String,
    will_payload: String,
    will_qos: u8,
    will_retain: bool,
    /// MQTT 5 User Properties, by packet. Rows with an empty name aren't sent.
    connect_user_properties: Vec<(String, String)>,
    will_user_properties: Vec<(String, String)>,
    publish_user_properties: Vec<(String, String)>,
    /// Sent with SUBSCRIBE and UNSUBSCRIBE.
    subscription_user_properties: Vec<(String, String)>,
    /// Ask GetConnection for client/endpoint IP addresses and ports.
    include_socket_info: bool,
    theme: ThemeChoice,
    /// Whether the logs drawer is open.
    logs_open: bool,
    view: View,
    sim: SimSettings,
}

impl Default for Settings {
    fn default() -> Self {
        let env = |k: &str| std::env::var(k).ok().filter(|v| !v.is_empty());
        Self {
            profile: env("AWS_PROFILE").unwrap_or_default(),
            region: env("AWS_REGION")
                .or_else(|| env("AWS_DEFAULT_REGION"))
                .unwrap_or_default(),
            mode: IdentityMode::Thing,
            thing: None,
            custom_client_id: String::new(),
            endpoint: String::new(),
            endpoint_custom: false,
            port: mqtt::DEFAULT_PORT,
            cert_path: None,
            key_path: None,
            subscribe_filter: String::new(),
            publish_topic: String::new(),
            mqtt_version: MqttVersion::V5,
            clean_session: true,
            session_expiry_secs: 0,
            will_enabled: false,
            will_topic: String::new(),
            will_payload: String::new(),
            will_qos: 0,
            will_retain: false,
            connect_user_properties: Vec::new(),
            will_user_properties: Vec::new(),
            publish_user_properties: Vec::new(),
            subscription_user_properties: Vec::new(),
            include_socket_info: false,
            theme: ThemeChoice::default(),
            logs_open: false,
            view: View::Client,
            sim: SimSettings::default(),
        }
    }
}

enum AppMsg {
    Session {
        generation: u64,
        result: Result<Session, AppError>,
    },
    Endpoints {
        generation: u64,
        result: Result<EndpointDiscovery, AppError>,
    },
    Things {
        generation: u64,
        result: Result<ThingPage, AppError>,
    },
    ThingLookup {
        generation: u64,
        result: Result<ThingSummary, AppError>,
    },
    Mqtt {
        connection_id: u64,
        event: MqttEvent,
    },
    Api(PacketLogEntry),
    /// `GetConnection` result for a live MQTT connection.
    LiveConnection {
        connection_id: u64,
        result: Result<ConnectionInfo, AppError>,
    },
    /// `ListSubscriptions` result for a live MQTT connection.
    LiveSubscriptions {
        connection_id: u64,
        result: Result<Vec<SubscriptionInfo>, AppError>,
    },
    Policies {
        identity: Identity,
        result: Result<Vec<String>, AppError>,
    },
    Provisioned {
        identity: Identity,
        policy: String,
        outcome: ProvisionOutcome,
    },
    /// Policy names for the simulator's "Load from account".
    SimPolicies {
        result: Result<Vec<String>, AppError>,
    },
    SimPolicyDocument {
        name: String,
        result: Result<String, AppError>,
    },
    /// `ListNamedShadowsForThing` for the Thing a connection is using.
    NamedShadows {
        connection_id: u64,
        result: Result<Vec<String>, AppError>,
    },
}

#[derive(Default)]
struct AwsState {
    generation: u64,
    session_loading: bool,
    session: Option<Session>,
    error: Option<AppError>,
    endpoints: Vec<DataEndpoint>,
    endpoints_loading: bool,
    endpoints_error: Option<AppError>,
    things: Vec<ThingSummary>,
    /// The account has more Things than the first page shows.
    things_has_more: bool,
    things_loading: bool,
    things_error: Option<AppError>,
    filter: String,
    lookup_loading: bool,
    lookup_error: Option<AppError>,
}

/// Broker-side details for the current MQTT connection, fetched only after CONNACK.
#[derive(Default)]
struct LiveState {
    info: Option<Result<ConnectionInfo, AppError>>,
    info_loading: bool,
    subscriptions: Option<Result<Vec<SubscriptionInfo>, AppError>>,
    subscriptions_loading: bool,
}

impl AwsState {
    fn busy(&self) -> bool {
        self.session_loading || self.endpoints_loading || self.things_loading
    }
}

#[derive(Debug, Clone, Copy, PartialEq, Eq)]
enum LogTab {
    Mqtt,
    Api,
}

/// State of the "Create certificate" dialog.
struct ProvisionDialog {
    identity: Identity,
    session: Session,
    create_new: bool,
    policies: Option<Result<Vec<String>, AppError>>,
    existing: String,
    policy_filter: String,
    new_name: String,
    document: String,
    default_document: String,
    storage_root: PathBuf,
    running: bool,
    error: Option<AppError>,
    /// Whether the policy JSON editor is expanded.
    show_document: bool,
}

impl ProvisionDialog {
    fn policy_choice(&self) -> PolicyChoice {
        if self.create_new {
            PolicyChoice::New {
                name: self.new_name.trim().to_owned(),
                document: self.document.clone(),
            }
        } else {
            PolicyChoice::Existing(self.existing.clone())
        }
    }

    fn validation_error(&self) -> Option<String> {
        if self.create_new {
            provision::validate_policy_name(self.new_name.trim())
                .and_then(|_| provision::validate_policy_document(&self.document))
                .err()
        } else if self.existing.is_empty() {
            Some("Choose a policy to attach".into())
        } else {
            None
        }
    }
}

pub struct MqttApp {
    rt: Arc<Runtime>,
    tx: Sender<AppMsg>,
    rx: Receiver<AppMsg>,
    settings: Settings,
    profiles: Vec<String>,
    aws: AwsState,
    started: bool,

    cert: Option<Result<LocalCertificate, AppError>>,
    key: Option<Result<LocalPrivateKey, AppError>>,

    connection: Option<Connection>,
    next_connection_id: u64,
    state: ConnectionState,
    connection_error: Option<AppError>,
    connected_as: Option<(String, String)>,

    subscriptions: Vec<String>,
    subscribe_qos: u8,
    publish_qos: u8,
    publish_retain: bool,
    publish_payload: String,
    action_error: Option<AppError>,
    messages: VecDeque<ReceivedMessage>,

    log: PacketLog,
    api_log: PacketLog,
    log_tab: LogTab,
    hide_pings: bool,
    show_cert_details: bool,
    /// Read-only connection settings, shown from the connected bar.
    show_conn_details: bool,
    /// Session and last-will options in the setup form.
    show_advanced: bool,
    /// Profile/region pickers once the account is loaded.
    account_open: bool,
    /// The less common GetConnection fields.
    show_more_info: bool,
    /// The composer's and subscribe row's User Properties editors.
    show_publish_props: bool,
    show_subscription_props: bool,
    /// Re-applies `assets/theme.json` on save (debug builds, or AWS_IOT_MQTT_THEME_FILE).
    theme_watcher: Option<ThemeWatcher>,
    sim: SimState,
    live: LiveState,
    shadow: ShadowState,

    provision: Option<ProvisionDialog>,
    /// "Copy as mosquitto_pub/sub" dialog.
    cli: Option<CliDialog>,
    notice: Option<String>,

    /// Tests: open sessions from this SDK config (an offline fake) instead of the real chain.
    #[cfg(test)]
    test_sdk_config: Option<aws_config::SdkConfig>,
    /// Tests: trust these roots (a local test broker's CA) instead of the bundled ones.
    #[cfg(test)]
    test_tls_roots: Option<rustls::RootCertStore>,
    /// Tests: save exported traces here instead of asking with a file dialog.
    #[cfg(test)]
    test_export_path: Option<PathBuf>,
}

impl MqttApp {
    pub fn new(cc: &eframe::CreationContext<'_>, rt: Arc<Runtime>) -> Self {
        let settings: Settings = cc
            .storage
            .and_then(|s| eframe::get_value(s, eframe::APP_KEY))
            .unwrap_or_default();
        Self::with_settings(&cc.egui_ctx, rt, settings)
    }

    /// Builds the app for a context. Separate from [`MqttApp::new`] so tests can render the UI
    /// headlessly without a window.
    fn with_settings(ctx: &egui::Context, rt: Arc<Runtime>, settings: Settings) -> Self {
        fonts::install(ctx);
        theme::install(ctx);
        ctx.set_theme(settings.theme.preference());
        let (tx, rx) = mpsc::channel();
        let mut app = Self {
            rt,
            tx,
            rx,
            profiles: aws::list_profiles(),
            aws: AwsState::default(),
            started: false,
            cert: None,
            key: None,
            connection: None,
            next_connection_id: 1,
            state: ConnectionState::Disconnected,
            connection_error: None,
            connected_as: None,
            subscriptions: Vec::new(),
            subscribe_qos: 1,
            publish_qos: 1,
            publish_retain: false,
            publish_payload: "{\n  \"message\": \"hello from aws-iot-mqtt-client\"\n}".into(),
            action_error: None,
            messages: VecDeque::new(),
            log: PacketLog::default(),
            api_log: PacketLog::default(),
            log_tab: LogTab::Mqtt,
            hide_pings: true,
            show_cert_details: false,
            show_conn_details: false,
            show_advanced: false,
            account_open: false,
            show_more_info: false,
            show_publish_props: false,
            show_subscription_props: false,
            theme_watcher: ThemeWatcher::for_build(),
            sim: SimState::default(),
            live: LiveState::default(),
            shadow: ShadowState::default(),
            provision: None,
            cli: None,
            notice: None,
            #[cfg(test)]
            test_sdk_config: None,
            #[cfg(test)]
            test_tls_roots: None,
            #[cfg(test)]
            test_export_path: None,
            settings,
        };
        if let Some(p) = app.settings.cert_path.clone() {
            app.load_certificate(&p);
        }
        if let Some(p) = app.settings.key_path.clone() {
            app.load_private_key(&p);
        }
        app
    }

    /// The client ID used to connect: the Thing name in Thing mode, otherwise the typed ID.
    fn effective_client_id(&self) -> Option<String> {
        match self.settings.mode {
            IdentityMode::Thing => self.settings.thing.clone(),
            IdentityMode::ClientId => {
                Some(self.settings.custom_client_id.trim().to_owned()).filter(|c| !c.is_empty())
            }
        }
    }

    fn current_identity(&self) -> Option<Identity> {
        match self.settings.mode {
            IdentityMode::Thing => self.settings.thing.clone().map(Identity::Thing),
            IdentityMode::ClientId => self
                .effective_client_id()
                .filter(|c| validate_client_id(c).is_ok())
                .map(Identity::ClientId),
        }
    }

    fn api_log_sink(&self, ctx: &egui::Context) -> ApiLogSink {
        let tx = self.tx.clone();
        let ctx = ctx.clone();
        Arc::new(move |entry| {
            let _ = tx.send(AppMsg::Api(entry));
            ctx.request_repaint();
        })
    }

    // -----------------------------------------------------------------------------------------
    // Background work
    // -----------------------------------------------------------------------------------------

    fn load_from_aws(&mut self, ctx: &egui::Context) {
        let previous_filter = std::mem::take(&mut self.aws.filter);
        self.aws = AwsState {
            generation: self.aws.generation + 1,
            session_loading: true,
            endpoints_loading: true,
            things_loading: true,
            filter: previous_filter,
            ..AwsState::default()
        };
        self.profiles = aws::list_profiles();

        let generation = self.aws.generation;
        let profile = self.settings.profile.clone();
        let region = self.settings.region.clone();
        let log = self.api_log_sink(ctx);
        let tx = self.tx.clone();
        let ctx = ctx.clone();
        #[cfg(test)]
        let test_config = self.test_sdk_config.clone();
        self.rt.spawn(async move {
            let send = |msg| {
                let _ = tx.send(msg);
                ctx.request_repaint();
            };
            #[cfg(test)]
            let opened = match test_config {
                Some(config) => aws::session_from_config(config, Some(&profile), log).await,
                None => aws::open_session(Some(&profile), Some(&region), log).await,
            };
            #[cfg(not(test))]
            let opened = aws::open_session(Some(&profile), Some(&region), log).await;
            let session = match opened {
                Ok(s) => s,
                Err(e) => {
                    return send(AppMsg::Session {
                        generation,
                        result: Err(e),
                    })
                }
            };
            send(AppMsg::Session {
                generation,
                result: Ok(session.clone()),
            });
            let (endpoints, things) = tokio::join!(
                session.list_data_endpoints(),
                session.list_things_first_page()
            );
            send(AppMsg::Endpoints {
                generation,
                result: endpoints,
            });
            send(AppMsg::Things {
                generation,
                result: things,
            });
        });
    }

    /// Selects a Thing. No AWS calls: its name is all the connection needs.
    fn select_thing(&mut self, name: String) {
        self.settings.thing = Some(name);
    }

    /// Looks up a Thing that isn't on the first page by its exact name (`DescribeThing`).
    fn lookup_thing(&mut self, ctx: &egui::Context, name: String) {
        let Some(session) = self.aws.session.clone() else {
            return;
        };
        self.aws.lookup_loading = true;
        self.aws.lookup_error = None;
        let generation = self.aws.generation;
        let tx = self.tx.clone();
        let ctx = ctx.clone();
        self.rt.spawn(async move {
            let result = session.describe_thing(&name).await;
            let _ = tx.send(AppMsg::ThingLookup { generation, result });
            ctx.request_repaint();
        });
    }

    fn connect(&mut self, ctx: &egui::Context) {
        self.connection_error = None;
        let (Some(Ok(cert)), Some(Ok(key))) = (&self.cert, &self.key) else {
            self.connection_error = Some(
                AppError::configuration("A valid certificate and private key are required")
                    .with_action(
                    "Choose the device certificate and private key files, or create a certificate.",
                ),
            );
            return;
        };
        let Some(client_id) = self.effective_client_id() else {
            self.connection_error = Some(AppError::configuration(match self.settings.mode {
                IdentityMode::Thing => "Select a Thing to connect as",
                IdentityMode::ClientId => "Enter a client ID",
            }));
            return;
        };
        let config = ConnectionConfig {
            endpoint: self.settings.endpoint.trim().to_owned(),
            port: self.settings.port,
            client_id,
            certificate_pem: cert.pem.clone(),
            private_key_pem: key.pem.clone(),
            version: self.settings.mqtt_version,
            clean_session: self.settings.clean_session,
            session_expiry_secs: (self.settings.mqtt_version == MqttVersion::V5
                && self.settings.session_expiry_secs > 0)
                .then_some(self.settings.session_expiry_secs),
            last_will: self.settings.will_enabled.then(|| LastWill {
                topic: self.settings.will_topic.trim().to_owned(),
                payload: self.settings.will_payload.clone().into_bytes(),
                qos: self.settings.will_qos,
                retain: self.settings.will_retain,
                user_properties: to_send(&self.settings.will_user_properties),
            }),
            user_properties: to_send(&self.settings.connect_user_properties),
        };

        let id = self.next_connection_id;
        self.next_connection_id += 1;
        let tx = self.tx.clone();
        let ctx = ctx.clone();
        let sink: EventSink = Arc::new(move |connection_id, event| {
            let _ = tx.send(AppMsg::Mqtt {
                connection_id,
                event,
            });
            ctx.request_repaint();
        });

        let (endpoint, client_id) = (config.endpoint.clone(), config.client_id.clone());
        match self.start_connection(id, config, sink) {
            Ok(conn) => {
                self.connection = Some(conn);
                self.connected_as = Some((endpoint, client_id));
                self.subscriptions.clear();
                self.live = LiveState::default();
            }
            Err(e) => {
                self.state = ConnectionState::Error;
                self.connection_error = Some(e);
            }
        }
    }

    fn start_connection(
        &self,
        id: u64,
        config: ConnectionConfig,
        sink: EventSink,
    ) -> Result<Connection, AppError> {
        #[cfg(test)]
        if let Some(roots) = self.test_tls_roots.clone() {
            config.validate()?;
            let tls = mqtt::build_tls_config_with_roots(
                &config.certificate_pem,
                &config.private_key_pem,
                roots,
            )?;
            return Ok(Connection::start_with_tls(
                self.rt.handle(),
                id,
                config,
                tls,
                sink,
            ));
        }
        Connection::start(self.rt.handle(), id, config, sink)
    }

    fn drain_messages(&mut self, ctx: &egui::Context) {
        while let Ok(msg) = self.rx.try_recv() {
            match msg {
                AppMsg::Session { generation, result } if generation == self.aws.generation => {
                    self.aws.session_loading = false;
                    match result {
                        Ok(s) => {
                            self.aws.session = Some(s);
                            self.account_open = false;
                        }
                        Err(e) => {
                            self.aws.endpoints_loading = false;
                            self.aws.things_loading = false;
                            self.aws.error = Some(e);
                        }
                    }
                }
                AppMsg::Endpoints { generation, result } if generation == self.aws.generation => {
                    self.aws.endpoints_loading = false;
                    match result {
                        Ok(found) => {
                            self.aws.endpoints = found.endpoints;
                            self.aws.endpoints_error = found.warning;
                            self.apply_default_endpoint();
                        }
                        Err(e) => self.aws.endpoints_error = Some(e),
                    }
                }
                AppMsg::Things { generation, result } if generation == self.aws.generation => {
                    self.aws.things_loading = false;
                    match result {
                        Ok(page) => {
                            self.aws.things = page.things;
                            self.aws.things_has_more = page.has_more;
                        }
                        Err(e) => self.aws.things_error = Some(e),
                    }
                }
                AppMsg::ThingLookup { generation, result } if generation == self.aws.generation => {
                    self.aws.lookup_loading = false;
                    match result {
                        Ok(thing) => {
                            let name = thing.name.clone();
                            if !self.aws.things.iter().any(|t| t.name == name) {
                                self.aws.things.push(thing);
                                self.aws.things.sort_by_key(|t| t.name.to_lowercase());
                            }
                            self.select_thing(name);
                        }
                        Err(e) => self.aws.lookup_error = Some(e),
                    }
                }
                AppMsg::Mqtt {
                    connection_id,
                    event,
                } if self
                    .connection
                    .as_ref()
                    .is_some_and(|c| c.id == connection_id) =>
                {
                    self.apply_mqtt_event(ctx, event)
                }
                AppMsg::Api(entry) => self.api_log.push(entry),
                AppMsg::LiveConnection {
                    connection_id,
                    result,
                } if self.is_current_connection(connection_id) => {
                    self.live.info_loading = false;
                    self.live.info = Some(result);
                }
                AppMsg::LiveSubscriptions {
                    connection_id,
                    result,
                } if self.is_current_connection(connection_id) => {
                    self.live.subscriptions_loading = false;
                    self.live.subscriptions = Some(result);
                }
                AppMsg::Policies { identity, result } => {
                    if let Some(d) = self.provision.as_mut().filter(|d| d.identity == identity) {
                        if let Ok(list) = &result {
                            let preferred = provision::default_policy_name(identity.name());
                            if d.existing.is_empty() && list.contains(&preferred) {
                                d.existing = preferred;
                            }
                        }
                        d.policies = Some(result);
                    }
                }
                AppMsg::Provisioned {
                    identity,
                    policy,
                    outcome,
                } => self.apply_provision_outcome(identity, policy, outcome),
                AppMsg::SimPolicies { result } => self.apply_sim_policies(result),
                AppMsg::SimPolicyDocument { name, result } => self.apply_sim_document(name, result),
                AppMsg::NamedShadows {
                    connection_id,
                    result,
                } => self.apply_named_shadows(connection_id, result),
                _ => {} // Stale result from a superseded request.
            }
        }
    }

    /// Picks the default endpoint unless the user typed one or already picked a discovered one.
    fn apply_default_endpoint(&mut self) {
        if self.settings.endpoint_custom {
            return;
        }
        let current_known = self
            .aws
            .endpoints
            .iter()
            .any(|e| e.domain_name == self.settings.endpoint && e.is_enabled());
        if !current_known {
            if let Some(default) = self.aws.endpoints.iter().find(|e| e.is_default) {
                self.settings.endpoint = default.domain_name.clone();
            }
        }
    }

    fn open_provision_dialog(&mut self, ctx: &egui::Context) {
        let (Some(session), Some(identity)) = (self.aws.session.clone(), self.current_identity())
        else {
            return;
        };
        let account = session.account().map(str::to_owned).or_else(|| {
            let Identity::Thing(name) = &identity else {
                return None;
            };
            self.aws
                .things
                .iter()
                .find(|t| &t.name == name)
                .and_then(|t| t.arn.as_deref())
                .and_then(provision::account_from_arn)
                .map(str::to_owned)
        });
        let scope = ArnScope::new(session.partition(), &session.region, account.as_deref());
        let document = provision::default_policy_document(&identity, &scope);
        self.provision = Some(ProvisionDialog {
            identity: identity.clone(),
            session: session.clone(),
            create_new: true,
            policies: None,
            existing: String::new(),
            policy_filter: String::new(),
            new_name: provision::default_policy_name(identity.name()),
            default_document: document.clone(),
            document,
            storage_root: provision::default_storage_root(),
            running: false,
            error: None,
            show_document: false,
        });

        let tx = self.tx.clone();
        let ctx = ctx.clone();
        self.rt.spawn(async move {
            let result = session.list_policies().await;
            let _ = tx.send(AppMsg::Policies { identity, result });
            ctx.request_repaint();
        });
    }

    fn apply_provision_outcome(
        &mut self,
        identity: Identity,
        policy: String,
        outcome: ProvisionOutcome,
    ) {
        // Load whatever was saved, even after a partial failure: AWS returns the private key
        // only once, so this is the user's only copy.
        if let Some(saved) = &outcome.saved {
            self.load_certificate(&saved.cert_path);
            self.load_private_key(&saved.key_path);
        }
        let success = outcome.is_success();
        if let Some(d) = self.provision.as_mut().filter(|d| d.identity == identity) {
            d.running = false;
            d.error = outcome.error;
        }
        if success {
            self.provision = None;
            if let Some(saved) = &outcome.saved {
                let attached = match &identity {
                    Identity::Thing(t) => format!(", attached it to {t},"),
                    Identity::ClientId(_) => String::new(),
                };
                self.notice = Some(format!(
                    "Created certificate {} for {}{attached} attached policy {policy}, and loaded it. Files: {}",
                    short_id(&saved.certificate_id),
                    identity.describe(),
                    saved.directory.display()
                ));
            }
        }
    }

    fn apply_mqtt_event(&mut self, ctx: &egui::Context, event: MqttEvent) {
        match event {
            MqttEvent::State { state, error } => {
                let newly_connected =
                    state == ConnectionState::Connected && self.state != ConnectionState::Connected;
                self.state = state;
                if error.is_some() {
                    self.connection_error = error;
                }
                if newly_connected {
                    // Give the broker a moment to register the session before asking about it.
                    self.fetch_live_connection(ctx, Duration::from_secs(1));
                    self.fetch_live_subscriptions(ctx, Duration::from_secs(1));
                    self.start_shadows(ctx);
                }
                if matches!(
                    state,
                    ConnectionState::Disconnected | ConnectionState::Error
                ) {
                    self.connection = None;
                    self.subscriptions.clear();
                    self.live = LiveState::default();
                    self.show_conn_details = false;
                }
            }
            MqttEvent::Packet(entry) => {
                let changed_subscriptions = entry.direction == Direction::Received
                    && (entry.kind == "SUBACK" || entry.kind == "UNSUBACK");
                self.shadow_packet(&entry);
                self.log.push(entry);
                if changed_subscriptions {
                    self.fetch_live_subscriptions(ctx, Duration::from_secs(1));
                }
            }
            MqttEvent::Message(m) => {
                // Shadow traffic goes to the Shadows tab; it's shown here too only if one of
                // the user's own subscriptions covers it.
                let shadow_topic = self.shadow_message(&m.topic, &m.payload);
                if shadow_topic
                    && !self
                        .subscriptions
                        .iter()
                        .any(|f| topic_matches(f, &m.topic))
                {
                    return;
                }
                if self.messages.len() == MAX_MESSAGES {
                    self.messages.pop_back();
                }
                self.messages.push_front(m);
            }
        }
    }

    fn is_current_connection(&self, id: u64) -> bool {
        self.connection.as_ref().is_some_and(|c| c.id == id)
    }

    /// The account's default data endpoint, used for data-plane API calls.
    fn data_endpoint(&self) -> Option<String> {
        self.aws
            .endpoints
            .iter()
            .find(|e| e.is_default)
            .map(|e| e.domain_name.clone())
    }

    /// Session, connection ID, and client ID for data-plane calls about the live connection.
    fn live_target(&self) -> Option<(Session, u64, String)> {
        let session = self.aws.session.clone()?;
        let conn = self.connection.as_ref()?;
        let (_, client_id) = self.connected_as.clone()?;
        Some((session, conn.id, client_id))
    }

    /// `GetConnection` for the live connection (only ever called after CONNACK).
    fn fetch_live_connection(&mut self, ctx: &egui::Context, delay: Duration) {
        let Some((session, connection_id, client_id)) = self.live_target() else {
            return;
        };
        self.live.info_loading = true;
        let endpoint = self.data_endpoint();
        let include_socket = self.settings.include_socket_info;
        let tx = self.tx.clone();
        let ctx = ctx.clone();
        self.rt.spawn(async move {
            tokio::time::sleep(delay).await;
            let result = session
                .get_connection(endpoint.as_deref(), &client_id, include_socket)
                .await;
            let _ = tx.send(AppMsg::LiveConnection {
                connection_id,
                result,
            });
            ctx.request_repaint();
        });
    }

    /// `ListSubscriptions` for the live connection (only ever called after CONNACK).
    fn fetch_live_subscriptions(&mut self, ctx: &egui::Context, delay: Duration) {
        let Some((session, connection_id, client_id)) = self.live_target() else {
            return;
        };
        self.live.subscriptions_loading = true;
        let endpoint = self.data_endpoint();
        let tx = self.tx.clone();
        let ctx = ctx.clone();
        self.rt.spawn(async move {
            tokio::time::sleep(delay).await;
            let result = session
                .list_subscriptions(endpoint.as_deref(), &client_id)
                .await;
            let _ = tx.send(AppMsg::LiveSubscriptions {
                connection_id,
                result,
            });
            ctx.request_repaint();
        });
    }

    /// About the connection the MQTT log came from, for an exported trace.
    fn trace_info(&self) -> trace_export::TraceInfo {
        let (endpoint, client_id) = match &self.connected_as {
            Some((endpoint, client_id)) => (Some(endpoint.clone()), Some(client_id.clone())),
            None => (
                Some(self.settings.endpoint.trim().to_owned()).filter(|e| !e.is_empty()),
                self.effective_client_id(),
            ),
        };
        trace_export::TraceInfo {
            exported_at: jiff::ZonedDateTime::now(),
            endpoint,
            client_id,
            protocol: self.settings.mqtt_version.label().to_owned(),
        }
    }

    /// Saves the MQTT log as a trace: `.txt` gets text, anything else JSON.
    fn export_trace(&mut self) {
        let info = self.trace_info();
        #[cfg(test)]
        let path = self.test_export_path.clone();
        #[cfg(not(test))]
        let path = rfd::FileDialog::new()
            .set_title("Export MQTT trace")
            .set_file_name(trace_export::file_name(&info, "json"))
            .add_filter("JSON", &["json"])
            .add_filter("Text", &["txt"])
            .save_file();
        let Some(path) = path else {
            return;
        };
        let entries: Vec<&PacketLogEntry> = self.log.entries().collect();
        let text = path
            .extension()
            .is_some_and(|e| e.eq_ignore_ascii_case("txt"));
        let contents = if text {
            trace_export::to_text(&info, &entries)
        } else {
            trace_export::to_json(&info, &entries)
        };
        match std::fs::write(&path, contents) {
            Ok(()) => {
                let n = entries.len();
                self.notice = Some(format!(
                    "Exported {n} MQTT packet{} to {}",
                    if n == 1 { "" } else { "s" },
                    path.display()
                ));
            }
            Err(e) => {
                self.action_error = Some(
                    AppError::configuration("Couldn't save the trace")
                        .with_details(e.to_string())
                        .with_action("Choose a folder you can write to."),
                );
            }
        }
    }

    fn load_certificate(&mut self, path: &Path) {
        self.settings.cert_path = Some(path.to_owned());
        self.cert = Some(certs::read_file(path).and_then(|b| certs::parse_certificate(&b)));
    }

    fn load_private_key(&mut self, path: &Path) {
        self.settings.key_path = Some(path.to_owned());
        self.key = Some(certs::read_file(path).and_then(|b| certs::parse_private_key(&b)));
    }

    // -----------------------------------------------------------------------------------------
    // UI: top bar
    // -----------------------------------------------------------------------------------------

    fn top_bar(&mut self, ui: &mut egui::Ui) {
        let p = Palette::of(ui);
        ui.horizontal(|ui| {
            ui.label(theme::title("AWS IoT MQTT"));
            ui.add_space(6.0);
            for (view, label) in [
                (View::Client, "Client".to_owned()),
                (View::Shadows, "Shadows".to_owned()),
                (View::Simulator, "Policy simulator".to_owned()),
            ] {
                if theme::tab(ui, self.settings.view == view, label).clicked() {
                    self.settings.view = view;
                }
            }
            ui.add_space(6.0);
            theme::status_pill(ui, self.state.label(), self.status_color(&p));
            if let (true, Some((endpoint, client_id))) =
                (self.state.is_active(), &self.connected_as)
            {
                ui.label(
                    RichText::new(format!("{client_id} · {endpoint}"))
                        .small()
                        .color(p.muted),
                );
            }
            ui.with_layout(egui::Layout::right_to_left(egui::Align::Center), |ui| {
                let next = self.settings.theme.next();
                if theme::icon_button(
                    ui,
                    self.settings.theme.icon(),
                    &format!(
                        "Theme: {} (switch to {})",
                        self.settings.theme.label(),
                        next.label()
                    ),
                    true,
                )
                .clicked()
                {
                    self.settings.theme = next;
                    ui.ctx().set_theme(next.preference());
                }
                let logs = format!(
                    "{} Logs {}",
                    icons::LOGS,
                    self.log.len() + self.api_log.len()
                );
                if theme::tab(ui, self.settings.logs_open, logs)
                    .on_hover_text("MQTT packets and AWS API calls")
                    .clicked()
                {
                    self.settings.logs_open = !self.settings.logs_open;
                }
                if let Some(watcher) = &self.theme_watcher {
                    let color = if watcher.error.is_some() {
                        p.err
                    } else {
                        p.muted
                    };
                    theme::pill(ui, "Live theme", color).on_hover_text(format!(
                        "Design tokens reload when you save {}",
                        watcher.path().display()
                    ));
                }
            });
        });
    }

    fn status_color(&self, p: &Palette) -> Color32 {
        match self.state {
            ConnectionState::Connected => p.ok,
            ConnectionState::Error => p.err,
            ConnectionState::Disconnected => p.muted,
            ConnectionState::Connecting | ConnectionState::Disconnecting => p.warn,
        }
    }

    // -----------------------------------------------------------------------------------------
    // UI: sidebar
    // -----------------------------------------------------------------------------------------

    fn sidebar(&mut self, ui: &mut egui::Ui) {
        let gap = theme::metrics(ui).gap;
        theme::card(ui, |ui| self.account_card(ui));
        ui.add_space(gap);
        theme::card(ui, |ui| self.identity_card(ui));
        if self.state == ConnectionState::Connected {
            ui.add_space(gap);
            self.live_panel(ui);
        }
    }

    fn account_card(&mut self, ui: &mut egui::Ui) {
        let p = Palette::of(ui);
        let busy = self.aws.busy();
        let mut reload = false;

        // Loaded: a single summary row; the pickers only appear when expanded.
        if let Some(session) = self.aws.session.clone() {
            ui.horizontal(|ui| {
                let summary = match &session.identity {
                    Some(identity) => format!("Account {}", identity.account),
                    None => "AWS account".to_owned(),
                };
                theme::disclosure(ui, &mut self.account_open, &summary).on_hover_text(
                    session
                        .identity
                        .as_ref()
                        .map(|i| i.arn.clone())
                        .unwrap_or_default(),
                );
                ui.with_layout(egui::Layout::right_to_left(egui::Align::Center), |ui| {
                    if busy {
                        ui.spinner();
                    } else {
                        reload = theme::icon_button(
                            ui,
                            icons::REFRESH,
                            "Reload endpoints and Things",
                            true,
                        )
                        .clicked();
                    }
                    theme::pill(ui, &session.region, p.muted);
                });
            });
            if self.account_open {
                ui.add_space(6.0);
                self.account_pickers(ui);
            }
        } else {
            theme::card_title(ui, "AWS account", |ui| {
                if busy {
                    ui.spinner();
                }
            });
            ui.add_space(6.0);
            self.account_pickers(ui);
            ui.add_space(6.0);
            let width = ui.available_width();
            let button = theme::primary_button(ui, format!("{} Load from AWS", icons::CLOUD))
                .min_size(egui::vec2(width, 30.0));
            if ui.add_enabled(!busy, button).clicked() {
                reload = true;
            }
        }
        if reload {
            self.load_from_aws(ui.ctx());
        }
        if self.aws.error.is_some() {
            ui.add_space(6.0);
            show_error(ui, self.aws.error.as_ref());
        }
    }

    fn account_pickers(&mut self, ui: &mut egui::Ui) {
        let width = ui.available_width();
        theme::field_label(ui, "Profile");
        let profile_label = if self.settings.profile.is_empty() {
            "Default chain".to_owned()
        } else {
            self.settings.profile.clone()
        };
        egui::ComboBox::from_id_salt("profile")
            .selected_text(profile_label)
            .width(width)
            .show_ui(ui, |ui| {
                ui.selectable_value(&mut self.settings.profile, String::new(), "Default chain");
                for profile in &self.profiles {
                    ui.selectable_value(&mut self.settings.profile, profile.clone(), profile);
                }
            });
        theme::field_label(ui, "Region");
        let region_label = if self.settings.region.is_empty() {
            "From profile".to_owned()
        } else {
            self.settings.region.clone()
        };
        egui::ComboBox::from_id_salt("region")
            .selected_text(region_label)
            .width(width)
            .show_ui(ui, |ui| {
                ui.selectable_value(&mut self.settings.region, String::new(), "From profile");
                for region in aws::IOT_REGIONS {
                    ui.selectable_value(&mut self.settings.region, (*region).to_owned(), *region);
                }
            });
    }

    fn identity_card(&mut self, ui: &mut egui::Ui) {
        let p = Palette::of(ui);
        // While a connection is active the identity can't change: show only who we are.
        if self.state.is_active() {
            theme::section_label(ui, "Connected as");
            ui.horizontal(|ui| {
                let id = self
                    .connected_as
                    .as_ref()
                    .map(|(_, id)| id.clone())
                    .unwrap_or_default();
                ui.label(RichText::new(id).monospace());
                ui.with_layout(egui::Layout::right_to_left(egui::Align::Center), |ui| {
                    let mode = match self.settings.mode {
                        IdentityMode::Thing => "Thing",
                        IdentityMode::ClientId => "Client ID",
                    };
                    theme::pill(ui, mode, p.muted);
                });
            });
            return;
        }

        theme::section_label(ui, "Connect as");
        theme::segmented(
            ui,
            &mut self.settings.mode,
            &[
                (IdentityMode::Thing, "IoT Thing"),
                (IdentityMode::ClientId, "Client ID"),
            ],
            true,
        );
        ui.add_space(4.0);
        match self.settings.mode {
            IdentityMode::Thing => self.things_section(ui),
            IdentityMode::ClientId => self.client_id_section(ui),
        }
    }

    fn things_section(&mut self, ui: &mut egui::Ui) {
        if self.aws.session.is_none() {
            if let Some(thing) = self.settings.thing.clone() {
                theme::inset(ui, |ui| {
                    theme::field_label(ui, "Selected Thing");
                    ui.label(RichText::new(thing).monospace());
                });
            }
            theme::hint(
                ui,
                "Load your AWS account to list Things, or switch to Client ID.",
            );
            return;
        }

        let p = Palette::of(ui);
        let response = ui.add(
            egui::TextEdit::singleline(&mut self.aws.filter)
                .hint_text(format!("{} Filter, or type an exact name", icons::SEARCH))
                .desired_width(f32::INFINITY),
        );
        let typed = self.aws.filter.trim().to_owned();
        let exact = self.aws.things.iter().any(|t| t.name == typed);
        let can_lookup = !typed.is_empty() && !self.aws.lookup_loading;
        if response.lost_focus()
            && ui.input(|i| i.key_pressed(egui::Key::Enter))
            && !typed.is_empty()
        {
            if exact {
                self.select_thing(typed.clone());
            } else if can_lookup {
                self.lookup_thing(ui.ctx(), typed.clone());
            }
        }
        if self.aws.things_error.is_some() {
            show_error(ui, self.aws.things_error.as_ref());
        }

        let filter = typed.to_lowercase();
        let visible: Vec<&ThingSummary> = self
            .aws
            .things
            .iter()
            .filter(|t| filter.is_empty() || t.name.to_lowercase().contains(&filter))
            .collect();
        let mut clicked = None;
        egui::ScrollArea::vertical()
            .id_salt("things")
            .max_height(220.0)
            .auto_shrink([false, true])
            .show(ui, |ui| {
                ui.spacing_mut().item_spacing.y = 2.0;
                if self.aws.things_loading {
                    ui.spinner();
                } else if visible.is_empty() {
                    theme::hint(
                        ui,
                        if filter.is_empty() {
                            "No Things in this account and region."
                        } else {
                            "No loaded Things match."
                        },
                    );
                }
                for thing in &visible {
                    let selected = self.settings.thing.as_deref() == Some(&thing.name);
                    let row = theme::list_row(ui, selected, true, RichText::new(&thing.name));
                    let row = match &thing.thing_type {
                        Some(kind) => row.on_hover_text(format!("Type: {kind}")),
                        None => row,
                    };
                    if row.clicked() && !selected {
                        clicked = Some(thing.name.clone());
                    }
                }
            });
        if let Some(name) = clicked {
            self.select_thing(name);
        }

        if !typed.is_empty() && !exact {
            ui.horizontal(|ui| {
                let button = theme::ghost_button(ui, format!("Use \"{typed}\""));
                if ui
                    .add_enabled(can_lookup, button)
                    .on_hover_text("Look up this exact Thing name in the account (DescribeThing)")
                    .clicked()
                {
                    self.lookup_thing(ui.ctx(), typed.clone());
                }
                if self.aws.lookup_loading {
                    ui.spinner();
                }
            });
        } else if self.aws.things_has_more && typed.is_empty() {
            theme::hint(
                ui,
                format!(
                    "First {} Things shown. Type an exact name for others.",
                    self.aws.things.len()
                ),
            );
        }
        if self.aws.lookup_error.is_some() {
            show_error(ui, self.aws.lookup_error.as_ref());
        }

        if self.settings.thing.is_some() {
            ui.add_space(2.0);
            self.create_certificate_button(
                ui,
                "Create a new certificate and key for this Thing in AWS IoT",
            );
        } else {
            ui.label(
                RichText::new("Select a Thing to connect as.")
                    .small()
                    .color(p.muted),
            );
        }
    }

    fn client_id_section(&mut self, ui: &mut egui::Ui) {
        let p = Palette::of(ui);
        ui.add(
            egui::TextEdit::singleline(&mut self.settings.custom_client_id)
                .hint_text("Client ID, e.g. my-client-1")
                .desired_width(f32::INFINITY),
        );
        let id = self.settings.custom_client_id.trim();
        let valid = match validate_client_id(id) {
            Ok(()) => true,
            Err(e) => {
                if !id.is_empty() {
                    ui.label(RichText::new(e).small().color(p.err));
                }
                false
            }
        };
        theme::hint(
            ui,
            "No Thing is involved. The certificate's policy must allow iot:Connect for this client ID.",
        );
        if valid && self.aws.session.is_some() {
            self.create_certificate_button(
                ui,
                "Create a new certificate whose policy allows connecting as this client ID",
            );
        }
    }

    fn create_certificate_button(&mut self, ui: &mut egui::Ui, hover: &str) {
        let width = ui.available_width();
        let button = theme::ghost_button(ui, format!("{} Create certificate", icons::ADD))
            .min_size(egui::vec2(width, 30.0));
        if ui
            .add_enabled(self.provision.is_none(), button)
            .on_hover_text(hover)
            .clicked()
        {
            self.open_provision_dialog(ui.ctx());
        }
    }

    /// Broker-side view of the live connection: GetConnection and ListSubscriptions.
    fn live_panel(&mut self, ui: &mut egui::Ui) {
        let p = Palette::of(ui);
        let gap = theme::metrics(ui).gap;
        if self.aws.session.is_none() {
            theme::card(ui, |ui| {
                theme::section_label(ui, "Live connection");
                theme::hint(
                    ui,
                    "Load your AWS account to see connectivity and subscriptions from the broker.",
                );
            });
            return;
        }

        let mut refresh_info = false;
        theme::card(ui, |ui| {
            theme::card_title(ui, "Connectivity", |ui| {
                if self.live.info_loading {
                    ui.spinner();
                } else {
                    refresh_info =
                        theme::icon_button(ui, icons::REFRESH, "Refresh (GetConnection)", true)
                            .clicked();
                }
            });
            match &self.live.info {
                Some(Ok(info)) => {
                    key_values(ui, "live_essentials", &connection_essentials(info));
                    theme::disclosure(ui, &mut self.show_more_info, "More");
                    if self.show_more_info {
                        key_values(ui, "live_more", &connection_more(info));
                        refresh_info |= ui
                            .checkbox(&mut self.settings.include_socket_info, "Include socket info")
                            .on_hover_text(
                                "Request client and endpoint IP addresses and ports. Needs separate authorization in the caller's IAM policy.",
                            )
                            .changed();
                    }
                }
                Some(Err(e)) => show_error(ui, Some(e)),
                None => theme::hint(ui, "Loading…"),
            }
        });
        if refresh_info {
            self.fetch_live_connection(ui.ctx(), Duration::ZERO);
        }

        ui.add_space(gap);
        let mut refresh_subs = false;
        let mut unsubscribe: Option<String> = None;
        theme::card(ui, |ui| {
            theme::card_title(ui, "Subscriptions", |ui| {
                if self.live.subscriptions_loading {
                    ui.spinner();
                } else {
                    refresh_subs =
                        theme::icon_button(ui, icons::REFRESH, "Refresh (ListSubscriptions)", true)
                            .clicked();
                }
            });
            if let Some(Ok(list)) = &self.live.subscriptions {
                if !list.is_empty() {
                    theme::count_caption(ui, list.len(), "active");
                }
            }
            let rows: Vec<(String, Option<i32>)> = match &self.live.subscriptions {
                Some(Ok(list)) => {
                    if list.is_empty() {
                        theme::hint(ui, "None yet. Subscribe from the messages panel.");
                    }
                    list.iter()
                        .map(|s| (s.topic_filter.clone(), Some(s.qos)))
                        .collect()
                }
                Some(Err(e)) => {
                    show_error(ui, Some(e));
                    if !self.subscriptions.is_empty() {
                        theme::hint(ui, "Subscribed from this app:");
                    }
                    self.subscriptions
                        .iter()
                        .map(|f| (f.clone(), None))
                        .collect()
                }
                None => Vec::new(),
            };
            let shadow_filter = self.shadow_filter();
            for (filter, qos) in rows {
                ui.horizontal(|ui| {
                    ui.label(RichText::new(&filter).monospace());
                    ui.with_layout(egui::Layout::right_to_left(egui::Align::Center), |ui| {
                        if Some(&filter) == shadow_filter.as_ref() {
                            // Unsubscribing would stop the Shadows tab from getting answers.
                            theme::pill(ui, "Used by Shadows", p.info).on_hover_text(
                                "The app subscribes here to read and update this Thing's \
                                 shadows.",
                            );
                        } else if theme::icon_button(ui, icons::CLOSE, "Unsubscribe", true)
                            .clicked()
                        {
                            unsubscribe = Some(filter.clone());
                        }
                        if let Some(qos) = qos {
                            theme::pill(ui, format!("QoS {qos}"), p.muted);
                        }
                    });
                });
            }
        });
        if refresh_subs {
            self.fetch_live_subscriptions(ui.ctx(), Duration::ZERO);
        }
        if let (Some(filter), Some(connection)) = (unsubscribe, &self.connection) {
            self.subscriptions.retain(|s| s != &filter);
            let props = to_send(&self.settings.subscription_user_properties);
            if let Err(e) = connection.unsubscribe(&filter, &props) {
                self.action_error = Some(e);
            }
        }
    }

    // -----------------------------------------------------------------------------------------
    // UI: main column
    // -----------------------------------------------------------------------------------------

    fn main_column(&mut self, ui: &mut egui::Ui) {
        let gap = theme::metrics(ui).gap;
        if let Some(error) = self.theme_watcher.as_ref().and_then(|w| w.error.clone()) {
            theme::banner(ui, Severity::Warning, icons::WARNING, false, |ui| {
                ui.label(theme::title("assets/theme.json has an error"));
                ui.label(RichText::new(error).small());
                ui.label(RichText::new("Showing the last valid theme.").small());
            });
            ui.add_space(gap);
        }
        if let Some(notice) = self.notice.clone() {
            let dismissed = theme::banner(ui, Severity::Success, icons::CHECK, true, |ui| {
                ui.label(RichText::new(notice));
            });
            if dismissed {
                self.notice = None;
            }
            ui.add_space(gap);
        }

        theme::card(ui, |ui| {
            if matches!(
                self.state,
                ConnectionState::Connected | ConnectionState::Disconnecting
            ) {
                self.connected_bar(ui);
            } else {
                self.connection_setup(ui);
            }
        });

        // Messages only matter once there's a connection (or something left to read).
        if self.state == ConnectionState::Connected || !self.messages.is_empty() {
            ui.add_space(gap);
            theme::card(ui, |ui| self.messages_panel(ui));
        }
    }

    /// Compact connection bar shown while connected; settings stay one click away.
    fn connected_bar(&mut self, ui: &mut egui::Ui) {
        let p = Palette::of(ui);
        ui.horizontal(|ui| {
            theme::dot(ui, self.status_color(&p));
            ui.label(theme::title("Connected"));
            ui.label(
                RichText::new(self.connection_summary())
                    .small()
                    .color(p.muted),
            );
            ui.with_layout(egui::Layout::right_to_left(egui::Align::Center), |ui| {
                let disconnecting = self.state == ConnectionState::Disconnecting;
                let button = theme::danger_button(
                    ui,
                    if disconnecting {
                        "Disconnecting…"
                    } else {
                        "Disconnect"
                    },
                );
                if ui.add_enabled(!disconnecting, button).clicked() {
                    if let Some(connection) = &self.connection {
                        connection.disconnect();
                    }
                }
                if theme::icon_button(ui, icons::SETTINGS, "Connection details", true).clicked() {
                    self.show_conn_details = !self.show_conn_details;
                }
            });
        });
        if self.show_conn_details {
            ui.add_space(8.0);
            ui.add_enabled_ui(false, |ui| {
                self.connection_essentials(ui);
                self.advanced_options(ui, true);
            });
        }
    }

    /// The setup form shown before connecting: essentials first, everything else tucked away.
    fn connection_setup(&mut self, ui: &mut egui::Ui) {
        let p = Palette::of(ui);
        theme::card_title(ui, "Connection", |ui| {
            if self.state == ConnectionState::Connecting {
                theme::pill(ui, "Connecting…", p.warn);
            }
        });
        ui.add_space(4.0);
        ui.add_enabled_ui(!self.state.is_active(), |ui| {
            self.connection_essentials(ui);
            ui.add_space(2.0);
            self.advanced_options(ui, false);
        });
        ui.add_space(8.0);
        self.connect_actions(ui);
    }

    fn connection_essentials(&mut self, ui: &mut egui::Ui) {
        egui::Grid::new("conn_grid")
            .num_columns(2)
            .spacing([16.0, 10.0])
            .show(ui, |ui| {
                theme::field_label(ui, "Endpoint");
                self.endpoint_picker(ui);
                ui.end_row();

                theme::field_label(ui, "Protocol");
                ui.horizontal(|ui| {
                    theme::segmented_inline(
                        ui,
                        &mut self.settings.mqtt_version,
                        &[(MqttVersion::V5, "MQTT 5"), (MqttVersion::V311, "3.1.1")],
                    );
                    ui.add_space(8.0);
                    theme::field_label(ui, "Port");
                    ui.add(egui::DragValue::new(&mut self.settings.port).range(1..=65535));
                });
                ui.end_row();

                self.certificate_fields(ui);
            });
    }

    /// Certificate and private key rows, for a two-column `Grid`.
    pub(super) fn certificate_fields(&mut self, ui: &mut egui::Ui) {
        let p = Palette::of(ui);
        theme::field_label(ui, "Certificate");
        ui.vertical(|ui| {
            if let Some(path) = file_row(
                ui,
                self.settings.cert_path.as_deref(),
                "cert",
                &["pem", "crt", "cer"],
            ) {
                self.load_certificate(&path);
            }
            match &self.cert {
                Some(Ok(c)) => {
                    ui.horizontal(|ui| {
                        if !c.currently_valid {
                            theme::pill(ui, "Expired", p.err);
                        }
                        theme::disclosure(ui, &mut self.show_cert_details, "Details");
                    });
                    if self.show_cert_details {
                        theme::inset(ui, |ui| {
                            for (key, value) in [
                                ("Subject", c.subject.clone()),
                                ("Issuer", c.issuer.clone()),
                                ("Valid", format!("{} to {}", c.not_before, c.not_after)),
                                ("ID", c.certificate_id.clone()),
                            ] {
                                ui.label(RichText::new(key).small().color(p.muted));
                                ui.label(RichText::new(value).small().monospace());
                            }
                        });
                    }
                }
                Some(Err(e)) => show_error(ui, Some(e)),
                None => {}
            }
        });
        ui.end_row();

        theme::field_label(ui, "Private key");
        ui.vertical(|ui| {
            if let Some(path) = file_row(
                ui,
                self.settings.key_path.as_deref(),
                "key",
                &["pem", "key"],
            ) {
                self.load_private_key(&path);
            }
            if let Some(Err(e)) = &self.key {
                show_error(ui, Some(e));
            }
        });
        ui.end_row();
    }

    /// Session and last-will options. Collapsed by default; non-default values are summarized
    /// next to the toggle so nothing hidden is a surprise.
    fn advanced_options(&mut self, ui: &mut egui::Ui, force_open: bool) {
        let p = Palette::of(ui);
        let mut chips = Vec::new();
        if !self.settings.clean_session {
            chips.push("Persistent session".to_owned());
        }
        if self.settings.mqtt_version == MqttVersion::V5 && self.settings.session_expiry_secs > 0 {
            chips.push(format!("Expiry {} s", self.settings.session_expiry_secs));
        }
        if self.settings.will_enabled {
            chips.push("Last will".to_owned());
        }
        let v5 = self.settings.mqtt_version == MqttVersion::V5;
        let connect_props = to_send(&self.settings.connect_user_properties).len();
        if v5 && connect_props > 0 {
            chips.push(format!(
                "{connect_props} user propert{}",
                if connect_props == 1 { "y" } else { "ies" }
            ));
        }
        let mut open = self.show_advanced || force_open;
        ui.horizontal(|ui| {
            theme::disclosure(ui, &mut open, "Advanced");
            for chip in &chips {
                theme::pill(ui, chip, p.info);
            }
        });
        if !force_open {
            self.show_advanced = open;
        }
        if !(open || force_open) {
            return;
        }
        theme::inset(ui, |ui| {
            egui::Grid::new("advanced_grid")
                .num_columns(2)
                .spacing([16.0, 10.0])
                .show(ui, |ui| {
                    theme::field_label(ui, "Session");
                    ui.horizontal(|ui| {
                        ui.checkbox(
                            &mut self.settings.clean_session,
                            self.settings.mqtt_version.clean_label(),
                        )
                        .on_hover_text(
                            "Start with no stored session. Turn off to resume the broker's persistent session (subscriptions and queued QoS 1 messages).",
                        );
                        if self.settings.mqtt_version == MqttVersion::V5 {
                            theme::field_label(ui, "Expires after");
                            ui.add(
                                egui::DragValue::new(&mut self.settings.session_expiry_secs)
                                    .range(0..=mqtt::MAX_SESSION_EXPIRY_SECS)
                                    .suffix(" s"),
                            )
                            .on_hover_text(
                                "How long the broker keeps the session after disconnect. 0 = the session ends when the connection closes. AWS IoT allows up to 7 days (1 hour by default).",
                            );
                        }
                    });
                    ui.end_row();

                    theme::field_label(ui, "Last will");
                    ui.vertical(|ui| {
                        ui.checkbox(
                            &mut self.settings.will_enabled,
                            "Publish a message if this client drops",
                        )
                        .on_hover_text(
                            "Sent by the broker if this client disconnects without sending DISCONNECT.",
                        );
                        if self.settings.will_enabled {
                            ui.horizontal(|ui| {
                                ui.add(
                                    egui::TextEdit::singleline(&mut self.settings.will_topic)
                                        .hint_text("will/topic")
                                        .desired_width(200.0),
                                );
                                qos_picker(ui, "will_qos", &mut self.settings.will_qos);
                                ui.checkbox(&mut self.settings.will_retain, "Retain");
                            });
                            ui.add(
                                egui::TextEdit::multiline(&mut self.settings.will_payload)
                                    .hint_text("Payload")
                                    .desired_rows(2)
                                    .desired_width(f32::INFINITY),
                            );
                            if let Err(e) = crate::validators::validate_publish_topic(
                                self.settings.will_topic.trim(),
                            ) {
                                ui.label(RichText::new(e).small().color(p.err));
                            }
                            if v5 {
                                theme::field_label(ui, "Will user properties");
                                user_properties_editor(
                                    ui,
                                    "will_props",
                                    &mut self.settings.will_user_properties,
                                );
                            }
                        }
                    });
                    ui.end_row();

                    theme::field_label(ui, "User properties");
                    ui.vertical(|ui| {
                        if v5 {
                            theme::hint(ui, "Sent with CONNECT.");
                            user_properties_editor(
                                ui,
                                "connect_props",
                                &mut self.settings.connect_user_properties,
                            );
                        } else {
                            theme::hint(ui, "User properties need MQTT 5.");
                        }
                    });
                    ui.end_row();
                });
        });
    }

    fn connect_actions(&mut self, ui: &mut egui::Ui) {
        let missing = self.missing_requirements();
        ui.horizontal(|ui| match self.state {
            ConnectionState::Connecting => {
                let button = theme::ghost_button(ui, "Cancel");
                if ui.add(button).clicked() {
                    if let Some(connection) = &self.connection {
                        connection.disconnect();
                    }
                }
                ui.spinner();
            }
            _ => {
                let button = theme::primary_button(ui, format!("{} Connect", icons::PLUG));
                if ui.add_enabled(missing.is_empty(), button).clicked() {
                    self.connect(ui.ctx());
                }
                if !missing.is_empty() {
                    theme::hint(ui, format!("Still needed: {}", missing.join(", ")));
                }
            }
        });
        if !self.state.is_active() && self.connection_error.is_some() {
            ui.add_space(8.0);
            show_error(ui, self.connection_error.as_ref());
        }
    }

    /// What's still needed before Connect can be pressed, in plain words.
    fn missing_requirements(&self) -> Vec<&'static str> {
        missing_requirements(
            !self.settings.endpoint.trim().is_empty(),
            self.effective_client_id()
                .is_some_and(|c| validate_client_id(&c).is_ok()),
            self.settings.mode,
            matches!(self.cert, Some(Ok(_))),
            matches!(self.key, Some(Ok(_))),
        )
    }

    /// One-line description of the connection, shown in the connected bar.
    fn connection_summary(&self) -> String {
        summarize(&self.settings, self.effective_client_id())
    }

    /// Discovered endpoints by name ("Default" for iot:Data-ATS), plus a free-text option.
    fn endpoint_picker(&mut self, ui: &mut egui::Ui) {
        let p = Palette::of(ui);
        ui.vertical(|ui| {
            let current = (!self.settings.endpoint_custom)
                .then(|| {
                    self.aws
                        .endpoints
                        .iter()
                        .find(|e| e.domain_name == self.settings.endpoint)
                })
                .flatten();
            let selected_text = match current {
                Some(e) => e.label.clone(),
                None if self.settings.endpoint_custom || !self.settings.endpoint.is_empty() => {
                    "Custom".to_owned()
                }
                None if self.aws.endpoints_loading => "Loading…".to_owned(),
                None => "Select an endpoint".to_owned(),
            };
            let show_text_field = self.settings.endpoint_custom || current.is_none();
            ui.horizontal(|ui| {
                egui::ComboBox::from_id_salt("endpoint")
                    .selected_text(selected_text)
                    .width(if show_text_field { 130.0 } else { 220.0 })
                    .show_ui(ui, |ui| {
                        for endpoint in &self.aws.endpoints {
                            let is_selected = !self.settings.endpoint_custom
                                && self.settings.endpoint == endpoint.domain_name;
                            let mut label = endpoint.label.clone();
                            if !endpoint.is_enabled() {
                                label.push_str(" (disabled)");
                            }
                            let response = ui
                                .add_enabled(
                                    endpoint.is_enabled(),
                                    egui::Button::selectable(is_selected, label),
                                )
                                .on_hover_text(endpoint_hover(endpoint));
                            if response.clicked() {
                                self.settings.endpoint = endpoint.domain_name.clone();
                                self.settings.endpoint_custom = false;
                            }
                        }
                        if ui
                            .add(egui::Button::selectable(
                                self.settings.endpoint_custom,
                                "Custom…",
                            ))
                            .on_hover_text("Type an endpoint hostname")
                            .clicked()
                        {
                            self.settings.endpoint_custom = true;
                        }
                    });
                if show_text_field {
                    ui.add(
                        egui::TextEdit::singleline(&mut self.settings.endpoint)
                            .hint_text("xxxxxxxx-ats.iot.<region>.amazonaws.com")
                            .desired_width(300.0),
                    );
                } else if self.aws.endpoints_loading {
                    ui.spinner();
                }
            });
            if !show_text_field {
                ui.label(
                    RichText::new(&self.settings.endpoint)
                        .monospace()
                        .small()
                        .color(p.muted),
                );
            }
            if let Some(e) = &self.aws.endpoints_error {
                ui.horizontal(|ui| {
                    ui.label(RichText::new(icons::WARNING).small().color(p.warn));
                    ui.label(RichText::new(e.summary()).small().color(p.warn))
                        .on_hover_text(e.full());
                });
            }
        });
    }

    // -----------------------------------------------------------------------------------------
    // UI: messages (subscribe, stream, publish)
    // -----------------------------------------------------------------------------------------

    fn messages_panel(&mut self, ui: &mut egui::Ui) {
        let p = Palette::of(ui);
        let connected = self.state == ConnectionState::Connected;
        ui.horizontal(|ui| {
            ui.label(theme::title("Messages"));
            theme::pill(ui, self.messages.len().to_string(), p.muted);
            ui.with_layout(egui::Layout::right_to_left(egui::Align::Center), |ui| {
                if !self.messages.is_empty() && theme::link_button(ui, "Clear").clicked() {
                    self.messages.clear();
                }
            });
        });

        if connected {
            ui.horizontal(|ui| {
                let hint = match (self.settings.mode, &self.settings.thing) {
                    (IdentityMode::Thing, Some(thing)) => {
                        format!("Subscribe, e.g. $aws/things/{thing}/shadow/#")
                    }
                    _ => "Subscribe to a topic filter, e.g. sensors/#".into(),
                };
                let width = (ui.available_width() - 175.0).max(140.0);
                let field = ui.add(
                    egui::TextEdit::singleline(&mut self.settings.subscribe_filter)
                        .hint_text(hint)
                        .desired_width(width),
                );
                qos_picker(ui, "sub_qos", &mut self.subscribe_qos);
                let button = theme::ghost_button(ui, "Subscribe");
                let submit = ui.add(button).clicked()
                    || (field.lost_focus() && ui.input(|i| i.key_pressed(egui::Key::Enter)));
                if submit {
                    self.subscribe_from_input();
                }
            });
            user_properties_section(
                ui,
                "subscription_props",
                "User properties for subscribe and unsubscribe",
                &mut self.show_subscription_props,
                self.settings.mqtt_version == MqttVersion::V5,
                &mut self.settings.subscription_user_properties,
            );
        }

        // The stream fills the card; the composer is pinned underneath it, including its
        // User Properties rows when they're open.
        let v5 = self.settings.mqtt_version == MqttVersion::V5;
        let props_height = match (v5, self.show_publish_props) {
            (false, _) => 0.0,
            (true, false) => 26.0,
            (true, true) => 26.0 + 30.0 * (self.settings.publish_user_properties.len() + 1) as f32,
        };
        let composer_height = if connected { 150.0 + props_height } else { 0.0 };
        let stream_height = (ui.available_height() - composer_height).max(90.0);
        egui::ScrollArea::vertical()
            .id_salt("messages")
            .max_height(stream_height)
            .auto_shrink([false, false])
            .stick_to_bottom(true)
            .show(ui, |ui| {
                if self.messages.is_empty() {
                    ui.add_space(18.0);
                    ui.vertical_centered(|ui| {
                        theme::hint(ui, "Messages from your subscriptions appear here.");
                    });
                }
                // Oldest first, so the newest message sits next to the composer.
                for message in self.messages.iter().rev() {
                    message_bubble(ui, message);
                }
            });

        if connected {
            ui.add_space(6.0);
            self.composer(ui);
        }
    }

    fn subscribe_from_input(&mut self) {
        let Some(connection) = &self.connection else {
            return;
        };
        let filter = self.settings.subscribe_filter.trim().to_owned();
        let props = to_send(&self.settings.subscription_user_properties);
        match connection.subscribe(&filter, self.subscribe_qos, &props) {
            Ok(()) => {
                self.action_error = None;
                if !self.subscriptions.contains(&filter) {
                    self.subscriptions.push(filter);
                }
            }
            Err(e) => self.action_error = Some(e),
        }
    }

    fn composer(&mut self, ui: &mut egui::Ui) {
        theme::inset(ui, |ui| {
            ui.horizontal(|ui| {
                let width = (ui.available_width() - 235.0).max(140.0);
                ui.add(
                    egui::TextEdit::singleline(&mut self.settings.publish_topic)
                        .hint_text("Publish to topic")
                        .desired_width(width),
                );
                qos_picker(ui, "pub_qos", &mut self.publish_qos);
                ui.checkbox(&mut self.publish_retain, "Retain");
                let button = theme::primary_button(ui, format!("{} Send", icons::SEND));
                if ui.add(button).clicked() {
                    if let Some(connection) = &self.connection {
                        let result = connection.publish(
                            self.settings.publish_topic.trim(),
                            self.publish_qos,
                            self.publish_retain,
                            self.publish_payload.clone().into_bytes(),
                            &to_send(&self.settings.publish_user_properties),
                        );
                        self.action_error = result.err();
                    }
                }
            });
            ui.add(
                egui::TextEdit::multiline(&mut self.publish_payload)
                    .code_editor()
                    .frame(egui::Frame::new())
                    .desired_rows(3)
                    .desired_width(f32::INFINITY),
            );
            user_properties_section(
                ui,
                "publish_props",
                "User properties",
                &mut self.show_publish_props,
                self.settings.mqtt_version == MqttVersion::V5,
                &mut self.settings.publish_user_properties,
            );
        });
        if self.action_error.is_some() {
            ui.add_space(4.0);
            show_error(ui, self.action_error.as_ref());
        }
    }

    // -----------------------------------------------------------------------------------------
    // UI: logs drawer
    // -----------------------------------------------------------------------------------------

    fn log_panel(&mut self, ui: &mut egui::Ui) {
        ui.horizontal(|ui| {
            if theme::tab(
                ui,
                self.log_tab == LogTab::Mqtt,
                format!("MQTT packets {}", self.log.len()),
            )
            .clicked()
            {
                self.log_tab = LogTab::Mqtt;
            }
            if theme::tab(
                ui,
                self.log_tab == LogTab::Api,
                format!("AWS API calls {}", self.api_log.len()),
            )
            .clicked()
            {
                self.log_tab = LogTab::Api;
            }
            ui.with_layout(egui::Layout::right_to_left(egui::Align::Center), |ui| {
                if theme::icon_button(ui, icons::CLOSE, "Hide logs", true).clicked() {
                    self.settings.logs_open = false;
                }
                let clear = theme::link_button(ui, "Clear").clicked();
                match self.log_tab {
                    LogTab::Mqtt => {
                        if clear {
                            self.log.clear();
                        }
                        let export = ui
                            .add_enabled_ui(!self.log.is_empty(), |ui| {
                                theme::link_button(ui, "Export…").on_hover_text(
                                    "Save this log (everything since it was last cleared) as \
                                     JSON or text, to share a trace",
                                )
                            })
                            .inner
                            .clicked();
                        if export {
                            self.export_trace();
                        }
                        ui.checkbox(&mut self.hide_pings, "Hide pings");
                    }
                    LogTab::Api => {
                        if clear {
                            self.api_log.clear();
                        }
                        theme::hint(ui, "Secrets are never logged.");
                    }
                }
            });
        });
        match self.log_tab {
            LogTab::Mqtt => {
                let rows: Vec<_> = self
                    .log
                    .entries()
                    .filter(|e| !(self.hide_pings && e.is_ping()))
                    .collect();
                if let Some(replay) = log_rows(ui, "mqtt_log", &rows, 11, true) {
                    self.open_cli_dialog(replay);
                }
            }
            LogTab::Api => {
                let rows: Vec<_> = self.api_log.entries().collect();
                log_rows(ui, "api_log", &rows, 27, false);
            }
        }
    }

    // -----------------------------------------------------------------------------------------
    // UI: create-certificate dialog
    // -----------------------------------------------------------------------------------------

    fn provision_dialog(&mut self, ctx: &egui::Context) {
        let Some(d) = self.provision.as_mut() else {
            return;
        };
        let mut cancel = false;
        let mut submit = false;
        let validation = d.validation_error();

        let response = egui::Modal::new(egui::Id::unique("provision_dialog"))
            .frame(modal_frame(ctx))
            .show(ctx, |ui| {
                let p = Palette::of(ui);
                ui.set_width(620.0);
                ui.label(
                    RichText::new(format!(
                        "{} Create certificate for {}",
                        icons::KEY,
                        d.identity.describe()
                    ))
                    .heading(),
                );
                let what = match &d.identity {
                    Identity::Thing(_) => "A new key pair and active certificate, with a policy attached, attached to the Thing.",
                    Identity::ClientId(_) => "A new key pair and active certificate with a policy attached. No Thing is involved.",
                };
                theme::hint(ui, format!("{what} Region {}.", d.session.region));
                ui.add_space(8.0);

                ui.add_enabled_ui(!d.running, |ui| {
                    theme::segmented(
                        ui,
                        &mut d.create_new,
                        &[(true, "New policy"), (false, "Existing policy")],
                        true,
                    );
                    ui.add_space(8.0);
                    if d.create_new {
                        theme::field_label(ui, "Policy name");
                        ui.add(
                            egui::TextEdit::singleline(&mut d.new_name)
                                .desired_width(f32::INFINITY),
                        );
                        ui.add_space(4.0);
                        ui.horizontal(|ui| {
                            theme::disclosure(ui, &mut d.show_document, "Policy document");
                            if d.document != d.default_document {
                                theme::pill(ui, "Edited", p.info);
                                ui.with_layout(
                                    egui::Layout::right_to_left(egui::Align::Center),
                                    |ui| {
                                        if theme::link_button(ui, "Reset to default").clicked() {
                                            d.document = d.default_document.clone();
                                        }
                                    },
                                );
                            }
                        });
                        let explain = match &d.identity {
                            Identity::Thing(_) => "Connect only with a client ID equal to the Thing name while this certificate is attached (AWS thing-policy best practice). Publish, receive, and subscribe on all topics.",
                            Identity::ClientId(_) => "Connect only as this client ID. Publish, receive, and subscribe on all topics.",
                        };
                        theme::hint(ui, explain);
                        if d.show_document {
                            theme::inset(ui, |ui| {
                                egui::ScrollArea::vertical()
                                    .id_salt("policy_doc")
                                    .max_height(240.0)
                                    .show(ui, |ui| {
                                        ui.add(
                                            egui::TextEdit::multiline(&mut d.document)
                                                .code_editor()
                                                .frame(egui::Frame::new())
                                                .desired_rows(14)
                                                .desired_width(f32::INFINITY),
                                        );
                                    });
                            });
                        }
                    } else {
                        match &d.policies {
                            None => {
                                ui.horizontal(|ui| {
                                    ui.spinner();
                                    theme::hint(ui, "Loading policies…");
                                });
                            }
                            Some(Err(e)) => show_error(ui, Some(e)),
                            Some(Ok(list)) if list.is_empty() => {
                                theme::banner(ui, Severity::Warning, icons::WARNING, false, |ui| {
                                    ui.label("No policies exist in this account and region.");
                                });
                            }
                            Some(Ok(list)) => {
                                ui.add(
                                    egui::TextEdit::singleline(&mut d.policy_filter)
                                        .hint_text(format!("{} Filter policies", icons::SEARCH))
                                        .desired_width(f32::INFINITY),
                                );
                                let filter = d.policy_filter.to_lowercase();
                                egui::ScrollArea::vertical()
                                    .id_salt("policy_list")
                                    .max_height(200.0)
                                    .auto_shrink([false, true])
                                    .show(ui, |ui| {
                                        ui.spacing_mut().item_spacing.y = 2.0;
                                        for name in list.iter().filter(|n| {
                                            filter.is_empty() || n.to_lowercase().contains(&filter)
                                        }) {
                                            if theme::list_row(
                                                ui,
                                                &d.existing == name,
                                                true,
                                                RichText::new(name),
                                            )
                                            .clicked()
                                            {
                                                d.existing = name.clone();
                                            }
                                        }
                                    });
                            }
                        }
                    }
                });

                if let Some(message) = &validation {
                    ui.label(RichText::new(message).small().color(p.err));
                }

                ui.add_space(8.0);
                theme::section_label(ui, "Changes in your AWS account");
                let policy_name = if d.create_new {
                    d.new_name.trim()
                } else {
                    d.existing.as_str()
                };
                let policy_name = if policy_name.is_empty() {
                    "<policy>"
                } else {
                    policy_name
                };
                let mut steps = Vec::new();
                if d.create_new {
                    steps.push(format!("Create IoT policy {policy_name}"));
                }
                steps.push("Create a key pair and an active certificate".to_owned());
                steps.push(format!("Attach policy {policy_name} to the certificate"));
                if let Identity::Thing(thing) = &d.identity {
                    steps.push(format!("Attach the certificate to Thing {thing}"));
                }
                theme::inset(ui, |ui| {
                    ui.spacing_mut().item_spacing.y = 4.0;
                    for (index, step) in steps.iter().enumerate() {
                        ui.horizontal(|ui| {
                            ui.label(
                                theme::title(format!("{}", index + 1))
                                    .small()
                                    .color(p.accent),
                            );
                            ui.label(RichText::new(step).small());
                        });
                    }
                });
                theme::hint(
                    ui,
                    format!(
                        "{} Saved under {} and loaded automatically.",
                        icons::FOLDER,
                        d.storage_root.display()
                    ),
                );

                if d.error.is_some() {
                    show_error(ui, d.error.as_ref());
                }

                ui.add_space(8.0);
                ui.horizontal(|ui| {
                    let can_submit = !d.running && validation.is_none();
                    let create = theme::primary_button(ui, "Create certificate");
                    if ui.add_enabled(can_submit, create).clicked() {
                        submit = true;
                    }
                    let cancel_button = theme::ghost_button(ui, "Cancel");
                    if ui.add_enabled(!d.running, cancel_button).clicked() {
                        cancel = true;
                    }
                    if d.running {
                        ui.spinner();
                        theme::hint(ui, "Creating…");
                    }
                });
            });

        if (cancel || response.should_close()) && !d.running {
            self.provision = None;
            return;
        }
        if submit {
            d.running = true;
            d.error = None;
            let policy = d.policy_choice();
            let policy_name = policy.name().to_owned();
            let request = ProvisionRequest {
                identity: d.identity.clone(),
                policy,
                storage_root: d.storage_root.clone(),
            };
            let session = d.session.clone();
            let tx = self.tx.clone();
            let ctx = ctx.clone();
            self.rt.spawn(async move {
                let outcome = session.provision_certificate(&request).await;
                let _ = tx.send(AppMsg::Provisioned {
                    identity: request.identity,
                    policy: policy_name,
                    outcome,
                });
                ctx.request_repaint();
            });
        }
    }
}

impl eframe::App for MqttApp {
    fn ui(&mut self, ui: &mut egui::Ui, _frame: &mut eframe::Frame) {
        self.frame(ui);
    }

    fn save(&mut self, storage: &mut dyn eframe::Storage) {
        eframe::set_value(storage, eframe::APP_KEY, &self.settings);
    }

    fn on_exit(&mut self, _gl: Option<&eframe::glow::Context>) {
        if let Some(c) = &self.connection {
            c.disconnect();
            // Give the DISCONNECT a moment to flush before the runtime is torn down.
            std::thread::sleep(std::time::Duration::from_millis(300));
        }
    }
}

impl MqttApp {
    /// One frame of the whole window. Separate from [`eframe::App::ui`] so tests can drive it
    /// without an eframe window.
    fn frame(&mut self, ui: &mut egui::Ui) {
        let ctx = ui.ctx().clone();
        if let Some(watcher) = &mut self.theme_watcher {
            watcher.poll(&ctx);
        }
        if !self.started {
            self.started = true;
            self.load_from_aws(&ctx);
        }
        self.drain_messages(&ctx);
        self.expire_shadow_requests();
        theme::paint_background(&ctx);
        self.provision_dialog(&ctx);
        self.cli_dialog(&ctx);
        theme::paint_toast(&ctx);

        let gap = theme::tokens(&ctx).metrics.gap;
        let g = gap as i8;
        let edge = |left, right, top, bottom| Margin {
            left,
            right,
            top,
            bottom,
        };

        egui::Panel::top("topbar")
            .resizable(false)
            .show_separator_line(false)
            .frame(egui::Frame::new().inner_margin(edge(g, g, g, 0)))
            .show(ui, |ui| {
                theme::glass_frame(ui)
                    .inner_margin(Margin::symmetric(14, 8))
                    .show(ui, |ui| {
                        ui.set_width(ui.available_width());
                        self.top_bar(ui);
                    });
            });

        if self.settings.logs_open {
            egui::Panel::bottom("logs")
                .resizable(true)
                .show_separator_line(false)
                .default_size(250.0)
                .min_size(130.0)
                .frame(egui::Frame::new().inner_margin(edge(g, g, 0, g)))
                .show(ui, |ui| {
                    theme::glass_frame(ui).show(ui, |ui| {
                        ui.set_width(ui.available_width());
                        self.log_panel(ui);
                    });
                });
        }

        if self.settings.view == View::Simulator {
            egui::CentralPanel::no_frame()
                .frame(egui::Frame::new().inner_margin(Margin::same(g)))
                .show(ui, |ui| self.simulator_view(ui));
            return;
        }
        if self.settings.view == View::Shadows {
            egui::CentralPanel::no_frame()
                .frame(egui::Frame::new().inner_margin(Margin::same(g)))
                .show(ui, |ui| self.shadows_view(ui));
            return;
        }

        egui::Panel::left("sidebar")
            .resizable(true)
            .show_separator_line(false)
            .default_size(320.0)
            .min_size(280.0)
            .frame(egui::Frame::new().inner_margin(edge(g, 0, g, g)))
            .show(ui, |ui| {
                egui::ScrollArea::vertical()
                    .id_salt("sidebar_scroll")
                    .auto_shrink([false, false])
                    .show(ui, |ui| self.sidebar(ui));
            });

        egui::CentralPanel::no_frame()
            .frame(egui::Frame::new().inner_margin(Margin::same(g)))
            .show(ui, |ui| self.main_column(ui));
    }
}

/// User Properties to send: rows with an empty name are left out.
fn to_send(props: &[(String, String)]) -> Vec<(String, String)> {
    props
        .iter()
        .filter(|(name, _)| !name.trim().is_empty())
        .map(|(name, value)| (name.trim().to_owned(), value.clone()))
        .collect()
}

/// Rows of name/value pairs with a remove button each, and "Add property".
fn user_properties_editor(ui: &mut egui::Ui, id: &str, props: &mut Vec<(String, String)>) {
    let mut remove = None;
    for (index, (name, value)) in props.iter_mut().enumerate() {
        ui.horizontal(|ui| {
            ui.add(
                egui::TextEdit::singleline(name)
                    .id_salt((id, "name", index))
                    .hint_text("name")
                    .desired_width(130.0),
            );
            let width = (ui.available_width() - 34.0).max(80.0);
            ui.add(
                egui::TextEdit::singleline(value)
                    .id_salt((id, "value", index))
                    .hint_text("value")
                    .desired_width(width),
            );
            if theme::icon_button(ui, icons::CLOSE, "Remove property", true).clicked() {
                remove = Some(index);
            }
        });
    }
    if let Some(index) = remove {
        props.remove(index);
    }
    if theme::link_button(ui, &format!("{} Add property", icons::ADD)).clicked() {
        props.push((String::new(), String::new()));
    }
}

/// A collapsible User Properties editor with a count, or a note on MQTT 3.1.1.
fn user_properties_section(
    ui: &mut egui::Ui,
    id: &str,
    label: &str,
    open: &mut bool,
    v5: bool,
    props: &mut Vec<(String, String)>,
) {
    if !v5 {
        return;
    }
    let count = to_send(props).len();
    ui.horizontal(|ui| {
        theme::disclosure(ui, open, label);
        if count > 0 {
            theme::pill(ui, count.to_string(), Palette::of(ui).info);
        }
    });
    if *open {
        user_properties_editor(ui, id, props);
    }
}

/// The glass frame used by dialogs.
fn modal_frame(ctx: &egui::Context) -> egui::Frame {
    let tokens = theme::tokens(ctx);
    let colors = tokens.colors(ctx.theme());
    let m = tokens.metrics;
    egui::Frame::new()
        .fill(colors.glass_strong.0)
        .stroke(egui::Stroke::new(1.0, colors.glass_border.0))
        .corner_radius(egui::CornerRadius::same(m.card_radius))
        .inner_margin(Margin::same(m.card_padding + 6))
        .shadow(egui::epaint::Shadow {
            offset: [0, m.shadow_offset_y],
            blur: m.shadow_blur,
            spread: 0,
            color: colors.shadow.0,
        })
}

/// One received message: the topic (as a badge) and metadata, the payload, then any User
/// Properties behind a toggle. The topic and payload each have a copy button.
fn message_bubble(ui: &mut egui::Ui, message: &ReceivedMessage) {
    let p = Palette::of(ui);
    // Stable across frames and unique enough: messages are distinct by time and topic.
    let key = (
        message.timestamp.timestamp().as_nanosecond(),
        message.topic.as_str(),
    );
    theme::inset(ui, |ui| {
        ui.horizontal(|ui| {
            theme::copyable_pill(ui, &message.topic, p.accent, "Topic");
            ui.with_layout(egui::Layout::right_to_left(egui::Align::Center), |ui| {
                ui.label(
                    RichText::new(message.timestamp.strftime("%H:%M:%S%.3f").to_string())
                        .small()
                        .color(p.muted),
                );
                if message.retain {
                    theme::pill(ui, "retained", p.warn);
                }
                theme::pill(ui, format!("QoS {}", message.qos), p.info);
            });
        });
        ui.add_space(2.0);
        ui.horizontal_top(|ui| {
            // Left-aligned in a fixed-width column, leaving room for the copy button.
            let width = (ui.available_width() - 30.0).max(80.0);
            ui.allocate_ui_with_layout(
                egui::vec2(width, 0.0),
                egui::Layout::top_down(egui::Align::Min),
                |ui| {
                    ui.set_width(width);
                    ui.add(
                        egui::Label::new(
                            RichText::new(format_payload(&message.payload, MESSAGE_PAYLOAD_MAX))
                                .monospace(),
                        )
                        .wrap(),
                    );
                },
            );
            theme::copy_button(
                ui,
                ("payload", key),
                || String::from_utf8_lossy(&message.payload).into_owned(),
                "Payload",
                "Copy payload",
            );
        });
        if !message.user_properties.is_empty() {
            ui.add_space(2.0);
            let id = ui.make_persistent_id(("message_props", key));
            let mut open = ui.data(|d| d.get_temp::<bool>(id)).unwrap_or(false);
            theme::disclosure(
                ui,
                &mut open,
                &format!("User properties ({})", message.user_properties.len()),
            );
            ui.data_mut(|d| d.insert_temp(id, open));
            if open {
                ui.indent(("message_props_list", key), |ui| {
                    for (name, value) in &message.user_properties {
                        ui.label(
                            RichText::new(format!("{name}: {value}"))
                                .small()
                                .monospace(),
                        );
                    }
                });
            }
        }
    });
    ui.add_space(6.0);
}

/// The `GetConnection` fields worth seeing at a glance.
fn connection_essentials(info: &ConnectionInfo) -> Vec<(&'static str, String)> {
    let fmt_time = |t: &jiff::ZonedDateTime| t.strftime("%H:%M:%S").to_string();
    let yes_no = |b: bool| if b { "Yes" } else { "No" }.to_owned();
    let mut rows = vec![("Connected", yes_no(info.connected))];
    if let Some(t) = &info.connected_since {
        rows.push(("Since", fmt_time(t)));
    }
    rows.push(("Clean session", yes_no(info.clean_session)));
    if info.keep_alive_secs > 0 {
        rows.push(("Keep alive", format!("{} s", info.keep_alive_secs)));
    }
    if info.session_expiry_secs > 0 {
        rows.push(("Session expiry", format!("{} s", info.session_expiry_secs)));
    }
    rows
}

/// Everything else `GetConnection` returned.
fn connection_more(info: &ConnectionInfo) -> Vec<(&'static str, String)> {
    let fmt_time = |t: &jiff::ZonedDateTime| t.strftime("%Y-%m-%d %H:%M:%S").to_string();
    let mut rows = Vec::new();
    if let Some(v) = &info.client_id {
        rows.push(("Client ID", v.clone()));
    }
    if let Some(v) = &info.thing_name {
        rows.push(("Thing", v.clone()));
    }
    if let Some(t) = &info.connected_since {
        rows.push(("Connected at", fmt_time(t)));
    }
    if let Some(t) = &info.disconnected_since {
        rows.push(("Disconnected", fmt_time(t)));
    }
    if let Some(v) = &info.disconnect_reason {
        rows.push(("Reason", v.clone()));
    }
    if let Some(v) = &info.source {
        rows.push(("Client address", v.clone()));
    }
    if let Some(v) = &info.target {
        rows.push(("Endpoint address", v.clone()));
    }
    if let Some(v) = &info.vpc_endpoint_id {
        rows.push(("VPC endpoint", v.clone()));
    }
    rows
}

fn key_values(ui: &mut egui::Ui, id: &str, rows: &[(&'static str, String)]) {
    let p = Palette::of(ui);
    egui::Grid::new(id)
        .num_columns(2)
        .spacing([12.0, 4.0])
        .show(ui, |ui| {
            for (key, value) in rows {
                ui.label(RichText::new(*key).small().color(p.muted));
                ui.label(RichText::new(value).small());
                ui.end_row();
            }
        });
}

/// Renders a log as monospace rows: time, direction, kind, details. Errors are red.
/// Log rows, newest at the bottom. With `copyable`, sent PUBLISH and SUBSCRIBE rows get a
/// "Copy as mosquitto_pub/sub" button; returns the one clicked.
fn log_rows(
    ui: &mut egui::Ui,
    id: &str,
    rows: &[&PacketLogEntry],
    kind_width: usize,
    copyable: bool,
) -> Option<Replay> {
    let p = Palette::of(ui);
    let mut clicked = None;
    let row_height = ui.text_style_height(&egui::TextStyle::Monospace) + 6.0;
    egui::ScrollArea::both()
        .id_salt(id)
        .stick_to_bottom(true)
        .auto_shrink([false, false])
        .show_rows(ui, row_height, rows.len(), |ui, range| {
            ui.spacing_mut().item_spacing.y = 0.0;
            for (offset, entry) in rows[range.clone()].iter().enumerate() {
                let (arrow, color) = match entry.direction {
                    Direction::Sent => (icons::SENT, p.info),
                    Direction::Received => (icons::RECEIVED, p.ok),
                };
                let failed = entry.direction == Direction::Received
                    && (entry.details.starts_with("ERROR") || entry.details.starts_with("TIMEOUT"));
                let fill = if (range.start + offset) % 2 == 1 {
                    p.inset
                } else {
                    Color32::TRANSPARENT
                };
                egui::Frame::new()
                    .fill(fill)
                    .corner_radius(egui::CornerRadius::same(6))
                    .inner_margin(Margin::symmetric(8, 3))
                    .show(ui, |ui| {
                        ui.set_min_width(ui.available_width());
                        ui.horizontal(|ui| {
                            ui.spacing_mut().item_spacing.x = 12.0;
                            ui.label(
                                RichText::new(entry.timestamp.strftime("%H:%M:%S%.3f").to_string())
                                    .monospace()
                                    .color(p.muted),
                            );
                            ui.label(
                                RichText::new(format!("{arrow} {:<kind_width$}", entry.kind))
                                    .monospace()
                                    .color(if failed { p.err } else { color }),
                            );
                            if copyable {
                                // Before the details, so it stays visible when they're long.
                                // Only for what the app sent: that's what a command repeats.
                                let replay = entry
                                    .replay
                                    .as_ref()
                                    .filter(|_| entry.direction == Direction::Sent);
                                match replay {
                                    Some(replay) => {
                                        let hover = match replay {
                                            Replay::Publish { .. } => "Copy as mosquitto_pub",
                                            Replay::Subscribe { .. } => "Copy as mosquitto_sub",
                                        };
                                        if theme::inline_icon_button(ui, icons::COPY, hover)
                                            .clicked()
                                        {
                                            clicked = Some(replay.clone());
                                        }
                                    }
                                    None => {
                                        let w = ui.text_style_height(&egui::TextStyle::Monospace);
                                        ui.add_space(w);
                                    }
                                }
                            }
                            ui.label(RichText::new(&entry.details).monospace().color(if failed {
                                p.err
                            } else {
                                p.text
                            }));
                        });
                    });
            }
        });
    clicked
}

fn endpoint_hover(e: &DataEndpoint) -> String {
    let mut s = e.domain_name.clone();
    if e.is_default {
        s.push_str("\nThe account's default data endpoint (iot:Data-ATS)");
    }
    if let Some(t) = &e.domain_type {
        s.push_str(&format!("\nDomain type: {t}"));
    }
    if let Some(st) = &e.status {
        s.push_str(&format!("\nStatus: {st}"));
    }
    s
}

fn show_error(ui: &mut egui::Ui, error: Option<&AppError>) {
    let Some(e) = error else { return };
    let p = Palette::of(ui);
    theme::banner(ui, Severity::Error, icons::WARNING, false, |ui| {
        ui.label(theme::title(&e.message).color(p.err));
        if let Some(action) = &e.action {
            ui.label(RichText::new(action).small().color(p.muted));
        }
        // Short details read fine inline; long ones (such as the whole credential provider
        // chain) stay behind a toggle.
        if let Some(details) = &e.details {
            if is_short(details) {
                ui.label(RichText::new(details).small());
            } else {
                let id = ui.scope_id().with(("error_details", &e.message));
                let mut open = ui.data(|d| d.get_temp::<bool>(id)).unwrap_or(false);
                theme::disclosure(ui, &mut open, "Details");
                ui.data_mut(|d| d.insert_temp(id, open));
                if open {
                    ui.label(RichText::new(details).small().monospace());
                }
            }
        }
    });
}

/// One line of at most 120 characters.
fn is_short(text: &str) -> bool {
    text.len() <= 120 && !text.contains('\n')
}

/// Shows the file name and a "Choose…" button. Returns a newly picked path.
fn file_row(
    ui: &mut egui::Ui,
    current: Option<&Path>,
    what: &str,
    exts: &[&str],
) -> Option<PathBuf> {
    let p = Palette::of(ui);
    let mut picked = None;
    ui.horizontal(|ui| {
        let choose = theme::ghost_button(ui, "Choose…").min_size(egui::vec2(0.0, 26.0));
        if ui.add(choose).clicked() {
            let mut dialog = rfd::FileDialog::new()
                .add_filter(what, exts)
                .add_filter("All files", &["*"]);
            if let Some(dir) = current.and_then(Path::parent) {
                dialog = dialog.set_directory(dir);
            }
            picked = dialog.pick_file();
        }
        if let Some(path) = current {
            let name = path
                .file_name()
                .map(|n| n.to_string_lossy().into_owned())
                .unwrap_or_else(|| path.display().to_string());
            ui.label(RichText::new(name).monospace())
                .on_hover_text(path.display().to_string());
            if theme::icon_button(ui, icons::REFRESH, "Reload from disk", true).clicked() {
                picked = Some(path.to_owned());
            }
        } else {
            ui.label(RichText::new("No file selected").color(p.muted));
        }
    });
    picked
}

fn qos_picker(ui: &mut egui::Ui, id: &str, qos: &mut u8) {
    // AWS IoT Core supports QoS 0 and 1 only, so QoS 2 is not offered.
    egui::ComboBox::from_id_salt(id)
        .selected_text(format!("QoS {qos}"))
        .width(64.0)
        .show_ui(ui, |ui| {
            ui.selectable_value(qos, 0, "QoS 0");
            ui.selectable_value(qos, 1, "QoS 1");
        });
}

fn short_id(id: &str) -> String {
    if id.len() > 12 {
        format!("{}…", &id[..12])
    } else {
        id.to_owned()
    }
}

/// `endpoint:port · protocol · client`, for the connected bar.
fn summarize(settings: &Settings, client_id: Option<String>) -> String {
    let endpoint = settings.endpoint.trim();
    let endpoint = if endpoint.is_empty() {
        "No endpoint"
    } else {
        endpoint
    };
    let mut summary = format!(
        "{endpoint}:{} · {}",
        settings.port,
        settings.mqtt_version.label()
    );
    if let Some(client) = client_id.filter(|c| !c.is_empty()) {
        summary.push_str(&format!(" · {client}"));
    }
    summary
}

/// Plain-language list of what's missing before a connection can start.
fn missing_requirements(
    has_endpoint: bool,
    has_client_id: bool,
    mode: IdentityMode,
    has_cert: bool,
    has_key: bool,
) -> Vec<&'static str> {
    let mut missing = Vec::new();
    if !has_endpoint {
        missing.push("an endpoint");
    }
    if !has_client_id {
        missing.push(match mode {
            IdentityMode::Thing => "a Thing",
            IdentityMode::ClientId => "a client ID",
        });
    }
    if !has_cert {
        missing.push("a certificate");
    }
    if !has_key {
        missing.push("a private key");
    }
    missing
}

#[cfg(test)]
mod ui_tests;

#[cfg(test)]
mod tests {
    use super::*;

    /// Renders every view headlessly in both themes: the top bar, the Client tab, and the
    /// simulator for every operation with all sections expanded. Catches layout panics and
    /// duplicate-ID assertions. Makes no AWS calls (the initial load only runs in `App::ui`).
    #[test]
    fn every_view_renders_without_panicking() {
        let rt = Arc::new(tokio::runtime::Runtime::new().unwrap());
        for theme in [egui::Theme::Dark, egui::Theme::Light] {
            let ctx = egui::Context::default();
            let mut settings = Settings::default();
            settings.sim.retain = true;
            settings.sim.attributes = "location=Seattle".into();
            let mut app = MqttApp::with_settings(&ctx, rt.clone(), settings);
            ctx.set_theme(theme);
            app.sim.show_context = true;
            app.sim.show_trace = true;
            app.show_advanced = true;
            app.settings.logs_open = true;
            for op in [
                simulator::SimOp::Connect,
                simulator::SimOp::Publish,
                simulator::SimOp::Subscribe,
                simulator::SimOp::Receive,
            ] {
                app.settings.sim.operation = op;
                let mut out = ctx.run_ui(Default::default(), |ui| {
                    theme::paint_background(ui.ctx());
                    app.top_bar(ui);
                    app.shadows_view(ui);
                    app.simulator_view(ui);
                    app.sidebar(ui);
                    app.main_column(ui);
                    app.log_panel(ui);
                });
                out.textures_delta.clear();
            }
        }
    }

    #[test]
    fn only_short_single_line_details_are_shown_inline() {
        assert!(is_short("certificate has expired"));
        assert!(!is_short(
            "no credentials found in chain. Attempted:\n  Environment: …"
        ));
        assert!(!is_short(&"x".repeat(121)));
    }

    #[test]
    fn short_ids() {
        assert_eq!(short_id("abc"), "abc");
        assert_eq!(short_id("0123456789abcdef"), "0123456789ab…");
    }

    #[test]
    fn theme_choices_cycle_and_map_to_egui_preferences() {
        assert_eq!(ThemeChoice::default(), ThemeChoice::System);
        let mut choice = ThemeChoice::System;
        let mut seen = Vec::new();
        for _ in 0..3 {
            seen.push(choice.preference());
            choice = choice.next();
        }
        assert_eq!(
            choice,
            ThemeChoice::System,
            "three steps return to the start"
        );
        assert_eq!(
            seen,
            [
                egui::ThemePreference::System,
                egui::ThemePreference::Light,
                egui::ThemePreference::Dark
            ]
        );
    }

    #[test]
    fn summary_mentions_endpoint_protocol_and_client() {
        let settings = Settings {
            endpoint: "abc-ats.iot.us-east-1.amazonaws.com".into(),
            ..Default::default()
        };
        let summary = summarize(&settings, Some("probe-1".into()));
        assert_eq!(
            summary,
            "abc-ats.iot.us-east-1.amazonaws.com:8883 · MQTT 5 · probe-1"
        );
        assert!(summarize(&Settings::default(), None).starts_with("No endpoint"));
    }

    #[test]
    fn missing_requirements_name_each_gap() {
        assert!(missing_requirements(true, true, IdentityMode::Thing, true, true).is_empty());
        assert_eq!(
            missing_requirements(false, false, IdentityMode::Thing, false, false),
            ["an endpoint", "a Thing", "a certificate", "a private key"]
        );
        assert_eq!(
            missing_requirements(true, false, IdentityMode::ClientId, true, true),
            ["a client ID"]
        );
    }

    #[test]
    fn connection_info_splits_into_essentials_and_more() {
        let info = ConnectionInfo {
            client_id: Some("probe-1".into()),
            thing_name: None,
            connected: true,
            clean_session: true,
            keep_alive_secs: 30,
            session_expiry_secs: 0,
            connected_since: None,
            disconnected_since: None,
            disconnect_reason: None,
            source: Some("10.0.0.1:5555".into()),
            target: None,
            vpc_endpoint_id: None,
        };
        let essentials = connection_essentials(&info);
        assert!(essentials.iter().any(|(k, _)| *k == "Keep alive"));
        assert!(!essentials.iter().any(|(k, _)| *k == "Session expiry"));
        let more = connection_more(&info);
        assert!(more
            .iter()
            .any(|(k, v)| *k == "Client address" && v == "10.0.0.1:5555"));
    }
}
