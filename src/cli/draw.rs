//! `draw` subcommand — transparent live overlay for screen drawing.

use anyhow::Result;
use clap::Args as ClapArgs;

#[derive(Debug, ClapArgs)]
pub struct Args {
    /// Open the overlay with input passthrough enabled (clicks fall through).
    #[arg(long)]
    pub passthrough: bool,
}

#[cfg(feature = "ui")]
pub async fn run(args: Args) -> Result<()> {
    use anyhow::Context as _;

    use crate::config::Config;
    use crate::context::Context;

    let config = Config::load_default().context("loading configuration")?;
    let ctx = Context::new(config).await?;
    crate::ui::overlay::run(ctx, args.passthrough).await
}

#[cfg(not(feature = "ui"))]
pub async fn run(_args: Args) -> Result<()> {
    anyhow::bail!("hyprsnap was built without the `ui` feature; `draw` is unavailable")
}
