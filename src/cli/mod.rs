//! Command-line interface.
//!
//! Each subcommand has its own module under [`crate::cli`].

use std::path::PathBuf;

use clap::{Parser, Subcommand, ValueEnum};
use serde::{Deserialize, Serialize};

pub mod daemon;
pub mod doctor;
pub mod draw;
pub mod screenshot;

/// HyprSnap — capture, annotate, and draw on the screen for Hyprland.
#[derive(Debug, Parser)]
#[command(name = "hyprsnap", version, about, long_about = None)]
pub struct Cli {
    /// Path to an alternative configuration file.
    #[arg(long, global = true, value_name = "FILE", env = "HYPRSNAP_CONFIG")]
    pub config: Option<PathBuf>,

    /// Increase verbosity (-v = debug, -vv = trace). Overrides RUST_LOG when present.
    #[arg(short, long, global = true, action = clap::ArgAction::Count)]
    pub verbose: u8,

    /// Override the active language as a BCP-47 tag (e.g. `fr`, `en-US`).
    /// Falls back to the `language` config field, then `LC_ALL`/`LC_MESSAGES`/`LANG`,
    /// then English.
    #[arg(long, global = true, value_name = "BCP47", env = "HYPRSNAP_LANG")]
    pub lang: Option<String>,

    #[command(subcommand)]
    pub command: Option<Command>,
}

#[derive(Debug, Subcommand)]
pub enum Command {
    /// Capture the screen and write it to the configured sinks. Pass `--edit` to open the
    /// annotation editor between capture and sinks.
    Screenshot(screenshot::Args),
    /// Open a transparent overlay to draw on top of the screen.
    Draw(draw::Args),
    /// Run a long-lived daemon listening on the IPC socket.
    Daemon(daemon::Args),
    /// Print a copy-pasteable Markdown diagnostic report (config, environment,
    /// live capability probes). Always exits with status 0.
    Doctor(doctor::Args),
}

/// Where a captured / annotated image should be written.
#[derive(Debug, Clone, ValueEnum, PartialEq, Eq)]
pub enum SinkKind {
    /// Write to a file.
    File,
    /// Copy to the Wayland clipboard as `image/png`.
    Clipboard,
}

/// Which Wayland selection(s) the clipboard sink targets.
///
/// Wayland exposes two independent selections: the *regular* clipboard
/// (Ctrl+C / Ctrl+V) and the *primary* selection (middle-click paste). By
/// default hyprsnap publishes the screenshot to the regular clipboard
/// only — matching how most graphical apps treat copy/paste.
#[derive(Debug, Default, Copy, Clone, ValueEnum, Deserialize, Serialize, PartialEq, Eq)]
#[serde(rename_all = "kebab-case")]
#[clap(rename_all = "kebab-case")]
pub enum ClipboardKind {
    /// Regular clipboard (Ctrl+V). Default.
    #[default]
    Regular,
    /// Primary selection (middle-click paste).
    Primary,
    /// Publish to both selections.
    Both,
}

/// Output target specification (`--to file=PATH`, `--to clipboard`,
/// `--to clipboard=primary`, or `--to file`). The `clipboard=KIND` form
/// overrides the global `--clipboard-type` for this specific entry.
#[derive(Debug, Clone, PartialEq, Eq)]
pub enum SinkSpec {
    File(Option<PathBuf>),
    /// Wayland clipboard sink. `None` means "use the effective default
    /// kind" (resolved from `--clipboard-type` or config); `Some(kind)`
    /// pins the kind on this entry.
    Clipboard(Option<ClipboardKind>),
}

impl SinkSpec {
    /// Replace `Clipboard(None)` entries with `Clipboard(Some(default))`,
    /// resolving the effective kind for each clipboard sink. `Clipboard`
    /// entries that already pin a kind are left untouched, as are file
    /// sinks. Used after CLI / config / IPC parsing so downstream code
    /// only ever sees fully-resolved kinds.
    pub fn resolve_clipboard_default(self, default: ClipboardKind) -> Self {
        match self {
            SinkSpec::Clipboard(None) => SinkSpec::Clipboard(Some(default)),
            other => other,
        }
    }
}

impl std::str::FromStr for SinkSpec {
    type Err = String;

    fn from_str(s: &str) -> Result<Self, Self::Err> {
        let (kind, rest) = match s.split_once('=') {
            Some((k, v)) => (k, Some(v)),
            None => (s, None),
        };
        match kind {
            "file" => Ok(SinkSpec::File(rest.map(PathBuf::from))),
            "clipboard" => match rest {
                None => Ok(SinkSpec::Clipboard(None)),
                Some("regular") => Ok(SinkSpec::Clipboard(Some(ClipboardKind::Regular))),
                Some("primary") => Ok(SinkSpec::Clipboard(Some(ClipboardKind::Primary))),
                Some("both") => Ok(SinkSpec::Clipboard(Some(ClipboardKind::Both))),
                Some(other) => Err(format!(
                    "unknown clipboard kind `{other}` (expected `regular`, `primary`, or `both`)"
                )),
            },
            other => Err(format!(
                "unknown sink `{other}` (expected `file` or `clipboard`)"
            )),
        }
    }
}

