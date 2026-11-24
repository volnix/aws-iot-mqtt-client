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
use crate::packet_log::Replay;
use crate::shadow;
use crate::shadow_fields::FieldValue;
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

/// Connecting as a Thing finds its classic and named shadows; the Shadows tab shows their state
/// and updates them over MQTT. Shadow traffic stays out of the Messages panel.
#[test]
fn shadows_tab_lists_shows_and_updates_shadows() {
    let rt = Arc::new(Runtime::new().unwrap());
    let (port, roots) = broker::start_with_shadows(
        &rt,
        broker::Behavior::Accept,
        vec![
            (
                "$aws/things/sensor-1/shadow".into(),
                json!({ "desired": { "led": "on" }, "reported": { "led": "off" } }),
            ),
            (
                "$aws/things/sensor-1/shadow/name/config".into(),
                json!({ "reported": { "fw": "1.0" } }),
            ),
        ],
    );
    let fake = account_fake().json(
        "GET",
        "/api/things/shadow/ListNamedShadowsForThing/sensor-1",
        json!({ "results": ["config"], "timestamp": 1 }),
    );
    let dir = tempfile::tempdir().unwrap();
    let ck = rcgen::generate_simple_self_signed(vec!["sensor-1".into()]).unwrap();
    let (cert, key) = (dir.path().join("cert.pem"), dir.path().join("key.pem"));
    std::fs::write(&cert, ck.cert.pem()).unwrap();
    std::fs::write(&key, ck.signing_key.serialize_pem()).unwrap();

    let mut app = test_app(rt, &fake, roots, port);
    app.load_certificate(&cert);
    app.load_private_key(&key);
    let mut h = harness(app);
    h.run_steps(2);

    // Connect from the Shadows tab's own short form: account, Thing, certificate, Connect.
    click(&mut h, "Shadows");
    h.get_by_label("Connect to see shadows");
    assert!(
        h.query_by_label("Protocol").is_none(),
        "advanced connection settings stay on the Client tab"
    );
    click_containing(&mut h, "Load from AWS");
    wait_until(&mut h, "the account", |a| {
        a.aws.session.is_some() && !a.aws.busy()
    });
    h.get_by_role_and_label(Role::ListBoxOption, "sensor-1")
        .click();
    h.run_steps(2);
    click_button(&mut h, &format!("{} Connect", icons::PLUG));
    wait_until(&mut h, "both shadows to load", |a| {
        a.shadow.shadows.as_ref().is_some_and(|s| {
            s.items.len() == 2
                && s.items
                    .values()
                    .all(|sh| sh.status == shadow::Status::Loaded)
        })
    });
    assert!(
        fake.requests_to(
            "GET",
            "/api/things/shadow/ListNamedShadowsForThing/sensor-1"
        )
        .len()
            == 1
    );

    assert_eq!(
        h.state().settings.view,
        View::Shadows,
        "still on the Shadows tab"
    );
    {
        let s = h.state().shadow.shadows.as_ref().unwrap();
        let classic = &s.items[&shadow::ShadowName::Classic];
        assert_eq!(classic.delta, Some(json!({ "led": "on" })));
        assert_eq!(classic.version, Some(1));
    }
    assert!(
        h.state().messages.is_empty(),
        "shadow traffic isn't shown as messages"
    );
    // Shown under both Desired and Delta.
    assert_eq!(h.get_all_by_label_contains("\"led\": \"on\"").count(), 2);

    // The form editor starts with the desired state's fields.
    let desired = |a: &MqttApp| {
        a.shadow.shadows.as_ref().unwrap().items[&shadow::ShadowName::Classic]
            .desired
            .clone()
    };
    assert_eq!(h.state().shadow.mode, shadows::EditMode::Fields);
    assert_eq!(h.state().shadow.fields.len(), 1);
    h.get_by_label("led");

    // Change a value and send it.
    h.state_mut().shadow.fields[0].value = FieldValue::Text("off".into());
    h.run_steps(2);
    click(&mut h, "Send update");
    wait_until(&mut h, "the update to be applied", |a| {
        let c = &a.shadow.shadows.as_ref().unwrap().items[&shadow::ShadowName::Classic];
        c.version == Some(2) && c.delta.is_none() && !c.updating
    });

    // Add a number field. A bad number is flagged and blocks sending.
    click_containing(&mut h, "Add field");
    {
        let added = h.state_mut().shadow.fields.last_mut().unwrap();
        assert!(!added.existing);
        added.key = "brightness".into();
        added.value = FieldValue::Number("8o".into());
    }
    h.run_steps(2);
    h.get_by_label_contains("isn't a number");
    h.state_mut().shadow.fields.last_mut().unwrap().value = FieldValue::Number("80".into());
    h.run_steps(2);
    click(&mut h, "Send update");
    wait_until(&mut h, "the new field", |a| {
        desired(a) == Some(json!({ "led": "off", "brightness": 80 }))
    });

    // Removing an existing field deletes it from the shadow (it's sent as null), with undo.
    // Remove buttons follow the field order, which depends on serde_json's features.
    let led_row = |a: &MqttApp| a.shadow.fields.iter().position(|f| f.key == "led").unwrap();
    let row = led_row(h.state());
    h.get_all_by_label("Remove field").nth(row).unwrap().click();
    h.run_steps(2);
    h.get_by_label("Removed when you send");
    click(&mut h, "Undo");
    assert!(h.query_by_label("Removed when you send").is_none());
    let row = led_row(h.state());
    h.get_all_by_label("Remove field").nth(row).unwrap().click();
    h.run_steps(2);
    click(&mut h, "Send update");
    wait_until(&mut h, "the field to be deleted", |a| {
        desired(a) == Some(json!({ "brightness": 80 }))
    });
    assert_eq!(
        h.state().shadow.fields.len(),
        1,
        "the form reloads from the shadow"
    );

    // Switch to JSON: the fields carry over. Invalid JSON can't switch back to fields.
    click(&mut h, "JSON");
    assert_eq!(h.state().shadow.mode, shadows::EditMode::Json);
    assert!(h.state().shadow.editor.contains("\"brightness\": 80"));
    h.state_mut().shadow.editor = "{ \"brightness\": ".into();
    h.run_steps(2);
    click(&mut h, "Fields");
    assert_eq!(h.state().shadow.mode, shadows::EditMode::Json);
    assert!(h
        .state()
        .shadow
        .edit_error
        .as_deref()
        .unwrap()
        .contains("valid object"));
    h.state_mut().shadow.editor = "{ \"brightness\": 50, \"fan\": 2 }".into();
    h.run_steps(2);
    click(&mut h, "Fields");
    assert_eq!(h.state().shadow.mode, shadows::EditMode::Fields);
    let keys: Vec<_> = h
        .state()
        .shadow
        .fields
        .iter()
        .map(|f| (f.key.clone(), f.existing))
        .collect();
    assert_eq!(
        keys,
        [("brightness".to_owned(), true), ("fan".to_owned(), false)],
        "only fields already in the shadow are marked existing"
    );
    click(&mut h, "Reset");

    // In JSON mode, the editor follows the selected section.
    click(&mut h, "JSON");
    click_button(&mut h, "Reported");
    assert!(h.state().shadow.editor.contains("\"led\": \"off\""));
    h.state_mut().shadow.editor = "[1, 2]".into();
    h.run_steps(2);
    click(&mut h, "Send update");
    assert!(h
        .state()
        .shadow
        .edit_error
        .as_deref()
        .unwrap()
        .contains("object"));
    click(&mut h, "Reset");
    assert!(h.state().shadow.edit_error.is_none());
    assert!(
        h.state().shadow.editor.contains("\"led\": \"off\""),
        "back to the reported state"
    );

    // Open a named shadow that doesn't exist yet, then create it with an update.
    h.state_mut().shadow.new_name = "settings".into();
    h.run_steps(2);
    click(&mut h, "Open");
    let settings = shadow::ShadowName::Named("settings".into());
    wait_until(&mut h, "the missing shadow", |a| {
        a.shadow.shadows.as_ref().unwrap().items[&settings].status == shadow::Status::Missing
    });
    h.state_mut().shadow.editor = "{ \"mode\": \"eco\" }".into();
    h.run_steps(2);
    click(&mut h, "Send update");
    wait_until(&mut h, "the new shadow", |a| {
        a.shadow.shadows.as_ref().unwrap().items[&settings].status == shadow::Status::Loaded
    });
    // The count sits under the list's title, not in the tab.
    h.get_by_label("3 active");

    // Pick the named shadow from the list, and refresh everything.
    h.get_by_role_and_label(Role::ListBoxOption, "config   v1")
        .click();
    h.run_steps(2);
    assert_eq!(
        h.state().shadow.selected,
        Some(shadow::ShadowName::Named("config".into()))
    );
    click_button(&mut h, "Refresh shadows");
    wait_until(&mut h, "the refresh", |a| {
        !a.shadow.listing
            && a.shadow
                .shadows
                .as_ref()
                .unwrap()
                .items
                .values()
                .all(|s| s.status == shadow::Status::Loaded)
    });

    // After disconnecting, the last known state stays visible, with a Connect button.
    click(&mut h, "Client");
    click_button(&mut h, "Disconnect");
    wait_until(&mut h, "the disconnect", |a| {
        a.state == ConnectionState::Disconnected
    });
    click_button(&mut h, "Shadows");
    h.get_by_label_contains("Disconnected. Showing the last known state.");
    click_button(&mut h, &format!("{} Connect", icons::PLUG));
    wait_until(&mut h, "the reconnect and reload", |a| {
        a.state == ConnectionState::Connected
            && a.shadow.shadows.as_ref().is_some_and(|s| {
                // Rebuilt from the account's list, which the fake doesn't update.
                s.items.len() == 2
                    && s.items
                        .values()
                        .all(|sh| sh.status == shadow::Status::Loaded)
            })
    });
}

