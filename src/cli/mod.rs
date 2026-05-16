//! Command-line interface.
//!
//! Each subcommand has its own module under [`crate::cli`].

use std::path::PathBuf;

use clap::{Parser, Subcommand, ValueEnum};

pub mod annotate;
pub mod capture;
pub mod daemon;
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

    /// Route the command through a running daemon instead of running locally.
    #[arg(long, global = true)]
    pub via_daemon: bool,

    #[command(subcommand)]
    pub command: Option<Command>,
}

#[derive(Debug, Subcommand)]
pub enum Command {
    /// Capture the screen and write it to the configured sinks.
    Screenshot(screenshot::Args),
    /// Open the annotation editor on an existing image.
    Annotate(annotate::Args),
    /// Capture, then open the annotation editor before writing the result.
    Capture(capture::Args),
    /// Open a transparent overlay to draw on top of the screen.
    Draw(draw::Args),
    /// Run a long-lived daemon listening on the IPC socket.
    Daemon(daemon::Args),
}

/// Where a captured / annotated image should be written.
#[derive(Debug, Clone, ValueEnum, PartialEq, Eq)]
pub enum SinkKind {
    /// Write to a file.
    File,
    /// Copy to the Wayland clipboard as `image/png`.
    Clipboard,
}

/// Output target specification (`--to file=PATH`, `--to clipboard`, or `--to file`).
#[derive(Debug, Clone, PartialEq, Eq)]
pub enum SinkSpec {
    File(Option<PathBuf>),
    Clipboard,
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
            "clipboard" => {
                if rest.is_some() {
                    Err(format!("`clipboard` sink does not take a value: {s}"))
                } else {
                    Ok(SinkSpec::Clipboard)
                }
            }
            other => Err(format!(
                "unknown sink `{other}` (expected `file` or `clipboard`)"
            )),
        }
    }
}

/// Top-level dispatch.
pub async fn dispatch(cli: Cli) -> anyhow::Result<()> {
    let command = cli.command.unwrap_or_else(|| {
        // No subcommand → default to an interactive screenshot.
        Command::Screenshot(screenshot::Args::default())
    });
    match command {
        Command::Screenshot(args) => screenshot::run(args).await,
        Command::Annotate(args) => annotate::run(args).await,
        Command::Capture(args) => capture::run(args).await,
        Command::Draw(args) => draw::run(args).await,
        Command::Daemon(args) => daemon::run(args).await,
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
    #[case::clipboard("clipboard", SinkSpec::Clipboard)]
    fn parses_sink_spec(#[case] input: &str, #[case] expected: SinkSpec) {
        assert_eq!(SinkSpec::from_str(input).unwrap(), expected);
    }

    #[rstest]
    #[case::unknown_kind("ftp")]
    #[case::clipboard_with_value("clipboard=foo")]
    fn rejects_invalid_sink_spec(#[case] input: &str) {
        assert!(SinkSpec::from_str(input).is_err());
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
            vec!["hyprsnap", "annotate", "/tmp/x.png"],
            vec!["hyprsnap", "capture"],
            vec!["hyprsnap", "draw"],
            vec!["hyprsnap", "daemon"],
        ] {
            Cli::try_parse_from(&args).unwrap_or_else(|e| panic!("{args:?} -> {e}"));
        }
    }
}
