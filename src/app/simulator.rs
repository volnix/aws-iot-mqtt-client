// Copyright Amazon.com, Inc. or its affiliates. All Rights Reserved.
// SPDX-License-Identifier: Apache-2.0

//! The "Policy simulator" tab: edit an IoT policy, pick an MQTT operation and connection
//! context, and see whether AWS IoT Core would accept or reject it. Evaluation is local and
//! live (see [`crate::policy_sim`]); the only AWS calls are optional reads to load a policy.

use egui::RichText;
use serde::{Deserialize, Serialize};

use super::{show_error, AppMsg, IdentityMode, MqttApp};
use crate::error::AppError;
use crate::icons;
use crate::policy_sim::{
    self, Decision, Effect, MqttOperation, Outcome, SimContext, Simulation, ThingContext,
};
use crate::provision::{self, ArnScope, Identity};
use crate::theme::{self, Palette, Severity};

pub const EXAMPLE_REGION: &str = "us-east-1";
pub const EXAMPLE_ACCOUNT: &str = "123456789012";
pub const EXAMPLE_THING: &str = "my-thing";

#[derive(Debug, Clone, Copy, PartialEq, Eq, Default, Serialize, Deserialize)]
pub enum SimOp {
    #[default]
    Connect,
    Publish,
    Subscribe,
    Receive,
}

/// Simulator inputs, persisted between runs.
#[derive(Debug, Clone, PartialEq, Serialize, Deserialize)]
#[serde(default)]
pub struct SimSettings {
    pub policy: String,
    pub operation: SimOp,
    pub topic: String,
    pub filter: String,
    pub retain: bool,
    pub client_id: String,
    pub thing_name: String,
    pub attached: bool,
    pub exclusive: bool,
    pub thing_type: String,
    /// `name=value` lines.
    pub attributes: String,
    /// `key=value` lines for other variables and condition keys.
    pub extra: String,
    pub region: String,
    pub account: String,
}

impl Default for SimSettings {
    fn default() -> Self {
        Self {
            policy: example_policy(EXAMPLE_REGION, EXAMPLE_ACCOUNT),
            operation: SimOp::Connect,
            topic: format!("{EXAMPLE_THING}/telemetry"),
            filter: format!("{EXAMPLE_THING}/commands/#"),
            retain: false,
            client_id: EXAMPLE_THING.into(),
            thing_name: EXAMPLE_THING.into(),
            attached: true,
            exclusive: false,
            thing_type: String::new(),
            attributes: String::new(),
            extra: String::new(),
            region: EXAMPLE_REGION.into(),
            account: EXAMPLE_ACCOUNT.into(),
        }
    }
}

/// The best-practice Thing policy the app creates, as a starting point.
pub fn example_policy(region: &str, account: &str) -> String {
    provision::default_policy_document(
        &Identity::Thing(EXAMPLE_THING.into()),
        &ArnScope::new("aws", region, Some(account)),
    )
}

/// Non-persisted simulator UI state.
#[derive(Default)]
pub struct SimState {
    pub policies: Option<Result<Vec<String>, AppError>>,
    pub policies_loading: bool,
    pub selected_policy: Option<String>,
    pub document_loading: bool,
    pub load_error: Option<AppError>,
    /// The document as loaded, to show "Edited" once it changes.
    pub loaded_document: Option<String>,
    pub show_context: bool,
    pub show_trace: bool,
}

impl SimSettings {
    fn operation(&self) -> MqttOperation {
        match self.operation {
            SimOp::Connect => MqttOperation::Connect,
            SimOp::Publish => MqttOperation::Publish {
                topic: self.topic.trim().to_owned(),
                retain: self.retain,
            },
            SimOp::Subscribe => MqttOperation::Subscribe {
                filter: self.filter.trim().to_owned(),
            },
            SimOp::Receive => MqttOperation::Receive {
                topic: self.topic.trim().to_owned(),
            },
        }
    }

