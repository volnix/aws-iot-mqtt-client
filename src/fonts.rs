// Copyright 2026 Nick Volgas
// SPDX-License-Identifier: Apache-2.0

//! Uses the operating system's UI font, loaded from the user's machine at runtime.
//!
//! Nothing is bundled or redistributed: San Francisco (macOS), Segoe UI (Windows), and the
//! fontconfig default (Linux) are read from their system locations. egui's bundled fonts stay in
//! each family as fallbacks, so emoji and icon glyphs keep rendering, and the app still works if
//! no system font is found.

use std::path::{Path, PathBuf};
use std::sync::Arc;

use egui::epaint::text::{FontData, FontTweak, VariationCoords};
use egui::{FontDefinitions, FontFamily};

/// Font family for headings and titles: a semibold cut of the UI font when available.
pub const SEMIBOLD: &str = "ui-semibold";

/// A font file on disk, plus the face index inside a `.ttc` collection.
#[derive(Debug, Clone, PartialEq, Eq)]
pub struct FontFile {
    pub path: PathBuf,
    pub index: u32,
}

impl FontFile {
    fn new(path: impl Into<PathBuf>) -> Self {
        Self {
            path: path.into(),
            index: 0,
        }
    }
}

/// System fonts found on this machine.
#[derive(Debug, Clone, Default, PartialEq, Eq)]
pub struct SystemFonts {
    pub proportional: Option<FontFile>,
    /// A separate semibold file (Windows). On macOS the variable font's weight axis is used.
    pub semibold: Option<FontFile>,
    pub monospace: Option<FontFile>,
}

/// Looks up the platform's UI fonts. Only returns files that exist and look like fonts.
pub fn discover() -> SystemFonts {
    let first = |candidates: Vec<PathBuf>| {
        candidates
            .into_iter()
            .find(|p| looks_like_font(p))
            .map(FontFile::new)
    };

    if cfg!(target_os = "macos") {
        SystemFonts {
            proportional: first(vec!["/System/Library/Fonts/SFNS.ttf".into()]),
            semibold: None,
            monospace: first(vec![
                "/System/Library/Fonts/SFNSMono.ttf".into(),
                "/System/Library/Fonts/Menlo.ttc".into(),
            ]),
        }
    } else if cfg!(windows) {
        let dir = std::env::var_os("WINDIR")
            .map(PathBuf::from)
            .unwrap_or_else(|| PathBuf::from(r"C:\Windows"))
            .join("Fonts");
        SystemFonts {
            proportional: first(vec![dir.join("segoeui.ttf")]),
            semibold: first(vec![dir.join("seguisb.ttf")]),
            monospace: first(vec![dir.join("CascadiaMono.ttf"), dir.join("consola.ttf")]),
        }
    } else {
        let mut sans = fontconfig_match("sans-serif")
            .into_iter()
            .collect::<Vec<_>>();
        sans.extend(
            [
                "/usr/share/fonts/truetype/noto/NotoSans-Regular.ttf",
                "/usr/share/fonts/noto/NotoSans-Regular.ttf",
                "/usr/share/fonts/cantarell/Cantarell-VF.otf",
                "/usr/share/fonts/truetype/dejavu/DejaVuSans.ttf",
            ]
            .map(PathBuf::from),
        );
        let mut mono = fontconfig_match("monospace")
            .into_iter()
            .collect::<Vec<_>>();
        mono.extend(
            [
                "/usr/share/fonts/truetype/noto/NotoSansMono-Regular.ttf",
                "/usr/share/fonts/truetype/dejavu/DejaVuSansMono.ttf",
            ]
            .map(PathBuf::from),
        );
        SystemFonts {
            proportional: first(sans),
            semibold: None,
            monospace: first(mono),
        }
    }
}

/// Asks fontconfig (if installed) which file it would use for a generic family.
fn fontconfig_match(family: &str) -> Option<PathBuf> {
    let out = std::process::Command::new("fc-match")
        .args(["-f", "%{file}", family])
        .output()
        .ok()?;
    let path = String::from_utf8(out.stdout).ok()?;
    let path = path.trim();
    (!path.is_empty()).then(|| PathBuf::from(path))
}

/// Checks the file's magic number so a corrupt or unexpected file is skipped, not loaded.
pub fn looks_like_font(path: &Path) -> bool {
    use std::io::Read;
    let Ok(mut file) = std::fs::File::open(path) else {
        return false;
    };
    let mut magic = [0u8; 4];
    if file.read_exact(&mut magic).is_err() {
        return false;
    }
    is_font_magic(&magic)
}

