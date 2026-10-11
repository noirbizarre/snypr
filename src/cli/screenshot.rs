//! `screenshot` subcommand — capture and write to sinks (optionally via the annotation editor).

use anyhow::{Context as _, Result, bail};
use clap::Args as ClapArgs;

use super::selection::SelectionArgs;
use super::{ClipboardKind, SinkSpec};
use crate::capture::{Capturer, Selection, wlr::WlrCapturer};
use crate::config::Config;
use crate::context::Context;
use crate::i18n::fl;
use crate::output::{OutputMode, Outputs};

#[derive(Debug, Default, ClapArgs)]
pub struct Args {
    /// What to capture — see [`SelectionArgs`].
    ///
    /// The clap-level `--per-output`/`--edit` conflict is enforced at runtime instead of via
    /// `conflicts_with` (see the guard in [`execute`]): `SelectionArgs` is shared with
    /// `record`, which has no `--edit` flag for clap to reference.
    #[command(flatten)]
    pub selection: SelectionArgs,

    /// Open the annotation editor on the captured image before writing to sinks.
    /// Incompatible with `--per-output` (one editor session per N frames doesn't compose).
    #[arg(long)]
    pub edit: bool,

    /// Sink(s) to receive the image. Repeatable.
    #[arg(long = "to", value_name = "SINK")]
    pub to: Vec<SinkSpec>,

    /// Default selection target for `--to clipboard` when no `=KIND`
    /// suffix is given on the entry itself. Precedence:
    /// `--to clipboard=KIND` > `--clipboard-type` > `[clipboard].default_kind`
    /// config > `regular`.
    #[arg(long, value_name = "KIND", value_enum)]
    pub clipboard_type: Option<ClipboardKind>,

    /// Delay before capture, in whole seconds (e.g. `--delay 3`). `0` is the same as
    /// omitting the flag. The UI countdown only operates on integer seconds, so the
    /// CLI / config / IPC representation all match.
    #[arg(long, value_name = "SECONDS")]
    pub delay: Option<u32>,

    /// Include the mouse cursor in the capture.
    #[arg(long)]
    pub cursor: bool,

    /// Route the command through a running daemon instead of running locally.
    #[arg(long)]
    pub via_daemon: bool,
}

pub async fn run(args: Args, config_override: Option<&std::path::Path>) -> Result<()> {
    let config = Config::resolve(config_override).context("loading configuration")?;
    let ctx = Context::new(config).await?;

    let selection = super::selection::parse(&args.selection)?;
    let kind = effective_clipboard_kind(args.clipboard_type, &ctx.config);
    let sinks = if args.to.is_empty() {
        ctx.config.default_sinks()
    } else {
        args.to
            .iter()
            .cloned()
            .map(|s| s.resolve_clipboard_default(kind))
            .collect()
    };

    // Effective delay: CLI flag wins, otherwise fall back to the `[capture].delay` config.
    // The selector's spinner can still override this interactively (see `execute`).
    let delay = effective_delay(args.delay, ctx.config.capture.delay);

    // Effective cursor: `--cursor` turns it on, otherwise `[capture].cursor` decides.
    let cursor = effective_cursor(args.cursor, ctx.config.capture.cursor);

    let paths = execute(ctx, selection, cursor, sinks, args.edit, delay).await?;
    for p in &paths {
        println!("{}", p.display());
    }
    Ok(())
}

/// Resolve the pre-capture delay using the documented precedence: CLI `--delay` flag wins,
/// otherwise fall back to `[capture].delay` from the config. A zero result collapses to
/// `None` so the sleep is a true no-op rather than a vacuous zero-length sleep round-trip.
pub fn effective_delay(cli: Option<u32>, config: Option<u32>) -> Option<u32> {
    cli.or(config).filter(|n| *n > 0)
}