/// Client ID connections have no Thing, so no shadow requests are made.
#[test]
fn client_id_connections_have_no_shadows() {
    let rt = Arc::new(Runtime::new().unwrap());
    let (port, roots) = broker::start(&rt, broker::Behavior::Accept);
    let fake = FakeAws::new();
    let dir = tempfile::tempdir().unwrap();
    let ck = rcgen::generate_simple_self_signed(vec!["c".into()]).unwrap();
    let (cert, key) = (dir.path().join("cert.pem"), dir.path().join("key.pem"));
    std::fs::write(&cert, ck.cert.pem()).unwrap();
    std::fs::write(&key, ck.signing_key.serialize_pem()).unwrap();
    let mut app = test_app(rt, &fake, roots, port);
    app.load_certificate(&cert);
    app.load_private_key(&key);
    app.settings.mode = IdentityMode::ClientId;
    app.settings.custom_client_id = "tester".into();
    let mut h = harness(app);
    h.run_steps(2);

    // Not connected yet: the Shadows tab offers to switch to a Thing.
    click(&mut h, "Shadows");
    h.get_by_label_contains("set to connect with a client ID");
    click(&mut h, "Use an IoT Thing");
    assert_eq!(h.state().settings.mode, IdentityMode::Thing);
    h.state_mut().settings.mode = IdentityMode::ClientId;
    click(&mut h, "Client");

    click_button(&mut h, &format!("{} Connect", icons::PLUG));
    wait_until(&mut h, "the connection", |a| {
        a.state == ConnectionState::Connected
    });
    assert!(h.state().shadow.shadows.is_none());
    click(&mut h, "Shadows");
    h.get_by_label_contains("connected with a client ID, which has no shadows");
    assert!(!h
        .state()
        .log
        .entries()
        .any(|e| e.details.contains("shadow")));
}

