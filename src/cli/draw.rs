//! `draw` subcommand — transparent live overlay for screen drawing.

use anyhow::Result;
use clap::Args as ClapArgs;

use super::{ClipboardKind, SinkSpec};

#[derive(Debug, ClapArgs)]
pub struct Args {
    /// Open the overlay with input passthrough enabled (clicks fall through).
    #[arg(long)]
    pub passthrough: bool,

    /// Toggle pointer passthrough on the currently running daemon-managed overlay instead
    /// of spawning one. Implies `--via-daemon` (and is meaningless without it). Bind to a
    /// Hyprland global keybind so users can flip passthrough back off when the overlay's
    /// own `P` shortcut is unreachable (passthrough mode detaches the surface from the
    /// keyboard).
    #[arg(long, conflicts_with = "passthrough", requires = "via_daemon")]
    pub toggle_passthrough: bool,

    /// Sink(s) to receive the image when the user presses Ctrl+S / Save in the overlay.
    /// Repeatable. When empty, falls back to `[output].default_sinks` from the config —
    /// same shape as `screenshot`'s `--to`.
    #[arg(long = "to", value_name = "SINK")]
    pub to: Vec<SinkSpec>,

    /// Default selection target for `--to clipboard` when no `=KIND`
    /// suffix is given on the entry itself. Precedence:
    /// `--to clipboard=KIND` > `--clipboard-type` > `[clipboard].default_kind`
    /// config > `regular`.
    #[arg(long, value_name = "KIND", value_enum)]
    pub clipboard_type: Option<ClipboardKind>,

    /// Include the mouse cursor in captures triggered by the overlay's Save action. The
    /// zone selector that pops on Save can still toggle this per-save via its own cursor
    /// button.
    #[arg(long)]
    pub cursor: bool,

    /// Route the command through a running daemon instead of running locally.
    #[arg(long)]
    pub via_daemon: bool,
}

#[cfg(feature = "ui")]
pub async fn run(args: Args) -> Result<()> {
    use anyhow::Context as _;

    use crate::config::Config;
    use crate::context::Context;
    use crate::ui::overlay::{OverlayMode, run as run_overlay};

    let config = Config::load_default().context("loading configuration")?;
    let ctx = Context::new(config).await?;
    let kind = crate::cli::screenshot::effective_clipboard_kind(args.clipboard_type, &ctx.config);
    let sinks: Vec<SinkSpec> = args
        .to
        .into_iter()
        .map(|s| s.resolve_clipboard_default(kind))
        .collect();
    let _ = run_overlay(
        ctx,
        OverlayMode::Draw {
            passthrough: args.passthrough,
            sinks,
            cursor: args.cursor,
        },
        None,
        None,
    )
    .await?;
    Ok(())
}

#[cfg(not(feature = "ui"))]
pub async fn run(_args: Args) -> Result<()> {
    anyhow::bail!("{}", crate::i18n::fl!("error-draw-requires-ui-feature"))
}