    fn context(&self, partition: &str) -> Result<SimContext, String> {
        let attributes = policy_sim::parse_key_values(&self.attributes)
            .map_err(|e| format!("Attributes: {e}"))?;
        let extra = policy_sim::parse_key_values(&self.extra)
            .map_err(|e| format!("Other variables: {e}"))?;
        let thing_name = self.thing_name.trim();
        Ok(SimContext {
            partition: partition.to_owned(),
            region: self.region.trim().to_owned(),
            account: self.account.trim().to_owned(),
            client_id: self.client_id.trim().to_owned(),
            thing: (!thing_name.is_empty()).then(|| ThingContext {
                name: thing_name.to_owned(),
                attached: self.attached,
                exclusive: self.exclusive,
                thing_type: Some(self.thing_type.trim().to_owned()).filter(|t| !t.is_empty()),
                attributes,
            }),
            extra,
        })
    }

    /// Short description of the connection context, shown when it's collapsed.
    fn context_summary(&self) -> String {
        let mut parts = vec![format!("client {}", self.client_id.trim())];
        let thing = self.thing_name.trim();
        if thing.is_empty() {
            parts.push("no Thing".into());
        } else if self.attached {
            parts.push(format!("Thing {thing} (attached)"));
        } else {
            parts.push(format!("Thing {thing} (not attached)"));
        }
        parts.push(format!("{} · {}", self.region.trim(), self.account.trim()));
        parts.join(" · ")
    }
}

/// Runs the simulation for the current inputs.
pub fn evaluate(settings: &SimSettings, partition: &str) -> Result<Simulation, String> {
    let policy = policy_sim::parse_policy(&settings.policy)?;
    let ctx = settings.context(partition)?;
    policy_sim::simulate(&policy, &settings.operation(), &ctx)
}

impl MqttApp {
    fn sim_partition(&self) -> String {
        self.aws
            .session
            .as_ref()
            .map(|s| s.partition().to_owned())
            .unwrap_or_else(|| "aws".into())
    }

    /// Copies the client tab's identity, region, and account into the simulator.
    fn sim_use_client_context(&mut self) {
        if let Some(client_id) = self.effective_client_id() {
            self.settings.sim.client_id = client_id;
        }
        match self.settings.mode {
            IdentityMode::Thing => {
                if let Some(thing) = &self.settings.thing {
                    self.settings.sim.thing_name = thing.clone();
                }
            }
            IdentityMode::ClientId => self.settings.sim.thing_name.clear(),
        }
        if let Some(session) = &self.aws.session {
            self.settings.sim.region = session.region.clone();
            if let Some(account) = session.account() {
                self.settings.sim.account = account.to_owned();
            }
        }
    }

    fn sim_load_policy_list(&mut self, ctx: &egui::Context) {
        let Some(session) = self.aws.session.clone() else {
            return;
        };
        self.sim.policies_loading = true;
        self.sim.load_error = None;
        let tx = self.tx.clone();
        let ctx = ctx.clone();
        self.rt.spawn(async move {
            let result = session.list_policies().await;
            let _ = tx.send(AppMsg::SimPolicies { result });
            ctx.request_repaint();
        });
    }

    fn sim_load_document(&mut self, ctx: &egui::Context, name: String) {
        let Some(session) = self.aws.session.clone() else {
            return;
        };
        self.sim.document_loading = true;
        self.sim.load_error = None;
        self.sim.selected_policy = Some(name.clone());
        let tx = self.tx.clone();
        let ctx = ctx.clone();
        self.rt.spawn(async move {
            let result = session.get_policy_document(&name).await;
            let _ = tx.send(AppMsg::SimPolicyDocument { name, result });
            ctx.request_repaint();
        });
    }

    pub(super) fn apply_sim_policies(&mut self, result: Result<Vec<String>, AppError>) {
        self.sim.policies_loading = false;
        self.sim.policies = Some(result);
    }

    pub(super) fn apply_sim_document(&mut self, name: String, result: Result<String, AppError>) {
        if self.sim.selected_policy.as_deref() != Some(name.as_str()) {
            return;
        }
        self.sim.document_loading = false;
        match result {
            Ok(document) => {
                self.settings.sim.policy = document.clone();
                self.sim.loaded_document = Some(document);
            }
            Err(e) => self.sim.load_error = Some(e),
        }
    }