/// The form editor renders every field type, nested groups, and a removed field, in both themes.
#[test]
fn shadow_form_renders_every_field_type() {
    for theme in [egui::Theme::Dark, egui::Theme::Light] {
        let rt = Arc::new(Runtime::new().unwrap());
        let mut app = test_app(rt, &FakeAws::new(), rustls::RootCertStore::empty(), 1);
        app.settings.view = View::Shadows;
        let doc = json!({
            "name": "lamp", "level": 3.5, "on": true, "tags": ["a", 1], "gone": null,
            "color": { "r": 255, "mode": { "kind": "rgb" } },
        });
        let mut shadows = shadow::Shadows::new("t1");
        let s = shadows.get_or_insert(shadow::ShadowName::Classic);
        s.status = shadow::Status::Loaded;
        s.desired = Some(doc.clone());
        s.version = Some(4);
        s.timestamp = Some(1_700_000_000);
        app.shadow.shadows = Some(shadows);
        app.shadow.selected = Some(shadow::ShadowName::Classic);
        let mut h = Harness::builder()
            .with_size(egui::vec2(1400.0, 1000.0))
            .with_theme(theme)
            .with_context_setup(|ctx| {
                fonts::install(ctx);
                theme::install(ctx);
            })
            .with_accessibility_check(false)
            .build_ui_state(|ui, app: &mut MqttApp| app.frame(ui), app);
        h.run_steps(2);
        assert_eq!(h.state().shadow.fields.len(), 6);
        h.get_by_label("2 fields");
        // True/false is a pair of radio buttons.
        let on = |a: &MqttApp| {
            a.shadow
                .fields
                .iter()
                .find(|f| f.key == "on")
                .unwrap()
                .value
                .clone()
        };
        assert_eq!(on(h.state()), FieldValue::Bool(true));
        h.get_by_label("False").click();
        h.run_steps(2);
        assert_eq!(on(h.state()), FieldValue::Bool(false));
        h.get_by_label("True").click();
        h.run_steps(2);
        assert_eq!(on(h.state()), FieldValue::Bool(true));
        h.get_by_label("mode");
        h.get_all_by_label("Remove field").next().unwrap().click();
        h.run_steps(2);
        h.get_by_label("Removed when you send");
        // Each group has its own "Add field"; the top-level one comes last.
        let adds: Vec<_> = h.get_all_by_label_contains("Add field").collect();
        assert_eq!(adds.len(), 3, "top level, color, and color.mode");
        // Fonts differ by platform, so it may be below the visible area.
        adds.last().unwrap().scroll_to_me();
        h.run_steps(3);
        h.get_all_by_label_contains("Add field")
            .last()
            .unwrap()
            .click();
        h.run_steps(2);
        assert_eq!(h.state().shadow.fields.len(), 7);
        h.get_by_label_contains("A field needs a name");
    }
}

