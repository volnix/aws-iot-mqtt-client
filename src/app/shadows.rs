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
use crate::shadow_fields::{self as form, Field, FieldValue, Kind};
use crate::theme::{self, Palette};

/// How the update is edited: typed fields, or raw JSON.
#[derive(Debug, Clone, Copy, PartialEq, Eq)]
pub(super) enum EditMode {
    Fields,
    Json,
}

/// Shadows of the Thing the client is (or was last) connected as.
pub(super) struct ShadowState {
    pub shadows: Option<Shadows>,
    /// The connection the shadows belong to; responses from older connections are ignored.
    pub connection_id: Option<u64>,
    /// The shadow-topic subscription was acknowledged, so `get`s will be answered.
    pub subscribed: bool,
    /// Packet ID of the pending shadow-topic SUBSCRIBE, to recognize its SUBACK.
    sub_pkid: Option<u16>,
    /// Requests timed out and the subscription was renewed; don't renew again until a
    /// response arrives (avoids retrying forever when the broker never answers).
    renewed: bool,
    /// How long to wait for a shadow response ([`shadow::RESPONSE_TIMEOUT`]; shorter in tests).
    pub timeout: std::time::Duration,
    /// `get`s waiting for the subscription to be acknowledged.
    pending: Vec<ShadowName>,
    pub listing: bool,
    pub selected: Option<ShadowName>,
    pub section: Section,
    pub mode: EditMode,
    /// The form editor's fields (`Fields` mode).
    pub fields: Vec<Field>,
    /// The JSON editor's text (`Json` mode).
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
            sub_pkid: None,
            renewed: false,
            timeout: shadow::RESPONSE_TIMEOUT,
            pending: Vec::new(),
            listing: false,
            selected: None,
            section: Section::Desired,
            mode: EditMode::Fields,
            fields: Vec::new(),
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
        let subscribe = conn.subscribe(&shadow::subscription_filter(&thing), 1, &[]);
        self.shadow = ShadowState {
            shadows: Some(Shadows::new(thing.clone())),
            connection_id: Some(connection_id),
            selected: Some(ShadowName::Classic),
            timeout: self.shadow.timeout,
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

    /// The shadow-topic filter for the current shadows, if any.
    pub(super) fn shadow_filter(&self) -> Option<String> {
        self.shadow
            .shadows
            .as_ref()
            .map(|s| shadow::subscription_filter(&s.thing))
    }

    /// Tracks the shadow subscription's SUBSCRIBE and SUBACK by packet ID.
    pub(super) fn shadow_packet(&mut self, entry: &crate::packet_log::PacketLogEntry) {
        use crate::packet_log::{Direction, Replay};
        let pkid = packet_id(&entry.details);
        match (entry.direction, entry.kind.as_str()) {
            (Direction::Sent, "SUBSCRIBE") => {
                if let Some(Replay::Subscribe { filter, .. }) = &entry.replay {
                    if Some(filter) == self.shadow_filter().as_ref() {
                        self.shadow.sub_pkid = pkid;
                    }
                }
            }
            (Direction::Received, "SUBACK") if pkid.is_some() && pkid == self.shadow.sub_pkid => {
                self.shadow.sub_pkid = None;
                self.shadow_suback(&entry.details);
            }
            _ => {}
        }
    }

    /// Subscribes to the shadow topics again. `get`s wait until it's acknowledged.
    fn renew_shadow_subscription(&mut self) {
        let (Some(filter), Some(conn)) = (self.shadow_filter(), &self.connection) else {
            return;
        };
        self.shadow.subscribed = false;
        if let Err(e) = conn.subscribe(&filter, 1, &[]) {
            if let Some(s) = &mut self.shadow.shadows {
                s.list_error = Some(e.summary());
            }
        }
    }

    /// The SUBACK for the shadow-topic subscription.
    fn shadow_suback(&mut self, details: &str) {
        if self.shadow.shadows.is_none() {
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
            if let Err(e) = conn.publish(&topic, 1, false, b"{}".to_vec(), &[]) {
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
            Applied::Handled => {
                self.shadow.renewed = false;
                true
            }
            Applied::Refresh(name) => {
                self.shadow.renewed = false;
                self.shadow_get(name);
                true
            }
        }
    }

    /// Times out unanswered shadow requests. The first time that happens, the shadow
    /// subscription may be gone (for example, unsubscribed from the Subscriptions list), so
    /// subscribe again and reload every shadow once.
    pub(super) fn expire_shadow_requests(&mut self) {
        let live = self.shadows_live();
        let Some(s) = &mut self.shadow.shadows else {
            return;
        };
        let expired = s.expire(Instant::now(), self.shadow.timeout);
        if expired == 0 || self.shadow.renewed || !live {
            return;
        }
        let names: Vec<ShadowName> = s.items.keys().cloned().collect();
        self.shadow.renewed = true;
        self.renew_shadow_subscription();
        for name in names {
            self.shadow_get(name);
        }
    }

    fn shadow_update(&mut self, name: ShadowName) {
        self.shadow.edit_error = None;
        let text = match self.shadow.mode {
            EditMode::Json => self.shadow.editor.clone(),
            EditMode::Fields => match form::to_value(&self.shadow.fields) {
                Ok(v) if v.as_object().is_some_and(|m| m.is_empty()) => {
                    self.shadow.edit_error = Some("Add a field to send.".into());
                    return;
                }
                Ok(v) => v.to_string(),
                Err(errors) => {
                    self.shadow.edit_error = Some(errors.join("\n"));
                    return;
                }
            },
        };
        let payload = match shadow::update_payload(self.shadow.section, &text) {
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
        match conn.publish(&topic, 1, false, payload, &[]) {
            Ok(()) => {
                entry.start_update(Instant::now());
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

    /// Switches between the form and JSON, carrying the edits over. Stays put (with an
    /// explanation) when the current edits can't be converted.
    fn switch_editor_mode(&mut self, mode: EditMode, shadow_value: Option<&Value>) {
        let s = &mut self.shadow;
        match mode {
            EditMode::Json => match form::to_value(&s.fields) {
                Ok(v) => {
                    s.editor = serde_json::to_string_pretty(&v).unwrap_or_default();
                    s.edit_error = None;
                    s.mode = mode;
                }
                Err(errors) => {
                    s.edit_error = Some(format!(
                        "Fix these before switching to JSON:\n{}",
                        errors.join("\n")
                    ));
                }
            },
            EditMode::Fields => match serde_json::from_str::<Value>(&s.editor) {
                Ok(v @ Value::Object(_)) => {
                    s.fields = form::fields_against(Some(&v), shadow_value);
                    s.edit_error = None;
                    s.mode = mode;
                }
                Ok(Value::Null) => {
                    s.fields.clear();
                    s.edit_error = None;
                    s.mode = mode;
                }
                _ => {
                    s.edit_error =
                        Some("To edit as fields, the JSON needs to be a valid object.".into());
                }
            },
        }
    }

    pub(super) fn shadows_view(&mut self, ui: &mut egui::Ui) {
        if self.shadow.shadows.is_none() {
            ui.vertical_centered(|ui| {
                ui.set_max_width(560.0);
                theme::card(ui, |ui| self.shadow_connect_pane(ui));
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

    /// A short connect form for someone who just wants to see a Thing's shadows: account,
    /// Thing, certificate, and Connect. Everything else uses the Client tab's settings.
    fn shadow_connect_pane(&mut self, ui: &mut egui::Ui) {
        let gap = theme::metrics(ui).gap;
        ui.with_layout(egui::Layout::top_down(egui::Align::Min), |ui| {
            theme::card_title(ui, "Connect to see shadows", |_| {});
            theme::hint(
                ui,
                "Shadows belong to an IoT Thing. Choose the Thing and its certificate, then \
                 connect. Its classic and named shadows load automatically.",
            );
            ui.add_space(gap);

            if self.state.is_active() && self.settings.mode == IdentityMode::ClientId {
                theme::hint(
                    ui,
                    "You're connected with a client ID, which has no shadows. Disconnect on \
                     the Client tab to connect as a Thing.",
                );
                return;
            }
            if self.settings.mode == IdentityMode::ClientId {
                ui.horizontal(|ui| {
                    theme::hint(ui, "The Client tab is set to connect with a client ID.");
                    if theme::link_button(ui, "Use an IoT Thing").clicked() {
                        self.settings.mode = IdentityMode::Thing;
                    }
                });
                return;
            }

            ui.add_enabled_ui(!self.state.is_active(), |ui| {
                theme::section_label(ui, "AWS account");
                self.account_card(ui);
                ui.add_space(gap);
                theme::section_label(ui, "Thing");
                self.things_section(ui);
                ui.add_space(gap);
                theme::section_label(ui, "Certificate");
                egui::Grid::new("shadow_conn_grid")
                    .num_columns(2)
                    .spacing([16.0, 10.0])
                    .show(ui, |ui| {
                        // The endpoint fills in when the account loads; only ask when it hasn't.
                        if self.settings.endpoint.trim().is_empty() {
                            theme::field_label(ui, "Endpoint");
                            self.endpoint_picker(ui);
                            ui.end_row();
                        }
                        self.certificate_fields(ui);
                    });
            });
            ui.add_space(gap);
            self.connect_actions(ui);
            if self.connection_error.is_some() {
                ui.add_space(4.0);
                super::show_error(ui, self.connection_error.as_ref());
            }
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
        let existing = self.shadow.shadows.as_ref().map_or(0, Shadows::existing);
        if existing > 0 {
            theme::count_caption(ui, existing, "active");
        }
        if !live && self.state != ConnectionState::Connected {
            // Reconnect right here, with the same Thing and certificate.
            self.connect_actions(ui);
            ui.add_space(4.0);
        }
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
        let topic = shadow::request_topic(&shadows.thing, &name, "update");

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
        let mut mode = self.shadow.mode;
        ui.horizontal(|ui| {
            theme::section_label(ui, "Update");
            ui.with_layout(egui::Layout::right_to_left(egui::Align::Center), |ui| {
                theme::segmented_inline(
                    ui,
                    &mut mode,
                    &[(EditMode::Fields, "Fields"), (EditMode::Json, "JSON")],
                );
            });
        });
        if mode != self.shadow.mode {
            self.switch_editor_mode(mode, sh.section(self.shadow.section));
        }
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
            self.shadow.fields = form::fields_from(sh.section(self.shadow.section));
            self.shadow.editor_dirty = false;
            self.shadow.edit_error = None;
            self.shadow.editor_source = Some(source);
        }
        let form_errors = match self.shadow.mode {
            EditMode::Fields => {
                let mut changed = false;
                theme::inset(ui, |ui| {
                    if self.shadow.fields.is_empty() {
                        theme::hint(ui, "No fields yet.");
                    }
                    field_rows(ui, &mut self.shadow.fields, &mut changed);
                });
                if changed {
                    self.shadow.editor_dirty = true;
                }
                theme::hint(
                    ui,
                    format!(
                        "Sends these fields as the {} state to {topic}. Fields you leave out \
                         keep their current values.",
                        self.shadow.section.key()
                    ),
                );
                form::to_value(&self.shadow.fields)
                    .err()
                    .unwrap_or_default()
            }
            EditMode::Json => {
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
                        "Publishes {{\"state\": {{\"{}\": …}}}} to {topic}. Use null to clear it.",
                        self.shadow.section.key()
                    ),
                );
                Vec::new()
            }
        };
        for error in &form_errors {
            ui.label(RichText::new(error).small().color(p.err));
        }
        let mut send = false;
        ui.horizontal(|ui| {
            let button = theme::primary_button(ui, "Send update");
            if ui
                .add_enabled(live && !sh.updating && form_errors.is_empty(), button)
                .clicked()
            {
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

/// One row per field: name, type, value, and a remove button. Groups nest below their row.
fn field_rows(ui: &mut egui::Ui, fields: &mut Vec<Field>, changed: &mut bool) {
    let p = Palette::of(ui);
    let mut dropped = None;
    for (index, field) in fields.iter_mut().enumerate() {
        ui.horizontal(|ui| {
            let key_width = 150.0;
            if field.existing {
                // An update can't rename a field, so names from the shadow are fixed.
                ui.add_sized(
                    [key_width, 20.0],
                    egui::Label::new(RichText::new(&field.key).monospace()).truncate(),
                );
            } else {
                let key = egui::TextEdit::singleline(&mut field.key)
                    .id_salt(("field_key", field.id))
                    .hint_text("name")
                    .desired_width(key_width);
                *changed |= ui.add(key).changed();
            }

            if field.is_removal() {
                theme::hint(ui, "Removed when you send");
                if theme::link_button(ui, "Undo").clicked() {
                    field.undo_remove();
                    *changed = true;
                }
                return;
            }

            let mut kind = field.value.kind();
            egui::ComboBox::from_id_salt(("field_kind", field.id))
                .width(100.0)
                .selected_text(kind.label())
                .show_ui(ui, |ui| {
                    for k in Kind::ALL {
                        ui.selectable_value(&mut kind, k, k.label());
                    }
                });
            if kind != field.value.kind() {
                field.value = field.value.convert(kind);
                *changed = true;
            }

            let width = (ui.available_width() - 34.0).max(80.0);
            let id = field.id;
            *changed |= match &mut field.value {
                FieldValue::Text(s) => ui
                    .add(
                        egui::TextEdit::singleline(s)
                            .id_salt(("field_value", id))
                            .hint_text("text")
                            .desired_width(width),
                    )
                    .changed(),
                FieldValue::Number(s) => {
                    let valid = form::is_valid_number(s);
                    let mut edit = egui::TextEdit::singleline(s)
                        .id_salt(("field_value", id))
                        .hint_text("0")
                        .desired_width(width);
                    if !valid {
                        edit = edit.text_color(p.err);
                    }
                    ui.add(edit).changed()
                }
                FieldValue::Bool(b) => {
                    let before = *b;
                    ui.radio_value(b, true, "True");
                    ui.radio_value(b, false, "False");
                    // Keep the remove button at the row's end, like the other types.
                    ui.add_space((width - 140.0).max(0.0));
                    *b != before
                }
                FieldValue::List(s) => {
                    let valid = form::is_valid_list(s);
                    let mut edit = egui::TextEdit::singleline(s)
                        .id_salt(("field_value", id))
                        .code_editor()
                        .hint_text("[1, 2, 3]")
                        .desired_width(width);
                    if !valid {
                        edit = edit.text_color(p.err);
                    }
                    ui.add(edit).changed()
                }
                FieldValue::Group(children) => {
                    let n = children.len();
                    theme::hint(ui, format!("{n} field{}", if n == 1 { "" } else { "s" }));
                    false
                }
                FieldValue::Null => {
                    theme::hint(ui, "null");
                    false
                }
            };

            if theme::icon_button(ui, icons::CLOSE, "Remove field", true).clicked() {
                if !field.remove() {
                    dropped = Some(index);
                }
                *changed = true;
            }
        });
        if let (FieldValue::Group(children), false) = (&mut field.value, field.removed.is_some()) {
            ui.indent(("field_group", field.id), |ui| {
                field_rows(ui, children, changed);
            });
        }
    }
    if let Some(index) = dropped {
        fields.remove(index);
    }
    if theme::link_button(ui, &format!("{} Add field", icons::ADD)).clicked() {
        fields.push(Field::new_empty());
        *changed = true;
    }
}

/// The packet ID from log details such as `pkid=3 codes=[...]`.
fn packet_id(details: &str) -> Option<u16> {
    let digits: String = details
        .strip_prefix("pkid=")?
        .chars()
        .take_while(char::is_ascii_digit)
        .collect();
    digits.parse().ok()
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
