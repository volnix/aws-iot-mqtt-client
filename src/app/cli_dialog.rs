// Copyright 2026 Nick Volgas
// SPDX-License-Identifier: Apache-2.0

//! "Copy as mosquitto_pub / mosquitto_sub", from a PUBLISH or SUBSCRIBE in the MQTT log:
//! shows the terminal command that repeats it with the connection's settings (see
//! [`crate::cli_export`]).

use egui::RichText;

use super::{modal_frame, to_send, IdentityMode, MqttApp};
use crate::cli_export::{self, Shell};
use crate::icons;
use crate::packet_log::Replay;
use crate::theme::{self, Palette};

pub(super) struct CliDialog {
    pub action: Replay,
    pub shell: Shell,
    /// The command was just copied; shows a confirmation on the button.
    pub copied: bool,
}

impl MqttApp {
    /// The connection the command should reproduce: the live one, or the one set up.
    fn cli_connection(&self) -> cli_export::Connection {
        let (endpoint, client_id) = self.connected_as.clone().unwrap_or_else(|| {
            (
                self.settings.endpoint.trim().to_owned(),
                self.effective_client_id().unwrap_or_default(),
            )
        });
        let path = |p: &Option<std::path::PathBuf>, placeholder: &str| {
            p.as_ref()
                .map(|p| p.display().to_string())
                .unwrap_or_else(|| placeholder.to_owned())
        };
        let s = &self.settings;
        cli_export::Connection {
            endpoint,
            port: s.port,
            client_id,
            cert_path: path(&s.cert_path, "certificate.pem.crt"),
            key_path: path(&s.key_path, "private.pem.key"),
            version: s.mqtt_version,
            clean_session: s.clean_session,
            session_expiry_secs: (s.session_expiry_secs > 0).then_some(s.session_expiry_secs),
            last_will: s.will_enabled.then(|| crate::mqtt::LastWill {
                topic: s.will_topic.trim().to_owned(),
                payload: s.will_payload.clone().into_bytes(),
                qos: s.will_qos,
                retain: s.will_retain,
                user_properties: to_send(&s.will_user_properties),
            }),
            user_properties: to_send(&s.connect_user_properties),
        }
    }

    pub(super) fn open_cli_dialog(&mut self, action: Replay) {
        self.cli = Some(CliDialog {
            action,
            shell: if cfg!(windows) {
                Shell::PowerShell
            } else {
                Shell::Posix
            },
            copied: false,
        });
    }

    pub(super) fn cli_command(&self, dialog: &CliDialog) -> String {
        let c = self.cli_connection();
        match &dialog.action {
            // The packet's own User Properties, as sent.
            Replay::Publish {
                topic,
                qos,
                retain,
                payload,
                user_properties,
            } => cli_export::mosquitto_pub(
                &c,
                topic,
                *qos,
                *retain,
                payload,
                user_properties,
                dialog.shell,
            ),
            Replay::Subscribe {
                filter,
                qos,
                user_properties,
            } => cli_export::mosquitto_sub(&c, filter, *qos, user_properties, dialog.shell),
        }
    }

    pub(super) fn cli_dialog(&mut self, ctx: &egui::Context) {
        let Some(dialog) = &self.cli else {
            return;
        };
        let command = self.cli_command(dialog);
        let connection = self.cli_connection();
        let thing_mode = self.settings.mode == IdentityMode::Thing;
        let connected = self.state.is_active();
        let Some(dialog) = &mut self.cli else {
            return;
        };
        let (program, what) = match &dialog.action {
            Replay::Publish { .. } => ("mosquitto_pub", "publish"),
            Replay::Subscribe { .. } => ("mosquitto_sub", "subscribe"),
        };
        let mut close = false;
        let response = egui::Modal::new(egui::Id::unique("cli_dialog"))
            .frame(modal_frame(ctx))
            .show(ctx, |ui| {
                let p = Palette::of(ui);
                ui.set_width(640.0);
                ui.horizontal(|ui| {
                    ui.label(RichText::new(format!("{program} command")).heading());
                    ui.with_layout(egui::Layout::right_to_left(egui::Align::Center), |ui| {
                        let before = dialog.shell;
                        theme::segmented_inline(
                            ui,
                            &mut dialog.shell,
                            &[
                                (Shell::Posix, "macOS / Linux"),
                                (Shell::PowerShell, "PowerShell"),
                            ],
                        );
                        if dialog.shell != before {
                            dialog.copied = false;
                        }
                    });
                });
                theme::hint(
                    ui,
                    format!(
                        "Repeats this {what} from a terminal with the same endpoint, client ID, \
                         and certificate files. The first line downloads Amazon Root CA 1."
                    ),
                );
                ui.add_space(6.0);
                theme::inset(ui, |ui| {
                    egui::ScrollArea::vertical()
                        .id_salt("cli_command")
                        .max_height(320.0)
                        .auto_shrink([false, true])
                        .show(ui, |ui| {
                            ui.add(egui::Label::new(RichText::new(&command).monospace()).wrap());
                        });
                });
                ui.add_space(6.0);

                let mut notes: Vec<String> = Vec::new();
                if connected || thing_mode {
                    notes.push(format!(
                        "AWS IoT allows one connection per client ID ({}). Running this while \
                         the app is connected as the same client disconnects the app.",
                        connection.client_id
                    ));
                }
                if !connection.endpoint.ends_with(".amazonaws.com") {
                    notes.push(
                        "For a custom domain whose server certificate isn't issued by Amazon, \
                         point --cafile at that certificate authority instead."
                            .into(),
                    );
                }
                if dialog.shell == Shell::PowerShell {
                    notes.push(
                        "Needs PowerShell 7.3 or later to pass JSON quotes correctly.".into(),
                    );
                }
                notes.push(format!(
                    "Install the Mosquitto clients from mosquitto.org (Homebrew: brew install \
                     mosquitto; Debian/Ubuntu: apt install mosquitto-clients) to run {program}."
                ));
                for note in notes {
                    ui.label(RichText::new(note).small().color(p.muted));
                }

                ui.add_space(8.0);
                ui.horizontal(|ui| {
                    let label = if dialog.copied {
                        format!("{} Copied", icons::CHECK)
                    } else {
                        format!("{} Copy", icons::COPY)
                    };
                    if ui.add(theme::primary_button(ui, label)).clicked() {
                        theme::copy_to_clipboard(ui.ctx(), command.clone(), "Command");
                        dialog.copied = true;
                    }
                    if ui.add(theme::ghost_button(ui, "Close")).clicked() {
                        close = true;
                    }
                });
            });
        if close || response.should_close() {
            self.cli = None;
        }
    }
}
