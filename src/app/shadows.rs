// Copyright 2026 Nick Volgas
// SPDX-License-Identifier: Apache-2.0

//! The "Shadows" tab: the connected Thing's classic and named Device Shadows.
//!
//! When the client connects as a Thing, the app subscribes to the Thing's shadow topics, lists
//! its named shadows (`ListNamedShadowsForThing`, read-only, when an AWS account is loaded), and
//! sends a `get` for each. Reads and updates go over MQTT as the device, so they're authorized
//! by the device's policy like any other publish. See [`crate::shadow`].

use std::time::Instant;

use egui::RichText;
use serde_json::Value;

use super::{AppMsg, IdentityMode, MqttApp};
use crate::error::AppError;
use crate::icons;
use crate::mqtt::ConnectionState;
use crate::shadow::{self, Applied, Section, ShadowName, Shadows, Status};
use crate::theme::{self, Palette};

/// Shadows of the Thing the client is (or was last) connected as.
pub(super) struct ShadowState {
    pub shadows: Option<Shadows>,
    /// The connection the shadows belong to; responses from older connections are ignored.
    pub connection_id: Option<u64>,
    /// The shadow-topic subscription was acknowledged, so `get`s will be answered.
    pub subscribed: bool,
    /// `get`s waiting for the subscription to be acknowledged.
    pending: Vec<ShadowName>,
    pub listing: bool,
    pub selected: Option<ShadowName>,
    pub section: Section,
    pub editor: String,
    /// What the editor was last filled from: shadow, section, and document version.
    editor_source: Option<(ShadowName, Section, Option<u64>)>,
    /// The user changed the editor, so new documents don't overwrite it.
    editor_dirty: bool,
    pub edit_error: Option<String>,
    pub new_name: String,
}

impl Default for ShadowState {
    fn default() -> Self {
        Self {
            shadows: None,
            connection_id: None,
            subscribed: false,
            pending: Vec::new(),
            listing: false,
            selected: None,
            section: Section::Desired,
            editor: String::new(),
            editor_source: None,
            editor_dirty: false,
            edit_error: None,
            new_name: String::new(),
        }
    }
}

impl MqttApp {
    /// True while connected as the Thing whose shadows are shown.
    fn shadows_live(&self) -> bool {
        self.state == ConnectionState::Connected
            && self.shadow.connection_id.is_some()
            && self.shadow.connection_id == self.connection.as_ref().map(|c| c.id)
    }

    /// Called once per new connection. Only Thing connections have shadows.
    pub(super) fn start_shadows(&mut self, ctx: &egui::Context) {
        if self.settings.mode != IdentityMode::Thing {
            self.shadow = ShadowState::default();
            return;
        }
        let (Some(conn), Some((_, thing))) = (&self.connection, &self.connected_as) else {
            return;
        };
        let thing = thing.clone();
        let connection_id = conn.id;
        let subscribe = conn.subscribe(&shadow::subscription_filter(&thing), 1);
        self.shadow = ShadowState {
            shadows: Some(Shadows::new(thing.clone())),
            connection_id: Some(connection_id),
            selected: Some(ShadowName::Classic),
            ..ShadowState::default()
        };
        if let Err(e) = subscribe {
            if let Some(s) = &mut self.shadow.shadows {
                s.list_error = Some(e.summary());
            }
            return;
        }
        self.shadow_get(ShadowName::Classic);
        self.list_named_shadows(ctx, thing, connection_id);
    }

    fn list_named_shadows(&mut self, ctx: &egui::Context, thing: String, connection_id: u64) {
        let Some(session) = self.aws.session.clone() else {
            if let Some(s) = &mut self.shadow.shadows {
                s.list_error =
                    Some("Load your AWS account on the Client tab to list named shadows.".into());
            }
            return;
        };
        self.shadow.listing = true;
        let endpoint = self.data_endpoint();
        let tx = self.tx.clone();
        let ctx = ctx.clone();
        self.rt.spawn(async move {
            let result = session
                .list_named_shadows(endpoint.as_deref(), &thing)
                .await;
            let _ = tx.send(AppMsg::NamedShadows {
                connection_id,
                result,
            });
            ctx.request_repaint();
        });
    }

