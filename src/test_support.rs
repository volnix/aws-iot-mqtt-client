// Copyright 2026 Nick Volgas
// SPDX-License-Identifier: Apache-2.0

//! Test helpers: an in-process fake of the AWS HTTP endpoints, so `Session` calls run offline
//! against canned responses. Nothing here makes a network call or needs credentials.

use std::sync::{Arc, Mutex};

use aws_credential_types::provider::SharedCredentialsProvider;
use aws_credential_types::Credentials;
use aws_smithy_http_client::test_util::infallible_client_fn;
use aws_smithy_types::body::SdkBody;
use aws_smithy_types::retry::RetryConfig;

use crate::aws::{session_from_config, ApiLogSink, Session};
use crate::packet_log::PacketLogEntry;

/// A request the fake received.
#[derive(Debug, Clone)]
pub struct Recorded {
    pub method: String,
    /// Path without the query string, still percent-encoded.
    pub path: String,
    pub query: String,
    pub body: String,
}

#[derive(Clone)]
struct Route {
    method: &'static str,
    path: String,
    status: u16,
    headers: Vec<(&'static str, String)>,
    body: String,
    /// Answers only one request; used for successive pages of a paginated call.
    once: bool,
}

/// Routes requests by method and exact path to canned responses. Unrouted requests get a 400
/// `UnroutedTestRequest` error, so a missing route shows up as a failed call.
#[derive(Clone, Default)]
pub struct FakeAws {
    routes: Arc<Mutex<Vec<Route>>>,
    requests: Arc<Mutex<Vec<Recorded>>>,
}

impl FakeAws {
    pub fn new() -> Self {
        Self::default()
    }

    fn push(self, route: Route) -> Self {
        self.routes.lock().unwrap().push(route);
        self
    }

    /// A JSON (`application/json`) response.
    pub fn json(self, method: &'static str, path: &str, body: serde_json::Value) -> Self {
        self.push(Route {
            method,
            path: path.to_owned(),
            status: 200,
            headers: vec![
                ("content-type", "application/json".into()),
                ("x-amzn-requestid", "req-1234".into()),
            ],
            body: body.to_string(),
            once: false,
        })
    }

    /// Like [`FakeAws::json`], but answers a single request; later ones fall through to the
    /// next matching route. Register pages in order.
    pub fn json_once(self, method: &'static str, path: &str, body: serde_json::Value) -> Self {
        let fake = self.json(method, path, body);
        fake.routes.lock().unwrap().last_mut().unwrap().once = true;
        fake
    }

    /// A REST-JSON service error, such as `UnauthorizedException` with status 401.
    pub fn error(self, method: &'static str, path: &str, status: u16, code: &str) -> Self {
        self.push(Route {
            method,
            path: path.to_owned(),
            status,
            headers: vec![
                ("content-type", "application/json".into()),
                ("x-amzn-errortype", code.to_owned()),
                ("x-amzn-requestid", "req-err".into()),
            ],
            body: serde_json::json!({ "message": format!("{code} from the fake") }).to_string(),
            once: false,
        })
    }

    /// A successful `sts:GetCallerIdentity` (query protocol, XML).
    pub fn caller_identity(self, account: &str, arn: &str) -> Self {
        self.push(Route {
            method: "POST",
            path: "/".into(),
            status: 200,
            headers: vec![("content-type", "text/xml".into())],
            body: format!(
                "<GetCallerIdentityResponse xmlns=\"https://sts.amazonaws.com/doc/2011-06-15/\">\
                 <GetCallerIdentityResult><Arn>{arn}</Arn><UserId>AIDATEST</UserId>\
                 <Account>{account}</Account></GetCallerIdentityResult>\
                 <ResponseMetadata><RequestId>sts-req</RequestId></ResponseMetadata>\
                 </GetCallerIdentityResponse>"
            ),
            once: false,
        })
    }

    /// Every request received so far, in order.
    pub fn requests(&self) -> Vec<Recorded> {
        self.requests.lock().unwrap().clone()
    }

    /// Requests to one method and path.
    pub fn requests_to(&self, method: &str, path: &str) -> Vec<Recorded> {
        self.requests()
            .into_iter()
            .filter(|r| r.method == method && r.path == path)
            .collect()
    }

    fn respond(&self, req: http::Request<SdkBody>) -> http::Response<SdkBody> {
        let recorded = Recorded {
            method: req.method().as_str().to_owned(),
            path: req.uri().path().to_owned(),
            query: req.uri().query().unwrap_or("").to_owned(),
            body: req
                .body()
                .bytes()
                .map(|b| String::from_utf8_lossy(b).into_owned())
                .unwrap_or_default(),
        };
        self.requests.lock().unwrap().push(recorded.clone());

        let mut routes = self.routes.lock().unwrap();
        let found = routes
            .iter()
            .position(|r| r.method == recorded.method && r.path == recorded.path);
        let route = match found {
            Some(i) if routes[i].once => routes.remove(i),
            Some(i) => routes[i].clone(),
            None => Route {
                method: "",
                path: String::new(),
                status: 400,
                headers: vec![("x-amzn-errortype", "UnroutedTestRequest".into())],
                body: format!(
                    "{{\"message\":\"no fake route for {} {}\"}}",
                    recorded.method, recorded.path
                ),
                once: false,
            },
        };
        let mut builder = http::Response::builder().status(route.status);
        for (k, v) in &route.headers {
            builder = builder.header(*k, v);
        }
        builder.body(SdkBody::from(route.body)).unwrap()
    }