/// Sent PUBLISH and SUBSCRIBE rows in the MQTT log copy as `mosquitto_pub`/`mosquitto_sub`, in
/// either shell. Acks and received packets have no copy button.
#[test]
fn mosquitto_commands_can_be_copied() {
    let rt = Arc::new(Runtime::new().unwrap());
    let (port, roots) = broker::start(&rt, broker::Behavior::Accept);
    let dir = tempfile::tempdir().unwrap();
    let ck = rcgen::generate_simple_self_signed(vec!["c".into()]).unwrap();
    let (cert, key) = (dir.path().join("cert.pem"), dir.path().join("my key.pem"));
    std::fs::write(&cert, ck.cert.pem()).unwrap();
    std::fs::write(&key, ck.signing_key.serialize_pem()).unwrap();
    let mut app = test_app(rt, &FakeAws::new(), roots, port);
    app.load_certificate(&cert);
    app.load_private_key(&key);
    app.settings.mode = IdentityMode::ClientId;
    app.settings.custom_client_id = "tester".into();
    app.settings.mqtt_version = MqttVersion::V311;
    let mut h = harness(app);
    h.run_steps(2);
    click_button(&mut h, &format!("{} Connect", icons::PLUG));
    wait_until(&mut h, "the connection", |a| {
        a.state == ConnectionState::Connected
    });

    // Subscribe and publish (QoS 1 and retained), so the log has both kinds plus their acks.
    h.state_mut().settings.subscribe_filter = "sensors/#".into();
    h.run_steps(2);
    click_button(&mut h, "Subscribe");
    h.state_mut().settings.publish_topic = "sensors/1".into();
    h.state_mut().publish_retain = true;
    h.state_mut().publish_payload = "{\n  \"t\": 21\n}".into();
    h.run_steps(2);
    click_containing(&mut h, "Send");
    wait_until(&mut h, "the echoed message", |a| !a.messages.is_empty());
    assert!(
        h.query_by_label("Copy as mosquitto_sub").is_none(),
        "no copy buttons outside the log"
    );

    click_containing(&mut h, "Logs ");
    assert_eq!(h.get_all_by_label("Copy as mosquitto_sub").count(), 1);
    assert_eq!(
        h.get_all_by_label("Copy as mosquitto_pub").count(),
        1,
        "only the sent PUBLISH, not the echoed one or any ack"
    );
    let copy = h.get_by_label("Copy as mosquitto_sub");
    assert!(
        copy.rect().height() < 24.0,
        "fits in a log row: {:?}",
        copy.rect()
    );
    // The SUBSCRIBE row is above the log's visible area; scroll to it like a user would.
    h.get_by_label("Copy as mosquitto_sub").scroll_to_me();
    h.run_steps(3);
    click_button(&mut h, "Copy as mosquitto_sub");
    // A modal takes a few frames to size itself before it's shown.
    h.run_steps(3);
    h.get_by_label("mosquitto_sub command");
    let command = |a: &MqttApp| a.cli_command(a.cli.as_ref().unwrap());
    let sub = command(h.state());
    for part in [
        "mosquitto_sub",
        "-i tester",
        "-V mqttv311",
        &format!("-p {port}"),
        "-t 'sensors/#'",
        "--cafile AmazonRootCA1.pem",
        "my key.pem'",
    ] {
        assert!(sub.contains(part), "missing {part:?} in\n{sub}");
    }
    h.get_by_label_contains("one connection per client ID");
    click(&mut h, "PowerShell");
    assert!(command(h.state()).starts_with("Invoke-WebRequest"));
    click(&mut h, "macOS / Linux");
    h.get_by_role_and_label(Role::Button, &format!("{} Copy", icons::COPY))
        .click();
    h.step();
    let copied: Vec<_> = h
        .output()
        .platform_output
        .commands
        .iter()
        .filter_map(|c| match c {
            egui::OutputCommand::CopyText(text) => Some(text.clone()),
            _ => None,
        })
        .collect();
    assert_eq!(
        copied,
        [command(h.state())],
        "the command goes to the clipboard"
    );
    h.run_steps(1);
    assert!(h.state().cli.as_ref().unwrap().copied);
    h.run_steps(2); // a new area sizes itself before it shows
    h.get_by_label("Command copied to the clipboard");
    h.get_by_label(&format!("{} Copied", icons::CHECK));
    click(&mut h, "Close");
    assert!(h.state().cli.is_none());

    // The publish carries its full topic, QoS, retain flag, and payload.
    h.get_by_label("Copy as mosquitto_pub").scroll_to_me();
    h.run_steps(3);
    click_button(&mut h, "Copy as mosquitto_pub");
    h.run_steps(3);
    h.get_by_label("mosquitto_pub command");
    // The dialog starts in PowerShell on Windows; these checks use the POSIX form.
    click(&mut h, "macOS / Linux");
    let pubc = command(h.state());
    assert!(pubc.contains("-t sensors/1"), "{pubc}");
    assert!(pubc.contains("-q 1"), "{pubc}");
    assert!(pubc.contains("  -r \\"), "{pubc}");
    assert!(pubc.ends_with("-m '{\"t\":21}'"), "{pubc}");
    click(&mut h, "Close");
}

