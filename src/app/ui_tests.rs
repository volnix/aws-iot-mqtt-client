// Copyright 2026 Nick Volgas
// SPDX-License-Identifier: Apache-2.0

//! Drives the whole window with `egui_kittest`: widgets are found by their accessible labels
//! and clicked, against the fake AWS endpoints and the local test broker. No network, no
//! credentials, and no AWS account are used.

use std::time::Instant;

use egui::accesskit::Role;
use egui_kittest::kittest::Queryable;
use egui_kittest::Harness;
use serde_json::json;

use super::*;
use crate::test_support::{broker, FakeAws};

const ACCOUNT: &str = "123456789012";

fn harness(app: MqttApp) -> Harness<'static, MqttApp> {
    Harness::builder()
        .with_size(egui::vec2(1400.0, 1000.0))
        .with_context_setup(|ctx| {
            fonts::install(ctx);
            theme::install(ctx);
        })
        // Several text fields are identified by placeholder text only; tracked separately.
        .with_accessibility_check(false)
        .build_ui_state(|ui, app: &mut MqttApp| app.frame(ui), app)
}

/// Steps the UI until `done` holds, letting background tasks deliver their results.
fn wait_until(h: &mut Harness<'_, MqttApp>, what: &str, done: impl Fn(&MqttApp) -> bool) {
    let deadline = Instant::now() + Duration::from_secs(20);
    loop {
        h.step();
        if done(h.state()) {
            h.step();
            return;
        }
        assert!(Instant::now() < deadline, "timed out waiting for {what}");
        std::thread::sleep(Duration::from_millis(10));
    }
}

fn click(h: &mut Harness<'_, MqttApp>, label: &str) {
    h.get_by_label(label).click();
    h.run_steps(2);
}

fn click_button(h: &mut Harness<'_, MqttApp>, label: &str) {
    h.get_by_role_and_label(Role::Button, label).click();
    h.run_steps(2);
}

fn click_containing(h: &mut Harness<'_, MqttApp>, label: &str) {
    h.get_by_label_contains(label).click();
    h.run_steps(2);
}

/// An app that talks only to `fake` and trusts `roots` (the test broker's CA).
fn test_app(rt: Arc<Runtime>, fake: &FakeAws, roots: rustls::RootCertStore, port: u16) -> MqttApp {
    let ctx = egui::Context::default();
    let settings = Settings {
        profile: "test".into(),
        region: "us-east-1".into(),
        endpoint: "localhost".into(),
        endpoint_custom: true,
        port,
        ..Settings::default()
    };
    let mut app = MqttApp::with_settings(&ctx, rt, settings);
    app.started = true; // Skip the automatic load, so it only happens when clicked.
    app.test_sdk_config = Some(fake.sdk_config());
    app.test_tls_roots = Some(roots);
    app.theme_watcher = None;
    app
}

/// A real certificate and key, so the created certificate can be loaded and used to connect.
fn created_cert() -> serde_json::Value {
    let ck = rcgen::generate_simple_self_signed(vec!["sensor-1".into()]).unwrap();
    json!({
        "certificateArn": format!("arn:aws:iot:us-east-1:{ACCOUNT}:cert/f00dcafe1234567890"),
        "certificateId": "f00dcafe1234567890",
        "certificatePem": ck.cert.pem(),
        "keyPair": { "PublicKey": "PUBLIC", "PrivateKey": ck.signing_key.serialize_pem() },
    })
}

