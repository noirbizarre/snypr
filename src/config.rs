//! TOML configuration loaded from `$XDG_CONFIG_HOME/snypr/config.toml`.

use std::collections::HashMap;
use std::path::{Path, PathBuf};

use anyhow::{Context as _, Result};
use serde::{Deserialize, Deserializer, Serialize, Serializer};

use crate::cli::{ClipboardKind, SinkSpec};

/// Top-level configuration.
#[derive(Debug, Default, Clone, Deserialize, Serialize, PartialEq, Eq)]
#[serde(default)]
pub struct Config {
    /// Active UI language as a BCP-47 tag (e.g. `"fr"`, `"en-US"`). `None`
    /// (the default) lets Snypr auto-detect from `LC_ALL`/`LC_MESSAGES`/
    /// `LANG`. The `--lang` CLI flag overrides this field.
    pub language: Option<String>,
    pub output: OutputConfig,
    pub capture: CaptureConfig,
    pub clipboard: ClipboardConfig,
    pub keybinds: KeybindConfig,
    pub notify: NotifyConfig,
    pub ui: UiConfig,
    pub annotate: AnnotateConfig,
}

/// Desktop-notification preferences. Notifications are best-effort: failures to talk to the
/// notification daemon are logged at `debug!` and otherwise ignored, regardless of these flags.
#[derive(Debug, Clone, Deserialize, Serialize, PartialEq, Eq)]
#[serde(default)]
pub struct NotifyConfig {
    /// Emit a desktop notification (with the screenshot as a thumbnail) on success.
    pub success: bool,
    /// Emit a desktop notification on a fatal error.
    pub error: bool,
    /// Notification expiry timeout in milliseconds.
    pub timeout_ms: u32,
}

impl Default for NotifyConfig {
    fn default() -> Self {
        Self {
            success: true,
            error: true,
            timeout_ms: 6000,
        }
    }
}

#[derive(Debug, Clone, Deserialize, Serialize, PartialEq, Eq)]
#[serde(default)]
pub struct OutputConfig {
    /// Directory to save screenshots into. Defaults to `$XDG_PICTURES_DIR/Screenshots`.
    pub directory: Option<PathBuf>,
    /// Filename template. Supports `{ts}`, `{date}`, `{time}`, `{output}`, `{selection}`.
    pub filename_template: String,
    /// Default sinks when `--to` is not provided.
    pub default_sinks: Vec<String>,
    /// Use UTC instead of the local timezone in `{ts}`, `{date}`, `{time}`.
    pub use_utc: bool,
    /// PNG compression preset. Trades encode time for file size.
    pub compression: PngCompression,
}

/// PNG encoder preset. Maps to `(image::codecs::png::CompressionType, FilterType)` in
/// [`crate::output::encode_png`].
///
/// * `Fast` — `CompressionType::Fast` + `FilterType::NoFilter`. ~5x larger files than `Best`
///   but encodes a 4K screenshot in well under a second. Original default.
/// * `Balanced` — `CompressionType::Default` + `FilterType::Adaptive`. ~30-50% smaller than
///   `Fast` for typical screenshots; encode time ~3-4x slower. Sensible all-rounder.
/// * `Best` — `CompressionType::Best` + `FilterType::Adaptive`. Smallest files miniz_oxide
///   can produce without invoking zopfli; ~10x slower than `Fast`.
#[derive(Debug, Default, Copy, Clone, Deserialize, Serialize, PartialEq, Eq)]
#[serde(rename_all = "lowercase")]
pub enum PngCompression {
    Fast,
    #[default]
    Balanced,
    Best,
}

impl Default for OutputConfig {
    fn default() -> Self {
        Self {
            directory: None,
            filename_template: "snypr_{ts}.png".to_owned(),
            default_sinks: vec!["file".to_owned()],
            use_utc: false,
            compression: PngCompression::default(),
        }
    }
}

#[derive(Debug, Default, Clone, Deserialize, Serialize, PartialEq, Eq)]
#[serde(default)]
pub struct CaptureConfig {
    /// Include the cursor by default.
    pub cursor: bool,
    /// Default delay before capture, in whole seconds, applied when `--delay` is not
    /// passed on the CLI. `None` or `0` means no delay. The UI countdown spinner only
    /// surfaces integer seconds, so the config / CLI representation matches.
    #[serde(default, with = "delay_secs_opt")]
    pub delay: Option<u32>,
    /// Mode button pre-selected when the interactive selector opens. Defaults to
    /// `screen` (per-monitor mode), matching the historical hardcoded behavior. The
    /// runtime mode buttons still let the user switch freely; this only affects the
    /// initial state.
    pub initial_mode: InitialMode,
}

/// Pre-selected mode button for the interactive selector. Mirrors
/// [`crate::ui::toolbar::ModeKind`] but lives in the config layer so that
/// `serde` does not bleed into the GTK-facing toolbar module. Convert with
/// `From<InitialMode> for ModeKind` at the boundary.
#[derive(Debug, Default, Copy, Clone, PartialEq, Eq, Hash, Deserialize, Serialize)]
#[serde(rename_all = "kebab-case")]
pub enum InitialMode {
    /// All monitors captured at once.
    Full,
    /// Focused monitor highlighted (historical default).
    #[default]
    Screen,
    /// Click-to-pick a window from the Hyprland client list.
    Window,
    /// Drag-to-pick a rectangle.
    Region,
}

/// Serde adapter that collapses `Some(0)` to `None` on the way in so a zero-second
/// delay round-trips as "no delay" rather than a vacuous one-second-zero sleep.
mod delay_secs_opt {
    use serde::{Deserialize, Deserializer, Serializer};

