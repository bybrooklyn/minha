//! User-local, versioned TUI preferences and Opaline theme plumbing.
//!
//! These settings deliberately do **not** use the project `minha.toml`: the
//! terminal's contrast, renderer, and editing affordances belong to the
//! person at the keyboard, not to a repository they happen to open.

use anyhow::{Context, Result, bail};
use opaline::{OpalineColor, Theme};
use serde::{Deserialize, Serialize};
use std::fs;
use std::path::{Path, PathBuf};

pub(crate) const SETTINGS_VERSION: u32 = 1;
const MAX_IMPORTED_THEME_BYTES: u64 = 64 * 1024;

const THEMES: &[&str] = &[
    "auto",
    "dark",
    "light",
    "ansi16",
    "high_contrast",
    "no_color",
    "imported",
];
const SURFACE_RENDERERS: &[&str] = &["auto", "kitty", "quadrant", "square"];

/// A compact, user-local preferences document.  `version` is intentionally a
/// required top-level field so an incompatible future document is rejected
/// rather than silently interpreted as different accessibility settings.
#[derive(Clone, Debug, Eq, PartialEq, Serialize, Deserialize)]
#[serde(deny_unknown_fields)]
pub(crate) struct TuiSettingsV1 {
    pub(crate) version: u32,
    pub(crate) theme: String,
    #[serde(default)]
    pub(crate) imported_theme: Option<ImportedThemeV1>,
    pub(crate) surface_renderer: String,
    pub(crate) reduced_motion: bool,
    pub(crate) vim_scroll: bool,
    pub(crate) scroll_lines: u16,
    pub(crate) raw_transcript: bool,
}

/// An imported Opaline document.  Retaining its source makes export
/// lossless, while its parsed form is validated on every load.
#[derive(Clone, Debug, Eq, PartialEq, Serialize, Deserialize)]
#[serde(deny_unknown_fields)]
pub(crate) struct ImportedThemeV1 {
    pub(crate) name: String,
    pub(crate) source: String,
    pub(crate) toml: String,
}

/// Semantic RGB values resolved through Opaline.  The default values match
/// Minha's pre-Opaline renderer exactly; UI code maps its existing semantic
/// colors onto this palette only for imported themes.
#[derive(Clone, Copy, Debug, Eq, PartialEq)]
pub(crate) struct ThemePalette {
    pub(crate) background: [u8; 3],
    pub(crate) surface: [u8; 3],
    pub(crate) surface_alt: [u8; 3],
    pub(crate) border: [u8; 3],
    pub(crate) text: [u8; 3],
    pub(crate) bright: [u8; 3],
    pub(crate) muted: [u8; 3],
    pub(crate) active: [u8; 3],
    pub(crate) good: [u8; 3],
    pub(crate) warn: [u8; 3],
    pub(crate) bad: [u8; 3],
}

/// Contrast information shown before a theme is committed.
#[derive(Clone, Copy, Debug, PartialEq)]
pub(crate) struct ContrastReport {
    pub(crate) normal: f64,
    pub(crate) muted: f64,
    pub(crate) active: f64,
}

impl ContrastReport {
    pub(crate) fn normal_passes(self) -> bool {
        self.normal >= 4.5
    }

    pub(crate) fn muted_passes(self) -> bool {
        self.muted >= 4.5
    }

    pub(crate) fn active_passes(self) -> bool {
        self.active >= 3.0
    }
}

impl Default for TuiSettingsV1 {
    fn default() -> Self {
        Self {
            version: SETTINGS_VERSION,
            theme: "dark".into(),
            imported_theme: None,
            surface_renderer: "auto".into(),
            reduced_motion: false,
            vim_scroll: false,
            scroll_lines: 3,
            raw_transcript: false,
        }
    }
}

impl TuiSettingsV1 {
    pub(crate) fn with_legacy_defaults(
        theme: impl Into<String>,
        surface_renderer: impl Into<String>,
        reduced_motion: bool,
    ) -> Self {
        Self {
            theme: theme.into(),
            surface_renderer: surface_renderer.into(),
            reduced_motion,
            ..Self::default()
        }
    }

