// Copyright 2026 Nick Volgas
// SPDX-License-Identifier: Apache-2.0

//! Icon glyphs used in the UI.
//!
//! egui only ships these glyphs when the `monochrome_emoji_fonts` feature is enabled, and even
//! then not every symbol is covered. Use only these constants in UI text; the test below fails
//! if any of them would render as a missing-glyph box.

pub const REFRESH: &str = "🔄";
pub const ADD: &str = "➕";
pub const CHECK: &str = "✔";
pub const WARNING: &str = "⚠";
pub const CLOSE: &str = "❌";
pub const KEY: &str = "🔑";
pub const CLOUD: &str = "☁";
pub const FOLDER: &str = "📁";
pub const COPY: &str = "📋";
pub const SEARCH: &str = "🔍";
pub const SETTINGS: &str = "⚙";
pub const LOGS: &str = "📜";
pub const SEND: &str = "📤";
pub const PLUG: &str = "🔌";
pub const THEME_AUTO: &str = "🌓";
pub const THEME_LIGHT: &str = "☀";
pub const THEME_DARK: &str = "🌙";
/// Direction arrows: used only in monospace text (the proportional font lacks them).
pub const SENT: &str = "→";
pub const RECEIVED: &str = "←";

#[cfg(test)]
mod tests {
    use super::*;

    fn glyphs_present(family: egui::FontFamily, text: &str) -> bool {
        let ctx = egui::Context::default();
        let mut out = ctx.run_ui(Default::default(), |_| {});
        out.textures_delta.clear();
        let font = egui::FontId::new(14.0, family);
        ctx.fonts_mut(|f| text.chars().all(|c| f.has_glyph(&font, c)))
    }

    #[test]
    fn ui_icons_render_in_proportional_and_monospace() {
        for icon in [
            REFRESH,
            ADD,
            CHECK,
            WARNING,
            CLOSE,
            KEY,
            CLOUD,
            FOLDER,
            COPY,
            SEARCH,
            SETTINGS,
            LOGS,
            SEND,
            PLUG,
            THEME_AUTO,
            THEME_LIGHT,
            THEME_DARK,
        ] {
            assert!(
                glyphs_present(egui::FontFamily::Proportional, icon),
                "missing proportional glyph {icon}"
            );
            assert!(
                glyphs_present(egui::FontFamily::Monospace, icon),
                "missing monospace glyph {icon}"
            );
        }
    }

    #[test]
    fn arrows_render_in_monospace() {
        for icon in [SENT, RECEIVED] {
            assert!(
                glyphs_present(egui::FontFamily::Monospace, icon),
                "missing monospace glyph {icon}"
            );
        }
    }

    /// Catches stray symbols typed directly into UI strings instead of using the constants.
    #[test]
    fn app_strings_only_use_renderable_glyphs() {
        let source = [
            include_str!("app.rs"),
            include_str!("app/simulator.rs"),
            include_str!("theme.rs"),
        ]
        .concat();
        let source = source.as_str();
        let ctx = egui::Context::default();
        let mut out = ctx.run_ui(Default::default(), |_| {});
        out.textures_delta.clear();
        let font = egui::FontId::proportional(14.0);
        let missing: Vec<char> = source
            .chars()
            .filter(|c| !c.is_ascii())
            .filter(|c| !ctx.fonts_mut(|f| f.has_glyph(&font, *c)))
            .collect();
        assert!(
            missing.is_empty(),
            "UI source uses glyphs the UI font can't render: {missing:?}"
        );
    }
}
