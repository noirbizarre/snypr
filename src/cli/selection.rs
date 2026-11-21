//! Shared capture-target selection: CLI flags, parsing, and compositor-aware resolution.
//!
//! Both `screenshot` and `record` accept the exact same "what to capture" flags
//! (`--full`/`--per-output`/`--focused`/`--output`/`--window`/`--region`/`--interactive`),
//! so the flags themselves ([`SelectionArgs`]), their parsing ([`parse`]), and the
//! compositor-aware resolution of `Interactive`/`Window`/`Focused` into concrete
//! `Region`/`Output` variants ([`resolve`]) live here instead of being duplicated.
//!
//! `screenshot`-specific concerns (the annotation editor, output-sink switching) stay out of
//! this module: [`ResolvedSelection`] carries `edit`/`output_mode` fields because the
//! interactive selector's toolbar always reports them, but callers that don't care (e.g.
//! `record`) simply ignore them.

use anyhow::{Context as _, Result};
use clap::Args as ClapArgs;

use super::SinkSpec;
use crate::capture::Selection;
use crate::capture::region::Rect;
use crate::i18n::fl;
use crate::output::OutputMode;

/// The "what to capture" flags shared by `screenshot` and `record`. Flatten this into a
/// subcommand's `Args` with `#[command(flatten)]`.
#[derive(Debug, Default, ClapArgs)]
pub struct SelectionArgs {
    /// Capture the entire virtual desktop, stitched across monitors.
    #[arg(long, group = "selection")]
    pub full: bool,
    /// Capture each connected output to a separate file.
    #[arg(long, group = "selection")]
    pub per_output: bool,
    /// Capture only the currently focused monitor (via Hyprland/Sway/Niri IPC, or the generic
    /// wlr-foreign-toplevel protocol on other compositors that advertise it).
    #[arg(long, group = "selection")]
    pub focused: bool,
    /// Capture a specific output by name (e.g. `DP-1`).
    #[arg(long, value_name = "NAME", group = "selection")]
    pub output: Option<String>,
    /// Capture the currently active window (via Hyprland, Sway, or Niri IPC).
    #[arg(long, group = "selection")]
    pub window: bool,
    /// Capture an explicit region as `X,Y,WxH`.
    #[arg(long, value_name = "X,Y,WxH", group = "selection")]
    pub region: Option<String>,
    /// Launch an interactive selector overlay.
    #[arg(short, long, group = "selection")]
    pub interactive: bool,
}