    pub(crate) fn validate(&mut self) -> Result<()> {
        if self.version != SETTINGS_VERSION {
            bail!(
                "unsupported TUI settings version {}; expected {}",
                self.version,
                SETTINGS_VERSION
            );
        }
        self.theme = canonical_theme(&self.theme)?;
        self.surface_renderer = canonical_renderer(&self.surface_renderer)?;
        if !(1..=100).contains(&self.scroll_lines) {
            bail!("scroll_lines must be between 1 and 100");
        }
        if let Some(imported) = &self.imported_theme {
            validate_imported_theme(imported)?;
        }
        if self.theme == "imported" && self.imported_theme.is_none() {
            bail!("the imported theme is not available; import one before selecting it");
        }
        Ok(())
    }

    pub(crate) fn set_imported_theme(&mut self, imported: ImportedThemeV1) -> Result<()> {
        validate_imported_theme(&imported)?;
        self.imported_theme = Some(imported);
        self.theme = "imported".into();
        Ok(())
    }

    pub(crate) fn palette(&self) -> Result<ThemePalette> {
        let theme = if self.theme == "imported" {
            let imported = self
                .imported_theme
                .as_ref()
                .context("the imported theme is not available")?;
            opaline::load_from_str(&imported.toml, None)
                .context("the saved imported theme is no longer valid")?
        } else {
            minha_default_theme()
        };
        Ok(ThemePalette::from_theme(&theme))
    }
}

impl ThemePalette {
    pub(crate) fn default_dark() -> Self {
        // Resolve the built-in token set through the same Opaline path used by
        // imports. The resulting RGB values intentionally match the legacy
        // renderer exactly.
        Self::from_theme(&minha_default_theme())
    }

    fn from_theme(theme: &Theme) -> Self {
        let defaults = default_colors();
        Self {
            background: token(
                theme,
                &["background", "bg.base", "bg", "canvas"],
                defaults.background,
            ),
            surface: token(theme, &["surface", "bg.surface", "panel"], defaults.surface),
            surface_alt: token(
                theme,
                &["surface_alt", "bg.surface_alt", "bg.elevated", "panel_alt"],
                defaults.surface_alt,
            ),
            border: token(theme, &["border", "border.default", "outline"], defaults.border),
            text: token(theme, &["text", "fg.primary", "foreground", "fg"], defaults.text),
            bright: token(theme, &["bright", "fg.bright", "text.strong"], defaults.bright),
            muted: token(theme, &["muted", "fg.muted", "text.muted"], defaults.muted),
            active: token(theme, &["active", "accent", "accent.primary"], defaults.active),
            good: token(theme, &["good", "success", "status.success"], defaults.good),
            warn: token(theme, &["warn", "warning", "status.warning"], defaults.warn),
            bad: token(theme, &["bad", "error", "status.error"], defaults.bad),
        }
    }

    pub(crate) fn contrast_report(self) -> ContrastReport {
        ContrastReport {
            normal: contrast_ratio(self.text, self.background),
            muted: contrast_ratio(self.muted, self.background),
            active: contrast_ratio(self.active, self.background),
        }
    }
}

/// Returns `None` when the host has no user config directory.  It never falls
/// back to the working tree, which is the crucial project-local boundary.
pub(crate) fn user_settings_path() -> Option<PathBuf> {
    dirs::config_dir().map(|path| path.join("minha").join("tui-settings-v1.json"))
}

pub(crate) fn load_user_settings(fallback: TuiSettingsV1) -> (TuiSettingsV1, Option<String>) {
    let Some(path) = user_settings_path() else {
        return (
            fallback,
            Some("no user configuration directory; TUI settings are session-only".into()),
        );
    };
    match load_from_path(&path) {
        Ok(Some(settings)) => (settings, None),
        Ok(None) => (fallback, None),
        Err(error) => (
            fallback,
            Some(format!("could not load user-local TUI settings: {error}")),
        ),
    }
}

pub(crate) fn load_from_path(path: &Path) -> Result<Option<TuiSettingsV1>> {
    let contents = match fs::read_to_string(path) {
        Ok(contents) => contents,
        Err(error) if error.kind() == std::io::ErrorKind::NotFound => return Ok(None),
        Err(error) => return Err(error).with_context(|| format!("read {}", path.display())),
    };
    let mut settings: TuiSettingsV1 =
        serde_json::from_str(&contents).with_context(|| format!("parse {}", path.display()))?;
    settings.validate()?;
    Ok(Some(settings))
}