/// If the shadow subscription disappears (here: unsubscribed behind the app's back), an update
/// times out instead of spinning forever, and the app subscribes again and reloads. The
/// subscription is marked as the app's own in the Subscriptions list, with no unsubscribe.
#[test]
fn lost_shadow_subscription_recovers_instead_of_hanging() {
    let rt = Arc::new(Runtime::new().unwrap());
    let (port, roots) = broker::start_with_shadows(
        &rt,
        broker::Behavior::Accept,
        vec![(
            "$aws/things/sensor-1/shadow".into(),
            json!({ "desired": { "led": "on" } }),
        )],
    );
    let filter = "$aws/things/sensor-1/shadow/#";
    let fake = account_fake()
        .json(
            "GET",
            "/api/things/shadow/ListNamedShadowsForThing/sensor-1",
            json!({ "results": [] }),
        )
        .json(
            "GET",
            "/connections/sensor-1/subscriptions",
            json!({ "subscriptions": [{ "topicFilter": filter, "qos": 1 }] }),
        );
    let dir = tempfile::tempdir().unwrap();
    let ck = rcgen::generate_simple_self_signed(vec!["sensor-1".into()]).unwrap();
    let (cert, key) = (dir.path().join("cert.pem"), dir.path().join("key.pem"));
    std::fs::write(&cert, ck.cert.pem()).unwrap();
    std::fs::write(&key, ck.signing_key.serialize_pem()).unwrap();
    let mut app = test_app(rt, &fake, roots, port);
    app.load_certificate(&cert);
    app.load_private_key(&key);
    app.shadow.timeout = Duration::from_secs(1);
    let mut h = harness(app);
    h.run_steps(2);
    click_containing(&mut h, "Load from AWS");
    wait_until(&mut h, "the account", |a| {
        a.aws.session.is_some() && !a.aws.busy()
    });
    h.get_by_role_and_label(Role::ListBoxOption, "sensor-1")
        .click();
    h.run_steps(2);
    click_button(&mut h, &format!("{} Connect", icons::PLUG));
    let classic = |a: &MqttApp| {
        a.shadow
            .shadows
            .as_ref()
            .and_then(|s| s.items.get(&shadow::ShadowName::Classic))
            .cloned()
    };
    wait_until(&mut h, "the shadow", |a| {
        classic(a).is_some_and(|c| c.status == shadow::Status::Loaded)
    });

    // The app's own subscription can't be unsubscribed from the list.
    wait_until(&mut h, "the subscription list", |a| {
        matches!(a.live.subscriptions, Some(Ok(_)))
    });
    h.get_by_label("Used by Shadows");
    assert!(h.query_by_label("Unsubscribe").is_none());

    // Drop it behind the app's back, then update.
    h.state()
        .connection
        .as_ref()
        .unwrap()
        .unsubscribe(filter, &[])
        .unwrap();
    wait_until(&mut h, "the unsubscribe", |a| {
        a.log.entries().any(|e| e.kind == "UNSUBACK")
    });
    click_button(&mut h, "Shadows");
    h.state_mut().shadow.fields[0].value = FieldValue::Text("off".into());
    h.run_steps(2);
    click(&mut h, "Send update");
    assert!(classic(h.state()).unwrap().updating);

    // It times out (not a forever spinner), resubscribes, and reloads the applied update.
    wait_until(&mut h, "the timeout and recovery", |a| {
        classic(a).is_some_and(|c| {
            !c.updating && c.status == shadow::Status::Loaded && c.version == Some(2)
        })
    });
    let c = classic(h.state()).unwrap();
    assert!(c
        .update_error
        .as_deref()
        .unwrap()
        .contains("may still have been applied"));
    assert_eq!(c.desired, Some(json!({ "led": "off" })));
    let subscribes = h
        .state()
        .log
        .entries()
        .filter(|e| {
            e.kind == "SUBSCRIBE"
                && matches!(&e.replay, Some(Replay::Subscribe { filter: f, .. }) if f == filter)
        })
        .count();
    assert_eq!(subscribes, 2, "subscribed again after the timeout");
    assert!(h.state().shadow.subscribed);
}