    // -----------------------------------------------------------------------------------------
    // UI
    // -----------------------------------------------------------------------------------------

    pub(super) fn simulator_view(&mut self, ui: &mut egui::Ui) {
        let gap = theme::metrics(ui).gap;
        let result = evaluate(&self.settings.sim, &self.sim_partition());
        ui.columns(2, |columns| {
            // Left: the editor fills the column height.
            theme::card(&mut columns[0], |ui| self.sim_policy_editor(ui));
            // Right: request and result scroll together when details are expanded.
            egui::ScrollArea::vertical()
                .id_salt("sim_right")
                .auto_shrink([false, false])
                .show(&mut columns[1], |ui| {
                    ui.spacing_mut().item_spacing.y = gap;
                    theme::card(ui, |ui| self.sim_request(ui));
                    theme::card(ui, |ui| self.sim_result(ui, &result));
                });
        });
    }

    fn sim_policy_editor(&mut self, ui: &mut egui::Ui) {
        let p = Palette::of(ui);
        theme::card_title(ui, "Policy document", |ui| {
            if theme::link_button(ui, "Reset to example").clicked() {
                let sim = &mut self.settings.sim;
                sim.policy = example_policy(sim.region.trim(), sim.account.trim());
                self.sim.selected_policy = None;
                self.sim.loaded_document = None;
            }
        });
        self.sim_policy_loader(ui);

        let parsed = policy_sim::parse_policy(&self.settings.sim.policy);
        // Fill the column, leaving room under the editor for the status line.
        let height = (ui.available_height() - 70.0).clamp(220.0, 4000.0);
        theme::inset(ui, |ui| {
            egui::ScrollArea::vertical()
                .id_salt("sim_policy")
                .max_height(height)
                .auto_shrink([false, false])
                .show(ui, |ui| {
                    ui.add(
                        egui::TextEdit::multiline(&mut self.settings.sim.policy)
                            .code_editor()
                            .frame(egui::Frame::new())
                            .desired_rows(22)
                            .desired_width(f32::INFINITY),
                    );
                });
        });
        match &parsed {
            Ok(policy) => {
                ui.horizontal_wrapped(|ui| {
                    ui.label(RichText::new(icons::CHECK).small().color(p.ok));
                    let n = policy.statements.len();
                    theme::hint(
                        ui,
                        format!("{n} statement{}", if n == 1 { "" } else { "s" }),
                    );
                    if self
                        .sim
                        .loaded_document
                        .as_ref()
                        .is_some_and(|d| *d != self.settings.sim.policy)
                    {
                        theme::pill(ui, "Edited", p.info);
                    }
                    if theme::link_button(ui, "Format").clicked() {
                        self.settings.sim.policy =
                            provision::pretty_json(&self.settings.sim.policy);
                    }
                });
                for warning in &policy.warnings {
                    ui.label(
                        RichText::new(format!("{} {warning}", icons::WARNING))
                            .small()
                            .color(p.warn),
                    );
                }
            }
            Err(e) => {
                ui.label(
                    RichText::new(format!("{} {e}", icons::WARNING))
                        .small()
                        .color(p.err),
                );
            }
        }
    }

