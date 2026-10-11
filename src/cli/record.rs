//! `record` subcommand — screen recording via the ScreenCast portal + PipeWire.
//!
//! See `src/record/mod.rs` for the capture/encode pipeline and its documented v1
//! limitations (the portal, not Snypr, owns the monitor/window picker).

use std::path::PathBuf;

use anyhow::Result;
use clap::Args as ClapArgs;

use super::selection::SelectionArgs;
use crate::config::VideoFormat;

#[derive(Debug, Default, ClapArgs)]
pub struct Args {
    /// What to record — see [`SelectionArgs`]. `--per-output` is parsed but rejected at
    /// runtime (multi-stream recording isn't implemented yet).
    #[command(flatten)]
    pub selection: SelectionArgs,

    /// Explicit output file path. Defaults to `[record].directory` /
    /// `[record].filename_template`, with the extension adjusted to match `--format`.
    #[arg(long, value_name = "PATH")]
    pub to: Option<PathBuf>,

    /// Output container/codec. Defaults to `[record].format` (`mp4`).
    #[arg(long, value_enum)]
    pub format: Option<VideoFormat>,

    /// Frames per second fed to the encoder. Defaults to `[record].fps` (`30`).
    #[arg(long, value_name = "FPS")]
    pub fps: Option<u32>,

    /// Include the mouse cursor, baked into the frames by the compositor.
    #[arg(long)]
    pub cursor: bool,

    /// Route the command through a running daemon instead of running locally: the first
    /// invocation starts a daemon-managed recording and returns immediately; a second
    /// invocation (with the same flag) stops it and prints the resulting file path. Bind
    /// both to the same keybind to toggle recording on/off.
    #[arg(long)]
    pub via_daemon: bool,
}

/// Resolve the final output path: an explicit `--to` wins verbatim; otherwise the config's
/// directory/filename template is expanded and its extension forced to match `format`.
#[cfg_attr(not(feature = "screencast"), allow(dead_code))]
pub(crate) fn resolve_output_path(
    config: &crate::config::Config,
    explicit: Option<PathBuf>,
    format: VideoFormat,
    selection: &crate::capture::Selection,
) -> PathBuf {
    if let Some(path) = explicit {
        return path;
    }
    let ctx_fname = crate::config::FilenameContext {
        output: None,
        selection: Some(super::selection::selection_label(selection)),
    };
    let filename = config.expand_record_filename(&ctx_fname);
    let mut path = config.record_save_directory().join(filename);
    path.set_extension(format.extension());
    path
}

/// Emit a best-effort "recording started" notification. Behind the `notify` feature so
/// non-notify builds compile cleanly without a stub call — mirrors
/// `screenshot::notify_written`, shared here between the foreground CLI path and the
/// daemon's `RecordToggle` handler.
#[cfg_attr(not(feature = "screencast"), allow(dead_code))]
#[inline]
pub(crate) fn notify_recording_started(_config: &crate::config::Config) {
    #[cfg(feature = "notify")]
    crate::notify::notify_recording_started(_config);
}

/// Emit a best-effort "recording saved" notification. See
/// [`notify_recording_started`] for why this is a feature-gated wrapper.
#[cfg_attr(not(feature = "screencast"), allow(dead_code))]
#[inline]
pub(crate) fn notify_recording_saved(_config: &crate::config::Config, _path: &std::path::Path) {
    #[cfg(feature = "notify")]
    crate::notify::notify_recording_saved(_config, _path);
}

#[cfg(feature = "screencast")]
pub async fn run(args: Args, config_override: Option<&std::path::Path>) -> Result<()> {
    use anyhow::Context as _;

    use crate::config::Config;
    use crate::context::Context;
    use crate::i18n::fl;

    let config = Config::resolve(config_override).context("loading configuration")?;
    let ctx = Context::new(config).await?;

    let selection = super::selection::parse(&args.selection)?;
    let cursor = args.cursor || ctx.config.record.cursor;
    let resolved = super::selection::resolve(selection, cursor, None, &[], &ctx).await?;

    let format = args.format.unwrap_or(ctx.config.record.format);
    let fps = args.fps.unwrap_or(ctx.config.record.fps);
    let crf = ctx.config.record.crf;
    let path = resolve_output_path(&ctx.config, args.to, format, &resolved.selection);

    let (stop_tx, stop_rx) = tokio::sync::oneshot::channel();
    tokio::spawn(async move {
        let _ = tokio::signal::ctrl_c().await;
        let _ = stop_tx.send(());
    });

    println!(
        "{}",
        fl!("record-in-progress", path = path.display().to_string())
    );
    notify_recording_started(&ctx.config);

    let opts = crate::record::RecordOptions {
        selection: resolved.selection,
        cursor: resolved.cursor,
        fps,
        format,
        crf,
        path,
    };
    let written = crate::record::run(opts, stop_rx).await?;
    notify_recording_saved(&ctx.config, &written);
    println!("{}", written.display());
    Ok(())
}

#[cfg(not(feature = "screencast"))]
pub async fn run(_args: Args, _config_override: Option<&std::path::Path>) -> Result<()> {
    anyhow::bail!(
        "{}",
        crate::i18n::fl!("error-record-requires-screencast-feature")
    )
}

#[cfg(test)]
mod tests {
    use pretty_assertions::assert_eq;

    use super::*;
    use crate::capture::Selection;
    use crate::config::Config;

    #[test]
    fn resolve_output_path_prefers_the_explicit_flag() {
        let cfg = Config::default();
        let explicit = PathBuf::from("/tmp/explicit.mp4");
        let path = resolve_output_path(
            &cfg,
            Some(explicit.clone()),
            VideoFormat::WebM,
            &Selection::Full,
        );
        assert_eq!(path, explicit);
    }

    #[test]
    fn resolve_output_path_forces_the_extension_to_match_the_format() {
        let cfg = Config::default();
        let path = resolve_output_path(&cfg, None, VideoFormat::WebM, &Selection::Full);
        assert_eq!(path.extension().unwrap(), "webm");
    }

    #[test]
    fn resolve_output_path_uses_the_configured_directory() {
        let cfg = Config {
            record: crate::config::RecordConfig {
                directory: Some(PathBuf::from("/tmp/casts")),
                ..Default::default()
            },
            ..Default::default()
        };
        let path = resolve_output_path(&cfg, None, VideoFormat::Mp4, &Selection::Full);
        assert_eq!(path.parent(), Some(std::path::Path::new("/tmp/casts")));
    }
}