    pub(super) fn apply_named_shadows(
        &mut self,
        connection_id: u64,
        result: Result<Vec<String>, AppError>,
    ) {
        if self.shadow.connection_id != Some(connection_id) {
            return;
        }
        self.shadow.listing = false;
        match result {
            Ok(names) => {
                if let Some(s) = &mut self.shadow.shadows {
                    s.list_error = None;
                }
                for name in names {
                    self.shadow_get(ShadowName::Named(name));
                }
            }
            Err(e) => {
                if let Some(s) = &mut self.shadow.shadows {
                    s.list_error = Some(format!("Named shadows: {}", e.summary()));
                }
            }
        }
    }

    /// The first SUBACK after connecting acknowledges the shadow subscription.
    pub(super) fn shadow_suback(&mut self, details: &str) {
        if self.shadow.subscribed || self.shadow.shadows.is_none() {
            return;
        }
        if !details.contains("Success") {
            if let Some(s) = &mut self.shadow.shadows {
                s.list_error = Some(
                    "The broker refused the shadow topic subscription. The device's policy \
                     must allow iot:Subscribe on its $aws/things/…/shadow/ topics."
                        .into(),
                );
            }
            return;
        }
        self.shadow.subscribed = true;
        for name in std::mem::take(&mut self.shadow.pending) {
            self.shadow_get(name);
        }
    }

    /// Sends a shadow `get`, or queues it until the subscription is acknowledged.
    fn shadow_get(&mut self, name: ShadowName) {
        let Some(shadows) = &mut self.shadow.shadows else {
            return;
        };
        shadows.requested(name.clone(), Instant::now());
        if !self.shadow.subscribed {
            if !self.shadow.pending.contains(&name) {
                self.shadow.pending.push(name);
            }
            return;
        }
        let topic = shadow::request_topic(&shadows.thing, &name, "get");
        if let Some(conn) = &self.connection {
            if let Err(e) = conn.publish(&topic, 1, false, b"{}".to_vec()) {
                shadows.get_or_insert(name).status = Status::Error(e.summary());
            }
        }
    }

    /// Folds an incoming message into the shadows. Returns true if it was a shadow topic.
    pub(super) fn shadow_message(&mut self, topic: &str, payload: &[u8]) -> bool {
        let Some(shadows) = &mut self.shadow.shadows else {
            return false;
        };
        match shadows.apply(topic, payload) {
            Applied::NotShadow => false,
            Applied::Handled => true,
            Applied::Refresh(name) => {
                self.shadow_get(name);
                true
            }
        }
    }

    pub(super) fn expire_shadow_requests(&mut self) {
        if let Some(s) = &mut self.shadow.shadows {
            s.expire(Instant::now(), shadow::RESPONSE_TIMEOUT);
        }
    }

    fn shadow_update(&mut self, name: ShadowName) {
        self.shadow.edit_error = None;
        let payload = match shadow::update_payload(self.shadow.section, &self.shadow.editor) {
            Ok(p) => p,
            Err(e) => {
                self.shadow.edit_error = Some(e);
                return;
            }
        };
        let (Some(shadows), Some(conn)) = (&mut self.shadow.shadows, &self.connection) else {
            return;
        };
        let topic = shadow::request_topic(&shadows.thing, &name, "update");
        let entry = shadows.get_or_insert(name);
        match conn.publish(&topic, 1, false, payload) {
            Ok(()) => {
                entry.updating = true;
                entry.update_error = None;
                self.shadow.editor_dirty = false;
            }
            Err(e) => entry.update_error = Some(e.summary()),
        }
    }

    fn refresh_shadows(&mut self, ctx: &egui::Context) {
        let Some(shadows) = &self.shadow.shadows else {
            return;
        };
        let names: Vec<ShadowName> = shadows.items.keys().cloned().collect();
        let thing = shadows.thing.clone();
        for name in names {
            self.shadow_get(name);
        }
        if let Some(id) = self.shadow.connection_id {
            self.list_named_shadows(ctx, thing, id);
        }
    }

    // -----------------------------------------------------------------------------------------
    // UI
    // -----------------------------------------------------------------------------------------

    /// "Shadows", with how many exist once they're known.
    pub(super) fn shadows_tab_label(&self) -> String {
        match &self.shadow.shadows {
            Some(s) if s.existing() > 0 => format!("Shadows {}", s.existing()),
            _ => "Shadows".into(),
        }
    }

