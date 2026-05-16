//! `annotate` subcommand — open the editor on an existing image.

use std::path::PathBuf;

use anyhow::{Result, bail};
use clap::Args as ClapArgs;

use super::SinkSpec;

#[derive(Debug, ClapArgs)]
pub struct Args {
    /// Image to open in the editor.
    pub image: PathBuf,
    /// Sink(s) to write the edited image to. Defaults to overwriting `image`.
    #[arg(long = "to", value_name = "SINK")]
    pub to: Vec<SinkSpec>,
}

#[cfg(feature = "ui")]
pub async fn run(args: Args) -> Result<()> {
    use anyhow::Context as _;

    use crate::config::Config;
    use crate::context::Context;

    let config = Config::load_default().context("loading configuration")?;
    let ctx = Context::new(config).await?;

    if !args.image.exists() {
        bail!("image does not exist: {}", args.image.display());
    }
    crate::ui::editor::run_standalone(ctx, args.image, args.to).await
}

#[cfg(not(feature = "ui"))]
pub async fn run(_args: Args) -> Result<()> {
    bail!("hyprsnap was built without the `ui` feature; `annotate` is unavailable")
}
