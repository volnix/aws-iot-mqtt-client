// Copyright Amazon.com, Inc. or its affiliates. All Rights Reserved.
// SPDX-License-Identifier: Apache-2.0

//! Glass theme: design tokens, egui style, the painted background, and the shared widgets the
//! UI is built from (glass cards, insets, banners, pills, disclosures, segmented controls).
//!
//! Every color and size comes from `assets/theme.json`. Release builds compile that file in;
//! debug builds also watch it and re-apply changes live (see [`ThemeWatcher`]), so the design
//! can be tuned without rebuilding. Widgets never hard-code colors, so light and dark stay
//! consistent, and the contrast tests at the bottom run against the real token file.

use std::path::PathBuf;
use std::sync::{Arc, OnceLock};
use std::time::{Duration, Instant, SystemTime};

use egui::epaint::{Mesh, Shadow};
use egui::{
    Color32, CornerRadius, FontFamily, FontId, Margin, Pos2, Rect, RichText, Shape, Stroke,
    TextStyle, Theme, Vec2,
};
use serde::{Deserialize, Deserializer, Serialize, Serializer};

use crate::{fonts, icons};

/// The token file compiled into every build.
pub const DEFAULT_TOKENS_JSON: &str = include_str!("../assets/theme.json");

// ---------------------------------------------------------------------------------------------
// Tokens
// ---------------------------------------------------------------------------------------------

/// A color written as `#RRGGBB` or `#RRGGBBAA` (unmultiplied alpha) in the token file.
#[derive(Debug, Clone, Copy, PartialEq, Eq)]
pub struct Hex(pub Color32);

impl Hex {
    pub fn parse(s: &str) -> Result<Self, String> {
        let h = s
            .strip_prefix('#')
            .ok_or_else(|| format!("{s:?}: expected #RRGGBB[AA]"))?;
        let byte = |i: usize| {
            u8::from_str_radix(h.get(i..i + 2).unwrap_or("zz"), 16)
                .map_err(|_| format!("{s:?}: invalid hex color"))
        };
        match h.len() {
            6 => Ok(Hex(Color32::from_rgb(byte(0)?, byte(2)?, byte(4)?))),
            8 => Ok(Hex(Color32::from_rgba_unmultiplied(
                byte(0)?,
                byte(2)?,
                byte(4)?,
                byte(6)?,
            ))),
            _ => Err(format!("{s:?}: expected #RRGGBB or #RRGGBBAA")),
        }
    }
}

impl Serialize for Hex {
    fn serialize<S: Serializer>(&self, serializer: S) -> Result<S::Ok, S::Error> {
        let [r, g, b, a] = self.0.to_srgba_unmultiplied();
        let text = if a == 255 {
            format!("#{r:02X}{g:02X}{b:02X}")
        } else {
            format!("#{r:02X}{g:02X}{b:02X}{a:02X}")
        };
        serializer.serialize_str(&text)
    }
}

impl<'de> Deserialize<'de> for Hex {
    fn deserialize<D: Deserializer<'de>>(deserializer: D) -> Result<Self, D::Error> {
        let s = String::deserialize(deserializer)?;
        Hex::parse(&s).map_err(serde::de::Error::custom)
    }
}

/// A soft radial light painted behind the glass. Position and radius are fractions of the
/// window (radius relative to the larger side).
#[derive(Debug, Clone, Copy, PartialEq, Serialize, Deserialize)]
pub struct Orb {
    pub x: f32,
    pub y: f32,
    pub radius: f32,
    pub color: Hex,
}

/// Colors for one theme, as written in the token file.
#[derive(Debug, Clone, PartialEq, Serialize, Deserialize)]
pub struct ThemeColors {
    pub background_top: Hex,
    pub background_bottom: Hex,
    pub orbs: Vec<Orb>,
    /// Card and panel fill: translucent, so the background shows through.
    pub glass: Hex,
    /// Denser glass for dialogs and popups that sit over other content.
    pub glass_strong: Hex,
    pub glass_border: Hex,
    pub shadow: Hex,
    /// A tint laid over glass for inputs, payloads, and grouped details.
    pub inset: Hex,
    pub border_strong: Hex,
    pub text: Hex,
    pub muted: Hex,
    pub accent: Hex,
    pub accent_hover: Hex,
    pub accent_text: Hex,
    pub ok: Hex,
    pub warn: Hex,
    pub err: Hex,
    pub info: Hex,
}

/// Sizes shared by both themes.
#[derive(Debug, Clone, Copy, PartialEq, Serialize, Deserialize)]
pub struct Metrics {
    pub card_radius: u8,
    pub control_radius: u8,
    pub card_padding: i8,
    pub gap: f32,
    pub shadow_blur: u8,
    pub shadow_offset_y: i8,
    pub font_heading: f32,
    pub font_body: f32,
    pub font_button: f32,
    pub font_small: f32,
    pub font_mono: f32,
}

/// The whole token file.
#[derive(Debug, Clone, PartialEq, Serialize, Deserialize)]
pub struct Tokens {
    pub metrics: Metrics,
    pub light: ThemeColors,
    pub dark: ThemeColors,
}

