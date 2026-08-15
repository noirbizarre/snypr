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

#[cfg(test)]
mod tests {
    use super::*;
    use crate::capture::Output;
    use crate::config::Config;
    use crate::context::Context;
    use pretty_assertions::assert_eq;

    /// 2x2 BGRA image with a padded stride, optionally attributed to an output so the
    /// `{output}` filename token has something to expand to.
    fn image(source: Option<&str>) -> CapturedImage {
        CapturedImage {
            width: 2,
            height: 2,
            stride: 12, // 2px * 4 bytes + 4 bytes of row padding
            pixels: std::sync::Arc::from(vec![0xFFu8; 24].into_boxed_slice()),
            source: source.map(|name| Output {
                name: name.to_owned(),
                logical: crate::capture::region::Rect {
                    x: 0,
                    y: 0,
                    w: 2,
                    h: 2,
                },
                scale: 1,
            }),
        }
    }

    /// Context writing into `dir`, with notifications off so the test does not depend on a
    /// running D-Bus session.
    async fn ctx(dir: &std::path::Path) -> Ctx {
        let mut config = Config::default();
        config.output.directory = Some(dir.to_path_buf());
        config.notify.success = false;
        config.notify.error = false;
        Context::new(config).await.unwrap()
    }

    #[tokio::test]
    async fn writes_a_png_and_records_the_path() {
        let dir = tempfile::tempdir().unwrap();
        let app_ctx = ctx(dir.path()).await;
        let collected = Arc::new(Mutex::new(Vec::new()));

        let paths = encode_and_write(
            &app_ctx,
            &[SinkSpec::File(None)],
            &image(None),
            "region",
            &collected,
        )
        .await
        .unwrap();

        assert_eq!(paths.len(), 1);
        let bytes = std::fs::read(&paths[0]).unwrap();
        assert!(bytes.starts_with(&[0x89, b'P', b'N', b'G']), "not a PNG");
        // The caller reads written paths off `collected`, so recording them is part of the
        // contract rather than a side effect.
        assert_eq!(*collected.lock().unwrap(), paths);
    }

    #[tokio::test]
    async fn expands_the_output_token_from_the_image_source() {
        let dir = tempfile::tempdir().unwrap();
        let mut config = Config::default();
        config.output.directory = Some(dir.path().to_path_buf());
        config.output.filename_template = "shot_{output}_{selection}.png".to_owned();
        config.notify.success = false;
        let app_ctx = Context::new(config).await.unwrap();
        let collected = Arc::new(Mutex::new(Vec::new()));

        let paths = encode_and_write(
            &app_ctx,
            &[SinkSpec::File(None)],
            &image(Some("DP-1")),
            "window",
            &collected,
        )
        .await
        .unwrap();

        let name = paths[0].file_name().unwrap().to_string_lossy().into_owned();
        assert_eq!(name, "shot_DP-1_window.png");
    }

    #[tokio::test]
    async fn an_explicit_file_sink_path_wins_over_the_template() {
        let dir = tempfile::tempdir().unwrap();
        let app_ctx = ctx(dir.path()).await;
        let target = dir.path().join("explicit.png");
        let collected = Arc::new(Mutex::new(Vec::new()));

        let paths = encode_and_write(
            &app_ctx,
            &[SinkSpec::File(Some(target.clone()))],
            &image(None),
            "full",
            &collected,
        )
        .await
        .unwrap();

        assert_eq!(paths, vec![target]);
    }

    #[tokio::test]
    async fn sinks_save_fn_falls_back_to_the_configured_default_sinks() {
        let dir = tempfile::tempdir().unwrap();
        let app_ctx = ctx(dir.path()).await;
        let collected = Arc::new(Mutex::new(Vec::new()));

        // An empty sink list means "use `[output].default_sinks`", which defaults to `file`.
        let save = sinks_save_fn(app_ctx, Vec::new(), "region", collected.clone());
        let paths = tokio::task::spawn_blocking(move || save(&image(None)))
            .await
            .unwrap()
            .unwrap();

        assert_eq!(paths.len(), 1);
        assert!(paths[0].starts_with(dir.path()));
        assert_eq!(*collected.lock().unwrap(), paths);
    }
}