    pub fn serialize<S: Serializer>(value: &Option<u32>, ser: S) -> Result<S::Ok, S::Error> {
        match value {
            None => ser.serialize_none(),
            Some(0) => ser.serialize_none(),
            Some(n) => ser.serialize_u32(*n),
        }
    }

    pub fn deserialize<'de, D: Deserializer<'de>>(de: D) -> Result<Option<u32>, D::Error> {
        let opt: Option<u32> = Option::deserialize(de)?;
        Ok(match opt {
            None | Some(0) => None,
            Some(n) => Some(n),
        })
    }
}

/// Clipboard-sink defaults. Applied when `--to clipboard` is passed
/// without a `=KIND` suffix and the global `--clipboard-type` flag is
/// also absent. See [`crate::cli::ClipboardKind`].
#[derive(Debug, Default, Clone, Deserialize, Serialize, PartialEq, Eq)]
#[serde(default)]
pub struct ClipboardConfig {
    /// Default selection target for `--to clipboard`. Defaults to
    /// [`ClipboardKind::Regular`] (the Ctrl-V clipboard).
    pub default_kind: ClipboardKind,
}

#[derive(Debug, Clone, Deserialize, Serialize, PartialEq, Eq)]
#[serde(default)]
pub struct KeybindConfig {
    pub selector: HashMap<String, String>,
    pub editor: HashMap<String, String>,
    pub overlay: HashMap<String, String>,
}

impl Default for KeybindConfig {
    fn default() -> Self {
        let mut selector = HashMap::new();
        selector.insert("cancel".to_owned(), "Escape".to_owned());
        selector.insert("confirm".to_owned(), "Return".to_owned());

        let mut editor = HashMap::new();
        editor.insert("save".to_owned(), "<Ctrl>s".to_owned());
        editor.insert("copy".to_owned(), "<Ctrl>c".to_owned());
        editor.insert("quit".to_owned(), "Escape".to_owned());

        let mut overlay = HashMap::new();
        overlay.insert("toggle_passthrough".to_owned(), "p".to_owned());
        overlay.insert("snapshot".to_owned(), "s".to_owned());
        overlay.insert("quit".to_owned(), "Escape".to_owned());

        Self {
            selector,
            editor,
            overlay,
        }
    }
}

/// UI-styling overrides. Currently scopes only to the selector overlay; future
/// per-surface tables (`ui.editor`, `ui.overlay`, `ui.toolbar`) can live next
/// to it without breaking the namespace.
#[derive(Debug, Default, Clone, Deserialize, Serialize, PartialEq, Eq)]
#[serde(default)]
pub struct UiConfig {
    pub selector: SelectorStyleConfig,
}

/// Chrome colors painted by the region/full/screen/window selector and the
/// standalone pre-capture countdown window. Each field is an RGBA hex string
/// in TOML (`"#RRGGBB"` or `"#RRGGBBAA"`). Defaults live in
/// [`SelectorStyleConfig::default`].
#[derive(Debug, Clone, Deserialize, Serialize, PartialEq, Eq)]
#[serde(default)]
pub struct SelectorStyleConfig {
    /// Stroke color for the active / selected zone: the region rectangle,
    /// the currently selected screen (Screen mode), and the currently selected
    /// window (Window mode). Default: `#FFFFFFF2`.
    pub outline: Color,
    /// Stroke color for the *hovered* (not yet committed) zone outline in
    /// Screen and Window mode. Drawn with a thinner stroke than [`Self::outline`]
    /// so users can distinguish "about to pick" from "already picked".
    /// Defaults to the same value as `outline` (`#FFFFFFF2`) so existing
    /// configs render identically until this field is overridden.
    pub outline_hover: Color,
    /// Fill color for the region size legend (drawn inside the region rect)
    /// and the top-of-monitor hint text. Default: `#FFFFFFE6`.
    pub label: Color,
    /// Heavy veil painted outside the region rect, over non-selected screens
    /// in screen mode, and across the full surface in window mode. Default:
    /// `#0000008C`.
    pub dim_strong: Color,
    /// Veil painted across the whole monitor in region mode when the user has
    /// not started dragging a rectangle yet. Default: `#00000073`.
    pub dim_full: Color,
    /// Lighter veil painted in full mode and on the currently
    /// selected/hovered screen in screen mode. Default: `#00000040`.
    pub dim_light: Color,
    /// Fill for the pre-capture countdown numeral, used both by the selector
    /// path and the standalone countdown window. Default: `#FFFFFFF2`.
    pub countdown_fg: Color,
    /// Background of the standalone countdown window
    /// ([`crate::ui::countdown`]). Default: `#0000008C`.
    pub countdown_bg: Color,
}

impl Default for SelectorStyleConfig {
    fn default() -> Self {
        Self {
            outline: Color::from_rgba_f32(1.0, 1.0, 1.0, 0.95),
            outline_hover: Color::from_rgba_f32(1.0, 1.0, 1.0, 0.95),
            label: Color::from_rgba_f32(1.0, 1.0, 1.0, 0.9),
            dim_strong: Color::from_rgba_f32(0.0, 0.0, 0.0, 0.55),
            dim_full: Color::from_rgba_f32(0.0, 0.0, 0.0, 0.45),
            dim_light: Color::from_rgba_f32(0.0, 0.0, 0.0, 0.25),
            countdown_fg: Color::from_rgba_f32(1.0, 1.0, 1.0, 0.95),
            countdown_bg: Color::from_rgba_f32(0.0, 0.0, 0.0, 0.55),
        }
    }
}

