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
use crate::config::{Config, FilenameContext};
use crate::output::Outputs;

/// Save action invoked when the user hits Ctrl+S / clicks Save. Returns the paths that were
/// written (clipboard sinks return none) so they can be echoed on stdout.
pub type SaveFn = Arc<dyn Fn(&CapturedImage) -> Result<Vec<PathBuf>> + Send + Sync + 'static>;

/// Build a save closure that routes the composed PNG through the configured `Outputs` sinks
/// and records the written paths into `collected` so the caller can return them over IPC.
/// We capture the tokio runtime handle so the GTK thread (inside `spawn_blocking`) can
/// `block_on` the async clipboard/file writes without spinning up a second runtime.
pub fn sinks_save_fn(
    config: Config,
    sinks: Vec<SinkSpec>,
    selection_label: &'static str,
    collected: Arc<Mutex<Vec<PathBuf>>>,
) -> SaveFn {
    let handle = tokio::runtime::Handle::current();
    let sinks = if sinks.is_empty() {
        config.default_sinks()
    } else {
        sinks
    };
    Arc::new(move |img: &CapturedImage| {
        let png = crate::output::encode_png(img, config.output.compression)?;
        let ctx = FilenameContext {
            output: img.source.as_ref().map(|o| o.name.as_str()),
            selection: Some(selection_label),
        };
        let outputs = Outputs::from_specs(&sinks, &config, &ctx)?;
        let paths = handle.block_on(outputs.write_png(&png))?;
        tracing::info!(
            bytes = png.len(),
            paths = ?paths.iter().map(|p| p.display().to_string()).collect::<Vec<_>>(),
            "saved annotated image"
        );
        #[cfg(feature = "notify")]
        crate::notify::notify_success(&config, &paths, &png);
        if let Ok(mut g) = collected.lock() {
            g.extend(paths.iter().cloned());
        }
        Ok(paths)
    })
}