/// Parse the mutually-exclusive selection flags into a [`Selection`]. Defaults to
/// `Interactive` when none are given.
pub(crate) fn parse(args: &SelectionArgs) -> Result<Selection> {
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

pub(crate) fn parse_region(spec: &str) -> Result<Rect> {
    let invalid_region = || anyhow::anyhow!("{}", fl!("error-invalid-region", spec = spec));
    let mut parts = spec.splitn(3, ',');
    let x = parts.next().ok_or_else(invalid_region)?;
    let y = parts.next().ok_or_else(invalid_region)?;
    let size = parts.next().ok_or_else(invalid_region)?;
    let (ws, hs) = size
        .split_once('x')
        .ok_or_else(|| anyhow::anyhow!("{}", fl!("error-invalid-region-size", size = size)))?;

    Ok(Rect {
        x: x.trim().parse()?,
        y: y.trim().parse()?,
        w: ws.trim().parse()?,
        h: hs.trim().parse()?,
    })
}

/// Short label for the filename `{selection}` token. Shared by `screenshot` and `record`.
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

/// What [`resolve`] produced. Every field but `selection` is an *override* the interactive
/// selector's toolbar may have applied on top of the CLI/config values.
#[derive(Debug)]
pub(crate) struct ResolvedSelection {
    pub selection: Selection,
    pub cursor: bool,
    /// Only meaningful to `screenshot`/`draw` (the annotation editor). `record` ignores it —
    /// there is no video-recording equivalent of "annotate this frame" in v1.
    pub edit: bool,
    pub delay: Option<u32>,
    /// Destination picked on the selector's output switcher. Only meaningful to
    /// `screenshot`/`draw`'s `SinkSpec`-based output; `record` ignores it (a recording
    /// always writes a single video file, there is no file/clipboard/both switcher).
    ///
    /// `None` whenever the selector did not run, which leaves the caller's sinks exactly as
    /// the CLI/config resolved them — that preserves multi-file fan-out
    /// (`--to file=A --to file=B`) for every non-interactive path, including the daemon.
    pub output_mode: Option<OutputMode>,
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

/// Resolve compositor-aware selections (`Interactive`, `Window`, `Focused`) into concrete ones
/// that the capture pipeline can act on directly. Also returns the (potentially updated)
/// cursor flag, an `edit` flag, the final pre-capture delay, and the chosen output
/// destination. The interactive selector's toolbar can override every one of these — its
/// delay spinner, cursor toggle, output switcher, and Annotate button take precedence over
/// the values threaded in from the caller.
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
///
/// `sinks` only seeds the interactive selector's output switcher (a `screenshot`/`draw`
/// concern); callers with no sink concept of their own (e.g. `record`) can pass `&[]`.
pub(crate) async fn resolve(
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
                // `Duration`; convert at the boundary so the rest of the pipeline stays in
                // plain integer seconds.
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
                let inner = Box::pin(resolve(
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
                .await
                .ok_or_else(|| anyhow::anyhow!("{}", fl!("error-unsupported-compositor")))?;
            let win = backend
                .active_window()
                .await
                .with_context(|| format!("querying active window from {}", backend.name()))?;
            let rect = win
                .rect()
                .ok_or_else(|| anyhow::anyhow!("{}", fl!("error-active-window-no-geometry")))?;
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
                .await
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

#[cfg(test)]
mod tests {
    use pretty_assertions::assert_eq;
    use rstest::rstest;

    use super::*;
    use crate::testing::test_ctx;

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
    #[case::region(Selection::Region(Rect { x: 0, y: 0, w: 4, h: 4 }))]
    #[case::output(Selection::Output("DP-1".to_owned()))]
    #[tokio::test]
    async fn non_interactive_selections_pass_through_untouched(#[case] selection: Selection) {
        // No compositor is queried for these, so they resolve without any window-manager
        // backend running.
        let ctx = test_ctx().await;
        let resolved = resolve(selection.clone(), true, Some(5), &[], &ctx)
            .await
            .unwrap();

        assert_eq!(resolved.selection, selection);
        assert!(resolved.cursor, "cursor flag must survive the pass-through");
        assert!(!resolved.edit, "only the selector can request the editor");
        assert_eq!(resolved.delay, Some(5));
        assert_eq!(
            resolved.output_mode, None,
            "no selector ran, so the caller's sinks must be left alone"
        );
    }

    #[rstest]
    #[case::window(Selection::Window)]
    #[case::focused(Selection::Focused)]
    #[tokio::test]
    async fn resolve_fails_clearly_without_a_detected_backend(#[case] selection: Selection) {
        crate::testing::set_compositor_env(None, None, None);
        let ctx = test_ctx().await;
        let err = resolve(selection, false, None, &[], &ctx)
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
    async fn resolve_window_resolves_via_the_detected_backend() {
        let dir = tempfile::tempdir().unwrap();
        let sock = dir.path().join("sway.sock");
        crate::testing::set_compositor_env(None, Some(&sock), None);
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
        let resolved = resolve(Selection::Window, true, None, &[], &ctx)
            .await
            .unwrap();
        assert_eq!(
            resolved.selection,
            Selection::Region(Rect {
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
    async fn resolve_focused_resolves_via_the_detected_backend() {
        let dir = tempfile::tempdir().unwrap();
        let sock = dir.path().join("sway.sock");
        crate::testing::set_compositor_env(None, Some(&sock), None);
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
        let resolved = resolve(Selection::Focused, false, Some(5), &[], &ctx)
            .await
            .unwrap();
        assert_eq!(resolved.selection, Selection::Output("DP-2".to_owned()));
        assert_eq!(resolved.delay, Some(5));
        server.await.unwrap();
    }

    #[rstest]
    #[case("10,20,100x200", Rect { x: 10, y: 20, w: 100, h: 200 })]
    #[case("-5,-10,1x1", Rect { x: -5, y: -10, w: 1, h: 1 })]
    #[case(" 10 , 20 , 100 x 200 ", Rect { x: 10, y: 20, w: 100, h: 200 })]
    fn parses_region(#[case] s: &str, #[case] expected: Rect) {
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
    /// so build `SelectionArgs` directly to pin the match-arm precedence in `parse` itself.
    #[test]
    fn selection_precedence_is_full_first() {
        let args = SelectionArgs {
            full: true,
            per_output: true,
            focused: true,
            output: Some("DP-1".into()),
            window: true,
            region: Some("0,0,1x1".into()),
            interactive: true,
        };
        assert_eq!(parse(&args).unwrap(), Selection::Full);
    }

    #[rstest]
    #[case::per_output(SelectionArgs { per_output: true, focused: true, output: Some("DP-1".into()), window: true, region: Some("0,0,1x1".into()), interactive: true, ..Default::default() }, Selection::PerOutput)]
    #[case::focused(SelectionArgs { focused: true, output: Some("DP-1".into()), window: true, region: Some("0,0,1x1".into()), interactive: true, ..Default::default() }, Selection::Focused)]
    #[case::output(SelectionArgs { output: Some("DP-1".into()), window: true, region: Some("0,0,1x1".into()), interactive: true, ..Default::default() }, Selection::Output("DP-1".into()))]
    #[case::window(SelectionArgs { window: true, region: Some("0,0,1x1".into()), interactive: true, ..Default::default() }, Selection::Window)]
    #[case::region(SelectionArgs { region: Some("1,2,3x4".into()), interactive: true, ..Default::default() }, Selection::Region(Rect { x: 1, y: 2, w: 3, h: 4 }))]
    #[case::interactive(SelectionArgs { interactive: true, ..Default::default() }, Selection::Interactive)]
    #[case::default_is_interactive(SelectionArgs::default(), Selection::Interactive)]
    fn parse_honours_precedence(#[case] args: SelectionArgs, #[case] expected: Selection) {
        assert_eq!(parse(&args).unwrap(), expected);
    }

    #[test]
    fn parse_propagates_region_parse_errors() {
        let args = SelectionArgs {
            region: Some("nonsense".into()),
            ..Default::default()
        };
        assert!(parse(&args).is_err());
    }

    #[rstest]
    #[case(Selection::Full, "full")]
    #[case(Selection::PerOutput, "output")]
    #[case(Selection::Focused, "focused")]
    #[case(Selection::Output("DP-1".into()), "output")]
    #[case(Selection::Window, "window")]
    #[case(Selection::Region(Rect { x: 0, y: 0, w: 1, h: 1 }), "region")]
    #[case(Selection::Interactive, "region")]
    fn selection_label_maps_every_variant(#[case] s: Selection, #[case] expected: &str) {
        assert_eq!(selection_label(&s), expected);
    }
}
