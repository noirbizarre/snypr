//! `screenshot` subcommand — capture and write to sinks (optionally via the annotation editor).

use anyhow::{Context as _, Result, bail};
use clap::Args as ClapArgs;

use super::{ClipboardKind, SinkSpec};
use crate::capture::{Capturer, Selection, wlr::WlrCapturer};
use crate::config::Config;
use crate::context::Context;
use crate::i18n::fl;
use crate::output::{OutputMode, Outputs};

#[derive(Debug, Default, ClapArgs)]
pub struct Args {
    /// Capture the entire virtual desktop, stitched across monitors.
    #[arg(long, group = "selection")]
    pub full: bool,
    /// Capture each connected output to a separate file.
    #[arg(long, group = "selection", conflicts_with = "edit")]
    pub per_output: bool,
    /// Capture only the currently focused monitor (via Hyprland or Sway IPC).
    #[arg(long, group = "selection")]
    pub focused: bool,
    /// Capture a specific output by name (e.g. `DP-1`).
    #[arg(long, value_name = "NAME", group = "selection")]
    pub output: Option<String>,
    /// Capture the currently active window (via Hyprland or Sway IPC).
    #[arg(long, group = "selection")]
    pub window: bool,
    /// Capture an explicit region as `X,Y,WxH`.
    #[arg(long, value_name = "X,Y,WxH", group = "selection")]
    pub region: Option<String>,
    /// Launch an interactive selector overlay.
    #[arg(short, long, group = "selection")]
    pub interactive: bool,

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

    let selection = parse_selection(&args)?;
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
    let resolved = resolve_selection(selection, cursor, delay, &sinks, &ctx).await?;
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
        selection: Some(selection_label(&selection)),
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

/// Convert a screencopy `CapturedImage` (BGRA, possibly with padded stride) into a tight RGBA
/// [`DocumentBase`] ready for the annotation canvas. Lives here so the editor branch of
/// `execute` can call it directly without going through `crate::ui`.
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

/// Short label for the filename `{selection}` token.
pub(crate) fn selection_label(s: &Selection) -> &'static str {
    match s {
        Selection::Full => "full",
        Selection::PerOutput => "output",
        Selection::Focused => "focused",
        Selection::Output(_) => "output",
        Selection::Window => "window",
        Selection::Region(_) => "region",
        Selection::Interactive => "region",
    }
}

/// What [`resolve_selection`] produced. Every field but `selection` is an *override* the
/// interactive selector's toolbar may have applied on top of the CLI/config values.
#[derive(Debug)]
struct ResolvedSelection {
    selection: Selection,
    cursor: bool,
    edit: bool,
    delay: Option<u32>,
    /// Destination picked on the selector's output switcher. `None` whenever the selector
    /// did not run, which leaves `sinks` exactly as the CLI/config resolved them — that
    /// preserves multi-file fan-out (`--to file=A --to file=B`) for every non-interactive
    /// path, including the daemon.
    output_mode: Option<OutputMode>,
}