/// Top-level dispatch.
pub async fn dispatch(cli: Cli) -> anyhow::Result<()> {
    // Captured before `cli` is consumed so subcommands that need it (currently only
    // `doctor`) can honor the global `--config` flag. Other subcommands still rely on
    // `Config::load_default()` internally — threading `--config` through them is tracked
    // separately.
    let config_override = cli.config.clone();
    let command = cli.command.unwrap_or_else(|| {
        // No subcommand → default to an interactive screenshot.
        Command::Screenshot(screenshot::Args::default())
    });
    match command {
        Command::Screenshot(args) if args.via_daemon => {
            dispatch_via_daemon(Command::Screenshot(args)).await
        }
        Command::Draw(args) if args.via_daemon => dispatch_via_daemon(Command::Draw(args)).await,
        Command::Screenshot(args) => screenshot::run(args).await,
        Command::Draw(args) => draw::run(args).await,
        Command::Daemon(args) => daemon::run(args).await,
        Command::Doctor(args) => doctor::run(args, config_override).await,
    }
}

/// Forward a `Command` to a running `hyprsnap daemon` over the IPC socket instead of executing
/// locally. Only `screenshot` (with or without `--edit`) and `draw` accept `--via-daemon`;
/// `daemon` rejects the flag at parse time.
async fn dispatch_via_daemon(command: Command) -> anyhow::Result<()> {
    use anyhow::{Context as _, anyhow, bail};
    use tokio::io::{AsyncBufReadExt, AsyncWriteExt, BufReader};

    let request = build_request(command)?;
    let socket = crate::daemon::default_socket_path();
    let stream = tokio::net::UnixStream::connect(&socket)
        .await
        .with_context(|| format!("connecting to daemon at {}", socket.display()))?;
    let (read, mut write) = stream.into_split();
    let mut payload = serde_json::to_vec(&request)?;
    payload.push(b'\n');
    write.write_all(&payload).await?;
    write.shutdown().await.ok();

    let mut reader = BufReader::new(read);
    let mut line = String::new();
    reader
        .read_line(&mut line)
        .await
        .context("reading daemon response")?;
    if line.trim().is_empty() {
        bail!("{}", crate::i18n::fl!("error-daemon-no-response"));
    }
    let resp: crate::ipc::Response = serde_json::from_str(line.trim())
        .with_context(|| format!("parsing daemon response `{}`", line.trim()))?;
    match resp {
        crate::ipc::Response::Ok => Ok(()),
        crate::ipc::Response::Paths { paths } => {
            for p in &paths {
                println!("{}", p.display());
            }
            Ok(())
        }
        crate::ipc::Response::Error { message } => Err(anyhow!(
            "{}",
            crate::i18n::fl!("error-daemon-message", message = message)
        )),
    }
}

fn build_request(command: Command) -> anyhow::Result<crate::ipc::Request> {
    match command {
        Command::Screenshot(args) => {
            let selection = screenshot::parse_selection(&args)?;
            // Pre-apply `--clipboard-type` to any `--to clipboard` entries that didn't pin a
            // kind via `=KIND` syntax. Entries that did pin one are left untouched. Entries
            // that still carry `None` after this (because neither `--to clipboard=KIND` nor
            // `--clipboard-type` was supplied) will fall back to the daemon's own
            // `[clipboard].default_kind` config on the server side.
            let resolved: Vec<SinkSpec> = match args.clipboard_type {
                Some(kind) => args
                    .to
                    .iter()
                    .cloned()
                    .map(|s| s.resolve_clipboard_default(kind))
                    .collect(),
                None => args.to.clone(),
            };
            let sinks = crate::daemon::sinks_to_specs(&resolved);
            // CLI flag wins; the daemon side falls back to its own config-loaded default when
            // the wire field is None. Whole-seconds precision matches the UI countdown.
            let delay_secs = args.delay;
            Ok(crate::ipc::Request::Screenshot(
                crate::ipc::ScreenshotRequest {
                    selection: crate::daemon::selection_to_spec(&selection),
                    cursor: args.cursor,
                    edit: args.edit,
                    delay_secs,
                    sinks,
                },
            ))
        }
        Command::Draw(args) => {
            if args.toggle_passthrough {
                Ok(crate::ipc::Request::PassthroughToggle)
            } else {
                Ok(crate::ipc::Request::DrawToggle)
            }
        }
        Command::Daemon(_) => {
            unreachable!(
                "daemon never reaches build_request: --via-daemon is rejected at parse time"
            )
        }
        Command::Doctor(_) => {
            unreachable!("doctor never reaches build_request: it has no --via-daemon flag")
        }
    }
}