fn account_fake() -> FakeAws {
    FakeAws::new()
        .caller_identity(ACCOUNT, &format!("arn:aws:iam::{ACCOUNT}:role/Dev"))
        .json(
            "GET",
            "/endpoint",
            json!({ "endpointAddress": "abc-ats.iot.us-east-1.amazonaws.com" }),
        )
        .json(
            "GET",
            "/domainConfigurations",
            json!({ "domainConfigurations": [{ "domainConfigurationName": "custom" }] }),
        )
        .json(
            "GET",
            "/domainConfigurations/custom",
            json!({
                "domainConfigurationName": "custom",
                "domainName": "iot.example.com",
                "domainConfigurationStatus": "ENABLED",
                "domainType": "CUSTOMER_MANAGED",
            }),
        )
        .json(
            "GET",
            "/things",
            json!({
                "things": [
                    { "thingName": "sensor-1", "thingTypeName": "Sensor",
                      "thingArn": format!("arn:aws:iot:us-east-1:{ACCOUNT}:thing/sensor-1") },
                    { "thingName": "sensor-2" },
                ],
                "nextToken": "more",
            }),
        )
        .json(
            "GET",
            "/things/far-away",
            json!({ "thingName": "far-away", "thingArn": "arn:aws:iot:us-east-1:123456789012:thing/far-away" }),
        )
        .error("GET", "/things/missing", 404, "ResourceNotFoundException")
        .json(
            "GET",
            "/policies",
            json!({ "policies": [{ "policyName": "sensor-1-policy" }, { "policyName": "shared" }] }),
        )
        .json(
            "GET",
            "/policies/shared",
            json!({
                "policyName": "shared",
                "defaultVersionId": "1",
                "policyDocument": json!({
                    "Version": "2012-10-17",
                    "Statement": [{ "Effect": "Allow", "Action": "iot:*", "Resource": "*" }],
                }).to_string(),
            }),
        )
}