impl Tokens {
    pub fn parse(json: &str) -> Result<Self, String> {
        let tokens: Tokens = serde_json::from_str(json).map_err(|e| e.to_string())?;
        tokens.validate()?;
        Ok(tokens)
    }

    /// The compiled-in tokens. The token file is checked by unit tests, so this can't fail.
    pub fn builtin() -> Arc<Tokens> {
        static BUILTIN: OnceLock<Arc<Tokens>> = OnceLock::new();
        BUILTIN
            .get_or_init(|| {
                Arc::new(Tokens::parse(DEFAULT_TOKENS_JSON).expect("assets/theme.json is valid"))
            })
            .clone()
    }

    pub fn colors(&self, theme: Theme) -> &ThemeColors {
        match theme {
            Theme::Dark => &self.dark,
            Theme::Light => &self.light,
        }
    }

    /// Rejects values that would break layout or rendering.
    fn validate(&self) -> Result<(), String> {
        let m = &self.metrics;
        for (name, size) in [
            ("font_heading", m.font_heading),
            ("font_body", m.font_body),
            ("font_button", m.font_button),
            ("font_small", m.font_small),
            ("font_mono", m.font_mono),
        ] {
            if !(6.0..=48.0).contains(&size) {
                return Err(format!("metrics.{name} must be between 6 and 48"));
            }
        }
        if !(0.0..=64.0).contains(&m.gap) {
            return Err("metrics.gap must be between 0 and 64".into());
        }
        for (theme, colors) in [("light", &self.light), ("dark", &self.dark)] {
            for (i, orb) in colors.orbs.iter().enumerate() {
                if !(0.0..=3.0).contains(&orb.radius) {
                    return Err(format!("{theme}.orbs[{i}].radius must be between 0 and 3"));
                }
            }
        }
        Ok(())
    }
}

/// Named colors for the current theme, as used by widget code.
#[derive(Debug, Clone, Copy, PartialEq, Eq)]
pub struct Palette {
    pub glass: Color32,
    pub glass_strong: Color32,
    pub glass_border: Color32,
    pub shadow: Color32,
    pub inset: Color32,
    pub border_strong: Color32,
    pub text: Color32,
    pub muted: Color32,
    pub accent: Color32,
    pub accent_hover: Color32,
    pub accent_text: Color32,
    pub ok: Color32,
    pub warn: Color32,
    pub err: Color32,
    pub info: Color32,
}

impl From<&ThemeColors> for Palette {
    fn from(c: &ThemeColors) -> Self {
        Self {
            glass: c.glass.0,
            glass_strong: c.glass_strong.0,
            glass_border: c.glass_border.0,
            shadow: c.shadow.0,
            inset: c.inset.0,
            border_strong: c.border_strong.0,
            text: c.text.0,
            muted: c.muted.0,
            accent: c.accent.0,
            accent_hover: c.accent_hover.0,
            accent_text: c.accent_text.0,
            ok: c.ok.0,
            warn: c.warn.0,
            err: c.err.0,
            info: c.info.0,
        }
    }
}

impl Palette {
    /// The palette matching the UI's current theme.
    pub fn of(ui: &egui::Ui) -> Self {
        let theme = Theme::from_dark_mode(ui.visuals().dark_mode);
        Self::from(tokens(ui.ctx()).colors(theme))
    }
}

/// Sizes from the active tokens.
pub fn metrics(ui: &egui::Ui) -> Metrics {
    tokens(ui.ctx()).metrics
}

fn tokens_id() -> egui::Id {
    egui::Id::unique("aws-iot-mqtt-client/theme-tokens")
}

/// The tokens currently applied to `ctx` (the built-in ones until [`apply`] is called).
pub fn tokens(ctx: &egui::Context) -> Arc<Tokens> {
    ctx.data(|d| d.get_temp::<Arc<Tokens>>(tokens_id()))
        .unwrap_or_else(Tokens::builtin)
}

/// Applies tokens to both themes and remembers them for widget code.
pub fn apply(ctx: &egui::Context, tokens: Arc<Tokens>) {
    for theme in [Theme::Dark, Theme::Light] {
        ctx.set_style_of(theme, style_for(theme, &tokens));
    }
    ctx.data_mut(|d| d.insert_temp(tokens_id(), tokens));
    ctx.request_repaint();
}

/// Installs the built-in tokens.
pub fn install(ctx: &egui::Context) {
    apply(ctx, Tokens::builtin());
}

