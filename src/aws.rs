// Copyright 2026 Nick Volgas
// SPDX-License-Identifier: Apache-2.0

//! AWS account discovery: profiles, the IoT data endpoint, Things, and their certificates.
//!
//! Every call here is read-only (Describe*/List*). The only write operations live in
//! `provision.rs` and run only when the user confirms the "Create certificate" dialog.

use std::collections::BTreeSet;
use std::path::PathBuf;
use std::time::Duration;

use aws_config::BehaviorVersion;
use std::sync::Arc;
use std::time::Instant;

use aws_sdk_iot::config::{ProvideCredentials, Region};
use aws_sdk_iot::error::{DisplayErrorContext, ProvideErrorMetadata, SdkError};
use aws_sdk_iot::operation::RequestId;
use aws_sdk_iot::types::ServiceType;

use crate::error::{categorize_message, AppError, ErrorCategory};
use crate::packet_log::{Direction, PacketLogEntry};

/// Timeout applied to each AWS API call (matches the original app's 10 s endpoint timeout).
pub const API_TIMEOUT: Duration = Duration::from_secs(10);
/// Things fetched by the single `ListThings` call. Other Things are looked up by exact name.
pub const THINGS_PAGE_SIZE: i32 = 100;

/// Regions where AWS IoT Core is offered (used to populate the region picker; free text is also allowed).
pub const IOT_REGIONS: &[&str] = &[
    "us-east-1",
    "us-east-2",
    "us-west-1",
    "us-west-2",
    "ca-central-1",
    "ca-west-1",
    "sa-east-1",
    "mx-central-1",
    "eu-west-1",
    "eu-west-2",
    "eu-west-3",
    "eu-central-1",
    "eu-central-2",
    "eu-north-1",
    "eu-south-1",
    "eu-south-2",
    "me-south-1",
    "me-central-1",
    "il-central-1",
    "af-south-1",
    "ap-east-1",
    "ap-south-1",
    "ap-south-2",
    "ap-northeast-1",
    "ap-northeast-2",
    "ap-northeast-3",
    "ap-southeast-1",
    "ap-southeast-2",
    "ap-southeast-3",
    "ap-southeast-4",
    "ap-southeast-5",
    "ap-southeast-7",
    "cn-north-1",
    "cn-northwest-1",
    "us-gov-east-1",
    "us-gov-west-1",
];

// ---------------------------------------------------------------------------------------------
// Profiles
// ---------------------------------------------------------------------------------------------

/// Lists profile names from the shared config and credentials files
/// (honoring `AWS_CONFIG_FILE` / `AWS_SHARED_CREDENTIALS_FILE`).
pub fn list_profiles() -> Vec<String> {
    let home = crate::paths::home_dir().unwrap_or_default();
    let path = |var: &str, default: &str| {
        std::env::var_os(var)
            .map(PathBuf::from)
            .unwrap_or_else(|| home.join(".aws").join(default))
    };
    let config = std::fs::read_to_string(path("AWS_CONFIG_FILE", "config")).unwrap_or_default();
    let creds = std::fs::read_to_string(path("AWS_SHARED_CREDENTIALS_FILE", "credentials"))
        .unwrap_or_default();
    parse_profile_names(&config, &creds)
}

/// Extracts profile names. In the config file, sections are `[default]` or `[profile name]`
/// (other section types like `[sso-session x]` are skipped); in the credentials file every
/// section is a profile.
pub fn parse_profile_names(config: &str, credentials: &str) -> Vec<String> {
    let mut names = BTreeSet::new();
    for header in section_headers(config) {
        if header == "default" {
            names.insert(header.to_owned());
        } else if let Some(rest) = header.strip_prefix("profile ") {
            let name = rest.trim();
            if !name.is_empty() {
                names.insert(name.to_owned());
            }
        }
    }
    for header in section_headers(credentials) {
        if !header.is_empty() && !header.contains(' ') {
            names.insert(header.to_owned());
        }
    }
    let mut out: Vec<String> = names.into_iter().collect();
    // Keep "default" first if present.
    if let Some(pos) = out.iter().position(|n| n == "default") {
        let d = out.remove(pos);
        out.insert(0, d);
    }
    out
}

fn section_headers(text: &str) -> impl Iterator<Item = &str> {
    text.lines().filter_map(|line| {
        let l = line.trim();
        l.strip_prefix('[')?.strip_suffix(']').map(str::trim)
    })
}

// ---------------------------------------------------------------------------------------------
// Discovery
// ---------------------------------------------------------------------------------------------

#[derive(Debug, Clone, PartialEq, Eq)]
pub struct ThingSummary {
    pub name: String,
    pub thing_type: Option<String>,
    pub arn: Option<String>,
}

/// The first page of `ListThings`.
#[derive(Debug, Clone, PartialEq, Eq, Default)]
pub struct ThingPage {
    pub things: Vec<ThingSummary>,
    /// True if the account has more Things than were returned.
    pub has_more: bool,
}

/// Receives one entry when each AWS API request is sent and one when its response arrives.
pub type ApiLogSink = Arc<dyn Fn(PacketLogEntry) + Send + Sync>;

/// A sink that drops log entries (for tests and callers that don't display them).
pub fn discard_log() -> ApiLogSink {
    Arc::new(|_| {})
}

#[derive(Debug, Clone, PartialEq, Eq)]
pub struct CallerIdentity {
    pub account: String,
    pub arn: String,
}

/// Resolved credentials plus an IoT control-plane client.
#[derive(Clone)]
pub struct Session {
    pub client: aws_sdk_iot::Client,
    pub region: String,
    /// From `sts:GetCallerIdentity`; `None` if that call failed (it's non-fatal).
    pub identity: Option<CallerIdentity>,
    /// Kept so data-plane clients can be built with the same credentials and region.
    config: Arc<aws_config::SdkConfig>,
    log: ApiLogSink,
}