    /// "Load from account": pick an existing policy (ListPolicies, then GetPolicy).
    fn sim_policy_loader(&mut self, ui: &mut egui::Ui) {
        if self.aws.session.is_none() {
            theme::hint(
                ui,
                "Paste a policy, or load your AWS account in the Client tab to pick an existing one.",
            );
            return;
        }
        let mut pick: Option<String> = None;
        ui.horizontal(|ui| match &self.sim.policies {
            None => {
                let button = theme::ghost_button(ui, format!("{} Load from account", icons::CLOUD));
                if ui.add_enabled(!self.sim.policies_loading, button).clicked() {
                    self.sim_load_policy_list(ui.ctx());
                }
                if self.sim.policies_loading {
                    ui.spinner();
                }
            }
            Some(Ok(list)) if list.is_empty() => {
                theme::hint(ui, "No policies in this account and region.");
            }
            Some(Ok(list)) => {
                let label = self
                    .sim
                    .selected_policy
                    .clone()
                    .unwrap_or_else(|| "Choose a policy…".into());
                egui::ComboBox::from_id_salt("sim_policy_pick")
                    .selected_text(label)
                    .width((ui.available_width() - 40.0).max(160.0))
                    .show_ui(ui, |ui| {
                        for name in list {
                            let selected = self.sim.selected_policy.as_deref() == Some(name);
                            if ui.selectable_label(selected, name).clicked() {
                                pick = Some(name.clone());
                            }
                        }
                    });
                if self.sim.document_loading {
                    ui.spinner();
                } else if theme::icon_button(ui, icons::REFRESH, "Reload policy list", true)
                    .clicked()
                {
                    self.sim.policies = None;
                    self.sim_load_policy_list(ui.ctx());
                }
            }
            Some(Err(e)) => {
                ui.label(
                    RichText::new(e.summary())
                        .small()
                        .color(Palette::of(ui).err),
                )
                .on_hover_text(e.full());
                if theme::link_button(ui, "Retry").clicked() {
                    self.sim.policies = None;
                    self.sim_load_policy_list(ui.ctx());
                }
            }
        });
        if let Some(name) = pick {
            self.sim_load_document(ui.ctx(), name);
        }
        if self.sim.load_error.is_some() {
            show_error(ui, self.sim.load_error.as_ref());
        }
    }

    fn sim_request(&mut self, ui: &mut egui::Ui) {
        let p = Palette::of(ui);
        theme::card_title(ui, "MQTT operation", |_| {});
        theme::segmented(
            ui,
            &mut self.settings.sim.operation,
            &[
                (SimOp::Connect, "CONNECT"),
                (SimOp::Publish, "PUBLISH"),
                (SimOp::Subscribe, "SUBSCRIBE"),
                (SimOp::Receive, "Receive"),
            ],
            true,
        );
        let sim = &mut self.settings.sim;
        let (explain, action) = match sim.operation {
            SimOp::Connect => (
                "The client opens a connection.",
                "iot:Connect on client/<client ID>",
            ),
            SimOp::Publish => (
                "The client publishes a message.",
                "iot:Publish on topic/<topic>, plus iot:RetainPublish if retained",
            ),
            SimOp::Subscribe => (
                "The client subscribes to a topic filter.",
                "iot:Subscribe on topicfilter/<filter>",
            ),
            SimOp::Receive => (
                "The broker delivers a PUBLISH to this client (it must also be subscribed).",
                "iot:Receive on topic/<topic>",
            ),
        };
        theme::hint(ui, explain);
        ui.label(RichText::new(action).small().monospace().color(p.muted));

        egui::Grid::new("sim_request_grid")
            .num_columns(2)
            .spacing([16.0, 10.0])
            .show(ui, |ui| {
                match sim.operation {
                    SimOp::Connect => {}
                    SimOp::Publish | SimOp::Receive => {
                        theme::field_label(ui, "Topic");
                        ui.horizontal(|ui| {
                            ui.add(
                                egui::TextEdit::singleline(&mut sim.topic)
                                    .hint_text("sensors/room-1/temperature")
                                    .desired_width(260.0),
                            );
                            if sim.operation == SimOp::Publish {
                                ui.checkbox(&mut sim.retain, "Retain");
                            }
                        });
                        ui.end_row();
                    }
                    SimOp::Subscribe => {
                        theme::field_label(ui, "Topic filter");
                        ui.add(
                            egui::TextEdit::singleline(&mut sim.filter)
                                .hint_text("sensors/+/temperature")
                                .desired_width(260.0),
                        );
                        ui.end_row();
                    }
                }
                theme::field_label(ui, "Client ID");
                ui.add(
                    egui::TextEdit::singleline(&mut sim.client_id)
                        .hint_text("Client ID in CONNECT")
                        .desired_width(260.0),
                );
                ui.end_row();
            });

        ui.horizontal(|ui| {
            theme::disclosure(ui, &mut self.sim.show_context, "Connection context");
            if !self.sim.show_context {
                theme::hint(ui, self.settings.sim.context_summary());
            }
        });
        if self.sim.show_context {
            self.sim_context(ui);
        }
    }

