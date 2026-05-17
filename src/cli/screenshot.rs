//! `screenshot` subcommand — capture and write to sinks (optionally via the annotation editor).

use anyhow::{Context as _, Result, bail};
use clap::Args as ClapArgs;

use super::SinkSpec;
use crate::capture::{Capturer, Selection, wlr::WlrCapturer};
use crate::config::Config;
use crate::context::Context;
use crate::output::Outputs;

#[derive(Debug, Default, ClapArgs)]
pub struct Args {
    /// Capture the entire virtual desktop, stitched across monitors.
    #[arg(long, group = "selection")]
    pub full: bool,
    /// Capture each connected output to a separate file.
    #[arg(long, group = "selection", conflicts_with = "edit")]
    pub per_output: bool,
    /// Capture only the currently focused monitor.
    #[arg(long, group = "selection")]
    pub focused: bool,
    /// Capture a specific output by name (e.g. `DP-1`).
    #[arg(long, value_name = "NAME", group = "selection")]
    pub output: Option<String>,
    /// Capture the currently active window (via Hyprland IPC).
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
    #[arg(long, alias = "annotate")]
    pub edit: bool,

    /// Sink(s) to receive the image. Repeatable.
    #[arg(long = "to", value_name = "SINK")]
    pub to: Vec<SinkSpec>,

    /// Delay before capture (e.g. `2s`, `500ms`).
    #[arg(long, value_parser = humantime::parse_duration)]
    pub delay: Option<std::time::Duration>,

    /// Include the mouse cursor in the capture.
    #[arg(long)]
    pub cursor: bool,
}

pub async fn run(args: Args) -> Result<()> {
    let config = Config::load_default().context("loading configuration")?;
    let ctx = Context::new(config).await?;

    let selection = parse_selection(&args)?;
    let sinks = if args.to.is_empty() {
        ctx.config.default_sinks()
    } else {
        args.to.clone()
    };

    if let Some(delay) = args.delay {
        tokio::time::sleep(delay).await;
    }

    let paths = execute(ctx, selection, args.cursor, sinks, args.edit).await?;
    for p in &paths {
        println!("{}", p.display());
    }
    Ok(())
}
/// Headless core of the screenshot pipeline used by both the CLI (`run`) and the daemon's IPC
/// handler. Resolves compositor-aware selections, captures, encodes, and writes — returning the
/// file paths produced by `OutputSink`s (clipboard sinks contribute nothing). `delay` and arg
/// parsing live one level up since they're CLI-specific concerns.
///
/// When `edit == true`, the captured image is handed to the annotation editor (in-memory, no
/// PNG round-trip) and the editor's save action fans the result out to `sinks` instead. The
/// editor path rejects `Selection::PerOutput` since the editor operates on a single image.
pub async fn execute(
    ctx: crate::context::Ctx,
    selection: Selection,
    cursor: bool,
    sinks: Vec<SinkSpec>,
    edit: bool,
) -> Result<Vec<std::path::PathBuf>> {
    if edit && matches!(selection, Selection::PerOutput) {
        bail!(
            "`--edit` is incompatible with `--per-output` (the annotation editor operates on a single image)"
        );
    }

    // Resolve compositor-aware selections up front (Hyprland IPC + interactive overlay) so the
    // rest of the pipeline only ever sees concrete Region/Output/Full/PerOutput variants. The
    // interactive selector can also override `cursor` via its toolbar toggle.
    let (selection, cursor) = resolve_selection(selection, cursor, &ctx).await?;

    let capturer = WlrCapturer::new()?;
    let t0 = std::time::Instant::now();
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
            let outputs = Outputs::from_specs_per_output(&sinks, &ctx.config, &ctx_fname)?;
            let png = crate::output::encode_png(img, ctx.config.output.compression)?;
            let paths = outputs.write_png(&png).await?;
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
            )
            .await;
        }
        #[cfg(not(feature = "ui"))]
        {
            anyhow::bail!(
                "`--edit` requires the `ui` cargo feature; rebuild with it or drop the flag"
            );
        }
    }

    let ctx_fname = crate::config::FilenameContext {
        output: None,
        selection: Some(selection_label(&selection)),
    };
    let outputs = Outputs::from_specs(&sinks, &ctx.config, &ctx_fname)?;
    let png = crate::output::encode_png(&stitched, ctx.config.output.compression)?;
    let paths = outputs.write_png(&png).await?;
    Ok(paths)
}

/// Convert a screencopy `CapturedImage` (BGRA, possibly with padded stride) into a tight RGBA
/// [`DocumentBase`] ready for the annotation canvas. Lives here so the editor branch of
/// `execute` can call it directly without going through `crate::ui`.
#[cfg(feature = "ui")]
fn base_from_captured(img: &crate::capture::CapturedImage) -> crate::annotate::DocumentBase {
    let w = img.width as usize;
    let h = img.height as usize;
    let row = w * 4;
    let stride = img.stride as usize;
    let mut rgba = vec![0u8; row * h];
    for y in 0..h {
        let src = &img.pixels[y * stride..y * stride + row];
        let dst = &mut rgba[y * row..(y + 1) * row];
        for (s, d) in src.chunks_exact(4).zip(dst.chunks_exact_mut(4)) {
            d[0] = s[2];
            d[1] = s[1];
            d[2] = s[0];
            d[3] = s[3];
        }
    }
    crate::annotate::DocumentBase {
        pixels: std::sync::Arc::from(rgba.into_boxed_slice()),
        width: img.width,
        height: img.height,
        stride: img.width * 4,
    }
}