pub(crate) fn save_user_settings(settings: &TuiSettingsV1) -> Result<PathBuf> {
    let path = user_settings_path().context("no user configuration directory for TUI settings")?;
    save_to_path(settings, &path)?;
    Ok(path)
}

pub(crate) fn save_to_path(settings: &TuiSettingsV1, path: &Path) -> Result<()> {
    let mut normalized = settings.clone();
    normalized.validate()?;
    let contents = serde_json::to_vec_pretty(&normalized).context("serialize TUI settings")?;
    if let Some(parent) = path.parent() {
        fs::create_dir_all(parent).with_context(|| format!("create {}", parent.display()))?;
    }
    fs::write(path, contents).with_context(|| format!("write {}", path.display()))
}

pub(crate) fn import_theme(path: &Path) -> Result<ImportedThemeV1> {
    let metadata = fs::metadata(path).with_context(|| format!("inspect {}", path.display()))?;
    if metadata.len() > MAX_IMPORTED_THEME_BYTES {
        bail!(
            "theme {} is {} bytes; the local import limit is {} bytes",
            path.display(),
            metadata.len(),
            MAX_IMPORTED_THEME_BYTES
        );
    }
    let toml = fs::read_to_string(path).with_context(|| format!("read {}", path.display()))?;
    let theme =
        opaline::load_from_str(&toml, Some(path)).with_context(|| format!("validate {}", path.display()))?;
    let source = path
        .file_name()
        .and_then(|name| name.to_str())
        .unwrap_or("imported-theme.toml")
        .to_owned();
    Ok(ImportedThemeV1 {
        name: theme.meta.name,
        source,
        toml,
    })
}

pub(crate) fn export_theme(settings: &TuiSettingsV1, path: &Path) -> Result<()> {
    let contents = settings
        .imported_theme
        .as_ref()
        .map_or_else(|| DEFAULT_THEME_TOML.to_owned(), |theme| theme.toml.clone());
    // Validate before writing an export, so it is always importable later.
    let _ = opaline::load_from_str(&contents, Some(path)).context("validate theme before export")?;
    if let Some(parent) = path.parent() {
        fs::create_dir_all(parent).with_context(|| format!("create {}", parent.display()))?;
    }
    fs::write(path, contents).with_context(|| format!("write {}", path.display()))
}

pub(crate) fn canonical_theme(value: &str) -> Result<String> {
    let normalized = value.trim().to_ascii_lowercase().replace('-', "_");
    if THEMES.contains(&normalized.as_str()) {
        Ok(normalized)
    } else {
        bail!("unknown theme {value:?}; available: {}", THEMES.join(", "))
    }
}

pub(crate) fn canonical_renderer(value: &str) -> Result<String> {
    let normalized = value.trim().to_ascii_lowercase();
    if SURFACE_RENDERERS.contains(&normalized.as_str()) {
        Ok(normalized)
    } else {
        bail!(
            "unknown surface renderer {value:?}; available: {}",
            SURFACE_RENDERERS.join(", ")
        )
    }
}

pub(crate) fn available_themes() -> &'static [&'static str] {
    THEMES
}

pub(crate) fn available_renderers() -> &'static [&'static str] {
    SURFACE_RENDERERS
}

fn validate_imported_theme(imported: &ImportedThemeV1) -> Result<()> {
    if imported.toml.len() as u64 > MAX_IMPORTED_THEME_BYTES {
        bail!(
            "saved imported theme exceeds the {} byte local limit",
            MAX_IMPORTED_THEME_BYTES
        );
    }
    let theme = opaline::load_from_str(&imported.toml, None).context("parse imported Opaline theme")?;
    if theme.meta.name.trim().is_empty() {
        bail!("imported theme must have a non-empty meta.name");
    }
    Ok(())
}

fn token(theme: &Theme, names: &[&str], fallback: [u8; 3]) -> [u8; 3] {
    names
        .iter()
        .find_map(|name| theme.try_color(name))
        .map_or(fallback, |color| {
            let (red, green, blue) = color.to_rgb_tuple();
            [red, green, blue]
        })
}

fn contrast_ratio(foreground: [u8; 3], background: [u8; 3]) -> f64 {
    let first = relative_luminance(foreground);
    let second = relative_luminance(background);
    (first.max(second) + 0.05) / (first.min(second) + 0.05)
}