    fn sim_context(&mut self, ui: &mut egui::Ui) {
        let have_client = self.effective_client_id().is_some() || self.aws.session.is_some();
        if have_client
            && theme::link_button(ui, "Use the Client tab's identity, region, and account")
                .clicked()
        {
            self.sim_use_client_context();
        }
        let sim = &mut self.settings.sim;
        theme::inset(ui, |ui| {
            egui::Grid::new("sim_context_grid")
                .num_columns(2)
                .spacing([16.0, 9.0])
                .show(ui, |ui| {
                    theme::field_label(ui, "Thing");
                    ui.add(
                        egui::TextEdit::singleline(&mut sim.thing_name)
                            .hint_text("None")
                            .desired_width(220.0),
                    );
                    ui.end_row();

                    if !sim.thing_name.trim().is_empty() {
                        theme::field_label(ui, "Attachment");
                        ui.vertical(|ui| {
                            ui.checkbox(&mut sim.attached, "Certificate is attached to the Thing");
                            ui.add_enabled(
                                sim.attached,
                                egui::Checkbox::new(&mut sim.exclusive, "Exclusive attachment"),
                            )
                            .on_hover_text(
                                "With a non-exclusive attachment, thing policy variables only resolve when the client ID equals the Thing name.",
                            );
                        });
                        ui.end_row();

                        theme::field_label(ui, "Thing type");
                        ui.add(
                            egui::TextEdit::singleline(&mut sim.thing_type)
                                .hint_text("Optional")
                                .desired_width(220.0),
                        );
                        ui.end_row();

                        theme::field_label(ui, "Attributes");
                        ui.add(
                            egui::TextEdit::multiline(&mut sim.attributes)
                                .hint_text("location=Seattle\nversion=v1")
                                .desired_rows(2)
                                .desired_width(220.0),
                        );
                        ui.end_row();
                    }

                    theme::field_label(ui, "Other variables");
                    ui.add(
                        egui::TextEdit::multiline(&mut sim.extra)
                            .hint_text("aws:SourceIp=203.0.113.10\niot:DomainName=example.com")
                            .desired_rows(2)
                            .desired_width(220.0),
                    );
                    ui.end_row();

                    theme::field_label(ui, "Region");
                    ui.add(egui::TextEdit::singleline(&mut sim.region).desired_width(140.0));
                    ui.end_row();

                    theme::field_label(ui, "Account");
                    ui.add(egui::TextEdit::singleline(&mut sim.account).desired_width(140.0));
                    ui.end_row();
                });
        });
    }

