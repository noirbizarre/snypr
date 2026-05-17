//! `draw` subcommand — transparent live overlay for screen drawing.

use anyhow::Result;
use clap::Args as ClapArgs;

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
    let _ = run_overlay(
        ctx,
        OverlayMode::Draw {
            passthrough: args.passthrough,
        },
        None,
        None,
    )
    .await?;
    Ok(())
}

#[cfg(not(feature = "ui"))]
pub async fn run(_args: Args) -> Result<()> {
    anyhow::bail!("hyprsnap was built without the `ui` feature; `draw` is unavailable")
}