/// 8-bit RGBA color, serialized as a `#RRGGBB` or `#RRGGBBAA` hex string.
///
/// Stored as bytes so [`Eq`] / [`Hash`] are derivable (unlike `gtk4::gdk::RGBA`,
/// which holds `f32` channels). Convert into a `gdk::RGBA` at the draw site
/// with [`Color::to_rgba`].
#[derive(Debug, Default, Copy, Clone, PartialEq, Eq, Hash)]
pub struct Color {
    pub r: u8,
    pub g: u8,
    pub b: u8,
    pub a: u8,
}

impl Color {
    /// Build a color from float RGBA channels in the `[0.0, 1.0]` range.
    /// Out-of-range components are clamped. Used by [`SelectorStyleConfig::default`].
    pub const fn from_rgba_f32(r: f32, g: f32, b: f32, a: f32) -> Self {
        Self {
            r: f32_to_u8(r),
            g: f32_to_u8(g),
            b: f32_to_u8(b),
            a: f32_to_u8(a),
        }
    }

    /// Convert to a `[f32; 4]` array (RGBA channels in the `[0.0, 1.0]` range).
    /// Used by the annotation canvas, which stores tool colors in this format
    /// for direct consumption by GSK render nodes.
    pub fn to_f32_array(self) -> [f32; 4] {
        [
            self.r as f32 / 255.0,
            self.g as f32 / 255.0,
            self.b as f32 / 255.0,
            self.a as f32 / 255.0,
        ]
    }

    /// Convert to a GDK `RGBA` for use with GSK snapshot draws.
    #[cfg(feature = "ui")]
    pub fn to_rgba(self) -> gtk4::gdk::RGBA {
        gtk4::gdk::RGBA::new(
            self.r as f32 / 255.0,
            self.g as f32 / 255.0,
            self.b as f32 / 255.0,
            self.a as f32 / 255.0,
        )
    }

    /// Render the color as a CSS `rgba(...)` literal suitable for embedding
    /// into a `gtk4::CssProvider` string.
    pub fn to_css_rgba(self) -> String {
        format!(
            "rgba({}, {}, {}, {:.4})",
            self.r,
            self.g,
            self.b,
            self.a as f32 / 255.0
        )
    }

    /// Render as a `#RRGGBB` (when fully opaque) or `#RRGGBBAA` hex literal.
    pub fn to_hex(self) -> String {
        if self.a == 0xFF {
            format!("#{:02X}{:02X}{:02X}", self.r, self.g, self.b)
        } else {
            format!("#{:02X}{:02X}{:02X}{:02X}", self.r, self.g, self.b, self.a)
        }
    }

    /// Strict hex-string parser. Accepts only `#RRGGBB` and `#RRGGBBAA` (no
    /// 3/4-digit short form, no named colors, no whitespace). Returns a
    /// human-readable error when the input is malformed.
    pub fn parse_hex(s: &str) -> Result<Self, String> {
        let Some(hex) = s.strip_prefix('#') else {
            return Err(format!(
                "color {s:?} must start with '#' and be #RRGGBB or #RRGGBBAA"
            ));
        };
        match hex.len() {
            6 => {
                let r = u8_from_hex(&hex[0..2], s)?;
                let g = u8_from_hex(&hex[2..4], s)?;
                let b = u8_from_hex(&hex[4..6], s)?;
                Ok(Self { r, g, b, a: 0xFF })
            }
            8 => {
                let r = u8_from_hex(&hex[0..2], s)?;
                let g = u8_from_hex(&hex[2..4], s)?;
                let b = u8_from_hex(&hex[4..6], s)?;
                let a = u8_from_hex(&hex[6..8], s)?;
                Ok(Self { r, g, b, a })
            }
            other => Err(format!(
                "color {s:?} has {other} hex digit(s); expected 6 (#RRGGBB) or 8 (#RRGGBBAA)"
            )),
        }
    }
}

const fn f32_to_u8(v: f32) -> u8 {
    let scaled = v * 255.0 + 0.5;
    if scaled <= 0.0 {
        0
    } else if scaled >= 255.0 {
        255
    } else {
        scaled as u8
    }
}

fn u8_from_hex(pair: &str, full: &str) -> Result<u8, String> {
    u8::from_str_radix(pair, 16)
        .map_err(|_| format!("color {full:?} contains non-hex digits in {pair:?}"))
}

impl Serialize for Color {
    fn serialize<S: Serializer>(&self, ser: S) -> Result<S::Ok, S::Error> {
        ser.serialize_str(&self.to_hex())
    }
}

impl<'de> Deserialize<'de> for Color {
    fn deserialize<D: Deserializer<'de>>(de: D) -> Result<Self, D::Error> {
        let s = String::deserialize(de)?;
        Color::parse_hex(&s).map_err(serde::de::Error::custom)
    }
}

/// Annotation-tool defaults applied when a fresh canvas is created.
///
/// Only the `colors` table is exposed for now; future per-tool defaults
/// (font sizes, stroke styles, …) can grow next to it without breaking
/// the namespace.
#[derive(Debug, Default, Clone, Deserialize, Serialize, PartialEq, Eq)]
#[serde(default)]
pub struct AnnotateConfig {
    /// Per-tool default stroke / fill colors. See [`AnnotateColors`].
    pub colors: AnnotateColors,
}