/// Load the account, pick and look up Things, create a certificate, connect to the test broker,
/// subscribe, publish, read the broker's view of the connection, use the logs, and disconnect.
#[test]
fn full_client_workflow() {
    let rt = Arc::new(Runtime::new().unwrap());
    let (port, roots) = broker::start(&rt, broker::Behavior::Accept);
    let fake = account_fake()
        .json(
            "POST",
            "/policies/sensor-1-policy",
            json!({ "policyArn": "arn:p" }),
        )
        .json("POST", "/keys-and-certificate", created_cert())
        .json("PUT", "/target-policies/sensor-1-policy", json!({}))
        .json("PUT", "/things/sensor-1/principals", json!({}))
        .json(
            "GET",
            "/connections/sensor-1",
            json!({
                "clientId": "sensor-1", "thingName": "sensor-1", "connected": true,
                "cleanSession": true, "keepAliveDuration": 30,
                "connectedSince": 1_700_000_000_000_i64,
                "sourceIp": "203.0.113.5", "sourcePort": 50000, "targetIp": "198.51.100.7",
            }),
        )
        .json(
            "GET",
            "/connections/sensor-1/subscriptions",
            json!({ "subscriptions": [{ "topicFilter": "sensors/#", "qos": 1 }] }),
        );
    let storage = tempfile::tempdir().unwrap();
    let mut h = harness(test_app(rt.clone(), &fake, roots, port));
    h.run_steps(2);

    // Load the account.
    click_containing(&mut h, "Load from AWS");
    wait_until(&mut h, "the account to load", |a| {
        a.aws.session.is_some() && !a.aws.busy()
    });
    assert_eq!(h.state().aws.things.len(), 2);
    assert!(h.state().aws.things_has_more);
    assert_eq!(h.state().aws.endpoints.len(), 2);
    assert_eq!(
        h.state().settings.endpoint,
        "localhost",
        "a typed endpoint is kept"
    );
    h.get_by_label(&format!("Account {ACCOUNT}"));

    // Pick a Thing from the list, then look one up by exact name.
    h.get_by_role_and_label(Role::ListBoxOption, "sensor-2")
        .click();
    h.run_steps(2);
    assert_eq!(h.state().settings.thing.as_deref(), Some("sensor-2"));
    h.state_mut().aws.filter = "far-away".into();
    h.run_steps(2);
    click(&mut h, "Use \"far-away\"");
    wait_until(&mut h, "the Thing lookup", |a| {
        a.settings.thing.as_deref() == Some("far-away")
    });
    assert_eq!(h.state().aws.things.len(), 3);
    h.state_mut().aws.filter = "missing".into();
    h.run_steps(2);
    click(&mut h, "Use \"missing\"");
    wait_until(&mut h, "the failed lookup", |a| {
        a.aws.lookup_error.is_some()
    });
    h.state_mut().aws.filter.clear();
    h.run_steps(2);
    h.get_by_role_and_label(Role::ListBoxOption, "sensor-1")
        .click();
    h.run_steps(2);
    assert_eq!(h.state().settings.thing.as_deref(), Some("sensor-1"));

    // Expand the account pickers and collapse them again.
    click_button(&mut h, &format!("Account {ACCOUNT}"));
    assert!(h.state().account_open);
    click_button(&mut h, &format!("Account {ACCOUNT}"));

    // Create a certificate with a new policy.
    click_containing(&mut h, "Create certificate");
    wait_until(&mut h, "the policy list", |a| {
        a.provision.as_ref().is_some_and(|d| d.policies.is_some())
    });
    h.state_mut().provision.as_mut().unwrap().storage_root = storage.path().to_owned();
    click_button(&mut h, "Policy document");
    h.state_mut().provision.as_mut().unwrap().document.push(' ');
    h.run_steps(2);
    click(&mut h, "Reset to default");
    {
        let d = h.state().provision.as_ref().unwrap();
        assert_eq!(d.document, d.default_document);
        assert_eq!(
            d.existing, "sensor-1-policy",
            "an existing default-named policy is preselected"
        );
    }
    click(&mut h, "Create certificate");
    wait_until(&mut h, "the certificate to be created", |a| {
        a.provision.is_none()
    });
    let notice = h.state().notice.clone().expect("notice");
    assert!(
        notice.contains("Created certificate f00dcafe1234"),
        "{notice}"
    );
    assert!(matches!(h.state().cert, Some(Ok(_))));
    assert!(matches!(h.state().key, Some(Ok(_))));
    assert!(h
        .state()
        .settings
        .key_path
        .as_ref()
        .unwrap()
        .starts_with(storage.path()));

    // Advanced options and the last will.
    click_button(&mut h, "Advanced");
    assert!(h.state().show_advanced);
    h.state_mut().settings.will_enabled = true;
    h.state_mut().settings.will_topic = "status/sensor-1".into();
    h.state_mut().settings.will_payload = "offline".into();
    h.state_mut().settings.session_expiry_secs = 120;
    h.run_steps(2);
    click_button(&mut h, "Details");
    assert!(h.state().show_cert_details);

    // Connect.
    click_button(&mut h, &format!("{} Connect", icons::PLUG));
    wait_until(&mut h, "the connection", |a| {
        a.state == ConnectionState::Connected
    });
    wait_until(&mut h, "the broker's view of the connection", |a| {
        matches!(a.live.info, Some(Ok(_))) && matches!(a.live.subscriptions, Some(Ok(_)))
    });
    click_button(&mut h, "More");
    assert!(h.state().show_more_info);
    click_button(&mut h, "Refresh (GetConnection)");
    click_button(&mut h, "Refresh (ListSubscriptions)");
    click_button(&mut h, "Connection details");
    assert!(h.state().show_conn_details);
    h.run_steps(2);
    // Close it again so the composer stays inside the test window.
    click_button(&mut h, "Connection details");
    assert!(!h.state().show_conn_details);

    // Subscribe and publish; the broker echoes the message back.
    h.state_mut().settings.subscribe_filter = "sensors/#".into();
    h.run_steps(2);
    click_button(&mut h, "Subscribe");
    wait_until(&mut h, "the subscription", |a| {
        a.subscriptions.iter().any(|s| s == "sensors/#")
    });
    h.state_mut().settings.publish_topic = "sensors/1".into();
    h.state_mut().publish_payload = "{\"t\":21}".into();
    h.run_steps(2);
    click_containing(&mut h, "Send");
    wait_until(&mut h, "the echoed message", |a| !a.messages.is_empty());
    assert_eq!(h.state().messages[0].topic, "sensors/1");
    h.state_mut().settings.publish_topic = "bad/#".into();
    h.run_steps(2);
    click_containing(&mut h, "Send");
    assert!(
        h.state().action_error.is_some(),
        "wildcard topics can't be published"
    );

    // Unsubscribe from the broker's list.
    click_button(&mut h, "Unsubscribe");
    wait_until(&mut h, "the unsubscribe", |a| a.subscriptions.is_empty());

    // Logs drawer: both tabs, ping filter, clear, close.
    click_containing(&mut h, "Logs ");
    assert!(h.state().settings.logs_open);
    click_containing(&mut h, "AWS API calls");
    assert_eq!(h.state().log_tab, LogTab::Api);
    click_containing(&mut h, "MQTT packets");
    click(&mut h, "Hide pings");
    assert!(!h.state().hide_pings);
    for clear in h.get_all_by_label("Clear").collect::<Vec<_>>() {
        clear.click();
    }
    h.run_steps(2);
    assert!(h.state().log.is_empty());
    click_button(&mut h, "Hide logs");
    assert!(!h.state().settings.logs_open);

    // Disconnect.
    click_button(&mut h, "Disconnect");
    wait_until(&mut h, "the disconnect", |a| {
        a.state == ConnectionState::Disconnected && a.connection.is_none()
    });

    // Cycle the theme back to Auto.
    for _ in 0..3 {
        click_containing(&mut h, "Theme: ");
    }
    assert_eq!(h.state().settings.theme, ThemeChoice::System);

    // Every request the app made was one the fake expected.
    let unrouted: Vec<_> = fake
        .requests()
        .into_iter()
        .filter(|r| !r.path.is_empty() && r.path != "/" && r.method == "DELETE")
        .collect();
    assert!(unrouted.is_empty(), "the app never deletes: {unrouted:?}");
    assert!(
        fake.requests_to("POST", "/keys-and-certificate").len() == 1,
        "exactly one certificate"
    );
}