fn style_for(theme: Theme, tokens: &Tokens) -> egui::Style {
    let p = Palette::from(tokens.colors(theme));
    let m = tokens.metrics;
    let mut style = theme.default_style();

    style.text_styles = [
        (
            TextStyle::Heading,
            FontId::new(m.font_heading, FontFamily::Name(fonts::SEMIBOLD.into())),
        ),
        (TextStyle::Body, FontId::proportional(m.font_body)),
        (TextStyle::Button, FontId::proportional(m.font_button)),
        (TextStyle::Small, FontId::proportional(m.font_small)),
        (TextStyle::Monospace, FontId::monospace(m.font_mono)),
    ]
    .into();

    let s = &mut style.spacing;
    s.item_spacing = Vec2::new(8.0, 8.0);
    s.button_padding = Vec2::new(12.0, 5.0);
    s.interact_size.y = 28.0;
    s.window_margin = Margin::same(m.card_padding);
    s.menu_margin = Margin::same(6);
    s.indent = 16.0;
    s.scroll.bar_width = 8.0;
    s.scroll.floating = true;

    let control = CornerRadius::same(m.control_radius);
    let v = &mut style.visuals;
    // Panels are transparent so the painted background shows through the glass.
    v.panel_fill = Color32::TRANSPARENT;
    v.window_fill = p.glass_strong;
    v.window_stroke = Stroke::new(1.0, p.glass_border);
    v.window_corner_radius = CornerRadius::same(m.card_radius);
    v.window_shadow = Shadow {
        offset: [0, m.shadow_offset_y],
        blur: m.shadow_blur,
        spread: 0,
        color: p.shadow,
    };
    v.popup_shadow = Shadow {
        offset: [0, 6],
        blur: 20,
        spread: 0,
        color: p.shadow,
    };
    v.menu_corner_radius = control;
    v.faint_bg_color = p.inset;
    v.extreme_bg_color = p.inset;
    v.text_edit_bg_color = Some(p.inset);
    v.code_bg_color = p.inset;
    v.warn_fg_color = p.warn;
    v.error_fg_color = p.err;
    v.hyperlink_color = p.accent;
    v.selection.bg_fill = p.accent.gamma_multiply(0.25);
    v.selection.stroke = Stroke::new(1.0, p.accent);
    v.button_frame = true;
    v.collapsing_header_frame = false;
    v.indent_has_left_vline = false;
    v.striped = false;

    let w = &mut v.widgets;
    for state in [
        &mut w.noninteractive,
        &mut w.inactive,
        &mut w.hovered,
        &mut w.active,
        &mut w.open,
    ] {
        state.corner_radius = control;
        state.expansion = 0.0;
        state.fg_stroke = Stroke::new(1.0, p.text);
    }
    w.noninteractive.bg_fill = Color32::TRANSPARENT;
    w.noninteractive.weak_bg_fill = Color32::TRANSPARENT;
    w.noninteractive.bg_stroke = Stroke::new(1.0, p.glass_border);

    w.inactive.bg_fill = p.inset;
    w.inactive.weak_bg_fill = p.inset;
    w.inactive.bg_stroke = Stroke::NONE;

    w.hovered.bg_fill = p.inset.gamma_multiply(1.8);
    w.hovered.weak_bg_fill = p.inset.gamma_multiply(1.8);
    w.hovered.bg_stroke = Stroke::new(1.0, p.border_strong);

    w.active.bg_fill = p.inset.gamma_multiply(2.2);
    w.active.weak_bg_fill = p.inset.gamma_multiply(2.2);
    w.active.bg_stroke = Stroke::new(1.0, p.accent);

    w.open.bg_fill = p.inset.gamma_multiply(1.8);
    w.open.weak_bg_fill = p.inset.gamma_multiply(1.8);
    w.open.bg_stroke = Stroke::new(1.0, p.border_strong);

    style
}

// ---------------------------------------------------------------------------------------------
// Live reload (development)
// ---------------------------------------------------------------------------------------------

/// Watches a token file and re-applies it when it changes.
///
/// Active in debug builds (watching `assets/theme.json` in the source tree), or in any build
/// when `AWS_IOT_MQTT_THEME_FILE` points at a token file.
pub struct ThemeWatcher {
    path: PathBuf,
    modified: Option<SystemTime>,
    last_check: Instant,
    /// The last parse error, shown in the UI until the file is fixed.
    pub error: Option<String>,
}

impl ThemeWatcher {
    const POLL: Duration = Duration::from_millis(400);

    /// The watcher for this build, if live reload is enabled.
    pub fn for_build() -> Option<Self> {
        if let Some(path) = std::env::var_os("AWS_IOT_MQTT_THEME_FILE") {
            return Some(Self::new(PathBuf::from(path)));
        }
        if cfg!(debug_assertions) {
            let path = PathBuf::from(concat!(env!("CARGO_MANIFEST_DIR"), "/assets/theme.json"));
            if path.exists() {
                return Some(Self::new(path));
            }
        }
        None
    }

    pub fn new(path: PathBuf) -> Self {
        Self {
            path,
            modified: None,
            last_check: Instant::now() - Self::POLL,
            error: None,
        }
    }

    pub fn path(&self) -> &std::path::Path {
        &self.path
    }

    /// Checks the file (at most every 400 ms) and applies it if it changed.
    /// Returns true when new tokens were applied.
    pub fn poll(&mut self, ctx: &egui::Context) -> bool {
        // Keep polling while idle, so a save is picked up without moving the mouse.
        ctx.request_repaint_after(Self::POLL);
        if self.last_check.elapsed() < Self::POLL {
            return false;
        }
        self.last_check = Instant::now();
        let modified = std::fs::metadata(&self.path)
            .and_then(|m| m.modified())
            .ok();
        if modified.is_none() || modified == self.modified {
            return false;
        }
        self.modified = modified;
        match std::fs::read_to_string(&self.path)
            .map_err(|e| e.to_string())
            .and_then(|json| Tokens::parse(&json))
        {
            Ok(tokens) => {
                apply(ctx, Arc::new(tokens));
                self.error = None;
                true
            }
            Err(e) => {
                self.error = Some(e);
                false
            }
        }
    }
}