/// Initial color picked by each annotation tool when the editor / draw
/// overlay opens. Each field is an RGBA hex string in TOML (`"#RRGGBB"` or
/// `"#RRGGBBAA"`). Defaults match the historical hardcoded values in
/// `src/ui/canvas.rs::AnnotationCanvas::default`.
///
/// `Blur`, `Crop`, and `Redact` are intentionally absent: those tools have
/// no user-controllable color (the toolbar color picker disables itself
/// for them).
#[derive(Debug, Clone, Deserialize, Serialize, PartialEq, Eq)]
#[serde(default)]
pub struct AnnotateColors {
    /// Rectangle outline color. Default: `#FF0000` (opaque red).
    pub rect: Color,
    /// Ellipse outline color. Default: `#FF0000`.
    pub ellipse: Color,
    /// Arrow stroke + head fill color. Default: `#FF0000`.
    pub arrow: Color,
    /// Straight-line stroke color. Default: `#FF0000`.
    pub line: Color,
    /// Freehand stroke color. Default: `#FF0000`.
    pub freehand: Color,
    /// Highlight fill color. Default: `#FFFF0059` (translucent yellow).
    pub highlight: Color,
    /// Number badge background fill. Default: `#E61A1A` (dark red).
    pub number: Color,
    /// Text foreground color. Default: `#FFF333` (warm yellow).
    pub text: Color,
}

impl Default for AnnotateColors {
    fn default() -> Self {
        Self {
            rect: Color::from_rgba_f32(1.0, 0.0, 0.0, 1.0),
            ellipse: Color::from_rgba_f32(1.0, 0.0, 0.0, 1.0),
            arrow: Color::from_rgba_f32(1.0, 0.0, 0.0, 1.0),
            line: Color::from_rgba_f32(1.0, 0.0, 0.0, 1.0),
            freehand: Color::from_rgba_f32(1.0, 0.0, 0.0, 1.0),
            highlight: Color::from_rgba_f32(1.0, 1.0, 0.0, 0.35),
            number: Color::from_rgba_f32(0.9, 0.1, 0.1, 1.0),
            text: Color::from_rgba_f32(1.0, 0.95, 0.2, 1.0),
        }
    }
}

impl Config {
    /// Default config file path: `$XDG_CONFIG_HOME/snypr/config.toml`.
    pub fn default_path() -> Option<PathBuf> {
        // Matches `crate::ui::APP_ID` (`noirbizar.re.Snypr`) so the config, cache and
        // data directories all live under the same reverse-DNS identity.
        directories::ProjectDirs::from("re", "noirbizar", "snypr")
            .map(|d| d.config_dir().join("config.toml"))
    }

    /// Load the default configuration (returns defaults if the file is missing).
    pub fn load_default() -> Result<Self> {
        match Self::default_path() {
            Some(path) if path.exists() => Self::load(&path),
            _ => Ok(Self::default()),
        }
    }

    pub fn load(path: &Path) -> Result<Self> {
        let text = std::fs::read_to_string(path)
            .with_context(|| format!("reading config at {}", path.display()))?;
        let cfg: Self =
            toml::from_str(&text).with_context(|| format!("parsing TOML at {}", path.display()))?;
        Ok(cfg)
    }

    /// Load the configuration honoring an explicit `--config` / `SNYPR_CONFIG` override.
    ///
    /// An explicit override is loaded with [`load`](Self::load), which fails when the file is
    /// missing or malformed: asking for a specific config and silently getting the default
    /// one is exactly the failure mode this indirection exists to prevent. Without an
    /// override, [`load_default`](Self::load_default) applies, which tolerates an absent file.
    pub fn resolve(override_path: Option<&Path>) -> Result<Self> {
        match override_path {
            Some(path) => Self::load(path),
            None => Self::load_default(),
        }
    }

    /// Resolved default save directory.
    ///
    /// - If `[output].directory` is set in the config, it is used verbatim.
    /// - Otherwise, `<XDG_PICTURES_DIR>/Screenshots` (e.g. `~/Pictures/Screenshots`).
    /// - As a last resort (no XDG dirs available), the current directory.
    pub fn save_directory(&self) -> PathBuf {
        if let Some(dir) = &self.output.directory {
            return dir.clone();
        }
        directories::UserDirs::new()
            .and_then(|d| d.picture_dir().map(Path::to_path_buf))
            .map(|p| p.join("Screenshots"))
            .unwrap_or_else(|| PathBuf::from("."))
    }

    /// Parse the configured default sinks. Clipboard entries inherit
    /// the configured [`ClipboardConfig::default_kind`] so callers see
    /// a fully-resolved kind.
    pub fn default_sinks(&self) -> Vec<SinkSpec> {
        let kind = self.clipboard.default_kind;
        self.output
            .default_sinks
            .iter()
            .filter_map(|s| s.parse::<SinkSpec>().ok())
            .map(|s| s.resolve_clipboard_default(kind))
            .collect()
    }

    /// Expand the filename template using the supplied context.
    pub fn expand_filename(&self, ctx: &FilenameContext<'_>) -> String {
        expand_template(&self.output.filename_template, ctx, self.output.use_utc)
    }

    /// Like [`Self::expand_filename`], but ensures the file basename varies with the output
    /// name — used by `--per-output` so multiple files don't collapse onto the same path when
    /// the user's template lacks `{output}`.
    pub fn expand_filename_per_output(&self, ctx: &FilenameContext<'_>) -> String {
        let template = if self.output.filename_template.contains("{output}") {
            self.output.filename_template.clone()
        } else {
            // Insert `-{output}` before the final extension, or append it if there is none.
            match self.output.filename_template.rsplit_once('.') {
                Some((stem, ext)) => format!("{stem}-{{output}}.{ext}"),
                None => format!("{}-{{output}}", self.output.filename_template),
            }
        };
        expand_template(&template, ctx, self.output.use_utc)
    }
}

#[derive(Debug, Default, Clone, Copy)]
pub struct FilenameContext<'a> {
    pub output: Option<&'a str>,
    pub selection: Option<&'a str>,
}

