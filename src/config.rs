//! TOML configuration loaded from `$XDG_CONFIG_HOME/hyprsnap/config.toml`.

use std::collections::HashMap;
use std::path::{Path, PathBuf};

use anyhow::{Context as _, Result};
use serde::{Deserialize, Serialize};

use crate::cli::SinkSpec;

/// Top-level configuration.
#[derive(Debug, Default, Clone, Deserialize, Serialize, PartialEq, Eq)]
#[serde(default)]
pub struct Config {
    pub output: OutputConfig,
    pub capture: CaptureConfig,
    pub keybinds: KeybindConfig,
    pub tray: TrayConfig,
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
}

impl Default for OutputConfig {
    fn default() -> Self {
        Self {
            directory: None,
            filename_template: "hyprsnap_{ts}.png".to_owned(),
            default_sinks: vec!["file".to_owned()],
            use_utc: false,
        }
    }
}

#[derive(Debug, Default, Clone, Deserialize, Serialize, PartialEq, Eq)]
#[serde(default)]
pub struct CaptureConfig {
    /// Include the cursor by default.
    pub cursor: bool,
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

#[derive(Debug, Default, Clone, Deserialize, Serialize, PartialEq, Eq)]
#[serde(default)]
pub struct TrayConfig {
    pub enabled: bool,
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

    /// Parse the configured default sinks.
    pub fn default_sinks(&self) -> Vec<SinkSpec> {
        self.output
            .default_sinks
            .iter()
            .filter_map(|s| s.parse::<SinkSpec>().ok())
            .collect()
    }

    /// Expand the filename template using the supplied context.
    pub fn expand_filename(&self, ctx: &FilenameContext<'_>) -> String {
        expand_template(&self.output.filename_template, ctx, self.output.use_utc)
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
            vec![SinkSpec::File(None), SinkSpec::Clipboard]
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
}