// ---------------------------------------------------------------------------------------------
// Background
// ---------------------------------------------------------------------------------------------

/// Paints the window background: a vertical gradient with soft lights behind the glass.
pub fn paint_background(ctx: &egui::Context) {
    let theme = ctx.theme();
    let tokens = tokens(ctx);
    let colors = tokens.colors(theme);
    let rect = ctx.content_rect();
    let painter = ctx.layer_painter(egui::LayerId::background());

    let mut gradient = Mesh::default();
    let (top, bottom) = (colors.background_top.0, colors.background_bottom.0);
    gradient.colored_vertex(rect.left_top(), top);
    gradient.colored_vertex(rect.right_top(), top);
    gradient.colored_vertex(rect.right_bottom(), bottom);
    gradient.colored_vertex(rect.left_bottom(), bottom);
    gradient.add_triangle(0, 1, 2);
    gradient.add_triangle(0, 2, 3);
    painter.add(Shape::mesh(gradient));

    let scale = rect.width().max(rect.height());
    for orb in &colors.orbs {
        let center = Pos2::new(
            rect.left() + orb.x * rect.width(),
            rect.top() + orb.y * rect.height(),
        );
        painter.add(Shape::mesh(radial_mesh(
            center,
            orb.radius * scale,
            orb.color.0,
        )));
    }
}

/// A disc that fades from `color` at the center to transparent at the rim.
fn radial_mesh(center: Pos2, radius: f32, color: Color32) -> Mesh {
    const SEGMENTS: u32 = 64;
    // (distance from center, opacity) for the two outer rings; the center vertex is opaque.
    const RINGS: [(f32, f32); 2] = [(0.45, 0.55), (1.0, 0.0)];
    let mut mesh = Mesh::default();
    mesh.colored_vertex(center, color);
    for &(t, alpha) in &RINGS {
        for i in 0..SEGMENTS {
            let angle = i as f32 / SEGMENTS as f32 * std::f32::consts::TAU;
            let pos = center + Vec2::angled(angle) * radius * t;
            mesh.colored_vertex(pos, color.gamma_multiply(alpha));
        }
    }
    let ring = |r: u32, i: u32| 1 + r * SEGMENTS + (i % SEGMENTS);
    for i in 0..SEGMENTS {
        mesh.add_triangle(0, ring(0, i), ring(0, i + 1));
        mesh.add_triangle(ring(0, i), ring(1, i), ring(1, i + 1));
        mesh.add_triangle(ring(0, i), ring(1, i + 1), ring(0, i + 1));
    }
    mesh
}

// ---------------------------------------------------------------------------------------------
// Shared widgets
// ---------------------------------------------------------------------------------------------

/// A frosted-glass frame: translucent fill, hairline light border, soft shadow.
pub fn glass_frame(ui: &egui::Ui) -> egui::Frame {
    let p = Palette::of(ui);
    let m = metrics(ui);
    egui::Frame::new()
        .fill(p.glass)
        .stroke(Stroke::new(1.0, p.glass_border))
        .corner_radius(CornerRadius::same(m.card_radius))
        .inner_margin(Margin::same(m.card_padding))
        .shadow(Shadow {
            offset: [0, m.shadow_offset_y],
            blur: m.shadow_blur,
            spread: 0,
            color: p.shadow,
        })
}

/// A glass card that groups related controls.
pub fn card<R>(ui: &mut egui::Ui, add: impl FnOnce(&mut egui::Ui) -> R) -> R {
    glass_frame(ui)
        .show(ui, |ui| {
            ui.set_width(ui.available_width());
            add(ui)
        })
        .inner
}

/// An inset region inside a card: payloads, key/value blocks, step lists.
pub fn inset<R>(ui: &mut egui::Ui, add: impl FnOnce(&mut egui::Ui) -> R) -> R {
    let p = Palette::of(ui);
    let m = metrics(ui);
    egui::Frame::new()
        .fill(p.inset)
        .corner_radius(CornerRadius::same(m.control_radius))
        .inner_margin(Margin::symmetric(10, 8))
        .show(ui, |ui| {
            ui.set_width(ui.available_width());
            add(ui)
        })
        .inner
}

/// Semibold text in the UI font, for titles.
pub fn title(text: impl Into<String>) -> RichText {
    RichText::new(text.into()).family(FontFamily::Name(fonts::SEMIBOLD.into()))
}

/// A small uppercase label that introduces a group of controls.
pub fn section_label(ui: &mut egui::Ui, text: &str) {
    let p = Palette::of(ui);
    ui.label(
        RichText::new(text.to_uppercase())
            .small()
            .color(p.muted)
            .family(FontFamily::Name(fonts::SEMIBOLD.into())),
    );
}