/// Resolve whether to include the cursor: `[capture].cursor` sets the default and the
/// `--cursor` flag turns it on.
///
/// This ORs rather than overrides because `--cursor` is a bare boolean flag — its absence
/// means "not requested", not "requested off", so letting it override would make the config
/// field unusable. Users who set `cursor = true` and want it off for one capture toggle it
/// in the interactive selector, which does carry three-state intent.
pub fn effective_cursor(cli: bool, config: bool) -> bool {
    cli || config
}

/// Resolve the effective default [`ClipboardKind`] using the documented precedence:
/// CLI `--clipboard-type` flag wins, otherwise fall back to
/// `[clipboard].default_kind` from the config. The per-entry
/// `--to clipboard=KIND` syntax overrides this on a sink-by-sink basis
/// (handled separately in [`SinkSpec::resolve_clipboard_default`]).
pub fn effective_clipboard_kind(cli: Option<ClipboardKind>, config: &Config) -> ClipboardKind {
    cli.unwrap_or(config.clipboard.default_kind)
}
/// Headless core of the screenshot pipeline used by both the CLI (`run`) and the daemon's IPC
/// handler. Resolves compositor-aware selections, captures, encodes, and writes — returning the
/// file paths produced by `OutputSink`s (clipboard sinks contribute nothing).
///
/// When `edit == true`, the captured image is handed to the annotation editor (in-memory, no
/// PNG round-trip) and the editor's save action fans the result out to `sinks` instead. The
/// editor path rejects `Selection::PerOutput` since the editor operates on a single image.
///
/// `delay` is the pre-capture sleep applied **after** any interactive selector has been
/// resolved (so the countdown only starts once the user has confirmed). The interactive
/// selector can override this default via its delay spinner; the final value is what is
/// honored. Pass `None` to skip the sleep entirely.
pub async fn execute(
    ctx: crate::context::Ctx,
    selection: Selection,
    cursor: bool,
    sinks: Vec<SinkSpec>,
    edit: bool,
    delay: Option<u32>,
) -> Result<Vec<std::path::PathBuf>> {
    // Resolve compositor-aware selections up front (window-manager IPC + interactive overlay) so the
    // rest of the pipeline only ever sees concrete Region/Output/Full/PerOutput variants. The
    // interactive selector can also override `cursor` and `delay` via its toolbar, and can
    // request the annotation editor via its "Annotate" button (Shift+Enter). The button choice
    // wins: if the selector explicitly opted in, we OR it into `edit`.
    let resolved = super::selection::resolve(selection, cursor, delay, &sinks, &ctx).await?;
    let (selection, cursor, delay) = (resolved.selection, resolved.cursor, resolved.delay);
    let edit = edit || resolved.edit;

    // The selector's output switcher wins over `--to` / `[output].default_sinks`. Applied
    // before every write branch below, so `--per-output`, the editor hand-off, and the plain
    // path all honor it — and the editor receives sinks that already reflect the choice,
    // seeding its own switcher.
    let sinks = apply_output_override(sinks, resolved.output_mode);

    if edit && matches!(selection, Selection::PerOutput) {
        bail!("{}", fl!("error-edit-incompatible-per-output"));
    }

    // Apply the pre-capture sleep after the selector closes so the countdown does not block
    // user interaction. For interactive paths the selector has already counted down inside
    // its own overlay and `delay` arrives as `None` here. For non-interactive paths
    // (`--full --delay 3`, `--monitor`, `--window`, daemon screenshot, tray) the selector
    // is short-circuited; we display a transient fullscreen countdown instead of sleeping
    // silently so the user is never caught mid-action by an unannounced capture.
    if let Some(secs) = delay
        && secs > 0
    {
        let d = std::time::Duration::from_secs(secs as u64);
        #[cfg(feature = "ui")]
        crate::ui::countdown::show_countdown(d, ctx.config.ui.selector.clone()).await?;
        #[cfg(not(feature = "ui"))]
        tokio::time::sleep(d).await;
    }

    let capturer = WlrCapturer::new()?;
    let t0 = std::time::Instant::now();
    tracing::debug!(?selection, "starting wlr-screencopy capture");
    let images = capturer
        .capture(selection.clone(), cursor)
        .await
        .with_context(|| format!("capturing {selection:?}"))?;
    tracing::info!(
        elapsed_ms = t0.elapsed().as_millis() as u64,
        count = images.len(),
        "captured"
    );

    // `--per-output` skips stitching entirely: each captured frame is encoded and written to its
    // own file (or copied to its own clipboard entry), with the output name interpolated into
    // the filename template. Templates that lack `{output}` get one auto-inserted to avoid the
    // multiple frames collapsing onto the same path. The `--edit` guard at the top of this
    // function ensures we never end up here in editor mode.
    if matches!(selection, Selection::PerOutput) {
        let mut all_paths = Vec::new();
        for img in &images {
            let name = img
                .source
                .as_ref()
                .map(|o| o.name.as_str())
                .unwrap_or("output");
            let ctx_fname = crate::config::FilenameContext {
                output: Some(name),
                selection: Some("output"),
            };
            let outputs = Outputs::from_specs_per_output(&sinks, &ctx, &ctx_fname)?;
            let png = crate::output::encode_png(img, ctx.config.output.compression)?;
            let paths = outputs.write_png(&png).await?;
            notify_written(&ctx.config, &paths, &png);
            all_paths.extend(paths);
        }
        return Ok(all_paths);
    }

    let stitched = crate::capture::region::stitch(&images, &selection)?;

    if edit {
        // Editor path: hand the stitched RGBA buffer to the in-place annotation overlay
        // (one layer-shell window per intersecting monitor) so the user keeps their on-screen
        // context. Skips a PNG encode + decode round-trip; the overlay re-encodes on save and
        // fans the bytes out to whichever sinks the caller supplied.
        #[cfg(feature = "ui")]
        {
            let base = base_from_captured(&stitched);
            // Where the stitched buffer sits on the virtual desktop. For Region, the stitch
            // call already cropped to the rect; otherwise the buffer spans the captured
            // images' bounding box.
            let origin = match &selection {
                Selection::Region(r) => (r.x, r.y),
                _ => crate::capture::region::bbox(&images)
                    .map(|b| (b.x, b.y))
                    .unwrap_or((0, 0)),
            };
            return crate::ui::overlay::run(
                ctx,
                crate::ui::overlay::OverlayMode::Edit {
                    base,
                    origin,
                    sinks,
                },
                None,
                None,
            )
            .await;
        }
        #[cfg(not(feature = "ui"))]
        {
            anyhow::bail!("{}", fl!("error-edit-requires-ui-feature"));
        }
    }

    let ctx_fname = crate::config::FilenameContext {
        output: None,
        selection: Some(super::selection::selection_label(&selection)),
    };
    let outputs = Outputs::from_specs(&sinks, &ctx, &ctx_fname)?;
    let png = crate::output::encode_png(&stitched, ctx.config.output.compression)?;
    let paths = outputs.write_png(&png).await?;
    notify_written(&ctx.config, &paths, &png);
    Ok(paths)
}