/// Loads the SDK config via the standard chain (env, profile, SSO, process, IMDS/ECS) and
/// verifies that credentials actually resolve, so failures surface as credential errors.
pub async fn open_session(
    profile: Option<&str>,
    region: Option<&str>,
    log: ApiLogSink,
) -> Result<Session, AppError> {
    let profile = profile.filter(|p| !p.is_empty());
    let mut loader = aws_config::defaults(BehaviorVersion::latest());
    if let Some(p) = profile {
        loader = loader.profile_name(p);
    }
    if let Some(r) = region.filter(|r| !r.is_empty()) {
        loader = loader.region(Region::new(r.to_owned()));
    }
    let config = loader.load().await;
    session_from_config(config, profile, log).await
}

/// Builds a [`Session`] from a loaded SDK config: checks the region, resolves credentials, and
/// looks up the caller identity. Split from [`open_session`] so tests can supply the config.
pub(crate) async fn session_from_config(
    config: aws_config::SdkConfig,
    profile: Option<&str>,
    log: ApiLogSink,
) -> Result<Session, AppError> {
    let region = config.region().map(|r| r.to_string()).ok_or_else(|| {
        AppError::configuration("No AWS region configured")
            .with_action("Choose a region, or set one on the selected profile / AWS_REGION.")
    })?;

    let provider = config
        .credentials_provider()
        .ok_or_else(|| AppError::credential("No AWS credentials provider is available"))?;

    const OP: &str = "ResolveCredentials";
    log(PacketLogEntry::new(
        Direction::Sent,
        OP,
        format!(
            "profile={} region={region}",
            profile.unwrap_or("(default chain)")
        ),
    ));
    let started = Instant::now();
    let resolved = tokio::time::timeout(API_TIMEOUT, provider.provide_credentials()).await;
    let ms = started.elapsed().as_millis();
    match resolved {
        Err(_) => {
            log(PacketLogEntry::new(
                Direction::Received,
                OP,
                format!("TIMEOUT after {}s", API_TIMEOUT.as_secs()),
            ));
            return Err(
                AppError::credential("Timed out resolving AWS credentials").with_action(
                    "If this profile uses SSO, run `aws sso login --profile <name>` and retry.",
                ),
            );
        }
        Ok(Err(e)) => {
            let details = DisplayErrorContext(&e).to_string();
            log(PacketLogEntry::new(
                Direction::Received,
                OP,
                format!("ERROR ({ms} ms) {details}"),
            ));
            return Err(AppError::credential("Could not resolve AWS credentials")
                .with_details(details)
                .with_action(
                    "Check the selected profile, or run `aws sso login` / refresh your credentials and retry.",
                ));
        }
        Ok(Ok(creds)) => {
            let kind = match creds.expiry() {
                Some(t) => format!(
                    "temporary credentials, expire {}",
                    jiff::ZonedDateTime::try_from(t)
                        .map(|z| z.strftime("%Y-%m-%d %H:%M:%S").to_string())
                        .unwrap_or_else(|_| "at an unknown time".into())
                ),
                None => "long-term credentials".to_owned(),
            };
            log(PacketLogEntry::new(
                Direction::Received,
                OP,
                format!(
                    "OK ({ms} ms) accessKeyId={} {kind}",
                    mask(creds.access_key_id())
                ),
            ));
        }
    }

    let mut session = Session {
        client: aws_sdk_iot::Client::new(&config),
        region,
        identity: None,
        config: Arc::new(config.clone()),
        log,
    };
    let sts = aws_sdk_sts::Client::new(&config);
    if let Ok(out) = session
        .call(
            "GetCallerIdentity",
            "",
            sts.get_caller_identity().send(),
            |o| {
                format!(
                    "account={} arn={}",
                    o.account().unwrap_or("?"),
                    o.arn().unwrap_or("?")
                )
            },
        )
        .await
    {
        if let (Some(account), Some(arn)) = (out.account(), out.arn()) {
            session.identity = Some(CallerIdentity {
                account: account.to_owned(),
                arn: arn.to_owned(),
            });
        }
    }
    Ok(session)
}

/// Shows only the last four characters of an identifier.
fn mask(s: &str) -> String {
    let tail: String = s
        .chars()
        .rev()
        .take(4)
        .collect::<Vec<_>>()
        .into_iter()
        .rev()
        .collect();
    format!("****{tail}")
}

/// A data endpoint the client can connect to.
#[derive(Debug, Clone, PartialEq, Eq)]
pub struct DataEndpoint {
    /// "Default" for the account's `iot:Data-ATS` endpoint, otherwise the domain configuration name.
    pub label: String,
    pub domain_name: String,
    pub is_default: bool,
    /// Domain configuration status (`ENABLED` / `DISABLED`); `None` for the default endpoint.
    pub status: Option<String>,
    /// `ENDPOINT`, `AWS_MANAGED`, or `CUSTOMER_MANAGED`.
    pub domain_type: Option<String>,
}

impl DataEndpoint {
    pub fn is_enabled(&self) -> bool {
        self.status.as_deref().is_none_or(|s| s == "ENABLED")
    }
}

/// Endpoints found, plus a non-fatal warning if some couldn't be read.
#[derive(Debug, Clone)]
pub struct EndpointDiscovery {
    pub endpoints: Vec<DataEndpoint>,
    pub warning: Option<AppError>,
}

pub const DEFAULT_ENDPOINT_LABEL: &str = "Default";

impl Session {
    pub fn account(&self) -> Option<&str> {
        self.identity.as_ref().map(|i| i.account.as_str())
    }