    pub(super) fn shadows_view(&mut self, ui: &mut egui::Ui) {
        if self.shadow.shadows.is_none() {
            theme::card(ui, |ui| {
                theme::card_title(ui, "Device shadows", |_| {});
                let text = match self.settings.mode {
                    IdentityMode::ClientId => {
                        "Shadows belong to a Thing. Connect as an IoT Thing on the Client tab to \
                         see its shadows."
                    }
                    IdentityMode::Thing => {
                        "Connect as a Thing on the Client tab. Its classic and named shadows \
                         load automatically."
                    }
                };
                theme::hint(ui, text);
            });
            return;
        }
        let gap = theme::metrics(ui).gap;
        ui.horizontal_top(|ui| {
            ui.vertical(|ui| {
                ui.set_width(280.0);
                theme::card(ui, |ui| self.shadow_list(ui));
            });
            ui.add_space(gap);
            ui.vertical(|ui| {
                egui::ScrollArea::vertical()
                    .id_salt("shadow_details")
                    .auto_shrink([false, false])
                    .show(ui, |ui| theme::card(ui, |ui| self.shadow_details(ui)));
            });
        });
    }

    fn shadow_list(&mut self, ui: &mut egui::Ui) {
        let p = Palette::of(ui);
        let live = self.shadows_live();
        let mut refresh = false;
        theme::card_title(ui, "Shadows", |ui| {
            if self.shadow.listing {
                ui.spinner();
            } else if theme::icon_button(ui, icons::REFRESH, "Refresh shadows", live).clicked() {
                refresh = true;
            }
        });
        let Some(shadows) = &self.shadow.shadows else {
            return;
        };
        theme::hint(ui, format!("Thing {}", shadows.thing));
        if !live {
            theme::hint(ui, "Disconnected. Showing the last known state.");
        }
        if let Some(e) = &shadows.list_error {
            ui.label(
                RichText::new(format!("{} {e}", icons::WARNING))
                    .small()
                    .color(p.warn),
            );
        }
        ui.add_space(4.0);
        let mut clicked = None;
        for (name, sh) in &shadows.items {
            let status = match &sh.status {
                Status::Loaded => sh.version.map(|v| format!("v{v}")).unwrap_or_default(),
                Status::Loading(_) => "loading…".into(),
                Status::Missing => "none".into(),
                Status::Error(_) => "error".into(),
            };
            let selected = self.shadow.selected.as_ref() == Some(name);
            let text = RichText::new(format!("{}   {status}", name.label()));
            if theme::list_row(ui, selected, true, text).clicked() {
                clicked = Some(name.clone());
            }
        }
        if let Some(name) = clicked {
            self.shadow.selected = Some(name);
        }

        ui.add_space(8.0);
        theme::field_label(ui, "Named shadow");
        ui.add(
            egui::TextEdit::singleline(&mut self.shadow.new_name)
                .hint_text("Shadow name")
                .desired_width(f32::INFINITY),
        );
        let name = self.shadow.new_name.trim().to_owned();
        let valid = shadow::validate_shadow_name(&name);
        if let (Err(e), false) = (&valid, name.is_empty()) {
            ui.label(RichText::new(e).small().color(p.err));
        }
        let add = theme::ghost_button(ui, "Open");
        if ui
            .add_enabled(live && valid.is_ok(), add)
            .on_hover_text("Load this named shadow, or start a new one")
            .clicked()
        {
            let name = ShadowName::Named(name);
            self.shadow.new_name.clear();
            self.shadow.selected = Some(name.clone());
            self.shadow_get(name);
        }
        if refresh {
            self.refresh_shadows(ui.ctx());
        }
    }