/// Emit a best-effort success notification for a freshly written screenshot. Behind the
/// `notify` feature so non-notify builds compile cleanly without a stub call.
///
/// Shared with the draw overlay's Save path (`crate::ui::overlay`) so both save routes
/// notify identically.
#[inline]
pub(crate) fn notify_written(_config: &Config, _paths: &[std::path::PathBuf], _png: &[u8]) {
    #[cfg(feature = "notify")]
    crate::notify::notify_success(_config, _paths, _png);
}

/// Convert a screencopy `CapturedImage` (possibly with padded stride, byte order per
/// `img.format`) into a tight RGBA [`DocumentBase`] ready for the annotation canvas. Lives
/// here so the editor branch of `execute` can call it directly without going through
/// `crate::ui`.
#[cfg(feature = "ui")]
fn base_from_captured(img: &crate::capture::CapturedImage) -> crate::annotate::DocumentBase {
    // Shares the swizzle with `output::encode_png`. This path used to carry its own scalar
    // copy, so opening the editor was several times slower than saving the same frame.
    let rgba = crate::output::bgra_to_rgba(img);
    crate::annotate::DocumentBase {
        pixels: std::sync::Arc::from(rgba.into_boxed_slice()),
        width: img.width,
        height: img.height,
        stride: img.width * 4,
    }
}