/// Card title, with optional controls aligned to the right edge.
pub fn card_title(ui: &mut egui::Ui, text: &str, right: impl FnOnce(&mut egui::Ui)) {
    ui.horizontal(|ui| {
        ui.label(title(text));
        ui.with_layout(egui::Layout::right_to_left(egui::Align::Center), right);
    });
}

/// A filled dot, for status indicators.
pub fn dot(ui: &mut egui::Ui, color: Color32) {
    let (rect, _) = ui.allocate_exact_size(Vec2::splat(9.0), egui::Sense::hover());
    ui.painter().circle_filled(rect.center(), 4.0, color);
}

/// A rounded chip: tinted background with `color` text.
pub fn pill(ui: &mut egui::Ui, text: impl Into<String>, color: Color32) -> egui::Response {
    egui::Frame::new()
        .fill(color.gamma_multiply(0.14))
        .corner_radius(CornerRadius::same(9))
        .inner_margin(Margin::symmetric(8, 2))
        .show(ui, |ui| {
            ui.label(RichText::new(text.into()).small().color(color))
        })
        .response
}

/// A status chip with a leading dot: used for the connection state.
pub fn status_pill(ui: &mut egui::Ui, text: &str, color: Color32) -> egui::Response {
    egui::Frame::new()
        .fill(color.gamma_multiply(0.14))
        .corner_radius(CornerRadius::same(11))
        .inner_margin(Margin::symmetric(10, 3))
        .show(ui, |ui| {
            ui.horizontal(|ui| {
                dot(ui, color);
                ui.label(RichText::new(text).small().color(color));
            });
        })
        .response
}

fn sized<'a>(ui: &egui::Ui, button: egui::Button<'a>) -> egui::Button<'a> {
    button
        .corner_radius(CornerRadius::same(metrics(ui).control_radius))
        .min_size(Vec2::new(0.0, 30.0))
}

/// The primary action button: filled with the accent color.
pub fn primary_button<'a>(ui: &egui::Ui, text: impl Into<String>) -> egui::Button<'a> {
    let p = Palette::of(ui);
    sized(
        ui,
        egui::Button::new(title(text).color(p.accent_text))
            .fill(p.accent)
            .stroke(Stroke::NONE),
    )
}

/// A secondary action button: quiet glass with a hairline border.
pub fn ghost_button<'a>(ui: &egui::Ui, text: impl Into<String>) -> egui::Button<'a> {
    let p = Palette::of(ui);
    sized(
        ui,
        egui::Button::new(RichText::new(text.into()).color(p.text))
            .fill(p.inset)
            .stroke(Stroke::new(1.0, p.glass_border)),
    )
}

/// A button for disconnecting or other destructive actions.
pub fn danger_button<'a>(ui: &egui::Ui, text: impl Into<String>) -> egui::Button<'a> {
    let p = Palette::of(ui);
    sized(
        ui,
        egui::Button::new(RichText::new(text.into()).color(p.err))
            .fill(p.err.gamma_multiply(0.10))
            .stroke(Stroke::new(1.0, p.err.gamma_multiply(0.35))),
    )
}

/// A text-only button in the muted color, for low-emphasis actions like "Clear".
pub fn link_button(ui: &mut egui::Ui, text: &str) -> egui::Response {
    let p = Palette::of(ui);
    ui.add(egui::Button::new(RichText::new(text).small().color(p.muted)).frame(false))
}

/// A borderless icon button, for inline actions like refresh and unsubscribe.
pub fn icon_button(ui: &mut egui::Ui, icon: &str, hover: &str, enabled: bool) -> egui::Response {
    ui.add_enabled(
        enabled,
        egui::Button::new(RichText::new(icon).small())
            .frame(false)
            .min_size(Vec2::splat(24.0)),
    )
    .on_hover_text(hover)
}

/// A segmented control, filling the available width.
pub fn segmented<T: PartialEq + Copy>(
    ui: &mut egui::Ui,
    value: &mut T,
    options: &[(T, &str)],
    enabled: bool,
) {
    let p = Palette::of(ui);
    let m = metrics(ui);
    egui::Frame::new()
        .fill(p.inset)
        .corner_radius(CornerRadius::same(m.control_radius))
        .inner_margin(Margin::same(3))
        .show(ui, |ui| {
            ui.spacing_mut().item_spacing.x = 3.0;
            let total = ui.available_width();
            let each = (total - 3.0 * (options.len() as f32 - 1.0)) / options.len() as f32;
            ui.horizontal(|ui| {
                for (option, label) in options {
                    let selected = *value == *option;
                    let text = if selected {
                        title(*label).color(p.text)
                    } else {
                        RichText::new(*label).color(p.muted)
                    };
                    let button = egui::Button::new(text)
                        .min_size(Vec2::new(each, 26.0))
                        .corner_radius(CornerRadius::same(m.control_radius.saturating_sub(3)))
                        .fill(if selected {
                            p.glass_strong
                        } else {
                            Color32::TRANSPARENT
                        })
                        .stroke(Stroke::NONE);
                    if ui.add_enabled(enabled, button).clicked() {
                        *value = *option;
                    }
                }
            });
        });
}