    /// AWS partition (`aws`, `aws-cn`, `aws-us-gov`), from the caller ARN; defaults to `aws`.
    pub fn partition(&self) -> &str {
        self.identity
            .as_ref()
            .and_then(|i| i.arn.split(':').nth(1))
            .filter(|p| !p.is_empty())
            .unwrap_or("aws")
    }

    /// Sends one AWS request with a timeout, logging the request and the response.
    /// `request` describes the parameters; never put secrets in it or in `summarize`'s output.
    pub(crate) async fn call<T, E>(
        &self,
        op: &str,
        request: impl Into<String>,
        fut: impl std::future::Future<Output = Result<T, SdkError<E>>>,
        summarize: impl FnOnce(&T) -> String,
    ) -> Result<T, AppError>
    where
        T: RequestId,
        E: ProvideErrorMetadata + std::error::Error + Send + Sync + 'static,
    {
        (self.log)(PacketLogEntry::new(Direction::Sent, op, request));
        let started = Instant::now();
        let result = tokio::time::timeout(API_TIMEOUT, fut).await;
        let ms = started.elapsed().as_millis();
        let request_id =
            |id: Option<&str>| id.map(|i| format!(" requestId={i}")).unwrap_or_default();
        match result {
            Err(_) => {
                (self.log)(PacketLogEntry::new(
                    Direction::Received,
                    op,
                    format!("TIMEOUT after {}s", API_TIMEOUT.as_secs()),
                ));
                Err(AppError::network(format!(
                    "{op} timed out after {} seconds",
                    API_TIMEOUT.as_secs()
                )))
            }
            Ok(Err(e)) => {
                let what = match (e.code(), e.message()) {
                    (Some(c), Some(m)) => format!("{c}: {m}"),
                    (Some(c), None) => c.to_owned(),
                    _ => DisplayErrorContext(&e).to_string(),
                };
                (self.log)(PacketLogEntry::new(
                    Direction::Received,
                    op,
                    format!("ERROR ({ms} ms){} {what}", request_id(e.request_id())),
                ));
                Err(map_sdk_error(op, e))
            }
            Ok(Ok(v)) => {
                let summary = summarize(&v);
                let sep = if summary.is_empty() { "" } else { " " };
                (self.log)(PacketLogEntry::new(
                    Direction::Received,
                    op,
                    format!("OK ({ms} ms){}{sep}{summary}", request_id(v.request_id())),
                ));
                Ok(v)
            }
        }
    }

    /// The default `iot:Data-ATS` endpoint plus every enabled-or-disabled DATA domain
    /// configuration. AWS-managed `iot:*` configurations are skipped: the default is covered
    /// by `DescribeEndpoint`, and the legacy `iot:Data` endpoint uses a CA this app doesn't trust.
    pub async fn list_data_endpoints(&self) -> Result<EndpointDiscovery, AppError> {
        let mut endpoints = Vec::new();
        let mut problems = Vec::new();

        let default = self
            .call(
                "DescribeEndpoint",
                "endpointType=iot:Data-ATS",
                self.client
                    .describe_endpoint()
                    .endpoint_type("iot:Data-ATS")
                    .send(),
                |o| format!("endpointAddress={}", o.endpoint_address().unwrap_or("")),
            )
            .await;
        let default_error = match default {
            Ok(out) => match out.endpoint_address().filter(|a| !a.is_empty()) {
                Some(addr) => {
                    endpoints.push(DataEndpoint {
                        label: DEFAULT_ENDPOINT_LABEL.to_owned(),
                        domain_name: addr.to_owned(),
                        is_default: true,
                        status: None,
                        domain_type: None,
                    });
                    None
                }
                None => Some(AppError::network(
                    "No endpoint address returned from AWS IoT",
                )),
            },
            Err(e) => Some(e),
        };

        match self.list_domain_configuration_names().await {
            Ok(names) => {
                for name in names {
                    match self.describe_domain_configuration(&name).await {
                        Ok(Some(ep)) => endpoints.push(ep),
                        Ok(None) => {}
                        Err(e) => problems.push(format!("{name}: {}", e.summary())),
                    }
                }
            }
            Err(e) => problems.push(e.summary()),
        }

        if endpoints.is_empty() {
            return Err(default_error.unwrap_or_else(|| {
                AppError::network("No IoT data endpoints found").with_details(problems.join("\n"))
            }));
        }
        if let Some(e) = &default_error {
            problems.insert(0, e.summary());
        }
        let warning = (!problems.is_empty()).then(|| {
            AppError::credential("Some endpoints could not be listed")
                .with_details(problems.join("\n"))
                .with_action(
                    "Custom domains need iot:ListDomainConfigurations and iot:DescribeDomainConfiguration. You can still type an endpoint manually.",
                )
        });
        Ok(EndpointDiscovery { endpoints, warning })
    }

    async fn list_domain_configuration_names(&self) -> Result<Vec<String>, AppError> {
        let mut names = Vec::new();
        let mut marker: Option<String> = None;
        loop {
            let out = self
                .call(
                    "ListDomainConfigurations",
                    format!(
                        "serviceType=DATA{}",
                        marker.as_ref().map(|_| " marker=…").unwrap_or("")
                    ),
                    self.client
                        .list_domain_configurations()
                        .service_type(ServiceType::Data)
                        .page_size(250)
                        .set_marker(marker.take())
                        .send(),
                    |o| format!("{} domain configurations", o.domain_configurations().len()),
                )
                .await?;
            names.extend(
                out.domain_configurations()
                    .iter()
                    .filter_map(|d| d.domain_configuration_name())
                    .filter(|n| !n.starts_with("iot:"))
                    .map(str::to_owned),
            );
            marker = out
                .next_marker()
                .filter(|m| !m.is_empty())
                .map(str::to_owned);
            if marker.is_none() {
                break;
            }
        }
        names.sort_by_key(|n| n.to_lowercase());
        Ok(names)
    }

