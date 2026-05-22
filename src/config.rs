//! TOML configuration loaded from `$XDG_CONFIG_HOME/hyprsnap/config.toml`.

use std::collections::HashMap;
use std::path::{Path, PathBuf};

use anyhow::{Context as _, Result};
use serde::{Deserialize, Serialize};

use crate::cli::{ClipboardKind, SinkSpec};

/// Top-level configuration.
#[derive(Debug, Default, Clone, Deserialize, Serialize, PartialEq, Eq)]
#[serde(default)]
pub struct Config {
    /// Active UI language as a BCP-47 tag (e.g. `"fr"`, `"en-US"`). `None`
    /// (the default) lets HyprSnap auto-detect from `LC_ALL`/`LC_MESSAGES`/
    /// `LANG`. The `--lang` CLI flag overrides this field.
    pub language: Option<String>,
    pub output: OutputConfig,
    pub capture: CaptureConfig,
    pub clipboard: ClipboardConfig,
    pub keybinds: KeybindConfig,
    pub notify: NotifyConfig,
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
            filename_template: "hyprsnap_{ts}.png".to_owned(),
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

impl Config {
    /// Default config file path: `$XDG_CONFIG_HOME/hyprsnap/config.toml`.
    pub fn default_path() -> Option<PathBuf> {
        directories::ProjectDirs::from("ai", "hyprtools", "hyprsnap")
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

    #[test]
    fn defaults_are_valid() {
        let cfg = Config::default();
        assert_eq!(cfg.output.filename_template, "hyprsnap_{ts}.png");
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
        assert!(out.starts_with("hyprsnap_"));
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
}
