//! Save-side helpers shared by the in-place annotation overlay.
//!
//! Both the live capture-edit flow (`screenshot --edit`) and the file-open flow
//! (`annotate <file>`) compose the annotated canvas into a [`CapturedImage`], then route the
//! bytes to either a fixed disk path or the configured [`Outputs`] sinks. The two closures
//! live here so the overlay module can stay focused on GTK plumbing.

use std::path::{Path, PathBuf};
use std::sync::{Arc, Mutex};

use anyhow::{Context as _, Result};

use crate::capture::CapturedImage;
use crate::cli::SinkSpec;
use crate::config::{Config, FilenameContext, PngCompression};
use crate::output::Outputs;

/// Save action invoked when the user hits Ctrl+S / clicks Save. Returns the paths that were
/// written (clipboard sinks return none) so they can be echoed on stdout.
pub type SaveFn = Arc<dyn Fn(&CapturedImage) -> Result<Vec<PathBuf>> + Send + Sync + 'static>;

/// Compute where a `Ctrl+S` from `annotate <file>` writes its output. Mirrors the rules the
/// standalone editor used to apply:
///
/// * Explicit `--to file=PATH` wins.
/// * Otherwise, if any sinks were provided, expand the configured filename template under the
///   configured save directory with `{selection} = "annotated"`.
/// * Otherwise, save next to the source image as `<stem>-annotated.<ext>`.
pub fn resolve_save_path(config: &Config, sinks: &[SinkSpec], source: &Path) -> Result<PathBuf> {
    for spec in sinks {
        if let SinkSpec::File(Some(p)) = spec {
            return Ok(p.clone());
        }
    }
    if !sinks.is_empty() {
        let dir = config.save_directory();
        std::fs::create_dir_all(&dir).with_context(|| format!("creating {}", dir.display()))?;
        return Ok(dir.join(config.expand_filename(&FilenameContext {
            output: None,
            selection: Some("annotated"),
        })));
    }
    let stem = source
        .file_stem()
        .and_then(|s| s.to_str())
        .unwrap_or("hyprsnap");
    let ext = source.extension().and_then(|s| s.to_str()).unwrap_or("png");
    let parent = source.parent().unwrap_or_else(|| Path::new("."));
    Ok(parent.join(format!("{stem}-annotated.{ext}")))
}

/// Build a synchronous save closure that writes the composed PNG to a fixed path.
pub fn path_save_fn(path: PathBuf, compression: PngCompression) -> SaveFn {
    Arc::new(move |img: &CapturedImage| {
        let png = crate::output::encode_png(img, compression)?;
        if let Some(parent) = path.parent()
            && !parent.as_os_str().is_empty()
        {
            std::fs::create_dir_all(parent)
                .with_context(|| format!("creating {}", parent.display()))?;
        }
        std::fs::write(&path, &png).with_context(|| format!("writing {}", path.display()))?;
        tracing::info!(path = %path.display(), bytes = png.len(), "saved annotated PNG");
        Ok(vec![path.clone()])
    })
}

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
        if let Ok(mut g) = collected.lock() {
            g.extend(paths.iter().cloned());
        }
        Ok(paths)
    })
}