    async fn describe_domain_configuration(
        &self,
        name: &str,
    ) -> Result<Option<DataEndpoint>, AppError> {
        let out = self
            .call(
                "DescribeDomainConfiguration",
                format!("domainConfigurationName={name}"),
                self.client
                    .describe_domain_configuration()
                    .domain_configuration_name(name)
                    .send(),
                |o| {
                    format!(
                        "domainName={} status={}",
                        o.domain_name().unwrap_or(""),
                        o.domain_configuration_status()
                            .map(|s| s.as_str())
                            .unwrap_or("?")
                    )
                },
            )
            .await?;
        Ok(out
            .domain_name()
            .filter(|d| !d.is_empty())
            .map(|domain| DataEndpoint {
                label: name.to_owned(),
                domain_name: domain.to_owned(),
                is_default: false,
                status: out
                    .domain_configuration_status()
                    .map(|s| s.as_str().to_owned()),
                domain_type: out.domain_type().map(|t| t.as_str().to_owned()),
            }))
    }

    /// One `ListThings` call (no pagination), sorted by name. Large fleets aren't listed in
    /// full; use [`Session::describe_thing`] for a Thing that isn't on this page.
    pub async fn list_things_first_page(&self) -> Result<ThingPage, AppError> {
        let out = self
            .call(
                "ListThings",
                format!("maxResults={THINGS_PAGE_SIZE}"),
                self.client
                    .list_things()
                    .max_results(THINGS_PAGE_SIZE)
                    .send(),
                |o| {
                    format!(
                        "{} things{}",
                        o.things().len(),
                        if o.next_token().is_some_and(|t| !t.is_empty()) {
                            " (more available)"
                        } else {
                            ""
                        }
                    )
                },
            )
            .await?;
        let mut things: Vec<ThingSummary> = out
            .things()
            .iter()
            .filter_map(|t| {
                Some(ThingSummary {
                    name: t.thing_name()?.to_owned(),
                    thing_type: t.thing_type_name().map(str::to_owned),
                    arn: t.thing_arn().map(str::to_owned),
                })
            })
            .collect();
        things.sort_by_key(|t| t.name.to_lowercase());
        Ok(ThingPage {
            things,
            has_more: out.next_token().is_some_and(|t| !t.is_empty()),
        })
    }

    /// `DescribeThing` for a name the user typed.
    pub async fn describe_thing(&self, thing_name: &str) -> Result<ThingSummary, AppError> {
        let out = self
            .call(
                "DescribeThing",
                format!("thingName={thing_name}"),
                self.client.describe_thing().thing_name(thing_name).send(),
                |o| format!("thingArn={}", o.thing_arn().unwrap_or("?")),
            )
            .await
            .map_err(|e| {
                if e.message.contains("ResourceNotFoundException") {
                    AppError::configuration(format!(
                        "Thing \"{thing_name}\" doesn't exist in {}",
                        self.region
                    ))
                    .with_action("Thing names are case-sensitive. Check the name and region.")
                } else {
                    e
                }
            })?;
        Ok(ThingSummary {
            name: out.thing_name().unwrap_or(thing_name).to_owned(),
            thing_type: out.thing_type_name().map(str::to_owned),
            arn: out.thing_arn().map(str::to_owned),
        })
    }

    fn data_client(&self, data_endpoint: Option<&str>) -> aws_sdk_iotdataplane::Client {
        let mut builder = aws_sdk_iotdataplane::config::Builder::from(self.config.as_ref());
        if let Some(ep) = data_endpoint.filter(|e| !e.is_empty()) {
            builder = builder.endpoint_url(format!("https://{ep}"));
        }
        aws_sdk_iotdataplane::Client::from_conf(builder.build())
    }

    /// `GetConnection` (IoT data plane) for a connected client. `data_endpoint` should be the
    /// account's default `iot:Data-ATS` hostname.
    pub async fn get_connection(
        &self,
        data_endpoint: Option<&str>,
        client_id: &str,
        include_socket_information: bool,
    ) -> Result<ConnectionInfo, AppError> {
        let client = self.data_client(data_endpoint);
        let out = self
            .call(
                "GetConnection",
                format!(
                    "clientId={client_id} includeSocketInformation={include_socket_information}"
                ),
                client
                    .get_connection()
                    .client_id(client_id)
                    .include_socket_information(include_socket_information)
                    .send(),
                |o| {
                    format!(
                        "connected={} cleanSession={}",
                        o.connected(),
                        o.clean_session()
                    )
                },
            )
            .await?;
        // Absent numeric fields come back as 0.
        let millis = |ms: i64| {
            (ms > 0)
                .then(|| jiff::Timestamp::from_millisecond(ms).ok())
                .flatten()
                .map(|ts| ts.to_zoned_datetime(jiff::tz::TimeZone::system()))
        };
        Ok(ConnectionInfo {
            client_id: out.client_id().map(str::to_owned),
            thing_name: out.thing_name().map(str::to_owned),
            connected: out.connected(),
            clean_session: out.clean_session(),
            keep_alive_secs: out.keep_alive_duration(),
            session_expiry_secs: out.session_expiry(),
            connected_since: millis(out.connected_since()),
            disconnected_since: millis(out.disconnected_since()),
            disconnect_reason: out.disconnect_reason().map(str::to_owned),
            source: socket(out.source_ip(), out.source_port()),
            target: socket(out.target_ip(), out.target_port()),
            vpc_endpoint_id: out.vpc_endpoint_id().map(str::to_owned),
        })
    }