/// Client ID mode: validation, creating a certificate that attaches an existing policy, and a
/// connection the broker refuses.
#[test]
fn client_id_mode_and_refused_connection() {
    let rt = Arc::new(Runtime::new().unwrap());
    let (port, roots) = broker::start(&rt, broker::Behavior::Refuse);
    let fake = account_fake()
        .json("POST", "/keys-and-certificate", created_cert())
        .json("PUT", "/target-policies/shared", json!({}));
    let storage = tempfile::tempdir().unwrap();
    let mut h = harness(test_app(rt, &fake, roots, port));
    h.run_steps(2);
    click_containing(&mut h, "Load from AWS");
    wait_until(&mut h, "the account to load", |a| {
        a.aws.session.is_some() && !a.aws.busy()
    });

    click(&mut h, "Client ID");
    assert_eq!(h.state().settings.mode, IdentityMode::ClientId);
    h.state_mut().settings.custom_client_id = "bad id".into();
    h.run_steps(2);
    assert!(
        h.query_by_label_contains("Create certificate").is_none(),
        "invalid ID"
    );
    h.state_mut().settings.custom_client_id = "tester-1".into();
    h.run_steps(2);

    click_containing(&mut h, "Create certificate");
    wait_until(&mut h, "the policy list", |a| {
        a.provision.as_ref().is_some_and(|d| d.policies.is_some())
    });
    h.state_mut().provision.as_mut().unwrap().storage_root = storage.path().to_owned();
    click(&mut h, "Existing policy");
    h.state_mut().provision.as_mut().unwrap().policy_filter = "sha".into();
    h.run_steps(2);
    h.state_mut().provision.as_mut().unwrap().existing = "shared".into();
    h.run_steps(2);
    click(&mut h, "Create certificate");
    wait_until(&mut h, "the certificate", |a| a.provision.is_none());
    assert!(fake.requests_to("PUT", "/target-policies/shared").len() == 1);

    click_button(&mut h, &format!("{} Connect", icons::PLUG));
    wait_until(&mut h, "the refusal", |a| a.state == ConnectionState::Error);
    let err = h.state().connection_error.clone().expect("error");
    assert_eq!(err.category, crate::error::ErrorCategory::Authentication);
}

/// The dialog can be cancelled, and a failure partway through is shown in it.
#[test]
fn provisioning_failure_is_shown_and_dialog_cancels() {
    let rt = Arc::new(Runtime::new().unwrap());
    let fake = account_fake().error(
        "POST",
        "/policies/sensor-1-policy",
        409,
        "ResourceAlreadyExistsException",
    );
    let mut h = harness(test_app(rt, &fake, rustls::RootCertStore::empty(), 1));
    h.run_steps(2);
    click_containing(&mut h, "Load from AWS");
    wait_until(&mut h, "the account", |a| {
        a.aws.session.is_some() && !a.aws.busy()
    });
    h.get_by_role_and_label(Role::ListBoxOption, "sensor-1")
        .click();
    h.run_steps(2);

    click_containing(&mut h, "Create certificate");
    wait_until(&mut h, "the dialog", |a| a.provision.is_some());
    h.state_mut().provision.as_mut().unwrap().storage_root = std::env::temp_dir();
    click(&mut h, "Create certificate");
    wait_until(&mut h, "the failure", |a| {
        a.provision
            .as_ref()
            .is_some_and(|d| !d.running && d.error.is_some())
    });
    assert!(fake.requests_to("POST", "/keys-and-certificate").is_empty());
    click(&mut h, "Cancel");
    assert!(h.state().provision.is_none());
}