/// A compact segmented control sized to its labels (for inline choices inside a form row).
pub fn segmented_inline<T: PartialEq + Copy>(
    ui: &mut egui::Ui,
    value: &mut T,
    options: &[(T, &str)],
) {
    let p = Palette::of(ui);
    let m = metrics(ui);
    egui::Frame::new()
        .fill(p.inset)
        .corner_radius(CornerRadius::same(m.control_radius))
        .inner_margin(Margin::same(3))
        .show(ui, |ui| {
            ui.spacing_mut().item_spacing.x = 3.0;
            ui.horizontal(|ui| {
                for (option, label) in options {
                    let selected = *value == *option;
                    let text = if selected {
                        title(*label).color(p.text)
                    } else {
                        RichText::new(*label).color(p.muted)
                    };
                    let button = egui::Button::new(text)
                        .min_size(Vec2::new(0.0, 24.0))
                        .corner_radius(CornerRadius::same(m.control_radius.saturating_sub(3)))
                        .fill(if selected {
                            p.glass_strong
                        } else {
                            Color32::TRANSPARENT
                        })
                        .stroke(Stroke::NONE);
                    if ui.add(button).clicked() {
                        *value = *option;
                    }
                }
            });
        });
}

/// A disclosure row: chevron and label. Toggles `open` when clicked.
pub fn disclosure(ui: &mut egui::Ui, open: &mut bool, label: &str) -> egui::Response {
    let p = Palette::of(ui);
    let response = ui
        .horizontal(|ui| {
            let (rect, _) = ui.allocate_exact_size(Vec2::splat(12.0), egui::Sense::hover());
            paint_chevron(ui.painter(), rect, *open, p.muted);
            ui.label(RichText::new(label).color(p.muted));
        })
        .response
        .interact(egui::Sense::click());
    if response.clicked() {
        *open = !*open;
    }
    response.on_hover_cursor(egui::CursorIcon::PointingHand)
}

/// A chevron pointing right (closed) or down (open), drawn as a vector shape so it doesn't
/// depend on font coverage.
pub fn paint_chevron(painter: &egui::Painter, rect: Rect, open: bool, color: Color32) {
    let c = rect.center();
    let s = rect.width().min(rect.height()) * 0.22;
    let points = if open {
        vec![
            c + Vec2::new(-s, -s * 0.5),
            c + Vec2::new(0.0, s * 0.6),
            c + Vec2::new(s, -s * 0.5),
        ]
    } else {
        vec![
            c + Vec2::new(-s * 0.5, -s),
            c + Vec2::new(s * 0.6, 0.0),
            c + Vec2::new(-s * 0.5, s),
        ]
    };
    painter.add(Shape::line(points, Stroke::new(1.6, color)));
}

/// A tab in a tab strip.
pub fn tab(ui: &mut egui::Ui, selected: bool, label: impl Into<String>) -> egui::Response {
    let p = Palette::of(ui);
    let label = label.into();
    let text = if selected {
        title(label).color(p.accent)
    } else {
        RichText::new(label).color(p.muted)
    };
    ui.add(
        egui::Button::new(text)
            .fill(if selected {
                p.accent.gamma_multiply(0.14)
            } else {
                Color32::TRANSPARENT
            })
            .stroke(Stroke::NONE)
            .corner_radius(CornerRadius::same(metrics(ui).control_radius)),
    )
}

/// A full-width, left-aligned, selectable row for lists (Things, policies).
pub fn list_row(
    ui: &mut egui::Ui,
    selected: bool,
    enabled: bool,
    text: RichText,
) -> egui::Response {
    let p = Palette::of(ui);
    let (rect, response) = ui.allocate_exact_size(
        Vec2::new(ui.available_width(), 28.0),
        if enabled {
            egui::Sense::click()
        } else {
            egui::Sense::hover()
        },
    );
    let fill = if selected {
        p.accent.gamma_multiply(0.18)
    } else if response.hovered() && enabled {
        p.inset
    } else {
        Color32::TRANSPARENT
    };
    if fill != Color32::TRANSPARENT {
        ui.painter()
            .rect_filled(rect, CornerRadius::same(metrics(ui).control_radius), fill);
    }
    let text = if selected {
        text.color(p.accent)
    } else if enabled {
        text
    } else {
        text.color(p.muted)
    };
    ui.put(
        rect.shrink2(Vec2::new(10.0, 0.0)),
        egui::Label::new(text)
            .truncate()
            .halign(egui::Align::LEFT)
            .selectable(false),
    );
    response
}

/// Severity of a [`banner`].
#[derive(Debug, Clone, Copy, PartialEq, Eq)]
pub enum Severity {
    Error,
    Warning,
    Success,
}

impl Severity {
    fn color(self, p: &Palette) -> Color32 {
        match self {
            Severity::Error => p.err,
            Severity::Warning => p.warn,
            Severity::Success => p.ok,
        }
    }
}

