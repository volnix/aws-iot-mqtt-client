// Copyright 2026 Nick Volgas
// SPDX-License-Identifier: Apache-2.0

//! The Settings view: AWS profile and region, where certificates are saved, appearance (theme
//! and window transparency), and which logs are recorded.

use std::path::PathBuf;

use egui::RichText;

use super::{MqttApp, ThemeChoice};
use crate::aws;
use crate::provision;
use crate::theme::{self, Palette};

impl MqttApp {
    /// Where created certificates are saved: the chosen folder, or the app's cache folder.
    pub(super) fn certificate_dir(&self) -> PathBuf {
        self.settings
            .cert_dir
            .clone()
            .unwrap_or_else(provision::default_storage_root)
    }

    /// Either log is recorded, so the Logs drawer and its button are shown.
    pub(super) fn logging_enabled(&self) -> bool {
        self.settings.mqtt_logging || self.settings.api_logging
    }

    pub(super) fn settings_view(&mut self, ui: &mut egui::Ui) {
        let gap = theme::metrics(ui).gap;
        egui::ScrollArea::vertical()
            .id_salt("settings_scroll")
            .auto_shrink([false, false])
            .show(ui, |ui| {
                ui.set_max_width(720.0);
                ui.spacing_mut().item_spacing.y = gap;
                theme::card(ui, |ui| self.settings_account(ui));
                theme::card(ui, |ui| self.settings_certificates(ui));
                theme::card(ui, |ui| self.settings_appearance(ui));
                theme::card(ui, |ui| self.settings_logging(ui));
            });
    }

    fn settings_account(&mut self, ui: &mut egui::Ui) {
        theme::card_title(ui, "AWS account", |_| {});
        theme::hint(
            ui,
            "The credentials profile and region used to find Things and endpoints, and to create \
             certificates. Profiles come from ~/.aws/config and ~/.aws/credentials.",
        );
        ui.add_space(6.0);
        let before = (self.settings.profile.clone(), self.settings.region.clone());
        egui::Grid::new("settings_account")
            .num_columns(2)
            .spacing([16.0, 10.0])
            .show(ui, |ui| {
                theme::field_label(ui, "Profile");
                let label = if self.settings.profile.is_empty() {
                    "Default chain".to_owned()
                } else {
                    self.settings.profile.clone()
                };
                egui::ComboBox::from_id_salt("settings_profile")
                    .selected_text(label)
                    .width(280.0)
                    .show_ui(ui, |ui| {
                        ui.selectable_value(
                            &mut self.settings.profile,
                            String::new(),
                            "Default chain",
                        );
                        for profile in &self.profiles {
                            ui.selectable_value(
                                &mut self.settings.profile,
                                profile.clone(),
                                profile,
                            );
                        }
                    });
                ui.end_row();

                theme::field_label(ui, "Region");
                let label = if self.settings.region.is_empty() {
                    "From profile".to_owned()
                } else {
                    self.settings.region.clone()
                };
                egui::ComboBox::from_id_salt("settings_region")
                    .selected_text(label)
                    .width(280.0)
                    .show_ui(ui, |ui| {
                        ui.selectable_value(
                            &mut self.settings.region,
                            String::new(),
                            "From profile",
                        );
                        for region in aws::IOT_REGIONS {
                            ui.selectable_value(
                                &mut self.settings.region,
                                (*region).to_owned(),
                                *region,
                            );
                        }
                    });
                ui.end_row();
            });
        let changed = before != (self.settings.profile.clone(), self.settings.region.clone());
        ui.add_space(4.0);
        ui.horizontal(|ui| {
            let busy = self.aws.busy();
            let label = if self.aws.session.is_some() {
                "Reload account"
            } else {
                "Load account"
            };
            if ui
                .add_enabled(!busy, theme::ghost_button(ui, label))
                .clicked()
                || (changed && self.aws.session.is_some() && !self.state.is_active())
            {
                self.load_from_aws(ui.ctx());
            }
            if busy {
                ui.spinner();
            }
            if self.state.is_active() && changed {
                theme::hint(ui, "Reload after disconnecting to switch accounts.");
            }
        });
        if self.aws.error.is_some() {
            super::show_error(ui, self.aws.error.as_ref());
        }
    }