fn expand_template(template: &str, ctx: &FilenameContext<'_>, utc: bool) -> String {
    let now = if utc {
        chrono::Utc::now().format("%Y%m%d-%H%M%S").to_string()
    } else {
        chrono::Local::now().format("%Y%m%d-%H%M%S").to_string()
    };
    let date = if utc {
        chrono::Utc::now().format("%Y-%m-%d").to_string()
    } else {
        chrono::Local::now().format("%Y-%m-%d").to_string()
    };
    let time = if utc {
        chrono::Utc::now().format("%H%M%S").to_string()
    } else {
        chrono::Local::now().format("%H%M%S").to_string()
    };

    template
        .replace("{ts}", &now)
        .replace("{date}", &date)
        .replace("{time}", &time)
        .replace("{output}", ctx.output.unwrap_or("all"))
        .replace("{selection}", ctx.selection.unwrap_or("snap"))
}

#[cfg(test)]
mod tests {
    use super::*;
    use pretty_assertions::assert_eq;
    use rstest::rstest;

    #[test]
    fn defaults_are_valid() {
        let cfg = Config::default();
        assert_eq!(cfg.output.filename_template, "snypr_{ts}.png");
        assert_eq!(cfg.default_sinks(), vec![SinkSpec::File(None)]);
    }

    #[test]
    fn parses_partial_config() {
        let toml = r#"
            [output]
            filename_template = "shot_{date}_{output}.png"
            default_sinks = ["file", "clipboard"]
        "#;
        let cfg: Config = toml::from_str(toml).unwrap();
        assert_eq!(cfg.output.filename_template, "shot_{date}_{output}.png");
        assert_eq!(
            cfg.default_sinks(),
            vec![
                SinkSpec::File(None),
                SinkSpec::Clipboard(Some(ClipboardKind::Regular))
            ]
        );
    }

    #[test]
    fn template_expansion_substitutes_tokens() {
        let cfg = Config {
            output: OutputConfig {
                filename_template: "{output}_{selection}.png".into(),
                ..Default::default()
            },
            ..Default::default()
        };
        let out = cfg.expand_filename(&FilenameContext {
            output: Some("DP-1"),
            selection: Some("region"),
        });
        assert_eq!(out, "DP-1_region.png");
    }

    #[test]
    fn template_expansion_uses_defaults_when_missing() {
        let cfg = Config::default();
        let out = cfg.expand_filename(&FilenameContext::default());
        assert!(out.starts_with("snypr_"));
        assert!(out.ends_with(".png"));
    }

    #[test]
    fn rejects_invalid_toml() {
        let dir = tempfile::tempdir().unwrap();
        let path = dir.path().join("bad.toml");
        std::fs::write(&path, "not = a [valid").unwrap();
        let err = Config::load(&path).unwrap_err();
        assert!(format!("{err:#}").contains("parsing TOML"));
    }

    #[test]
    fn resolve_reads_the_override_path() {
        let dir = tempfile::tempdir().unwrap();
        let path = dir.path().join("alt.toml");
        std::fs::write(&path, "language = \"fr\"\n").unwrap();
        let cfg = Config::resolve(Some(&path)).unwrap();
        assert_eq!(cfg.language.as_deref(), Some("fr"));
    }

    #[test]
    fn resolve_fails_loudly_on_a_missing_override() {
        let dir = tempfile::tempdir().unwrap();
        let missing = dir.path().join("nope.toml");
        // Falling back to the default config here would silently ignore an explicit
        // `--config`, which is precisely the bug `resolve` exists to prevent.
        let err = Config::resolve(Some(&missing)).unwrap_err();
        assert!(format!("{err:#}").contains("reading config"));
    }

    #[test]
    fn notify_defaults_enable_both_channels() {
        let cfg = Config::default();
        assert!(cfg.notify.success);
        assert!(cfg.notify.error);
        assert_eq!(cfg.notify.timeout_ms, 6000);
    }

    #[test]
    fn capture_delay_defaults_to_none() {
        let cfg = Config::default();
        assert!(cfg.capture.delay.is_none());
    }

    #[test]
    fn capture_delay_parses_integer_seconds() {
        let toml = r#"
            [capture]
            delay = 3
        "#;
        let cfg: Config = toml::from_str(toml).unwrap();
        assert_eq!(cfg.capture.delay, Some(3));
    }

    #[test]
    fn capture_delay_zero_collapses_to_none() {
        let toml = r#"
            [capture]
            delay = 0
        "#;
        let cfg: Config = toml::from_str(toml).unwrap();
        assert!(cfg.capture.delay.is_none());
    }

    #[test]
    fn capture_delay_round_trips_via_toml() {
        let mut cfg = Config::default();
        cfg.capture.delay = Some(10);
        let text = toml::to_string(&cfg).unwrap();
        let back: Config = toml::from_str(&text).unwrap();
        assert_eq!(back.capture.delay, cfg.capture.delay);
    }

    #[test]
    fn capture_delay_rejects_non_integer() {
        let toml = r#"
            [capture]
            delay = "3s"
        "#;
        let err = toml::from_str::<Config>(toml).unwrap_err();
        let msg = format!("{err}");
        assert!(msg.contains("integer") || msg.contains("expected"), "{msg}");
    }

    #[test]
    fn clipboard_defaults_to_regular() {
        let cfg = Config::default();
        assert_eq!(cfg.clipboard.default_kind, ClipboardKind::Regular);
    }

    #[test]
    fn clipboard_default_kind_propagates_to_default_sinks() {
        let toml = r#"
            [clipboard]
            default_kind = "primary"

            [output]
            default_sinks = ["clipboard"]
        "#;
        let cfg: Config = toml::from_str(toml).unwrap();
        assert_eq!(cfg.clipboard.default_kind, ClipboardKind::Primary);
        assert_eq!(
            cfg.default_sinks(),
            vec![SinkSpec::Clipboard(Some(ClipboardKind::Primary))]
        );
    }