#[cfg(test)]
mod tests {
    use std::str::FromStr;

    use clap::Parser;
    use pretty_assertions::assert_eq;
    use rstest::rstest;

    use super::*;

    #[rstest]
    #[case::file_no_path("file", SinkSpec::File(None))]
    #[case::file_with_path("file=/tmp/x.png", SinkSpec::File(Some("/tmp/x.png".into())))]
    #[case::clipboard_bare("clipboard", SinkSpec::Clipboard(None))]
    #[case::clipboard_regular(
        "clipboard=regular",
        SinkSpec::Clipboard(Some(ClipboardKind::Regular))
    )]
    #[case::clipboard_primary(
        "clipboard=primary",
        SinkSpec::Clipboard(Some(ClipboardKind::Primary))
    )]
    #[case::clipboard_both("clipboard=both", SinkSpec::Clipboard(Some(ClipboardKind::Both)))]
    fn parses_sink_spec(#[case] input: &str, #[case] expected: SinkSpec) {
        assert_eq!(SinkSpec::from_str(input).unwrap(), expected);
    }

    #[rstest]
    #[case::unknown_kind("ftp")]
    #[case::clipboard_unknown_value("clipboard=foo")]
    #[case::clipboard_empty_value("clipboard=")]
    fn rejects_invalid_sink_spec(#[case] input: &str) {
        assert!(SinkSpec::from_str(input).is_err());
    }

    #[test]
    fn resolve_clipboard_default_pins_unspecified_kind() {
        assert_eq!(
            SinkSpec::Clipboard(None).resolve_clipboard_default(ClipboardKind::Primary),
            SinkSpec::Clipboard(Some(ClipboardKind::Primary))
        );
    }

    #[test]
    fn resolve_clipboard_default_preserves_explicit_kind() {
        assert_eq!(
            SinkSpec::Clipboard(Some(ClipboardKind::Regular))
                .resolve_clipboard_default(ClipboardKind::Primary),
            SinkSpec::Clipboard(Some(ClipboardKind::Regular))
        );
    }

    #[test]
    fn resolve_clipboard_default_ignores_file_sinks() {
        let f = SinkSpec::File(Some("/tmp/a.png".into()));
        assert_eq!(f.clone().resolve_clipboard_default(ClipboardKind::Both), f);
    }

    #[test]
    fn cli_parses_screenshot_full() {
        let cli = Cli::try_parse_from([
            "hyprsnap",
            "screenshot",
            "--full",
            "--to",
            "file=/tmp/a.png",
        ])
        .unwrap();
        assert!(matches!(cli.command, Some(Command::Screenshot(_))));
    }

    #[test]
    fn cli_allows_no_subcommand() {
        let cli = Cli::try_parse_from(["hyprsnap"]).unwrap();
        assert!(cli.command.is_none());
    }

    #[test]
    fn cli_parses_all_subcommands() {
        for args in [
            vec!["hyprsnap", "screenshot", "--full"],
            vec!["hyprsnap", "screenshot", "--edit"],
            vec!["hyprsnap", "draw"],
            vec!["hyprsnap", "daemon"],
            vec!["hyprsnap", "daemon", "--systray"],
            vec!["hyprsnap", "doctor"],
        ] {
            Cli::try_parse_from(&args).unwrap_or_else(|e| panic!("{args:?} -> {e}"));
        }
    }

    #[test]
    fn doctor_honors_global_config_flag() {
        let cli = Cli::try_parse_from(["hyprsnap", "--config", "/tmp/alt.toml", "doctor"]).unwrap();
        assert_eq!(
            cli.config.as_deref(),
            Some(std::path::Path::new("/tmp/alt.toml"))
        );
        assert!(matches!(cli.command, Some(Command::Doctor(_))));
    }

    #[test]
    fn via_daemon_accepted_on_screenshot_and_draw() {
        for args in [
            vec!["hyprsnap", "screenshot", "--via-daemon"],
            vec!["hyprsnap", "screenshot", "--full", "--via-daemon"],
            vec!["hyprsnap", "draw", "--via-daemon"],
        ] {
            Cli::try_parse_from(&args).unwrap_or_else(|e| panic!("{args:?} -> {e}"));
        }
    }

    #[test]
    fn via_daemon_rejected_on_daemon() {
        assert!(Cli::try_parse_from(["hyprsnap", "daemon", "--via-daemon"]).is_err());
    }

    #[test]
    fn via_daemon_is_not_global() {
        // Placing `--via-daemon` before the subcommand must fail now that it's no longer global.
        assert!(Cli::try_parse_from(["hyprsnap", "--via-daemon", "screenshot"]).is_err());
    }
}