/// MQTT 5 User Properties: edited for CONNECT, PUBLISH, and SUBSCRIBE, shown on received
/// messages, and carried into the copied mosquitto commands. Hidden for MQTT 3.1.1.
#[test]
fn user_properties_are_sent_shown_and_copied() {
    let rt = Arc::new(Runtime::new().unwrap());
    let (port, roots) = broker::start(&rt, broker::Behavior::Accept);
    let dir = tempfile::tempdir().unwrap();
    let ck = rcgen::generate_simple_self_signed(vec!["c".into()]).unwrap();
    let (cert, key) = (dir.path().join("cert.pem"), dir.path().join("key.pem"));
    std::fs::write(&cert, ck.cert.pem()).unwrap();
    std::fs::write(&key, ck.signing_key.serialize_pem()).unwrap();
    let mut app = test_app(rt, &FakeAws::new(), roots, port);
    app.load_certificate(&cert);
    app.load_private_key(&key);
    app.settings.mode = IdentityMode::ClientId;
    app.settings.custom_client_id = "tester".into();
    app.settings.mqtt_version = MqttVersion::V311;
    let mut h = harness(app);
    h.run_steps(2);

    // MQTT 3.1.1 has no User Properties.
    click_button(&mut h, "Advanced");
    h.get_by_label("User properties need MQTT 5.");
    h.state_mut().settings.mqtt_version = MqttVersion::V5;
    h.run_steps(2);
    h.get_by_label("Sent with CONNECT.");

    // CONNECT properties: add a row; an empty name isn't sent.
    click_containing(&mut h, "Add property");
    h.state_mut().settings.connect_user_properties[0] = ("app".into(), "ui-test".into());
    h.state_mut()
        .settings
        .connect_user_properties
        .push((String::new(), "ignored".into()));
    h.run_steps(2);
    h.get_by_label("1 user property");
    click_button(&mut h, "Advanced");

    click_button(&mut h, &format!("{} Connect", icons::PLUG));
    wait_until(&mut h, "the connection", |a| {
        a.state == ConnectionState::Connected
    });
    let connect = h
        .state()
        .log
        .entries()
        .find(|e| e.kind == "CONNECT")
        .unwrap()
        .details
        .clone();
    assert!(
        connect.contains("user_properties=[app=ui-test]"),
        "{connect}"
    );
    assert!(!connect.contains("ignored"), "{connect}");

    // SUBSCRIBE and PUBLISH properties, each behind its own toggle.
    click_button(&mut h, "User properties for subscribe and unsubscribe");
    h.state_mut().settings.subscription_user_properties = vec![("sub".into(), "yes".into())];
    h.state_mut().settings.subscribe_filter = "sensors/#".into();
    h.run_steps(2);
    click_button(&mut h, "Subscribe");
    click_button(&mut h, "User properties");
    h.state_mut().settings.publish_user_properties = vec![("source".into(), "ui".into())];
    h.state_mut().settings.publish_topic = "sensors/1".into();
    h.state_mut().publish_payload = "hi".into();
    h.run_steps(2);
    click_containing(&mut h, "Send");
    wait_until(&mut h, "the echoed message", |a| !a.messages.is_empty());
    assert_eq!(
        h.state().messages[0].user_properties,
        [("source".to_owned(), "ui".to_owned())]
    );
    // Properties sit behind a toggle under the payload.
    assert!(h.query_by_label("source: ui").is_none());
    click_button(&mut h, "User properties (1)");
    h.get_by_label("source: ui");

    // The copied commands carry each packet's own properties, plus CONNECT's.
    click_containing(&mut h, "Logs ");
    let command = |a: &MqttApp| a.cli_command(a.cli.as_ref().unwrap());
    h.get_by_label("Copy as mosquitto_pub").scroll_to_me();
    h.run_steps(3);
    click_button(&mut h, "Copy as mosquitto_pub");
    h.run_steps(3);
    let pubc = command(h.state());
    assert!(
        pubc.contains("-D publish user-property source ui"),
        "{pubc}"
    );
    assert!(
        pubc.contains("-D connect user-property app ui-test"),
        "{pubc}"
    );
    click(&mut h, "Close");
    h.get_by_label("Copy as mosquitto_sub").scroll_to_me();
    h.run_steps(3);
    click_button(&mut h, "Copy as mosquitto_sub");
    h.run_steps(3);
    let subc = command(h.state());
    assert!(
        subc.contains("-D subscribe user-property sub yes"),
        "{subc}"
    );
    click(&mut h, "Close");
}