/// A tinted message block. Returns true if the dismiss button was clicked.
pub fn banner(
    ui: &mut egui::Ui,
    severity: Severity,
    icon: &str,
    dismissible: bool,
    body: impl FnOnce(&mut egui::Ui),
) -> bool {
    let p = Palette::of(ui);
    let color = severity.color(&p);
    let mut dismissed = false;
    egui::Frame::new()
        .fill(color.gamma_multiply(0.10))
        .stroke(Stroke::new(1.0, color.gamma_multiply(0.30)))
        .corner_radius(CornerRadius::same(metrics(ui).control_radius))
        .inner_margin(Margin::symmetric(12, 9))
        .show(ui, |ui| {
            ui.set_width(ui.available_width());
            ui.horizontal_top(|ui| {
                ui.label(RichText::new(icon).color(color));
                ui.vertical(|ui| {
                    ui.spacing_mut().item_spacing.y = 3.0;
                    body(ui);
                });
                if dismissible {
                    ui.with_layout(egui::Layout::right_to_left(egui::Align::TOP), |ui| {
                        dismissed = icon_button(ui, icons::CLOSE, "Dismiss", true).clicked();
                    });
                }
            });
        });
    dismissed
}

/// A field label in the muted color.
pub fn field_label(ui: &mut egui::Ui, text: &str) {
    let p = Palette::of(ui);
    ui.label(RichText::new(text).color(p.muted));
}

/// Secondary helper text.
pub fn hint(ui: &mut egui::Ui, text: impl Into<String>) {
    let p = Palette::of(ui);
    ui.label(RichText::new(text.into()).small().color(p.muted));
}

#[cfg(test)]
mod tests {
    use super::*;

    fn luminance(c: [f64; 3]) -> f64 {
        let channel = |v: f64| {
            let v = v / 255.0;
            if v <= 0.040_45 {
                v / 12.92
            } else {
                ((v + 0.055) / 1.055).powf(2.4)
            }
        };
        0.2126 * channel(c[0]) + 0.7152 * channel(c[1]) + 0.0722 * channel(c[2])
    }

    fn contrast(a: [f64; 3], b: [f64; 3]) -> f64 {
        let (x, y) = (luminance(a), luminance(b));
        (x.max(y) + 0.05) / (x.min(y) + 0.05)
    }

    /// `top` (with alpha) composited over opaque `bottom`.
    fn over(top: Color32, bottom: [f64; 3]) -> [f64; 3] {
        let [r, g, b, a] = top.to_srgba_unmultiplied();
        let a = a as f64 / 255.0;
        [
            r as f64 * a + bottom[0] * (1.0 - a),
            g as f64 * a + bottom[1] * (1.0 - a),
            b as f64 * a + bottom[2] * (1.0 - a),
        ]
    }

    fn rgb(c: Color32) -> [f64; 3] {
        let [r, g, b, _] = c.to_srgba_unmultiplied();
        [r as f64, g as f64, b as f64]
    }

    /// Every surface text can sit on: glass (and inset glass) over the background, including
    /// directly over the center of each light orb (the worst case).
    fn text_surfaces(c: &ThemeColors) -> Vec<[f64; 3]> {
        let mut surfaces = Vec::new();
        for base in [c.background_top.0, c.background_bottom.0] {
            let base = rgb(base);
            let mut under = vec![base];
            under.extend(c.orbs.iter().map(|o| over(o.color.0, base)));
            for u in under {
                let glass = over(c.glass.0, u);
                surfaces.push(glass);
                surfaces.push(over(c.inset.0, glass));
                let strong = over(c.glass_strong.0, u);
                surfaces.push(strong);
                surfaces.push(over(c.inset.0, strong));
            }
        }
        surfaces
    }

    #[test]
    fn builtin_tokens_parse_and_round_trip() {
        let tokens = Tokens::parse(DEFAULT_TOKENS_JSON).unwrap();
        let json = serde_json::to_string(&tokens).unwrap();
        assert_eq!(Tokens::parse(&json).unwrap(), tokens);
    }

    #[test]
    fn hex_parsing() {
        assert_eq!(
            Hex::parse("#FF0000").unwrap().0,
            Color32::from_rgb(255, 0, 0)
        );
        assert_eq!(
            Hex::parse("#00FF0080").unwrap().0,
            Color32::from_rgba_unmultiplied(0, 255, 0, 128)
        );
        assert!(Hex::parse("FF0000").is_err());
        assert!(Hex::parse("#FF00").is_err());
        assert!(Hex::parse("#GG0000").is_err());
    }

    #[test]
    fn invalid_token_files_are_rejected_with_a_message() {
        assert!(Tokens::parse("{").is_err());
        let mut tokens = Tokens::parse(DEFAULT_TOKENS_JSON).unwrap();
        tokens.metrics.font_body = 200.0;
        let json = serde_json::to_string(&tokens).unwrap();
        assert!(Tokens::parse(&json).unwrap_err().contains("font_body"));
    }