    #[test]
    fn clipboard_section_round_trips() {
        let toml = r#"
            [clipboard]
            default_kind = "both"
        "#;
        let cfg: Config = toml::from_str(toml).unwrap();
        assert_eq!(cfg.clipboard.default_kind, ClipboardKind::Both);
    }

    #[test]
    fn notify_section_round_trips() {
        let toml = r#"
            [notify]
            success = false
            error = true
            timeout_ms = 2500
        "#;
        let cfg: Config = toml::from_str(toml).unwrap();
        assert!(!cfg.notify.success);
        assert!(cfg.notify.error);
        assert_eq!(cfg.notify.timeout_ms, 2500);
    }

    #[test]
    fn color_parses_six_and_eight_digit_hex() {
        assert_eq!(
            Color::parse_hex("#FF8000").unwrap(),
            Color {
                r: 0xFF,
                g: 0x80,
                b: 0x00,
                a: 0xFF
            }
        );
        assert_eq!(
            Color::parse_hex("#01020304").unwrap(),
            Color {
                r: 0x01,
                g: 0x02,
                b: 0x03,
                a: 0x04
            }
        );
    }

    #[test]
    fn color_parser_is_case_insensitive() {
        assert_eq!(
            Color::parse_hex("#aabbcc").unwrap(),
            Color::parse_hex("#AABBCC").unwrap(),
        );
    }

    #[test]
    fn color_parser_rejects_short_and_named_forms() {
        for bad in [
            "fff", "#fff", "#ffff", "#fffff", "#fffffff", "red", "#GGHHII",
        ] {
            assert!(
                Color::parse_hex(bad).is_err(),
                "{bad:?} should not parse as a Color"
            );
        }
    }

    #[test]
    fn color_hex_round_trips() {
        let c = Color {
            r: 0x12,
            g: 0x34,
            b: 0x56,
            a: 0x78,
        };
        assert_eq!(c.to_hex(), "#12345678");
        assert_eq!(Color::parse_hex(&c.to_hex()).unwrap(), c);

        let opaque = Color {
            r: 0xAB,
            g: 0xCD,
            b: 0xEF,
            a: 0xFF,
        };
        assert_eq!(opaque.to_hex(), "#ABCDEF");
        assert_eq!(Color::parse_hex(&opaque.to_hex()).unwrap(), opaque);
    }

    #[test]
    fn selector_style_defaults_match_legacy_literals() {
        let s = SelectorStyleConfig::default();
        // Values mirror the literals previously hardcoded in src/ui/selector.rs.
        assert_eq!(s.outline, Color::from_rgba_f32(1.0, 1.0, 1.0, 0.95));
        // `outline_hover` opts into the same default as `outline` so configs
        // that pre-date the field render identically.
        assert_eq!(s.outline_hover, s.outline);
        assert_eq!(s.label, Color::from_rgba_f32(1.0, 1.0, 1.0, 0.9));
        assert_eq!(s.dim_strong, Color::from_rgba_f32(0.0, 0.0, 0.0, 0.55));
        assert_eq!(s.dim_full, Color::from_rgba_f32(0.0, 0.0, 0.0, 0.45));
        assert_eq!(s.dim_light, Color::from_rgba_f32(0.0, 0.0, 0.0, 0.25));
        assert_eq!(s.countdown_fg, Color::from_rgba_f32(1.0, 1.0, 1.0, 0.95));
        assert_eq!(s.countdown_bg, Color::from_rgba_f32(0.0, 0.0, 0.0, 0.55));
    }

    #[test]
    fn outline_hover_falls_back_to_default_when_omitted() {
        let toml = r##"
            [ui.selector]
            outline = "#FF00FFFF"
        "##;
        let cfg: Config = toml::from_str(toml).unwrap();
        assert_eq!(
            cfg.ui.selector.outline_hover,
            SelectorStyleConfig::default().outline_hover
        );
    }

    #[test]
    fn ui_selector_section_parses_partial_overrides() {
        let toml = r##"
            [ui.selector]
            outline      = "#FF00FFFF"
            dim_strong   = "#00008080"
        "##;
        let cfg: Config = toml::from_str(toml).unwrap();
        assert_eq!(
            cfg.ui.selector.outline,
            Color {
                r: 0xFF,
                g: 0x00,
                b: 0xFF,
                a: 0xFF
            }
        );
        assert_eq!(
            cfg.ui.selector.dim_strong,
            Color {
                r: 0x00,
                g: 0x00,
                b: 0x80,
                a: 0x80
            }
        );
        // Untouched fields keep their defaults.
        assert_eq!(cfg.ui.selector.label, SelectorStyleConfig::default().label);
        assert_eq!(
            cfg.ui.selector.countdown_bg,
            SelectorStyleConfig::default().countdown_bg
        );
    }

    #[test]
    fn ui_selector_rejects_malformed_color() {
        let toml = r##"
            [ui.selector]
            outline = "not-a-color"
        "##;
        let err = toml::from_str::<Config>(toml).unwrap_err();
        let msg = format!("{err}");
        assert!(msg.contains("must start with '#'"), "{msg}");
    }

    #[test]
    fn ui_section_round_trips_via_toml() {
        let mut cfg = Config::default();
        cfg.ui.selector.outline = Color {
            r: 0xAA,
            g: 0xBB,
            b: 0xCC,
            a: 0xDD,
        };
        let text = toml::to_string(&cfg).unwrap();
        let back: Config = toml::from_str(&text).unwrap();
        assert_eq!(back.ui, cfg.ui);
    }

