//! `capture` subcommand — selector → capture → editor → outputs.

use anyhow::Result;
use clap::Args as ClapArgs;

use super::SinkSpec;

#[derive(Debug, ClapArgs)]
pub struct Args {
    /// Sink(s) to receive the final image.
    #[arg(long = "to", value_name = "SINK")]
    pub to: Vec<SinkSpec>,
    /// Include the cursor.
    #[arg(long)]
    pub cursor: bool,
}

#[cfg(feature = "ui")]
pub async fn run(args: Args) -> Result<()> {
    use anyhow::Context as _;

    use crate::config::Config;
    use crate::context::Context;

    let config = Config::load_default().context("loading configuration")?;
    let ctx = Context::new(config).await?;
    crate::ui::run_capture_flow(ctx, args.to, args.cursor).await
}

#[cfg(not(feature = "ui"))]
pub async fn run(_args: Args) -> Result<()> {
    anyhow::bail!("hyprsnap was built without the `ui` feature; `capture` is unavailable")
}