    fn shadow_details(&mut self, ui: &mut egui::Ui) {
        let p = Palette::of(ui);
        let live = self.shadows_live();
        let (Some(shadows), Some(name)) = (&self.shadow.shadows, self.shadow.selected.clone())
        else {
            theme::hint(ui, "Choose a shadow.");
            return;
        };
        let Some(sh) = shadows.items.get(&name).cloned() else {
            theme::hint(ui, "Choose a shadow.");
            return;
        };

        theme::card_title(ui, &format!("{} shadow", name.label()), |ui| {
            if let Some(ts) = sh
                .timestamp
                .and_then(|t| jiff::Timestamp::from_second(t).ok())
            {
                let local = ts.to_zoned_datetime(jiff::tz::TimeZone::system());
                theme::pill(ui, local.strftime("%Y-%m-%d %H:%M:%S").to_string(), p.muted);
            }
            if let Some(v) = sh.version {
                theme::pill(ui, format!("version {v}"), p.muted);
            }
        });
        match &sh.status {
            Status::Loading(_) => {
                ui.horizontal(|ui| {
                    ui.spinner();
                    theme::hint(ui, "Loading…");
                });
            }
            Status::Missing => theme::hint(
                ui,
                "This shadow doesn't exist yet. Send an update below to create it.",
            ),
            Status::Error(e) => {
                ui.label(
                    RichText::new(format!("{} {e}", icons::WARNING))
                        .small()
                        .color(p.err),
                );
            }
            Status::Loaded => {
                for (label, value) in [
                    ("Desired", sh.desired.as_ref()),
                    ("Reported", sh.reported.as_ref()),
                    ("Delta", sh.delta.as_ref()),
                ] {
                    ui.add_space(4.0);
                    theme::field_label(ui, label);
                    json_block(ui, label, value);
                }
            }
        }

        ui.add_space(10.0);
        theme::section_label(ui, "Update");
        theme::segmented(
            ui,
            &mut self.shadow.section,
            &[
                (Section::Desired, "Desired"),
                (Section::Reported, "Reported"),
            ],
            true,
        );
        let source = (name.clone(), self.shadow.section, sh.version);
        let same_target = self
            .shadow
            .editor_source
            .as_ref()
            .is_some_and(|(n, s, _)| *n == source.0 && *s == source.1);
        if !same_target
            || (!self.shadow.editor_dirty && self.shadow.editor_source != Some(source.clone()))
        {
            self.shadow.editor = editor_text(sh.section(self.shadow.section));
            self.shadow.editor_dirty = false;
            self.shadow.edit_error = None;
            self.shadow.editor_source = Some(source);
        }
        theme::inset(ui, |ui| {
            let edit = ui.add(
                egui::TextEdit::multiline(&mut self.shadow.editor)
                    .code_editor()
                    .frame(egui::Frame::new())
                    .desired_rows(8)
                    .desired_width(f32::INFINITY),
            );
            if edit.changed() {
                self.shadow.editor_dirty = true;
            }
        });
        theme::hint(
            ui,
            format!(
                "Publishes {{\"state\": {{\"{}\": …}}}} to {}. Use null to clear it.",
                self.shadow.section.key(),
                shadow::request_topic(&shadows.thing, &name, "update")
            ),
        );
        let mut send = false;
        ui.horizontal(|ui| {
            let button = theme::primary_button(ui, "Send update");
            if ui.add_enabled(live && !sh.updating, button).clicked() {
                send = true;
            }
            if theme::link_button(ui, "Reset").clicked() {
                self.shadow.editor_source = None;
            }
            if sh.updating {
                ui.spinner();
            }
        });
        for error in [self.shadow.edit_error.as_ref(), sh.update_error.as_ref()]
            .into_iter()
            .flatten()
        {
            ui.label(
                RichText::new(format!("{} {error}", icons::WARNING))
                    .small()
                    .color(p.err),
            );
        }
        if send {
            self.shadow_update(name);
        }
    }
}

/// Read-only, pretty-printed JSON, or "None".
fn json_block(ui: &mut egui::Ui, id: &str, value: Option<&Value>) {
    match value {
        None => theme::hint(ui, "None"),
        Some(v) => {
            theme::inset(ui, |ui| {
                egui::ScrollArea::vertical()
                    .id_salt(("shadow_json", id))
                    .max_height(180.0)
                    .auto_shrink([false, true])
                    .show(ui, |ui| {
                        ui.add(
                            egui::Label::new(
                                RichText::new(serde_json::to_string_pretty(v).unwrap_or_default())
                                    .monospace(),
                            )
                            .wrap(),
                        );
                    });
            });
        }
    }
}

/// The editor's starting text: the section's current value, or an empty object.
fn editor_text(value: Option<&Value>) -> String {
    match value {
        Some(v) => serde_json::to_string_pretty(v).unwrap_or_else(|_| "{}".into()),
        None => "{\n  \n}".into(),
    }
}