    #[test]
    fn text_is_readable_on_glass_over_any_part_of_the_background() {
        let tokens = Tokens::builtin();
        for theme in [Theme::Dark, Theme::Light] {
            let c = tokens.colors(theme);
            for surface in text_surfaces(c) {
                let body = contrast(rgb(c.text.0), surface);
                assert!(body >= 7.0, "{theme:?} body text {body:.2}");
                for (name, color) in [
                    ("muted", c.muted.0),
                    ("ok", c.ok.0),
                    ("warn", c.warn.0),
                    ("err", c.err.0),
                    ("info", c.info.0),
                ] {
                    let ratio = contrast(rgb(color), surface);
                    assert!(ratio >= 4.5, "{theme:?} {name} {ratio:.2}");
                }
            }
            let button = contrast(rgb(c.accent_text.0), rgb(c.accent.0));
            assert!(button >= 4.5, "{theme:?} accent button text {button:.2}");
        }
    }

    #[test]
    fn style_uses_tokens_for_both_themes() {
        let tokens = Tokens::builtin();
        for theme in [Theme::Dark, Theme::Light] {
            let style = style_for(theme, &tokens);
            let p = Palette::from(tokens.colors(theme));
            assert_eq!(style.visuals.panel_fill, Color32::TRANSPARENT);
            assert_eq!(style.visuals.window_fill, p.glass_strong);
            assert_eq!(style.visuals.widgets.noninteractive.fg_stroke.color, p.text);
            assert_eq!(style.visuals.dark_mode, theme == Theme::Dark);
        }
    }

    #[test]
    fn watcher_applies_changes_and_reports_errors() {
        let dir = tempfile::tempdir().unwrap();
        let path = dir.path().join("theme.json");
        let mut tokens = Tokens::parse(DEFAULT_TOKENS_JSON).unwrap();
        std::fs::write(&path, serde_json::to_string(&tokens).unwrap()).unwrap();

        let ctx = egui::Context::default();
        let mut watcher = ThemeWatcher::new(path.clone());
        assert!(watcher.poll(&ctx), "first poll applies the file");

        tokens.metrics.card_radius = 3;
        // Ensure a distinct mtime on filesystems with coarse timestamps.
        std::thread::sleep(Duration::from_millis(1100));
        std::fs::write(&path, serde_json::to_string(&tokens).unwrap()).unwrap();
        watcher.last_check = Instant::now() - ThemeWatcher::POLL;
        assert!(watcher.poll(&ctx));
        assert_eq!(super::tokens(&ctx).metrics.card_radius, 3);

        std::thread::sleep(Duration::from_millis(1100));
        std::fs::write(&path, "{ not json").unwrap();
        watcher.last_check = Instant::now() - ThemeWatcher::POLL;
        assert!(!watcher.poll(&ctx));
        assert!(watcher.error.is_some());
        assert_eq!(
            super::tokens(&ctx).metrics.card_radius,
            3,
            "a broken file keeps the last good tokens"
        );
    }

    #[test]
    fn radial_mesh_is_well_formed() {
        let mesh = radial_mesh(Pos2::new(50.0, 50.0), 40.0, Color32::RED);
        assert!(mesh.is_valid());
        assert_eq!(mesh.vertices[0].color, Color32::RED);
        assert_eq!(mesh.vertices.last().unwrap().color, Color32::TRANSPARENT);
    }

    /// Renders every widget and the background headlessly in both themes, with the real font
    /// families installed. Catches layout panics and duplicate-ID assertions.
    #[test]
    fn widgets_render_without_panicking() {
        for theme in [Theme::Dark, Theme::Light] {
            let ctx = egui::Context::default();
            ctx.set_fonts(fonts::definitions(&fonts::SystemFonts::default()));
            install(&ctx);
            ctx.set_theme(theme);
            let mut mode = 1u8;
            let mut open = false;
            let mut out = ctx.run_ui(Default::default(), |ui| {
                paint_background(ui.ctx());
                card(ui, |ui| {
                    card_title(ui, "Title", |ui| {
                        icon_button(ui, icons::REFRESH, "Refresh", true);
                    });
                    section_label(ui, "Section");
                    field_label(ui, "Label");
                    hint(ui, "A hint");
                    let p = Palette::of(ui);
                    dot(ui, p.ok);
                    pill(ui, "chip", p.muted);
                    status_pill(ui, "Connected", p.ok);
                    segmented(ui, &mut mode, &[(1u8, "One"), (2u8, "Two")], true);
                    segmented_inline(ui, &mut mode, &[(1u8, "One"), (2u8, "Two")]);
                    disclosure(ui, &mut open, "Advanced");
                    inset(ui, |ui| {
                        ui.label("inset");
                    });
                    for selected in [true, false] {
                        list_row(ui, selected, true, RichText::new("row"));
                        tab(ui, selected, "tab");
                    }
                    let b = primary_button(ui, "Primary");
                    ui.add(b);
                    let b = ghost_button(ui, "Ghost");
                    ui.add(b);
                    let b = danger_button(ui, "Danger");
                    ui.add(b);
                    link_button(ui, "Clear");
                    for severity in [Severity::Error, Severity::Warning, Severity::Success] {
                        banner(ui, severity, icons::WARNING, true, |ui| {
                            ui.label("message");
                        });
                    }
                });
            });
            out.textures_delta.clear();
            assert_eq!(mode, 1, "{theme:?}: segmented must not change on its own");
            assert!(!open, "{theme:?}: disclosure must not toggle on its own");
        }
    }
}