/// Received messages: topic badge and payload each copy to the clipboard (with a check mark
/// for feedback), the properties toggle only appears when there are properties, and the MQTT
/// log exports as JSON or text, without what was cleared.
#[test]
fn messages_copy_and_the_log_exports_as_a_trace() {
    let rt = Arc::new(Runtime::new().unwrap());
    let (port, roots) = broker::start(&rt, broker::Behavior::Accept);
    let dir = tempfile::tempdir().unwrap();
    let ck = rcgen::generate_simple_self_signed(vec!["c".into()]).unwrap();
    let (cert, key) = (dir.path().join("cert.pem"), dir.path().join("key.pem"));
    std::fs::write(&cert, ck.cert.pem()).unwrap();
    std::fs::write(&key, ck.signing_key.serialize_pem()).unwrap();
    let mut app = test_app(rt, &FakeAws::new(), roots, port);
    app.load_certificate(&cert);
    app.load_private_key(&key);
    app.settings.mode = IdentityMode::ClientId;
    app.settings.custom_client_id = "tracer".into();
    let mut h = harness(app);
    h.run_steps(2);
    click_button(&mut h, &format!("{} Connect", icons::PLUG));
    wait_until(&mut h, "the connection", |a| {
        a.state == ConnectionState::Connected
    });
    h.state_mut().settings.subscribe_filter = "t/#".into();
    h.run_steps(2);
    click_button(&mut h, "Subscribe");
    h.state_mut().settings.publish_topic = "t/1".into();
    h.state_mut().publish_payload = "{\"t\": 21}".into();
    h.run_steps(2);
    click_containing(&mut h, "Send");
    wait_until(&mut h, "the message", |a| !a.messages.is_empty());

    // No properties on this message, so no properties toggle.
    assert!(h.query_by_label_contains("User properties (").is_none());
    let copied = |h: &Harness<'_, MqttApp>| -> Vec<String> {
        h.output()
            .platform_output
            .commands
            .iter()
            .filter_map(|c| match c {
                egui::OutputCommand::CopyText(text) => Some(text.clone()),
                _ => None,
            })
            .collect()
    };
    // The payload lines up with the topic badge, on the left.
    let topic = h.get_by_role_and_label(Role::Button, "Copy topic").rect();
    let payload = h.get_by_label("{\"t\": 21}").rect();
    assert!(
        (payload.left() - topic.left()).abs() < 12.0,
        "payload at {payload:?}, topic at {topic:?}"
    );

    // Clicking the topic badge copies it, with a toast as feedback. It has no copy button.
    assert!(h.query_by_label("Copy topic").is_some());
    h.get_by_role_and_label(Role::Button, "Copy topic").click();
    h.step();
    assert_eq!(copied(&h), ["t/1"]);
    h.run_steps(1);
    assert_eq!(
        theme::toast_message(&h.ctx).as_deref(),
        Some("Topic copied to the clipboard")
    );
    h.run_steps(2); // a new area sizes itself before it shows
    h.get_by_label("Topic copied to the clipboard");
    h.get_by_label("Copy payload").click();
    h.step();
    assert_eq!(copied(&h), ["{\"t\": 21}"]);
    h.run_steps(1);
    h.run_steps(2); // a new area sizes itself before it shows
    h.get_by_label("Payload copied to the clipboard");

    // Export the log as JSON and as text.
    click_containing(&mut h, "Logs ");
    let json_path = dir.path().join("trace.json");
    h.state_mut().test_export_path = Some(json_path.clone());
    click(&mut h, "Export…");
    let doc: serde_json::Value =
        serde_json::from_str(&std::fs::read_to_string(&json_path).unwrap()).unwrap();
    let count = h.state().log.len();
    assert_eq!(doc["packet_count"], count);
    assert_eq!(doc["client_id"], "tracer");
    let receive = doc["packets"]
        .as_array()
        .unwrap()
        .iter()
        .find(|p| p["type"] == "RECEIVE")
        .unwrap();
    assert_eq!(receive["payload"], "{\"t\": 21}");
    assert!(h.state().notice.as_deref().unwrap().contains("Exported"));
    let text_path = dir.path().join("trace.txt");
    h.state_mut().test_export_path = Some(text_path.clone());
    click(&mut h, "Export…");
    let text = std::fs::read_to_string(&text_path).unwrap();
    assert!(text.contains("<- RECEIVE"), "{text}");
    assert!(text.contains("    | {\"t\": 21}"), "{text}");

    // Cleared history isn't exported; an empty log can't be exported.
    for clear in h.get_all_by_label("Clear").collect::<Vec<_>>() {
        clear.click();
    }
    h.run_steps(2);
    std::fs::remove_file(&json_path).unwrap();
    h.state_mut().test_export_path = Some(json_path.clone());
    click(&mut h, "Export…");
    assert!(!json_path.exists(), "nothing to export after clearing");
}