    #[test]
    fn initial_mode_defaults_to_screen() {
        let cfg = Config::default();
        assert_eq!(cfg.capture.initial_mode, InitialMode::Screen);
    }

    #[test]
    fn initial_mode_parses_kebab_case_variants() {
        for (raw, expected) in [
            ("full", InitialMode::Full),
            ("screen", InitialMode::Screen),
            ("window", InitialMode::Window),
            ("region", InitialMode::Region),
        ] {
            let toml = format!("[capture]\ninitial_mode = \"{raw}\"\n");
            let cfg: Config = toml::from_str(&toml).unwrap();
            assert_eq!(cfg.capture.initial_mode, expected, "raw = {raw:?}");
        }
    }

    #[test]
    fn initial_mode_rejects_unknown_value() {
        let toml = r#"
            [capture]
            initial_mode = "wat"
        "#;
        let err = toml::from_str::<Config>(toml).unwrap_err();
        let msg = format!("{err}");
        assert!(
            msg.contains("unknown variant") || msg.contains("expected"),
            "{msg}"
        );
    }

    #[test]
    fn annotate_colors_defaults_match_canvas_literals() {
        // Values mirror the literals previously hardcoded in
        // `src/ui/canvas.rs::AnnotationCanvas::default`.
        let c = AnnotateColors::default();
        assert_eq!(c.rect.to_f32_array(), [1.0, 0.0, 0.0, 1.0]);
        assert_eq!(c.ellipse.to_f32_array(), [1.0, 0.0, 0.0, 1.0]);
        assert_eq!(c.arrow.to_f32_array(), [1.0, 0.0, 0.0, 1.0]);
        assert_eq!(c.line.to_f32_array(), [1.0, 0.0, 0.0, 1.0]);
        assert_eq!(c.freehand.to_f32_array(), [1.0, 0.0, 0.0, 1.0]);
        // Highlight alpha rounds to 0x59 (=89) at u8 precision (0.35 * 255 + 0.5 = 89.75).
        assert_eq!(c.highlight.r, 0xFF);
        assert_eq!(c.highlight.g, 0xFF);
        assert_eq!(c.highlight.b, 0x00);
        assert_eq!(c.highlight.a, 0x59);
        assert_eq!(c.number.r, 0xE6);
        assert_eq!(c.number.g, 0x1A);
        assert_eq!(c.number.b, 0x1A);
        assert_eq!(c.number.a, 0xFF);
        assert_eq!(c.text.a, 0xFF);
    }

    #[test]
    fn annotate_section_parses_partial_override() {
        let toml = r##"
            [annotate.colors]
            rect      = "#00FF00"
            highlight = "#00FFFF80"
        "##;
        let cfg: Config = toml::from_str(toml).unwrap();
        assert_eq!(
            cfg.annotate.colors.rect,
            Color {
                r: 0x00,
                g: 0xFF,
                b: 0x00,
                a: 0xFF
            }
        );
        assert_eq!(
            cfg.annotate.colors.highlight,
            Color {
                r: 0x00,
                g: 0xFF,
                b: 0xFF,
                a: 0x80
            }
        );
        // Untouched fields keep their defaults.
        assert_eq!(cfg.annotate.colors.text, AnnotateColors::default().text);
    }

    #[test]
    fn annotate_section_round_trips_via_toml() {
        let mut cfg = Config::default();
        cfg.annotate.colors.arrow = Color {
            r: 0x12,
            g: 0x34,
            b: 0x56,
            a: 0xFF,
        };
        let text = toml::to_string(&cfg).unwrap();
        let back: Config = toml::from_str(&text).unwrap();
        assert_eq!(back.annotate, cfg.annotate);
    }

    #[test]
    fn annotate_rejects_malformed_color() {
        let toml = r##"
            [annotate.colors]
            rect = "tomato"
        "##;
        let err = toml::from_str::<Config>(toml).unwrap_err();
        let msg = format!("{err}");
        assert!(msg.contains("must start with '#'"), "{msg}");
    }

    fn cfg_with_template(template: &str) -> Config {
        Config {
            output: OutputConfig {
                filename_template: template.into(),
                ..Default::default()
            },
            ..Default::default()
        }
    }

    #[rstest]
    // Already contains the token: used verbatim, wherever the token sits.
    #[case("{output}_{selection}.png", "DP-1_region.png")]
    #[case("shot-{output}.png", "shot-DP-1.png")]
    // No token but an extension: `-{output}` goes before the *last* dot.
    #[case("shot.png", "shot-DP-1.png")]
    #[case("my.shot.png", "my.shot-DP-1.png")]
    // No token and no dot at all: appended.
    #[case("shot", "shot-DP-1")]
    fn per_output_template_always_varies_with_the_output(
        #[case] template: &str,
        #[case] expected: &str,
    ) {
        let out = cfg_with_template(template).expand_filename_per_output(&FilenameContext {
            output: Some("DP-1"),
            selection: Some("region"),
        });
        assert_eq!(out, expected);
    }

    #[test]
    fn per_output_filenames_do_not_collide() {
        let cfg = cfg_with_template("shot.png");
        let a = cfg.expand_filename_per_output(&FilenameContext {
            output: Some("DP-1"),
            ..Default::default()
        });
        let b = cfg.expand_filename_per_output(&FilenameContext {
            output: Some("DP-2"),
            ..Default::default()
        });
        assert_ne!(a, b);
    }