    fn settings_certificates(&mut self, ui: &mut egui::Ui) {
        let p = Palette::of(ui);
        theme::card_title(ui, "Certificates", |_| {});
        theme::hint(
            ui,
            "Where certificates and keys created by the app are saved. Each one gets its own \
             folder, readable only by you on macOS and Linux.",
        );
        ui.add_space(6.0);
        let dir = self.certificate_dir();
        ui.horizontal(|ui| {
            ui.label(RichText::new(dir.display().to_string()).monospace().small());
            if self.settings.cert_dir.is_none() {
                theme::pill(ui, "Default", p.muted);
            }
        });
        ui.horizontal(|ui| {
            if ui.add(theme::ghost_button(ui, "Choose folder…")).clicked() {
                if let Some(folder) = pick_folder(&dir) {
                    self.settings.cert_dir = Some(folder);
                }
            }
            if self.settings.cert_dir.is_some() && theme::link_button(ui, "Use default").clicked() {
                self.settings.cert_dir = None;
            }
        });
    }

    fn settings_appearance(&mut self, ui: &mut egui::Ui) {
        theme::card_title(ui, "Appearance", |_| {});
        ui.add_space(4.0);
        egui::Grid::new("settings_appearance")
            .num_columns(2)
            .spacing([16.0, 12.0])
            .show(ui, |ui| {
                theme::field_label(ui, "Theme");
                let before = self.settings.theme;
                theme::segmented_inline(
                    ui,
                    &mut self.settings.theme,
                    &[
                        (ThemeChoice::System, "Auto"),
                        (ThemeChoice::Light, "Light"),
                        (ThemeChoice::Dark, "Dark"),
                    ],
                );
                if self.settings.theme != before {
                    ui.ctx().set_theme(self.settings.theme.preference());
                }
                ui.end_row();

                theme::field_label(ui, "Window transparency");
                ui.vertical(|ui| {
                    // Shown as transparency (0% = solid), stored as opacity.
                    let mut transparency = ((1.0 - self.settings.window_opacity) * 100.0).round();
                    ui.spacing_mut().slider_width = 260.0;
                    let slider = egui::Slider::new(&mut transparency, 0.0..=100.0)
                        .suffix("%")
                        .step_by(5.0)
                        .fixed_decimals(0);
                    if ui.add(slider).changed() {
                        self.settings.window_opacity = 1.0 - transparency / 100.0;
                    }
                    theme::hint(ui, theme::TRANSPARENCY_HINT);
                });
                ui.end_row();
            });
    }

    fn settings_logging(&mut self, ui: &mut egui::Ui) {
        theme::card_title(ui, "Logging", |_| {});
        theme::hint(
            ui,
            "Logs stay in memory only and are never written to disk unless you export them. \
             Turning a log off clears it and stops recording; nothing is kept while it's off.",
        );
        ui.add_space(6.0);
        if ui
            .checkbox(&mut self.settings.mqtt_logging, "Record MQTT packets")
            .on_hover_text("Every packet sent and received, with topics and payloads")
            .changed()
            && !self.settings.mqtt_logging
        {
            self.log.clear();
        }
        if ui
            .checkbox(&mut self.settings.api_logging, "Record AWS API calls")
            .on_hover_text("Each AWS call, with timing and request IDs. Secrets are never logged.")
            .changed()
            && !self.settings.api_logging
        {
            self.api_log.clear();
        }
        if !self.logging_enabled() {
            theme::hint(ui, "Both logs are off, so the Logs button is hidden.");
        } else if !self.settings.mqtt_logging {
            theme::hint(
                ui,
                "Copying commands and exporting traces come from the MQTT log, so they're \
                 unavailable while it's off.",
            );
        }
    }
}

fn pick_folder(current: &std::path::Path) -> Option<PathBuf> {
    #[cfg(test)]
    {
        let _ = current;
        None
    }
    #[cfg(not(test))]
    {
        let mut dialog = rfd::FileDialog::new().set_title("Folder for new certificates");
        if current.is_dir() {
            dialog = dialog.set_directory(current);
        }
        dialog.pick_folder()
    }
}
