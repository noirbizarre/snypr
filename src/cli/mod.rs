//! Command-line interface.
//!
//! Each subcommand has its own module under [`crate::cli`].

use std::path::PathBuf;

use clap::{Parser, Subcommand, ValueEnum};

pub mod annotate;
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

    #[command(subcommand)]
    pub command: Option<Command>,
}

#[derive(Debug, Subcommand)]
pub enum Command {
    /// Capture the screen and write it to the configured sinks. Pass `--edit` to open the
    /// annotation editor between capture and sinks.
    Screenshot(screenshot::Args),
    /// Open the annotation editor on an existing image.
    Annotate(annotate::Args),
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
        Command::Screenshot(args) if args.via_daemon => {
            dispatch_via_daemon(Command::Screenshot(args)).await
        }
        Command::Draw(args) if args.via_daemon => dispatch_via_daemon(Command::Draw(args)).await,
        Command::Screenshot(args) => screenshot::run(args).await,
        Command::Annotate(args) => annotate::run(args).await,
        Command::Draw(args) => draw::run(args).await,
        Command::Daemon(args) => daemon::run(args).await,
    }
}

/// Forward a `Command` to a running `hyprsnap daemon` over the IPC socket instead of executing
/// locally. Only `screenshot` (with or without `--edit`) and `draw` accept `--via-daemon`;
/// `annotate` and `daemon` reject the flag at parse time.
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
        bail!("daemon closed connection without responding");
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
        crate::ipc::Response::Error { message } => Err(anyhow!("daemon: {message}")),
    }
}

fn build_request(command: Command) -> anyhow::Result<crate::ipc::Request> {
    match command {
        Command::Screenshot(args) => {
            let selection = screenshot::parse_selection(&args)?;
            let sinks = crate::daemon::sinks_to_specs(&args.to);
            Ok(crate::ipc::Request::Screenshot(
                crate::ipc::ScreenshotRequest {
                    selection: crate::daemon::selection_to_spec(&selection),
                    cursor: args.cursor,
                    edit: args.edit,
                    sinks,
                },
            ))
        }
        Command::Draw(_) => Ok(crate::ipc::Request::DrawToggle),
        Command::Annotate(_) | Command::Daemon(_) => {
            unreachable!(
                "annotate/daemon never reach build_request: --via-daemon is rejected at parse time"
            )
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
            vec!["hyprsnap", "screenshot", "--edit"],
            vec!["hyprsnap", "annotate", "/tmp/x.png"],
            vec!["hyprsnap", "draw"],
            vec!["hyprsnap", "daemon"],
            vec!["hyprsnap", "daemon", "--systray"],
        ] {
            Cli::try_parse_from(&args).unwrap_or_else(|e| panic!("{args:?} -> {e}"));
        }
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
    fn via_daemon_rejected_on_annotate_and_daemon() {
        for args in [
            vec!["hyprsnap", "annotate", "/tmp/x.png", "--via-daemon"],
            vec!["hyprsnap", "daemon", "--via-daemon"],
        ] {
            assert!(
                Cli::try_parse_from(&args).is_err(),
                "expected parse failure for {args:?}",
            );
        }
    }

    #[test]
    fn via_daemon_is_not_global() {
        // Placing `--via-daemon` before the subcommand must fail now that it's no longer global.
        assert!(Cli::try_parse_from(["hyprsnap", "--via-daemon", "screenshot"]).is_err());
    }
}