/// Short label for the filename `{selection}` token.
fn selection_label(s: &Selection) -> &'static str {
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

/// Resolve compositor-aware selections (`Interactive`, `Window`, `Focused`) into concrete ones
/// that the capture pipeline can act on directly. Also passes the (potentially updated) cursor
/// flag back — the interactive selector's toolbar can override the CLI default.
///
/// - `Interactive` opens the GTK overlay; the resulting selection + cursor flag come from the
///   user's toolbar choices.
/// - `Window` reads the currently active window from Hyprland and is replaced with `Region(rect)`.
/// - `Focused` reads the currently focused monitor from Hyprland and is replaced with
///   `Output(name)`.
///
/// All other variants pass through unchanged (with the input `cursor` unchanged).
async fn resolve_selection(
    selection: Selection,
    cursor: bool,
    _ctx: &std::sync::Arc<crate::context::Context>,
) -> Result<(Selection, bool)> {
    match selection {
        Selection::Interactive => {
            #[cfg(feature = "ui")]
            {
                let outcome = crate::ui::selector::pick_region(_ctx.clone(), cursor)
                    .await
                    .context("interactive region selection")?;
                tracing::info!(?outcome.selection, cursor = outcome.cursor, "selector outcome");
                // Resolve any compositor-aware variants the user picked (Window) by recursing.
                let (resolved, _) =
                    Box::pin(resolve_selection(outcome.selection, outcome.cursor, _ctx)).await?;
                Ok((resolved, outcome.cursor))
            }
            #[cfg(not(feature = "ui"))]
            {
                anyhow::bail!(
                    "interactive selector requires the `ui` cargo feature; pass a concrete --region, --full, or other flag"
                );
            }
        }
        Selection::Window => {
            let win = crate::hypr::active_window()
                .await
                .context("querying active window from Hyprland")?;
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
            Ok((Selection::Region(rect), cursor))
        }
        Selection::Focused => {
            let name = crate::hypr::focused_monitor()
                .await
                .context("querying focused monitor from Hyprland")?;
            tracing::info!(monitor = %name, "focused monitor resolved");
            Ok((Selection::Output(name), cursor))
        }
        other => Ok((other, cursor)),
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
    let mut parts = spec.splitn(3, ',');
    let x = parts
        .next()
        .ok_or_else(|| anyhow::anyhow!("invalid region: {spec} (expected X,Y,WxH)"))?;
    let y = parts
        .next()
        .ok_or_else(|| anyhow::anyhow!("invalid region: {spec} (expected X,Y,WxH)"))?;
    let size = parts
        .next()
        .ok_or_else(|| anyhow::anyhow!("invalid region: {spec} (expected X,Y,WxH)"))?;
    let (ws, hs) = size
        .split_once('x')
        .ok_or_else(|| anyhow::anyhow!("invalid region size: {size} (expected WxH)"))?;

    Ok(crate::capture::region::Rect {
        x: x.trim().parse()?,
        y: y.trim().parse()?,
        w: ws.trim().parse()?,
        h: hs.trim().parse()?,
    })
}

/// Minimal humantime-like duration parser to avoid an extra dependency.
mod humantime {
    use std::time::Duration;

    pub fn parse_duration(s: &str) -> Result<Duration, String> {
        let s = s.trim();
        let (num, unit) = s
            .find(|c: char| c.is_alphabetic())
            .map(|i| s.split_at(i))
            .ok_or_else(|| format!("missing unit in duration `{s}` (try `2s` or `500ms`)"))?;
        let value: u64 = num
            .trim()
            .parse()
            .map_err(|e| format!("invalid number in duration `{s}`: {e}"))?;
        match unit {
            "ms" => Ok(Duration::from_millis(value)),
            "s" => Ok(Duration::from_secs(value)),
            "m" => Ok(Duration::from_secs(value * 60)),
            other => Err(format!("unknown duration unit `{other}` (try ms, s, m)")),
        }
    }
}

#[cfg(test)]
mod tests {
    use super::*;
    use clap::Parser;
    use pretty_assertions::assert_eq;
    use rstest::rstest;

    #[rstest]
    #[case("10,20,100x200", crate::capture::region::Rect { x: 10, y: 20, w: 100, h: 200 })]
    fn parses_region(#[case] s: &str, #[case] expected: crate::capture::region::Rect) {
        assert_eq!(parse_region(s).unwrap(), expected);
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
    fn parses_annotate_alias() {
        let cli = Harness::try_parse_from(["test", "screenshot", "--annotate"]).unwrap();
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
}