    /// `ListSubscriptions` (IoT data plane): every subscription the broker holds for the client.
    pub async fn list_subscriptions(
        &self,
        data_endpoint: Option<&str>,
        client_id: &str,
    ) -> Result<Vec<SubscriptionInfo>, AppError> {
        let client = self.data_client(data_endpoint);
        let mut subs = Vec::new();
        let mut token: Option<String> = None;
        loop {
            let out = self
                .call(
                    "ListSubscriptions",
                    format!(
                        "clientId={client_id} maxResults=200{}",
                        token.as_ref().map(|_| " nextToken=…").unwrap_or("")
                    ),
                    client
                        .list_subscriptions()
                        .client_id(client_id)
                        .max_results(200)
                        .set_next_token(token.take())
                        .send(),
                    |o| format!("{} subscriptions", o.subscriptions().len()),
                )
                .await?;
            subs.extend(out.subscriptions().iter().map(|s| SubscriptionInfo {
                topic_filter: s.topic_filter().to_owned(),
                qos: s.qos(),
            }));
            token = out
                .next_token()
                .filter(|t| !t.is_empty())
                .map(str::to_owned);
            if token.is_none() {
                break;
            }
        }
        subs.sort_by(|a, b| a.topic_filter.cmp(&b.topic_filter));
        Ok(subs)
    }
}

fn socket(ip: Option<&str>, port: i32) -> Option<String> {
    let ip = ip.filter(|i| !i.is_empty())?;
    Some(if port > 0 {
        format!("{ip}:{port}")
    } else {
        ip.to_owned()
    })
}

/// Broker-side view of a client connection (`GetConnection`).
#[derive(Debug, Clone, PartialEq)]
pub struct ConnectionInfo {
    pub client_id: Option<String>,
    pub thing_name: Option<String>,
    pub connected: bool,
    pub clean_session: bool,
    pub keep_alive_secs: i32,
    pub session_expiry_secs: i64,
    pub connected_since: Option<jiff::ZonedDateTime>,
    pub disconnected_since: Option<jiff::ZonedDateTime>,
    pub disconnect_reason: Option<String>,
    /// `ip:port` of the client, when socket information was requested and authorized.
    pub source: Option<String>,
    /// `ip:port` of the AWS IoT endpoint.
    pub target: Option<String>,
    pub vpc_endpoint_id: Option<String>,
}

/// One subscription held by the broker for a client (`ListSubscriptions`).
#[derive(Debug, Clone, PartialEq, Eq)]
pub struct SubscriptionInfo {
    pub topic_filter: String,
    pub qos: i32,
}

fn map_sdk_error<E>(op: &str, err: SdkError<E>) -> AppError
where
    E: ProvideErrorMetadata + std::error::Error + Send + Sync + 'static,
{
    let code = err.code().map(str::to_owned);
    let details = DisplayErrorContext(&err).to_string();
    let category = match code.as_deref() {
        Some(
            "UnauthorizedException"
            | "AccessDeniedException"
            | "AccessDenied"
            | "ExpiredToken"
            | "ExpiredTokenException"
            | "UnrecognizedClientException"
            | "InvalidClientTokenId"
            | "InvalidSignatureException"
            | "SignatureDoesNotMatch",
        ) => ErrorCategory::Credential,
        Some(_) => ErrorCategory::Configuration,
        None => match categorize_message(&details) {
            ErrorCategory::Configuration => ErrorCategory::Network,
            c => c,
        },
    };
    let message = match &code {
        Some(c) => format!("{op} failed: {c}"),
        None => format!("{op} failed"),
    };
    let err = AppError::new(category, message).with_details(details);
    if category == ErrorCategory::Credential {
        err.with_action(format!(
            "Ensure the credentials are current and allow iot:{op}."
        ))
    } else {
        err
    }
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn parses_profiles_from_both_files() {
        let config = "\
[default]
region = us-east-1

[profile dev]
region = us-west-2
[ profile  prod-ro ]
[sso-session corp]
sso_start_url = https://example
";
        let creds = "[default]\naws_access_key_id=x\n[legacy]\n";
        assert_eq!(
            parse_profile_names(config, creds),
            ["default", "dev", "legacy", "prod-ro"]
        );
    }

    #[test]
    fn parses_empty_files() {
        assert!(parse_profile_names("", "").is_empty());
    }

    #[test]
    fn socket_formatting() {
        assert_eq!(
            socket(Some("10.0.0.1"), 8883).as_deref(),
            Some("10.0.0.1:8883")
        );
        assert_eq!(socket(Some("10.0.0.1"), 0).as_deref(), Some("10.0.0.1"));
        assert_eq!(socket(None, 1), None);
        assert_eq!(socket(Some(""), 1), None);
    }

    #[test]
    fn masks_identifiers() {
        assert_eq!(mask("AKIAABCDEFGH1234"), "****1234");
        assert_eq!(mask("ab"), "****ab");
    }

    #[test]
    fn disabled_endpoints_are_not_enabled() {
        let mut ep = DataEndpoint {
            label: "custom".into(),
            domain_name: "iot.example.com".into(),
            is_default: false,
            status: Some("DISABLED".into()),
            domain_type: Some("CUSTOMER_MANAGED".into()),
        };
        assert!(!ep.is_enabled());
        ep.status = None;
        assert!(ep.is_enabled());
    }
}

/// `Session` calls against an in-process fake of the AWS endpoints (no network).
#[cfg(test)]
mod session_tests {
    use super::*;
    use crate::test_support::FakeAws;
    use aws_credential_types::provider::SharedCredentialsProvider;
    use aws_credential_types::provider::{error::CredentialsError, future};
    use serde_json::json;

    const ACCOUNT: &str = "123456789012";

    fn arn() -> String {
        format!("arn:aws:iam::{ACCOUNT}:role/Dev")
    }