    /// An SDK config that sends every request to this fake, with static test credentials.
    pub fn sdk_config(&self) -> aws_config::SdkConfig {
        self.sdk_config_with(SharedCredentialsProvider::new(Credentials::for_tests()))
    }

    pub fn sdk_config_with(&self, creds: SharedCredentialsProvider) -> aws_config::SdkConfig {
        let fake = self.clone();
        aws_config::SdkConfig::builder()
            .behavior_version(aws_config::BehaviorVersion::latest())
            .region(aws_config::Region::new("us-east-1"))
            .credentials_provider(creds)
            .retry_config(RetryConfig::disabled())
            .http_client(infallible_client_fn(move |req| fake.respond(req)))
            .build()
    }

    /// Opens a [`Session`] against this fake. Register `caller_identity` first if the test
    /// needs an account; without it the identity lookup fails, which is non-fatal.
    pub async fn session(&self) -> (Session, LogCapture) {
        let log = LogCapture::default();
        let session = session_from_config(self.sdk_config(), Some("test"), log.sink())
            .await
            .expect("session against the fake");
        (session, log)
    }
}

/// Collects API log entries for assertions.
#[derive(Clone, Default)]
pub struct LogCapture(Arc<Mutex<Vec<PacketLogEntry>>>);

impl LogCapture {
    pub fn sink(&self) -> ApiLogSink {
        let entries = self.0.clone();
        Arc::new(move |e| entries.lock().unwrap().push(e))
    }

    pub fn entries(&self) -> Vec<PacketLogEntry> {
        self.0.lock().unwrap().clone()
    }

    /// Every log line, joined, for `contains` checks.
    pub fn text(&self) -> String {
        self.entries()
            .iter()
            .map(|e| format!("{} {} {}", e.direction.arrow(), e.kind, e.details))
            .collect::<Vec<_>>()
            .join("\n")
    }
}

/// A minimal in-process MQTT broker over TLS on localhost. It speaks just enough MQTT 3.1.1
/// and 5 to drive both client event loops: CONNECT, PUBLISH (echoed back at QoS 1), the QoS 1
/// and 2 handshakes, SUBSCRIBE, UNSUBSCRIBE, PINGREQ, and DISCONNECT.
pub mod broker {
    use std::sync::Arc;

    use rustls::pki_types::pem::PemObject;
    use rustls::pki_types::PrivateKeyDer;
    use rustls::RootCertStore;
    use tokio::io::{AsyncReadExt, AsyncWriteExt};
    use tokio::net::TcpListener;

    /// Starts a broker that serves one client, and returns its port and the roots that
    /// trust its certificate (for `localhost`).
    pub fn start(rt: &tokio::runtime::Runtime, behavior: Behavior) -> (u16, RootCertStore) {
        let ca = test_ca();
        let listener = rt.block_on(TcpListener::bind("127.0.0.1:0")).unwrap();
        let port = listener.local_addr().unwrap().port();
        rt.spawn(serve(listener, ca.server, behavior));
        (port, ca.roots)
    }

    /// How the broker answers a CONNECT.
    #[derive(Clone, Copy, PartialEq, Eq, Debug)]
    pub enum Behavior {
        Accept,
        /// CONNACK with "not authorized".
        Refuse,
        /// CONNACK, then the broker ends the session (DISCONNECT on MQTT 5, socket close on 3.1.1).
        KickAfterConnect,
    }

    struct TestCa {
        roots: RootCertStore,
        server: tokio_rustls::TlsAcceptor,
    }

    /// A CA, a `localhost` server certificate it signed, and a TLS acceptor using them.
    fn test_ca() -> TestCa {
        use rcgen::{BasicConstraints, CertificateParams, IsCa, Issuer, KeyPair};
        let ca_key = KeyPair::generate().unwrap();
        let mut ca_params = CertificateParams::new(Vec::<String>::new()).unwrap();
        ca_params.is_ca = IsCa::Ca(BasicConstraints::Unconstrained);
        let ca_cert = ca_params.self_signed(&ca_key).unwrap();
        let issuer = Issuer::new(ca_params, ca_key);

        let leaf_key = KeyPair::generate().unwrap();
        let leaf = CertificateParams::new(vec!["localhost".into()])
            .unwrap()
            .signed_by(&leaf_key, &issuer)
            .unwrap();

        let mut roots = RootCertStore::empty();
        roots.add(ca_cert.der().clone()).unwrap();
        let key = PrivateKeyDer::from_pem_slice(leaf_key.serialize_pem().as_bytes()).unwrap();
        let server = rustls::ServerConfig::builder()
            .with_no_client_auth()
            .with_single_cert(vec![leaf.der().clone()], key)
            .unwrap();
        TestCa {
            roots,
            server: tokio_rustls::TlsAcceptor::from(Arc::new(server)),
        }
    }