/// Convert a selector-reported delay into whole seconds for the downstream sleep / countdown.
///
/// The selector counts down inside its own overlay and reports `Duration::ZERO`, so this
/// normally yields `None`; a non-zero value would only arise from a future code path that
/// bypasses the in-overlay countdown. Rounds up so the visible wait is never shorter than
/// requested.
#[cfg_attr(not(feature = "ui"), allow(dead_code))]
fn selector_delay_secs(delay: std::time::Duration) -> Option<u32> {
    if delay.is_zero() {
        return None;
    }
    let secs = delay.as_secs_f64().ceil() as u32;
    Some(secs).filter(|n| *n > 0)
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

impl ResolvedSelection {
    /// Pass-through result for a selection that needed no user interaction.
    fn passthrough(selection: Selection, cursor: bool, delay: Option<u32>) -> Self {
        Self {
            selection,
            cursor,
            edit: false,
            delay,
            output_mode: None,
        }
    }
}

/// Resolve compositor-aware selections (`Interactive`, `Window`, `Focused`) into concrete ones
/// that the capture pipeline can act on directly. Also returns the (potentially updated)
/// cursor flag, an `edit` flag, the final pre-capture delay, and the chosen output
/// destination. The interactive selector's toolbar can override every one of these — its
/// delay spinner, cursor toggle, output switcher, and Annotate button take precedence over
/// the values threaded in from the CLI.
///
/// - `Interactive` opens the GTK overlay; the resulting selection + cursor + edit flag +
///   delay + destination come from the user's toolbar choices (the delay spinner is seeded
///   from `initial_delay`, the output switcher from `sinks`).
/// - `Window` reads the currently active window (via `crate::wm`) and is replaced with
///   `Region(rect)`.
/// - `Focused` reads the currently focused monitor (via `crate::wm`) and is replaced with
///   `Output(name)`.
///
/// All other variants pass through unchanged (with `edit = false`, `cursor`/`delay` unchanged).
async fn resolve_selection(
    selection: Selection,
    cursor: bool,
    initial_delay: Option<u32>,
    _sinks: &[SinkSpec],
    _ctx: &std::sync::Arc<crate::context::Context>,
) -> Result<ResolvedSelection> {
    match selection {
        Selection::Interactive => {
            #[cfg(feature = "ui")]
            {
                // Selector internals (countdown timer, outcome struct) operate on
                // `Duration`; convert at the boundary so the rest of the screenshot
                // pipeline stays in plain integer seconds.
                let seed = std::time::Duration::from_secs(initial_delay.unwrap_or(0) as u64);
                // Seed the toolbar's output switcher from whatever `--to` /
                // `[output].default_sinks` resolved to, so it opens on the destination the
                // user would have gotten had they not touched it.
                let seed_output = crate::output::SinkSelection::from_sinks(_sinks).mode();
                let outcome =
                    crate::ui::selector::pick_region(_ctx.clone(), cursor, seed, true, seed_output)
                        .await
                        .context("interactive region selection")?;
                tracing::info!(
                    ?outcome.selection,
                    cursor = outcome.cursor,
                    edit = outcome.edit,
                    delay_secs = outcome.delay.as_secs(),
                    output_mode = ?outcome.output_mode,
                    "selector outcome",
                );
                // The selector counts down internally and returns `Duration::ZERO`, so
                // any non-zero value here would only arise from a future code path that
                // bypasses the in-overlay countdown. Convert it to seconds (rounding up
                // to keep the visible wait at least as long as requested) for downstream
                // sleep / countdown logic.
                let chosen_delay = selector_delay_secs(outcome.delay);
                // Resolve any compositor-aware variants the user picked (Window) by recursing.
                let inner = Box::pin(resolve_selection(
                    outcome.selection,
                    outcome.cursor,
                    chosen_delay,
                    _sinks,
                    _ctx,
                ))
                .await?;
                Ok(ResolvedSelection {
                    edit: outcome.edit || inner.edit,
                    output_mode: Some(outcome.output_mode),
                    ..inner
                })
            }
            #[cfg(not(feature = "ui"))]
            {
                let _ = initial_delay;
                anyhow::bail!("{}", fl!("error-interactive-requires-ui-feature"));
            }
        }
        Selection::Window => {
            let backend = crate::wm::detect()
                .ok_or_else(|| anyhow::anyhow!("{}", fl!("error-unsupported-compositor")))?;
            let win = backend
                .active_window()
                .await
                .with_context(|| format!("querying active window from {}", backend.name()))?;
            let rect = win.rect();
            tracing::info!(
                class = %win.class,
                title = %win.title,
                monitor = %win.monitor,
                x = rect.x,
                y = rect.y,
                w = rect.w,
                h = rect.h,
                "active window resolved"
            );
            Ok(ResolvedSelection::passthrough(
                Selection::Region(rect),
                cursor,
                initial_delay,
            ))
        }
        Selection::Focused => {
            let backend = crate::wm::detect()
                .ok_or_else(|| anyhow::anyhow!("{}", fl!("error-unsupported-compositor")))?;
            let name = backend
                .focused_output()
                .await
                .with_context(|| format!("querying focused output from {}", backend.name()))?;
            tracing::info!(monitor = %name, "focused monitor resolved");
            Ok(ResolvedSelection::passthrough(
                Selection::Output(name),
                cursor,
                initial_delay,
            ))
        }
        other => Ok(ResolvedSelection::passthrough(other, cursor, initial_delay)),
    }
}

pub(crate) fn parse_selection(args: &Args) -> Result<Selection> {
    match (
        args.full,
        args.per_output,
        args.focused,
        args.output.as_deref(),
        args.window,
        args.region.as_deref(),
        args.interactive,
    ) {
        (true, _, _, _, _, _, _) => Ok(Selection::Full),
        (_, true, _, _, _, _, _) => Ok(Selection::PerOutput),
        (_, _, true, _, _, _, _) => Ok(Selection::Focused),
        (_, _, _, Some(name), _, _, _) => Ok(Selection::Output(name.to_owned())),
        (_, _, _, _, true, _, _) => Ok(Selection::Window),
        (_, _, _, _, _, Some(spec), _) => Ok(Selection::Region(parse_region(spec)?)),
        (_, _, _, _, _, _, true) => Ok(Selection::Interactive),
        // No selection specified → default to interactive.
        _ => Ok(Selection::Interactive),
    }
}

fn parse_region(spec: &str) -> Result<crate::capture::region::Rect> {
    let invalid_region = || anyhow::anyhow!("{}", fl!("error-invalid-region", spec = spec));
    let mut parts = spec.splitn(3, ',');
    let x = parts.next().ok_or_else(invalid_region)?;
    let y = parts.next().ok_or_else(invalid_region)?;
    let size = parts.next().ok_or_else(invalid_region)?;
    let (ws, hs) = size
        .split_once('x')
        .ok_or_else(|| anyhow::anyhow!("{}", fl!("error-invalid-region-size", size = size)))?;

    Ok(crate::capture::region::Rect {
        x: x.trim().parse()?,
        y: y.trim().parse()?,
        w: ws.trim().parse()?,
        h: hs.trim().parse()?,
    })
}

#[cfg(test)]
mod tests {
    use super::*;
    use clap::Parser;
    use pretty_assertions::assert_eq;
    use rstest::rstest;

    /// Build a context with notifications off so tests never touch a D-Bus session.
    use crate::testing::test_ctx;

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

    #[rstest]
    #[case::zero(std::time::Duration::ZERO, None)]
    #[case::whole(std::time::Duration::from_secs(3), Some(3))]
    // Rounded up: a 2.1 s request must never wait less than the user asked for.
    #[case::rounds_up(std::time::Duration::from_millis(2100), Some(3))]
    #[case::sub_second(std::time::Duration::from_millis(1), Some(1))]
    fn converts_the_selector_delay_to_whole_seconds(
        #[case] delay: std::time::Duration,
        #[case] expected: Option<u32>,
    ) {
        assert_eq!(selector_delay_secs(delay), expected);
    }

    #[rstest]
    #[case::full(Selection::Full)]
    #[case::per_output(Selection::PerOutput)]
    #[case::region(Selection::Region(crate::capture::region::Rect { x: 0, y: 0, w: 4, h: 4 }))]
    #[case::output(Selection::Output("DP-1".to_owned()))]
    #[tokio::test]
    async fn non_interactive_selections_pass_through_untouched(#[case] selection: Selection) {
        // No compositor is queried for these, so they resolve without any window-manager
        // backend running.
        let ctx = test_ctx().await;
        let resolved = resolve_selection(selection.clone(), true, Some(5), &[], &ctx)
            .await
            .unwrap();

        assert_eq!(resolved.selection, selection);
        assert!(resolved.cursor, "cursor flag must survive the pass-through");
        assert!(!resolved.edit, "only the selector can request the editor");
        assert_eq!(resolved.delay, Some(5));
        assert_eq!(
            resolved.output_mode, None,
            "no selector ran, so the CLI sinks must be left alone"
        );
    }

    #[rstest]
    #[case::window(Selection::Window)]
    #[case::focused(Selection::Focused)]
    #[tokio::test]
    async fn resolve_selection_fails_clearly_without_a_detected_backend(
        #[case] selection: Selection,
    ) {
        crate::testing::set_compositor_env(None, None);
        let ctx = test_ctx().await;
        let err = resolve_selection(selection, false, None, &[], &ctx)
            .await
            .unwrap_err();
        // The `error-unsupported-compositor` i18n key, not a raw Hyprland/Sway IPC error —
        // there's no backend to even attempt a connection with.
        assert!(
            format!("{err:#}").contains("no supported window manager IPC was detected"),
            "unexpected error: {err:#}"
        );
    }

    #[tokio::test]
    async fn resolve_selection_window_resolves_via_the_detected_backend() {
        let dir = tempfile::tempdir().unwrap();
        let sock = dir.path().join("sway.sock");
        crate::testing::set_compositor_env(None, Some(&sock));
        let listener = crate::testing::bind_fake_sway_socket(&sock);
        let tree = serde_json::json!({
            "type": "root",
            "nodes": [{
                "type": "output",
                "name": "eDP-1",
                "nodes": [{
                    "type": "workspace",
                    "id": 1,
                    "nodes": [{
                        "type": "con",
                        "id": 1,
                        "pid": 111,
                        "app_id": "kitty",
                        "name": "term",
                        "focused": true,
                        "visible": true,
                        "rect": {"x": 10, "y": 20, "width": 300, "height": 200}
                    }]
                }]
            }]
        });
        let server = tokio::spawn(async move {
            crate::testing::serve_fake_sway_reply(listener, crate::wm::sway::GET_TREE, &tree).await;
        });

        let ctx = test_ctx().await;
        let resolved = resolve_selection(Selection::Window, true, None, &[], &ctx)
            .await
            .unwrap();
        assert_eq!(
            resolved.selection,
            Selection::Region(crate::capture::region::Rect {
                x: 10,
                y: 20,
                w: 300,
                h: 200
            })
        );
        assert!(resolved.cursor);
        server.await.unwrap();
    }

    #[tokio::test]
    async fn resolve_selection_focused_resolves_via_the_detected_backend() {
        let dir = tempfile::tempdir().unwrap();
        let sock = dir.path().join("sway.sock");
        crate::testing::set_compositor_env(None, Some(&sock));
        let listener = crate::testing::bind_fake_sway_socket(&sock);
        let outputs = serde_json::json!([
            {"name": "eDP-1", "focused": false},
            {"name": "DP-2", "focused": true},
        ]);
        let server = tokio::spawn(async move {
            crate::testing::serve_fake_sway_reply(listener, crate::wm::sway::GET_OUTPUTS, &outputs)
                .await;
        });

        let ctx = test_ctx().await;
        let resolved = resolve_selection(Selection::Focused, false, Some(5), &[], &ctx)
            .await
            .unwrap();
        assert_eq!(resolved.selection, Selection::Output("DP-2".to_owned()));
        assert_eq!(resolved.delay, Some(5));
        server.await.unwrap();
    }

    #[rstest]
    #[case("10,20,100x200", crate::capture::region::Rect { x: 10, y: 20, w: 100, h: 200 })]
    #[case("-5,-10,1x1", crate::capture::region::Rect { x: -5, y: -10, w: 1, h: 1 })]
    #[case(" 10 , 20 , 100 x 200 ", crate::capture::region::Rect { x: 10, y: 20, w: 100, h: 200 })]
    fn parses_region(#[case] s: &str, #[case] expected: crate::capture::region::Rect) {
        assert_eq!(parse_region(s).unwrap(), expected);
    }

    #[rstest]
    #[case::missing_size("10,20")]
    #[case::size_without_x("10,20,100")]
    #[case::non_numeric_x("a,20,100x200")]
    #[case::non_numeric_height("10,20,100xb")]
    #[case::negative_width("10,20,-1x200")]
    #[case::empty("")]
    fn rejects_malformed_region(#[case] s: &str) {
        assert!(parse_region(s).is_err(), "{s:?} should not parse");
    }

    /// The clap `selection` group makes these flags mutually exclusive on the command line,
    /// so build `Args` directly to pin the match-arm precedence in `parse_selection` itself.
    #[test]
    fn selection_precedence_is_full_first() {
        let args = Args {
            full: true,
            per_output: true,
            focused: true,
            output: Some("DP-1".into()),
            window: true,
            region: Some("0,0,1x1".into()),
            interactive: true,
            ..Args::default()
        };
        assert_eq!(parse_selection(&args).unwrap(), Selection::Full);
    }

    #[rstest]
    #[case::per_output(Args { per_output: true, focused: true, output: Some("DP-1".into()), window: true, region: Some("0,0,1x1".into()), interactive: true, ..Args::default() }, Selection::PerOutput)]
    #[case::focused(Args { focused: true, output: Some("DP-1".into()), window: true, region: Some("0,0,1x1".into()), interactive: true, ..Args::default() }, Selection::Focused)]
    #[case::output(Args { output: Some("DP-1".into()), window: true, region: Some("0,0,1x1".into()), interactive: true, ..Args::default() }, Selection::Output("DP-1".into()))]
    #[case::window(Args { window: true, region: Some("0,0,1x1".into()), interactive: true, ..Args::default() }, Selection::Window)]
    #[case::region(Args { region: Some("1,2,3x4".into()), interactive: true, ..Args::default() }, Selection::Region(crate::capture::region::Rect { x: 1, y: 2, w: 3, h: 4 }))]
    #[case::interactive(Args { interactive: true, ..Args::default() }, Selection::Interactive)]
    #[case::default_is_interactive(Args::default(), Selection::Interactive)]
    fn parse_selection_honours_precedence(#[case] args: Args, #[case] expected: Selection) {
        assert_eq!(parse_selection(&args).unwrap(), expected);
    }

    #[test]
    fn parse_selection_propagates_region_parse_errors() {
        let args = Args {
            region: Some("nonsense".into()),
            ..Args::default()
        };
        assert!(parse_selection(&args).is_err());
    }

    #[rstest]
    #[case(Selection::Full, "full")]
    #[case(Selection::PerOutput, "output")]
    #[case(Selection::Focused, "focused")]
    #[case(Selection::Output("DP-1".into()), "output")]
    #[case(Selection::Window, "window")]
    #[case(Selection::Region(crate::capture::region::Rect { x: 0, y: 0, w: 1, h: 1 }), "region")]
    #[case(Selection::Interactive, "region")]
    fn selection_label_maps_every_variant(#[case] s: Selection, #[case] expected: &str) {
        assert_eq!(selection_label(&s), expected);
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

    #[test]
    fn edit_conflicts_with_per_output() {
        let err =
            Harness::try_parse_from(["test", "screenshot", "--edit", "--per-output"]).unwrap_err();
        // Clap emits an ArgumentConflict error for `conflicts_with` violations.
        assert_eq!(err.kind(), clap::error::ErrorKind::ArgumentConflict);
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