    #[rstest]
    #[case(Color { r: 0, g: 0, b: 0, a: 0 }, [0.0, 0.0, 0.0, 0.0])]
    #[case(Color { r: 255, g: 255, b: 255, a: 255 }, [1.0, 1.0, 1.0, 1.0])]
    #[case(Color { r: 255, g: 0, b: 51, a: 128 }, [1.0, 0.0, 0.2, 128.0 / 255.0])]
    fn color_to_f32_array_normalises_channels(#[case] c: Color, #[case] expected: [f32; 4]) {
        let got = c.to_f32_array();
        for (i, (g, e)) in got.iter().zip(expected.iter()).enumerate() {
            assert!((g - e).abs() < 1e-6, "channel {i}: {g} != {e}");
        }
    }

    #[rstest]
    #[case(Color { r: 255, g: 0, b: 0, a: 255 }, "rgba(255, 0, 0, 1.0000)")]
    #[case(Color { r: 1, g: 2, b: 3, a: 0 }, "rgba(1, 2, 3, 0.0000)")]
    #[case(Color { r: 0, g: 0, b: 0, a: 128 }, "rgba(0, 0, 0, 0.5020)")]
    fn color_to_css_rgba_formats_alpha_with_four_decimals(
        #[case] c: Color,
        #[case] expected: &str,
    ) {
        assert_eq!(c.to_css_rgba(), expected);
    }

    #[rstest]
    // Saturating and rounding behaviour of the private `f32_to_u8` helper.
    #[case(-1.0, 0)]
    #[case(0.0, 0)]
    #[case(1.0, 255)]
    #[case(2.0, 255)]
    #[case(0.5, 128)]
    fn color_from_f32_clamps_and_rounds(#[case] v: f32, #[case] expected: u8) {
        assert_eq!(Color::from_rgba_f32(v, v, v, v).r, expected);
    }

    #[test]
    fn color_f32_round_trips_through_to_f32_array() {
        let c = Color {
            r: 12,
            g: 34,
            b: 56,
            a: 78,
        };
        let [r, g, b, a] = c.to_f32_array();
        assert_eq!(Color::from_rgba_f32(r, g, b, a), c);
    }

    #[test]
    fn keybind_defaults_cover_every_surface() {
        let k = KeybindConfig::default();
        assert_eq!(k.selector.get("cancel").map(String::as_str), Some("Escape"));
        assert_eq!(
            k.selector.get("confirm").map(String::as_str),
            Some("Return")
        );
        assert_eq!(k.editor.get("save").map(String::as_str), Some("<Ctrl>s"));
        assert_eq!(k.editor.get("copy").map(String::as_str), Some("<Ctrl>c"));
        assert_eq!(k.editor.get("quit").map(String::as_str), Some("Escape"));
        assert_eq!(k.overlay.get("snapshot").map(String::as_str), Some("s"));
        assert_eq!(
            k.overlay.get("toggle_passthrough").map(String::as_str),
            Some("p")
        );
        assert_eq!(k.overlay.get("quit").map(String::as_str), Some("Escape"));
    }

    #[test]
    fn keybind_config_round_trips_through_toml() {
        let k = KeybindConfig::default();
        let text = toml::to_string(&k).unwrap();
        assert_eq!(toml::from_str::<KeybindConfig>(&text).unwrap(), k);
    }

    /// A partial `[keybinds]` table must not wipe the other surfaces — that's what the
    /// `#[serde(default)]` on the struct buys us.
    #[test]
    fn partial_keybind_table_keeps_the_other_surfaces_at_their_defaults() {
        let cfg: Config = toml::from_str(
            r#"
            [keybinds.selector]
            cancel = "q"
        "#,
        )
        .unwrap();
        assert_eq!(
            cfg.keybinds.selector.get("cancel").map(String::as_str),
            Some("q")
        );
        assert_eq!(cfg.keybinds.editor, KeybindConfig::default().editor);
    }

    #[test]
    fn save_directory_uses_the_configured_directory_verbatim() {
        let cfg = Config {
            output: OutputConfig {
                directory: Some(PathBuf::from("/tmp/shots")),
                ..Default::default()
            },
            ..Default::default()
        };
        assert_eq!(cfg.save_directory(), PathBuf::from("/tmp/shots"));
    }

    /// With no `[output].directory`, the save path is derived from the XDG user-dirs
    /// definition of the pictures directory (read from `$XDG_CONFIG_HOME/user-dirs.dirs`),
    /// with `Screenshots` appended.
    #[test]
    fn save_directory_falls_back_to_the_xdg_pictures_dir() {
        let home = tempfile::tempdir().unwrap();
        let config_home = home.path().join(".config");
        std::fs::create_dir_all(&config_home).unwrap();
        std::fs::write(
            config_home.join("user-dirs.dirs"),
            "XDG_PICTURES_DIR=\"$HOME/Snaps\"\n",
        )
        .unwrap();
        // SAFETY: nextest runs each test in its own process, so no other thread observes
        // these env vars concurrently.
        unsafe {
            std::env::set_var("HOME", home.path());
            std::env::set_var("XDG_CONFIG_HOME", &config_home);
        }
        assert_eq!(
            Config::default().save_directory(),
            home.path().join("Snaps").join("Screenshots")
        );
    }

    /// Last-resort branch: when the environment defines no XDG pictures directory at all
    /// (no `user-dirs.dirs`, as on a bare CI runner), the save directory degrades to the
    /// current directory rather than guessing a location.
    #[test]
    fn save_directory_falls_back_to_the_current_directory() {
        let home = tempfile::tempdir().unwrap();
        let config_home = home.path().join(".config");
        std::fs::create_dir_all(&config_home).unwrap();
        // Deliberately no `user-dirs.dirs` inside `config_home`.
        // SAFETY: nextest runs each test in its own process.
        unsafe {
            std::env::set_var("HOME", home.path());
            std::env::set_var("XDG_CONFIG_HOME", &config_home);
        }
        assert_eq!(Config::default().save_directory(), PathBuf::from("."));
    }
}