    #[tokio::test]
    async fn opens_session_and_reads_identity_without_logging_secrets() {
        let fake = FakeAws::new().caller_identity(ACCOUNT, &arn());
        let (session, log) = fake.session().await;
        assert_eq!(session.region, "us-east-1");
        assert_eq!(session.account(), Some(ACCOUNT));
        assert_eq!(session.partition(), "aws");

        let text = log.text();
        assert!(text.contains("ResolveCredentials"), "{text}");
        assert!(text.contains("long-term credentials"), "{text}");
        assert!(text.contains("GetCallerIdentity OK"), "{text}");
        let creds = aws_credential_types::Credentials::for_tests();
        assert!(
            !text.contains(creds.access_key_id()),
            "access key logged: {text}"
        );
        assert!(
            !text.contains(creds.secret_access_key()),
            "secret logged: {text}"
        );
    }

    #[tokio::test]
    async fn identity_failure_is_not_fatal() {
        let fake = FakeAws::new().error("POST", "/", 403, "AccessDenied");
        let (session, log) = fake.session().await;
        assert_eq!(session.identity, None);
        assert_eq!(session.account(), None);
        assert_eq!(session.partition(), "aws");
        assert!(
            log.text().contains("GetCallerIdentity ERROR"),
            "{}",
            log.text()
        );
    }

    #[tokio::test]
    async fn partition_comes_from_the_caller_arn() {
        let fake = FakeAws::new().caller_identity(ACCOUNT, "arn:aws-cn:iam::123456789012:role/Dev");
        let (session, _) = fake.session().await;
        assert_eq!(session.partition(), "aws-cn");
    }

    #[tokio::test]
    async fn temporary_credentials_log_their_expiry() {
        let fake = FakeAws::new();
        let expires = std::time::SystemTime::now() + Duration::from_secs(3600);
        let creds = aws_credential_types::Credentials::new(
            "ASIATESTKEY1234",
            "not-a-real-secret",
            Some("not-a-real-token".into()),
            Some(expires),
            "test",
        );
        let log = crate::test_support::LogCapture::default();
        let config = fake.sdk_config_with(SharedCredentialsProvider::new(creds));
        session_from_config(config, None, log.sink()).await.unwrap();
        let text = log.text();
        assert!(text.contains("temporary credentials, expire 20"), "{text}");
        assert!(text.contains("profile=(default chain)"), "{text}");
        assert!(text.contains("****1234"), "{text}");
        assert!(!text.contains("not-a-real"), "{text}");
    }

    #[derive(Debug)]
    struct FailingProvider;

