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

    use crate::annotate::DocumentBase;
    use crate::config::Config;
    use crate::context::Context;
    use crate::ui::overlay::{OverlayMode, run as run_overlay};

    let config = Config::load_default().context("loading configuration")?;
    let ctx = Context::new(config).await?;

    if !args.image.exists() {
        bail!("image does not exist: {}", args.image.display());
    }
    let bytes = tokio::fs::read(&args.image)
        .await
        .with_context(|| format!("reading {}", args.image.display()))?;
    let decoded = image::load_from_memory(&bytes)
        .with_context(|| format!("decoding {}", args.image.display()))?
        .to_rgba8();
    let (w, h) = decoded.dimensions();
    let base = DocumentBase {
        pixels: std::sync::Arc::from(decoded.into_raw().into_boxed_slice()),
        width: w,
        height: h,
        stride: w * 4,
    };

    // Anchor the overlay on the focused monitor's origin so the loaded image lands where the
    // user is currently looking. Falls back to (0, 0) when not running under Hyprland (the
    // overlay then opens on whichever monitor contains the virtual-desktop origin).
    let origin = crate::hypr::focused_monitor_origin()
        .await
        .unwrap_or((0, 0));

    // If the user didn't pass any sinks, default to writing back to the original path so
    // `annotate <file>` round-trips on Ctrl+S without configuration. Explicit `--to` sinks are
    // honoured by the standard Outputs pipeline.
    let sinks = if args.to.is_empty() {
        vec![super::SinkSpec::File(Some(args.image.clone()))]
    } else {
        args.to
    };

    let _ = run_overlay(
        ctx,
        OverlayMode::Edit {
            base,
            origin,
            sinks,
        },
        None,
    )
    .await?;
    Ok(())
}

#[cfg(not(feature = "ui"))]
pub async fn run(_args: Args) -> Result<()> {
    bail!("hyprsnap was built without the `ui` feature; `annotate` is unavailable")
}