/// A failed account load shows the error, and connecting without a certificate explains why.
#[test]
fn account_errors_and_missing_requirements() {
    let rt = Arc::new(Runtime::new().unwrap());
    let fake = FakeAws::new()
        .error("GET", "/endpoint", 403, "AccessDeniedException")
        .error("GET", "/domainConfigurations", 403, "AccessDeniedException")
        .error("GET", "/things", 403, "AccessDeniedException");
    let mut h = harness(test_app(rt, &fake, rustls::RootCertStore::empty(), 1));
    h.run_steps(2);
    click_containing(&mut h, "Load from AWS");
    wait_until(&mut h, "the account", |a| {
        a.aws.session.is_some() && !a.aws.busy()
    });
    assert!(h.state().aws.endpoints_error.is_some());
    assert!(h.state().aws.things_error.is_some());
    h.get_by_label("AWS account");

    // Reload, superseding nothing; the second load also completes.
    click_button(&mut h, "Reload endpoints and Things");
    wait_until(&mut h, "the reload", |a| {
        a.aws.session.is_some() && !a.aws.busy()
    });
    assert_eq!(h.state().aws.generation, 2);

    // Connect is disabled until a Thing and certificate are chosen; calling it reports why.
    let ctx = h.ctx.clone();
    h.state_mut().connect(&ctx);
    assert!(h.state().connection_error.is_some());

    // A session that can't be opened is shown in the account card.
    let config = aws_config::SdkConfig::builder()
        .behavior_version(aws_config::BehaviorVersion::latest())
        .build();
    h.state_mut().test_sdk_config = Some(config);
    h.state_mut().aws.session = None;
    h.run_steps(2);
    click_containing(&mut h, "Load from AWS");
    wait_until(&mut h, "the load to fail", |a| {
        a.aws.error.is_some() && !a.aws.busy()
    });
}

/// The simulator tab: load a policy from the account, switch operations, use the connection
/// context, and expand the trace.
#[test]
fn simulator_tab_loads_policies_and_evaluates() {
    let rt = Arc::new(Runtime::new().unwrap());
    let fake = account_fake();
    let mut h = harness(test_app(rt, &fake, rustls::RootCertStore::empty(), 1));
    h.run_steps(2);

    click(&mut h, "Policy simulator");
    assert_eq!(h.state().settings.view, View::Simulator);
    // Without an account, the simulator explains how to load one.
    h.get_by_label_contains("load your AWS account");

    click(&mut h, "Client");
    click_containing(&mut h, "Load from AWS");
    wait_until(&mut h, "the account", |a| {
        a.aws.session.is_some() && !a.aws.busy()
    });
    h.get_by_role_and_label(Role::ListBoxOption, "sensor-1")
        .click();
    h.run_steps(2);
    click(&mut h, "Policy simulator");

    click_containing(&mut h, "Load from account");
    wait_until(&mut h, "the policy list", |a| {
        matches!(a.sim.policies, Some(Ok(_)))
    });
    let ctx = h.ctx.clone();
    h.state_mut().sim_load_document(&ctx, "shared".into());
    wait_until(&mut h, "the policy document", |a| {
        a.settings.sim.policy.contains("iot:*")
    });
    click(&mut h, "Format");
    click_button(&mut h, "Reload policy list");
    wait_until(&mut h, "the reloaded list", |a| {
        matches!(a.sim.policies, Some(Ok(_)))
    });

    click_button(&mut h, "Connection context");
    assert!(h.state().sim.show_context);
    click(&mut h, "Use the Client tab's identity, region, and account");
    assert_eq!(h.state().settings.sim.client_id, "sensor-1");
    click_button(&mut h, "How this was decided");
    for op in ["PUBLISH", "SUBSCRIBE", "Receive", "CONNECT"] {
        click(&mut h, op);
    }
    click(&mut h, "Reset to example");
    assert!(!h.state().settings.sim.policy.contains("iot:*"));
}