    impl ProvideCredentials for FailingProvider {
        fn provide_credentials<'a>(&'a self) -> future::ProvideCredentials<'a>
        where
            Self: 'a,
        {
            future::ProvideCredentials::ready(Err(CredentialsError::not_loaded("no profile")))
        }
    }

    #[tokio::test]
    async fn unresolvable_credentials_are_a_credential_error() {
        let fake = FakeAws::new();
        let log = crate::test_support::LogCapture::default();
        let config = fake.sdk_config_with(SharedCredentialsProvider::new(FailingProvider));
        let err = session_from_config(config, Some("dev"), log.sink())
            .await
            .err()
            .expect("credentials should fail");
        assert_eq!(err.category, ErrorCategory::Credential);
        assert!(err.action.unwrap().contains("aws sso login"));
        assert!(
            log.text().contains("ResolveCredentials ERROR"),
            "{}",
            log.text()
        );
        assert!(
            fake.requests().is_empty(),
            "no AWS call after a credential failure"
        );
    }

    #[tokio::test]
    async fn missing_region_is_a_configuration_error() {
        let config = aws_config::SdkConfig::builder()
            .behavior_version(BehaviorVersion::latest())
            .build();
        let err = session_from_config(config, None, discard_log())
            .await
            .err()
            .expect("no region");
        assert_eq!(err.category, ErrorCategory::Configuration);
    }

    #[tokio::test]
    async fn missing_credentials_provider_is_a_credential_error() {
        let config = aws_config::SdkConfig::builder()
            .behavior_version(BehaviorVersion::latest())
            .region(Region::new("us-east-1"))
            .build();
        let err = session_from_config(config, None, discard_log())
            .await
            .err()
            .expect("no provider");
        assert_eq!(err.category, ErrorCategory::Credential);
    }

    fn domain(name: &str) -> serde_json::Value {
        json!({ "domainConfigurationName": name, "serviceType": "DATA" })
    }

    fn described(name: &str, domain: &str, status: &str) -> serde_json::Value {
        json!({
            "domainConfigurationName": name,
            "domainName": domain,
            "domainConfigurationStatus": status,
            "domainType": "CUSTOMER_MANAGED",
        })
    }

    #[tokio::test]
    async fn lists_default_and_custom_endpoints_across_pages() {
        let fake = FakeAws::new()
            .json(
                "GET",
                "/endpoint",
                json!({ "endpointAddress": "abc-ats.iot.us-east-1.amazonaws.com" }),
            )
            .json_once(
                "GET",
                "/domainConfigurations",
                json!({
                    "domainConfigurations": [domain("zeta"), domain("iot:Data-ATS")],
                    "nextMarker": "page-2",
                }),
            )
            .json(
                "GET",
                "/domainConfigurations",
                json!({ "domainConfigurations": [domain("Alpha"), domain("empty")] }),
            )
            .json(
                "GET",
                "/domainConfigurations/zeta",
                described("zeta", "zeta.example.com", "DISABLED"),
            )
            .json(
                "GET",
                "/domainConfigurations/Alpha",
                described("Alpha", "alpha.example.com", "ENABLED"),
            )
            .json(
                "GET",
                "/domainConfigurations/empty",
                json!({ "domainConfigurationName": "empty" }),
            );
        let (session, log) = fake.session().await;
        let found = session.list_data_endpoints().await.unwrap();
        assert!(found.warning.is_none(), "{:?}", found.warning);

        let labels: Vec<_> = found.endpoints.iter().map(|e| e.label.as_str()).collect();
        assert_eq!(labels, [DEFAULT_ENDPOINT_LABEL, "Alpha", "zeta"]);
        assert!(found.endpoints[0].is_default);
        assert!(found.endpoints[0].is_enabled());
        assert_eq!(found.endpoints[1].domain_name, "alpha.example.com");
        assert_eq!(
            found.endpoints[1].domain_type.as_deref(),
            Some("CUSTOMER_MANAGED")
        );
        assert!(!found.endpoints[2].is_enabled());

        let pages = fake.requests_to("GET", "/domainConfigurations");
        assert_eq!(pages.len(), 2);
        assert!(
            pages[0].query.contains("serviceType=DATA"),
            "{}",
            pages[0].query
        );
        assert!(
            pages[1].query.contains("marker=page-2"),
            "{}",
            pages[1].query
        );
        assert!(
            fake.requests_to("GET", "/domainConfigurations/iot%3AData-ATS")
                .is_empty(),
            "AWS-managed configurations are skipped"
        );
        assert!(log.text().contains("requestId=req-1234"), "{}", log.text());
    }

    #[tokio::test]
    async fn missing_domain_permissions_leave_the_default_with_a_warning() {
        let fake = FakeAws::new()
            .json(
                "GET",
                "/endpoint",
                json!({ "endpointAddress": "abc-ats.iot.example" }),
            )
            .error("GET", "/domainConfigurations", 403, "AccessDeniedException");
        let (session, _) = fake.session().await;
        let found = session.list_data_endpoints().await.unwrap();
        assert_eq!(found.endpoints.len(), 1);
        let warning = found.warning.expect("warning");
        assert_eq!(warning.category, ErrorCategory::Credential);
        assert!(warning.details.unwrap().contains("AccessDeniedException"));
    }

    #[tokio::test]
    async fn one_unreadable_domain_is_reported_and_the_rest_listed() {
        let fake = FakeAws::new()
            .error("GET", "/endpoint", 401, "UnauthorizedException")
            .json(
                "GET",
                "/domainConfigurations",
                json!({ "domainConfigurations": [domain("good"), domain("bad")] }),
            )
            .json(
                "GET",
                "/domainConfigurations/good",
                described("good", "good.example.com", "ENABLED"),
            )
            .error(
                "GET",
                "/domainConfigurations/bad",
                403,
                "AccessDeniedException",
            );
        let (session, _) = fake.session().await;
        let found = session.list_data_endpoints().await.unwrap();
        assert_eq!(found.endpoints.len(), 1);
        assert_eq!(found.endpoints[0].label, "good");
        let details = found.warning.unwrap().details.unwrap();
        assert!(details.contains("DescribeEndpoint failed"), "{details}");
        assert!(details.contains("bad: "), "{details}");
    }

    #[tokio::test]
    async fn no_endpoints_at_all_is_an_error() {
        let fake = FakeAws::new()
            .error("GET", "/endpoint", 401, "UnauthorizedException")
            .error("GET", "/domainConfigurations", 401, "UnauthorizedException");
        let (session, _) = fake.session().await;
        let err = session.list_data_endpoints().await.unwrap_err();
        assert_eq!(err.category, ErrorCategory::Credential);
        assert!(err.message.contains("DescribeEndpoint"), "{}", err.message);
        assert!(err.action.unwrap().contains("iot:DescribeEndpoint"));
    }

    #[tokio::test]
    async fn empty_default_address_with_no_domains_is_an_error() {
        let fake = FakeAws::new()
            .json("GET", "/endpoint", json!({ "endpointAddress": "" }))
            .json(
                "GET",
                "/domainConfigurations",
                json!({ "domainConfigurations": [] }),
            );
        let (session, _) = fake.session().await;
        let err = session.list_data_endpoints().await.unwrap_err();
        assert_eq!(err.category, ErrorCategory::Network);
        assert!(
            err.message.contains("No endpoint address"),
            "{}",
            err.message
        );
    }

    #[tokio::test]
    async fn lists_the_first_page_of_things_sorted() {
        let fake = FakeAws::new().json(
            "GET",
            "/things",
            json!({
                "things": [
                    { "thingName": "sensor-b", "thingTypeName": "Sensor", "thingArn": "arn:b" },
                    { "thingName": "Sensor-a" },
                    { "thingTypeName": "nameless" },
                ],
                "nextToken": "more",
            }),
        );
        let (session, log) = fake.session().await;
        let page = session.list_things_first_page().await.unwrap();
        assert!(page.has_more);
        let names: Vec<_> = page.things.iter().map(|t| t.name.as_str()).collect();
        assert_eq!(names, ["Sensor-a", "sensor-b"]);
        assert_eq!(page.things[1].thing_type.as_deref(), Some("Sensor"));
        assert!(fake.requests_to("GET", "/things")[0]
            .query
            .contains("maxResults=100"));
        assert!(
            log.text().contains("3 things (more available)"),
            "{}",
            log.text()
        );
    }

    #[tokio::test]
    async fn a_single_page_of_things_has_no_more() {
        let fake = FakeAws::new().json("GET", "/things", json!({ "things": [] }));
        let (session, _) = fake.session().await;
        let page = session.list_things_first_page().await.unwrap();
        assert!(!page.has_more);
        assert!(page.things.is_empty());
    }

    #[tokio::test]
    async fn describes_a_typed_thing_name() {
        let fake = FakeAws::new().json(
            "GET",
            "/things/my%20thing",
            json!({ "thingName": "my thing", "thingArn": "arn:aws:iot:us-east-1:123456789012:thing/my thing" }),
        );
        let (session, _) = fake.session().await;
        let thing = session.describe_thing("my thing").await.unwrap();
        assert_eq!(thing.name, "my thing");
        assert_eq!(thing.thing_type, None);
        assert!(thing.arn.unwrap().ends_with("thing/my thing"));
    }

    #[tokio::test]
    async fn unknown_thing_gets_a_friendly_error() {
        let fake = FakeAws::new().error("GET", "/things/missing", 404, "ResourceNotFoundException");
        let (session, _) = fake.session().await;
        let err = session.describe_thing("missing").await.unwrap_err();
        assert_eq!(err.category, ErrorCategory::Configuration);
        assert!(
            err.message
                .contains("\"missing\" doesn't exist in us-east-1"),
            "{}",
            err.message
        );
        assert!(err.action.unwrap().contains("case-sensitive"));
    }

    #[tokio::test]
    async fn other_describe_thing_errors_pass_through() {
        let fake = FakeAws::new().error("GET", "/things/locked", 403, "AccessDeniedException");
        let (session, _) = fake.session().await;
        let err = session.describe_thing("locked").await.unwrap_err();
        assert_eq!(err.category, ErrorCategory::Credential);
        assert_eq!(err.message, "DescribeThing failed: AccessDeniedException");
    }

    #[tokio::test]
    async fn reads_the_broker_view_of_a_connection() {
        let fake = FakeAws::new().json(
            "GET",
            "/connections/device-1",
            json!({
                "clientId": "device-1",
                "thingName": "device-1",
                "connected": true,
                "cleanSession": false,
                "keepAliveDuration": 30,
                "sessionExpiry": 3600,
                "connectedSince": 1_700_000_000_000_i64,
                "sourceIp": "203.0.113.5",
                "sourcePort": 51234,
                "targetIp": "198.51.100.7",
                "vpcEndpointId": "vpce-1",
            }),
        );
        let (session, _) = fake.session().await;
        let info = session
            .get_connection(Some("abc-ats.iot.example"), "device-1", true)
            .await
            .unwrap();
        assert!(info.connected);
        assert!(!info.clean_session);
        assert_eq!(info.keep_alive_secs, 30);
        assert_eq!(info.session_expiry_secs, 3600);
        assert_eq!(
            info.connected_since.unwrap().timestamp().as_millisecond(),
            1_700_000_000_000
        );
        assert_eq!(info.disconnected_since, None, "0 means absent");
        assert_eq!(info.source.as_deref(), Some("203.0.113.5:51234"));
        assert_eq!(info.target.as_deref(), Some("198.51.100.7"));
        assert_eq!(info.vpc_endpoint_id.as_deref(), Some("vpce-1"));
        let req = &fake.requests_to("GET", "/connections/device-1")[0];
        assert!(
            req.query.contains("includeSocketInformation=true"),
            "{}",
            req.query
        );
    }

    #[tokio::test]
    async fn lists_subscriptions_across_pages_sorted() {
        let fake = FakeAws::new()
            .json_once(
                "GET",
                "/connections/dev/subscriptions",
                json!({
                    "subscriptions": [{ "topicFilter": "z/#", "qos": 1 }],
                    "nextToken": "t2",
                }),
            )
            .json(
                "GET",
                "/connections/dev/subscriptions",
                json!({ "subscriptions": [{ "topicFilter": "a/+", "qos": 0 }] }),
            );
        let (session, _) = fake.session().await;
        let subs = session.list_subscriptions(None, "dev").await.unwrap();
        assert_eq!(
            subs,
            [
                SubscriptionInfo {
                    topic_filter: "a/+".into(),
                    qos: 0
                },
                SubscriptionInfo {
                    topic_filter: "z/#".into(),
                    qos: 1
                },
            ]
        );
        let pages = fake.requests_to("GET", "/connections/dev/subscriptions");
        assert_eq!(pages.len(), 2);
        assert!(
            pages[1].query.contains("nextToken=t2"),
            "{}",
            pages[1].query
        );
    }

    #[tokio::test]
    async fn data_plane_errors_are_categorized() {
        let fake = FakeAws::new().error(
            "GET",
            "/connections/dev/subscriptions",
            403,
            "ForbiddenException",
        );
        let (session, _) = fake.session().await;
        let err = session.list_subscriptions(None, "dev").await.unwrap_err();
        assert_eq!(err.category, ErrorCategory::Configuration);
        assert_eq!(err.message, "ListSubscriptions failed: ForbiddenException");
    }

    #[tokio::test(start_paused = true)]
    async fn calls_time_out() {
        let fake = FakeAws::new();
        let (session, log) = fake.session().await;
        let never = std::future::pending::<
            Result<
                aws_sdk_iot::operation::list_things::ListThingsOutput,
                SdkError<aws_sdk_iot::operation::list_things::ListThingsError>,
            >,
        >();
        let err = session
            .call("ListThings", "", never, |_| String::new())
            .await
            .unwrap_err();
        assert_eq!(err.category, ErrorCategory::Network);
        assert!(
            err.message.contains("timed out after 10 seconds"),
            "{}",
            err.message
        );
        assert!(log.text().contains("ListThings TIMEOUT"), "{}", log.text());
    }

    #[test]
    fn sdk_errors_without_a_code_are_categorized_by_message() {
        let err: SdkError<aws_sdk_iot::operation::list_things::ListThingsError> =
            SdkError::timeout_error("connection timed out");
        let mapped = map_sdk_error("ListThings", err);
        assert_eq!(mapped.message, "ListThings failed");
        assert_ne!(mapped.category, ErrorCategory::Configuration);
    }
}