fn relative_luminance(rgb: [u8; 3]) -> f64 {
    let channel = |value: u8| {
        let value = f64::from(value) / 255.0;
        if value <= 0.04045 {
            value / 12.92
        } else {
            ((value + 0.055) / 1.055).powf(2.4)
        }
    };
    0.2126 * channel(rgb[0]) + 0.7152 * channel(rgb[1]) + 0.0722 * channel(rgb[2])
}

fn default_colors() -> ThemePalette {
    ThemePalette {
        background: [5, 12, 24],
        surface: [10, 24, 43],
        surface_alt: [15, 34, 57],
        border: [43, 70, 101],
        text: [192, 192, 192],
        bright: [255, 255, 255],
        muted: [128, 128, 128],
        active: [0, 255, 255],
        good: [0, 255, 0],
        warn: [255, 255, 0],
        bad: [255, 0, 0],
    }
}

fn minha_default_theme() -> Theme {
    Theme::builder("Minha Dark")
        .version("1")
        .token("background", OpalineColor::from(default_colors().background))
        .token("surface", OpalineColor::from(default_colors().surface))
        .token("surface_alt", OpalineColor::from(default_colors().surface_alt))
        .token("border", OpalineColor::from(default_colors().border))
        .token("text", OpalineColor::from(default_colors().text))
        .token("bright", OpalineColor::from(default_colors().bright))
        .token("muted", OpalineColor::from(default_colors().muted))
        .token("active", OpalineColor::from(default_colors().active))
        .token("good", OpalineColor::from(default_colors().good))
        .token("warn", OpalineColor::from(default_colors().warn))
        .token("bad", OpalineColor::from(default_colors().bad))
        .build()
}

const DEFAULT_THEME_TOML: &str = r##"[meta]
name = "Minha Dark"
version = "1"
variant = "dark"
description = "Minha's original dark terminal palette"

[tokens]
background = "#050c18"
surface = "#0a182b"
surface_alt = "#0f2239"
border = "#2b4665"
text = "#c0c0c0"
bright = "#ffffff"
muted = "#808080"
active = "#00ffff"
good = "#00ff00"
warn = "#ffff00"
bad = "#ff0000"
"##;

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn default_palette_matches_the_pre_opaline_dark_rgb_values() {
        assert_eq!(ThemePalette::default_dark().background, [5, 12, 24]);
        assert_eq!(ThemePalette::default_dark().surface, [10, 24, 43]);
        assert_eq!(ThemePalette::default_dark().surface_alt, [15, 34, 57]);
        assert_eq!(ThemePalette::default_dark().border, [43, 70, 101]);
    }

    #[test]
    fn settings_round_trip_only_through_the_explicit_user_path() {
        let directory = tempfile::tempdir().expect("temporary directory");
        let path = directory.path().join("tui-settings-v1.json");
        let expected = TuiSettingsV1 {
            vim_scroll: true,
            scroll_lines: 9,
            ..TuiSettingsV1::default()
        };
        save_to_path(&expected, &path).expect("save settings");
        assert_eq!(load_from_path(&path).expect("load settings"), Some(expected));
    }

    #[test]
    fn imported_opaline_theme_is_validated_and_drives_contrast() {
        let imported = ImportedThemeV1 {
            name: "Test".into(),
            source: "test.toml".into(),
            toml: r##"[meta]
name = "Test"
variant = "dark"

[tokens]
background = "#000000"
text = "#ffffff"
muted = "#666666"
active = "#00ffff"
"##
            .into(),
        };
        let mut settings = TuiSettingsV1::default();
        settings
            .set_imported_theme(imported)
            .expect("valid imported theme");
        let contrast = settings.palette().expect("palette").contrast_report();
        assert!(contrast.normal_passes());
        assert!(contrast.active_passes());
        assert!(!contrast.muted_passes());
    }

    #[test]
    fn malformed_or_unknown_settings_never_silently_apply() {
        let mut settings = TuiSettingsV1 {
            theme: "laser".into(),
            ..TuiSettingsV1::default()
        };
        assert!(settings.validate().is_err());
        settings.theme = "imported".into();
        assert!(settings.validate().is_err());
        settings.theme = "dark".into();
        settings.scroll_lines = 0;
        assert!(settings.validate().is_err());
    }
}