    async fn read_packet<S: AsyncReadExt + Unpin>(s: &mut S) -> Option<(u8, Vec<u8>)> {
        let header = s.read_u8().await.ok()?;
        let (mut len, mut shift) = (0usize, 0);
        loop {
            let b = s.read_u8().await.ok()?;
            len |= ((b & 0x7f) as usize) << shift;
            if b & 0x80 == 0 {
                break;
            }
            shift += 7;
        }
        let mut body = vec![0; len];
        s.read_exact(&mut body).await.ok()?;
        Some((header, body))
    }

    fn packet(header: u8, body: &[u8]) -> Vec<u8> {
        let mut out = vec![header];
        let mut len = body.len();
        loop {
            let mut b = (len % 128) as u8;
            len /= 128;
            if len > 0 {
                b |= 0x80;
            }
            out.push(b);
            if len == 0 {
                break;
            }
        }
        out.extend_from_slice(body);
        out
    }

    /// Skips an MQTT 5 properties block (variable-length integer length, then the properties).
    fn skip_properties(body: &[u8], mut at: usize) -> usize {
        let (mut len, mut shift) = (0usize, 0);
        loop {
            let b = body[at];
            at += 1;
            len |= ((b & 0x7f) as usize) << shift;
            if b & 0x80 == 0 {
                break;
            }
            shift += 7;
        }
        at + len
    }

    /// Accepts one client and serves it until it disconnects.
    async fn serve(listener: TcpListener, acceptor: tokio_rustls::TlsAcceptor, behavior: Behavior) {
        let (tcp, _) = listener.accept().await.unwrap();
        let mut s = acceptor.accept(tcp).await.unwrap();
        let mut v5 = false;
        while let Some((header, body)) = read_packet(&mut s).await {
            let pkid = |at: usize| [body[at], body[at + 1]];
            let reply = match header >> 4 {
                1 => {
                    // CONNECT: protocol name "MQTT" (6 bytes), then the protocol level.
                    v5 = body[6] == 5;
                    let code = if behavior == Behavior::Refuse {
                        if v5 {
                            0x87
                        } else {
                            0x05
                        }
                    } else {
                        0x00
                    };
                    let mut out = if v5 {
                        packet(0x20, &[0x00, code, 0x00])
                    } else {
                        packet(0x20, &[0x00, code])
                    };
                    if behavior == Behavior::KickAfterConnect {
                        if v5 {
                            out.extend(packet(0xE0, &[0x87, 0x00]));
                        }
                        s.write_all(&out).await.unwrap();
                        s.flush().await.unwrap();
                        let _ = s.shutdown().await;
                        return;
                    }
                    out
                }
                3 => {
                    // PUBLISH from the client: ack it, then echo it back at QoS 1.
                    let qos = (header >> 1) & 0x03;
                    let topic_len = u16::from_be_bytes([body[0], body[1]]) as usize;
                    let topic = &body[2..2 + topic_len];
                    let mut at = 2 + topic_len;
                    let id = (qos > 0).then(|| pkid(at));
                    if qos > 0 {
                        at += 2;
                    }
                    if v5 {
                        at = skip_properties(&body, at);
                    }
                    let payload = &body[at..];
                    let mut out = match (qos, id) {
                        (1, Some(id)) => packet(0x40, &id),
                        (2, Some(id)) => packet(0x50, &id),
                        _ => Vec::new(),
                    };
                    let mut echo = Vec::new();
                    echo.extend_from_slice(&(topic_len as u16).to_be_bytes());
                    echo.extend_from_slice(topic);
                    echo.extend_from_slice(&[0x00, 0x01]);
                    if v5 {
                        echo.push(0x00);
                    }
                    echo.extend_from_slice(payload);
                    out.extend(packet(0x32, &echo));
                    out
                }
                6 => packet(0x70, &pkid(0)), // PUBREL → PUBCOMP
                8 => {
                    // SUBSCRIBE → SUBACK granting QoS 1.
                    let mut b = pkid(0).to_vec();
                    if v5 {
                        b.push(0x00);
                    }
                    b.push(0x01);
                    packet(0x90, &b)
                }
                10 => {
                    // UNSUBSCRIBE → UNSUBACK.
                    let mut b = pkid(0).to_vec();
                    if v5 {
                        b.extend_from_slice(&[0x00, 0x00]);
                    }
                    packet(0xB0, &b)
                }
                12 => packet(0xD0, &[]), // PINGREQ → PINGRESP
                14 => return,            // DISCONNECT
                _ => Vec::new(),         // PUBACK etc. from the client
            };
            if !reply.is_empty() {
                s.write_all(&reply).await.unwrap();
                s.flush().await.unwrap();
            }
        }
    }
}