    fn sim_result(&mut self, ui: &mut egui::Ui, result: &Result<Simulation, String>) {
        let p = Palette::of(ui);
        let sim = match result {
            Ok(sim) => sim,
            Err(e) => {
                theme::card_title(ui, "Result", |_| {});
                ui.label(RichText::new(e).color(p.warn));
                return;
            }
        };
        let (word, color, icon) = match sim.decision {
            Decision::Accept => ("Accept", p.ok, icons::CHECK),
            Decision::Reject => ("Reject", p.err, icons::CLOSE),
        };
        ui.horizontal(|ui| {
            egui::Frame::new()
                .fill(color.gamma_multiply(0.14))
                .corner_radius(egui::CornerRadius::same(theme::metrics(ui).control_radius))
                .inner_margin(egui::Margin::symmetric(14, 6))
                .show(ui, |ui| {
                    ui.label(
                        theme::title(format!("{icon} {word}"))
                            .size(theme::metrics(ui).font_heading + 4.0)
                            .color(color),
                    );
                });
            if !sim.certain {
                theme::pill(ui, "Not certain", p.warn);
            }
        });
        ui.label(RichText::new(&sim.summary));
        if !sim.certain {
            theme::banner(ui, Severity::Warning, icons::WARNING, false, |ui| {
                ui.label(
                    RichText::new(
                        "Part of the policy uses a feature the simulator can't evaluate, and it could change this result. Check the details below.",
                    )
                    .small(),
                );
            });
        }
        if !sim.unresolved.is_empty() {
            theme::hint(
                ui,
                format!(
                    "No value for: {}. Statements that need these variables don't match.",
                    sim.unresolved
                        .iter()
                        .map(|v| format!("${{{v}}}"))
                        .collect::<Vec<_>>()
                        .join(", ")
                ),
            );
        }

        theme::disclosure(ui, &mut self.sim.show_trace, "How this was decided");
        if !self.sim.show_trace {
            return;
        }
        for check in &sim.checks {
            theme::inset(ui, |ui| {
                ui.horizontal(|ui| {
                    let (mark, color) = if check.allowed {
                        (icons::CHECK, p.ok)
                    } else {
                        (icons::CLOSE, p.err)
                    };
                    ui.label(RichText::new(mark).small().color(color));
                    ui.label(theme::title(check.check.action).monospace());
                });
                ui.label(
                    RichText::new(&check.check.resource)
                        .small()
                        .monospace()
                        .color(p.muted),
                );
                ui.label(RichText::new(&check.summary).small());
                ui.add_space(2.0);
                for statement in &check.trace {
                    let effect = match statement.effect {
                        Effect::Allow => "Allow",
                        Effect::Deny => "Deny",
                    };
                    let (status, color) = match (statement.outcome, statement.effect) {
                        (Outcome::Applies, Effect::Allow) => ("applies", p.ok),
                        (Outcome::Applies, Effect::Deny) => ("applies", p.err),
                        (Outcome::Unknown, _) => ("can't evaluate", p.warn),
                        (Outcome::DoesNotApply, _) => ("doesn't apply", p.muted),
                    };
                    ui.horizontal_wrapped(|ui| {
                        ui.label(
                            RichText::new(format!("{} {effect}", statement.statement))
                                .small()
                                .monospace(),
                        );
                        ui.label(RichText::new(status).small().color(color));
                        ui.label(RichText::new(&statement.reason).small().color(p.muted));
                    });
                }
            });
        }
    }
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn default_inputs_evaluate_and_accept_connect() {
        let settings = SimSettings::default();
        let sim = evaluate(&settings, "aws").unwrap();
        assert_eq!(sim.decision, Decision::Accept, "{}", sim.summary);
        assert!(sim.certain);
    }

    #[test]
    fn default_policy_rejects_connect_with_another_client_id() {
        let settings = SimSettings {
            client_id: "intruder".into(),
            ..Default::default()
        };
        let sim = evaluate(&settings, "aws").unwrap();
        assert_eq!(sim.decision, Decision::Reject);
    }

    #[test]
    fn every_operation_evaluates_with_defaults() {
        for op in [
            SimOp::Connect,
            SimOp::Publish,
            SimOp::Subscribe,
            SimOp::Receive,
        ] {
            let settings = SimSettings {
                operation: op,
                ..Default::default()
            };
            let sim = evaluate(&settings, "aws").unwrap();
            assert_eq!(sim.decision, Decision::Accept, "{op:?}: {}", sim.summary);
        }
    }

    #[test]
    fn bad_context_lines_are_reported() {
        let settings = SimSettings {
            attributes: "not a pair".into(),
            ..Default::default()
        };
        assert!(evaluate(&settings, "aws")
            .unwrap_err()
            .starts_with("Attributes"));
    }

    #[test]
    fn context_summary_describes_thing_attachment() {
        let mut settings = SimSettings::default();
        assert!(settings.context_summary().contains("(attached)"));
        settings.attached = false;
        assert!(settings.context_summary().contains("not attached"));
        settings.thing_name.clear();
        assert!(settings.context_summary().contains("no Thing"));
    }
}