fn is_font_magic(magic: &[u8; 4]) -> bool {
    matches!(magic, [0, 1, 0, 0] | b"true" | b"OTTO" | b"ttcf")
}

/// Puts the system fonts first in each family and keeps egui's fonts as fallbacks.
pub fn definitions(fonts: &SystemFonts) -> FontDefinitions {
    let mut defs = FontDefinitions::default();
    let fallback_proportional = defs
        .families
        .get(&FontFamily::Proportional)
        .cloned()
        .unwrap_or_default();
    let fallback_monospace = defs
        .families
        .get(&FontFamily::Monospace)
        .cloned()
        .unwrap_or_default();

    let mut load = |name: &str, file: &FontFile, tweak: FontTweak| -> bool {
        match std::fs::read(&file.path) {
            Ok(bytes) => {
                let mut data = FontData::from_owned(bytes).tweak(tweak);
                data.index = file.index;
                defs.font_data.insert(name.to_owned(), Arc::new(data));
                true
            }
            Err(_) => false,
        }
    };

    let mut proportional = fallback_proportional.clone();
    let mut semibold = fallback_proportional.clone();
    if let Some(file) = &fonts.proportional {
        if load("system-ui", file, FontTweak::default()) {
            proportional.insert(0, "system-ui".to_owned());
            // Variable fonts (San Francisco) get the semibold cut from the weight axis.
            let heavier = FontTweak {
                coords: VariationCoords::new([("wght", 600.0)]),
                ..FontTweak::default()
            };
            let semibold_file = fonts.semibold.as_ref().unwrap_or(file);
            let semibold_tweak = if fonts.semibold.is_some() {
                FontTweak::default()
            } else {
                heavier
            };
            if load("system-ui-semibold", semibold_file, semibold_tweak) {
                semibold.insert(0, "system-ui-semibold".to_owned());
            }
        }
    }

    let mut monospace = fallback_monospace;
    if let Some(file) = &fonts.monospace {
        if load("system-mono", file, FontTweak::default()) {
            monospace.insert(0, "system-mono".to_owned());
        }
    }

    defs.families.insert(FontFamily::Proportional, proportional);
    defs.families.insert(FontFamily::Monospace, monospace);
    defs.families
        .insert(FontFamily::Name(SEMIBOLD.into()), semibold);
    defs
}

/// Installs the system fonts (if any) into the egui context.
pub fn install(ctx: &egui::Context) {
    ctx.set_fonts(definitions(&discover()));
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn recognizes_font_signatures() {
        assert!(is_font_magic(&[0, 1, 0, 0]));
        assert!(is_font_magic(b"OTTO"));
        assert!(is_font_magic(b"ttcf"));
        assert!(!is_font_magic(b"%PDF"));
    }

    #[test]
    fn missing_or_bogus_files_are_skipped() {
        assert!(!looks_like_font(Path::new("/definitely/not/here.ttf")));
        let dir = tempfile::tempdir().unwrap();
        let bogus = dir.path().join("x.ttf");
        std::fs::write(&bogus, b"not a font").unwrap();
        assert!(!looks_like_font(&bogus));
    }

    /// Without system fonts the families still exist and fall back to egui's own fonts.
    #[test]
    fn definitions_without_system_fonts_keep_fallbacks() {
        let defs = definitions(&SystemFonts::default());
        assert!(!defs.families[&FontFamily::Proportional].is_empty());
        assert!(!defs.families[&FontFamily::Monospace].is_empty());
        assert_eq!(
            defs.families[&FontFamily::Name(SEMIBOLD.into())],
            defs.families[&FontFamily::Proportional]
        );
    }

    /// On this machine's platform, any discovered fonts must load and render text and still
    /// fall back to egui's fonts for emoji/icon glyphs.
    #[test]
    fn discovered_fonts_render_text_and_icons() {
        let fonts = discover();
        let defs = definitions(&fonts);
        if fonts.proportional.is_some() {
            assert_eq!(defs.families[&FontFamily::Proportional][0], "system-ui");
        }
        let ctx = egui::Context::default();
        ctx.set_fonts(defs);
        let mut out = ctx.run_ui(Default::default(), |ui| {
            ui.label("Connected ✔ 🔄");
            ui.label(egui::RichText::new("Heading").family(FontFamily::Name(SEMIBOLD.into())));
        });
        out.textures_delta.clear();
        let font = egui::FontId::proportional(14.0);
        for c in ['A', 'z', '✔', '🔄'] {
            assert!(
                ctx.fonts_mut(|f| f.has_glyph(&font, c)),
                "missing glyph {c:?}"
            );
        }
    }
}