/// Apply the destination the interactive selector settled on, keeping the explicit path and
/// clipboard kind the CLI/config resolved to (see [`SinkSelection`]).
///
/// `None` means the selector never ran, and `sinks` is returned untouched — which is what
/// preserves multi-file fan-out (`--to file=A --to file=B`) for every non-interactive path,
/// including the daemon.
fn apply_output_override(sinks: Vec<SinkSpec>, mode: Option<OutputMode>) -> Vec<SinkSpec> {
    match mode {
        Some(mode) => {
            let mut selection = crate::output::SinkSelection::from_sinks(&sinks);
            selection.set_mode(mode);
            selection.to_sinks()
        }
        None => sinks,
    }
}

#[cfg(test)]
mod tests {
    use super::*;
    use clap::Parser;
    use pretty_assertions::assert_eq;
    use rstest::rstest;

    #[rstest]
    #[case::file(OutputMode::File, vec![SinkSpec::File(None)])]
    #[case::clipboard(OutputMode::Clipboard, vec![SinkSpec::Clipboard(None)])]
    #[case::both(
        OutputMode::Both,
        vec![SinkSpec::File(None), SinkSpec::Clipboard(None)]
    )]
    fn the_selector_override_replaces_the_resolved_sinks(
        #[case] mode: OutputMode,
        #[case] expected: Vec<SinkSpec>,
    ) {
        let sinks = vec![SinkSpec::File(None)];
        assert_eq!(apply_output_override(sinks, Some(mode)), expected);
    }

    #[test]
    fn no_selector_override_leaves_the_sinks_untouched() {
        // The daemon and every non-interactive path land here; collapsing a multi-file
        // fan-out would silently drop one of the requested targets.
        let sinks = vec![
            SinkSpec::File(Some("/tmp/a.png".into())),
            SinkSpec::File(Some("/tmp/b.png".into())),
        ];
        assert_eq!(apply_output_override(sinks.clone(), None), sinks);
    }

    #[test]
    fn the_selector_override_keeps_an_explicit_cli_path() {
        let target = std::path::PathBuf::from("/tmp/from-cli.png");
        let sinks = vec![SinkSpec::File(Some(target.clone()))];
        assert_eq!(
            apply_output_override(sinks, Some(OutputMode::Both)),
            vec![SinkSpec::File(Some(target)), SinkSpec::Clipboard(None)]
        );
    }

    /// `base_from_captured` is a second, independent BGRA -> RGBA converter (used by the
    /// `--edit` path), so it needs the same distinct-channel and padded-stride assertions as
    /// `output::encode_png`.
    #[cfg(feature = "ui")]
    #[rstest]
    #[case::tight(0)]
    #[case::padded(8)]
    fn base_from_captured_swizzles_and_compacts(#[case] padding: u32) {
        let (width, height) = (3u32, 2u32);
        let stride = width * 4 + padding;
        let mut pixels = vec![0xABu8; (stride * height) as usize];
        for y in 0..height {
            for x in 0..width {
                let i = (y * stride + x * 4) as usize;
                pixels[i] = 10 + x as u8; // B
                pixels[i + 1] = 20 + x as u8; // G
                pixels[i + 2] = 30 + x as u8; // R
                pixels[i + 3] = 40 + y as u8; // A
            }
        }
        let img = crate::capture::CapturedImage {
            width,
            height,
            stride,
            pixels: std::sync::Arc::from(pixels.into_boxed_slice()),
            format: crate::capture::PixelFormat::Bgra,
            source: None,
        };

        let base = base_from_captured(&img);
        assert_eq!(base.width, width);
        assert_eq!(base.height, height);
        assert_eq!(base.stride, width * 4, "stride must be compacted");
        assert_eq!(base.pixels.len(), (width * height * 4) as usize);
        for y in 0..height {
            for x in 0..width {
                let i = ((y * width + x) * 4) as usize;
                assert_eq!(
                    &base.pixels[i..i + 4],
                    &[30 + x as u8, 20 + x as u8, 10 + x as u8, 40 + y as u8],
                    "pixel ({x}, {y}) has the wrong channel order"
                );
            }
        }
    }

    /// Minimal top-level parser to exercise `screenshot::Args` through clap.
    #[derive(Debug, Parser)]
    struct Harness {
        #[command(subcommand)]
        cmd: HarnessCmd,
    }

    #[derive(Debug, clap::Subcommand)]
    enum HarnessCmd {
        Screenshot(Args),
    }

    #[test]
    fn parses_edit_flag() {
        let cli = Harness::try_parse_from(["test", "screenshot", "--edit"]).unwrap();
        let HarnessCmd::Screenshot(args) = cli.cmd;
        assert!(args.edit);
    }

    /// `--per-output`/`--edit` used to conflict at the clap level via `conflicts_with`, but
    /// that attribute lived on a field now shared with `record::Args` (which has no `--edit`
    /// arg for clap to reference — `Command::debug_assert` would panic at parse time).
    /// The combination is still rejected, just later: `execute()` bails with
    /// `error-edit-incompatible-per-output` once the concrete `Selection` is known (see the
    /// guard right after `resolve_selection` in `execute`).
    #[test]
    fn per_output_and_edit_now_parse_together_and_conflict_only_at_runtime() {
        let cli =
            Harness::try_parse_from(["test", "screenshot", "--edit", "--per-output"]).unwrap();
        let HarnessCmd::Screenshot(args) = cli.cmd;
        assert!(args.edit);
        assert!(args.selection.per_output);
    }

    #[test]
    fn effective_delay_prefers_cli_over_config() {
        assert_eq!(effective_delay(Some(5), Some(10)), Some(5));
    }

    #[test]
    fn effective_delay_falls_back_to_config() {
        assert_eq!(effective_delay(None, Some(10)), Some(10));
    }

    #[test]
    fn effective_delay_is_none_when_unset() {
        assert_eq!(effective_delay(None, None), None);
    }

    #[test]
    fn effective_delay_collapses_zero_to_none() {
        assert_eq!(effective_delay(Some(0), None), None);
        assert_eq!(effective_delay(None, Some(0)), None);
    }

    #[rstest]
    #[case::neither(false, false, false)]
    #[case::config_only(false, true, true)]
    #[case::flag_only(true, false, true)]
    #[case::both(true, true, true)]
    fn effective_cursor_ors_the_flag_with_the_config(
        #[case] cli: bool,
        #[case] config: bool,
        #[case] expected: bool,
    ) {
        assert_eq!(effective_cursor(cli, config), expected);
    }

    #[test]
    fn parses_delay_as_integer_seconds() {
        let cli = Harness::try_parse_from(["test", "screenshot", "--delay", "3"]).unwrap();
        let HarnessCmd::Screenshot(args) = cli.cmd;
        assert_eq!(args.delay, Some(3));
    }

    #[test]
    fn rejects_humantime_delay() {
        let err = Harness::try_parse_from(["test", "screenshot", "--delay", "2s"]).unwrap_err();
        assert_eq!(err.kind(), clap::error::ErrorKind::ValueValidation);
    }
}
