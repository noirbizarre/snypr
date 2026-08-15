//! Save-side helpers shared by the in-place annotation overlay.
//!
//! The capture-edit flow (`screenshot --edit`, Shift-click / Shift+Enter on the selector's
//! Capture button, and the tray "Annotate region…" entry) composes the annotated canvas
//! into a [`CapturedImage`], then routes the bytes through the configured [`Outputs`]
//! sinks. The closure lives here so the overlay module can stay focused on GTK plumbing.

use std::path::PathBuf;
use std::sync::{Arc, Mutex};

use anyhow::Result;

use crate::capture::CapturedImage;
use crate::cli::SinkSpec;
use crate::config::FilenameContext;
use crate::context::Ctx;
use crate::output::Outputs;

/// Save action invoked when the user hits Ctrl+S / clicks Save. Returns the paths that were
/// written (clipboard sinks return none) so they can be echoed on stdout.
pub type SaveFn = Arc<dyn Fn(&CapturedImage) -> Result<Vec<PathBuf>> + Send + Sync + 'static>;

/// Encode `img`, fan the PNG out to `sinks`, notify the user, and record the written paths
/// into `collected`.
///
/// The single implementation of the save pipeline. Both save routes reach it: the editor's
/// [`sinks_save_fn`] closure (which `block_on`s it from the GTK thread) and the draw
/// overlay's Save action. They previously had separate copies, which is how the draw route
/// ended up silently skipping the success notification.
pub async fn encode_and_write(
    app_ctx: &Ctx,
    sinks: &[SinkSpec],
    img: &CapturedImage,
    selection_label: &'static str,
    collected: &Arc<Mutex<Vec<PathBuf>>>,
) -> Result<Vec<PathBuf>> {
    let png = crate::output::encode_png(img, app_ctx.config.output.compression)?;
    let fname = FilenameContext {
        output: img.source.as_ref().map(|o| o.name.as_str()),
        selection: Some(selection_label),
    };
    let outputs = Outputs::from_specs(sinks, app_ctx, &fname)?;
    let paths = outputs.write_png(&png).await?;
    tracing::info!(
        bytes = png.len(),
        paths = ?paths.iter().map(|p| p.display().to_string()).collect::<Vec<_>>(),
        "saved annotated image"
    );
    crate::cli::screenshot::notify_written(&app_ctx.config, &paths, &png);
    collected
        .lock()
        .unwrap_or_else(|e| e.into_inner())
        .extend(paths.iter().cloned());
    Ok(paths)
}

/// Build a save closure that routes the composed PNG through the configured `Outputs` sinks
/// and records the written paths into `collected` so the caller can return them over IPC.
/// We capture the tokio runtime handle so the GTK thread (inside `spawn_blocking`) can
/// `block_on` the async clipboard/file writes without spinning up a second runtime.
pub fn sinks_save_fn(
    app_ctx: Ctx,
    sinks: Vec<SinkSpec>,
    selection_label: &'static str,
    collected: Arc<Mutex<Vec<PathBuf>>>,
) -> SaveFn {
    let handle = tokio::runtime::Handle::current();
    let sinks = if sinks.is_empty() {
        app_ctx.config.default_sinks()
    } else {
        sinks
    };
    Arc::new(move |img: &CapturedImage| {
        handle.block_on(encode_and_write(
            &app_ctx,
            &sinks,
            img,
            selection_label,
            &collected,
        ))
    })
}
