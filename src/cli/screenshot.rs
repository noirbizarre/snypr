//! `screenshot` subcommand — capture and write to sinks.

use anyhow::{Context as _, Result};
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
    #[arg(long, group = "selection")]
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

    let mut selection = parse_selection(&args)?;

    // Resolve compositor-aware selections up front (Hyprland IPC + interactive overlay) so the
    // rest of the pipeline only ever sees concrete Region/Output/Full/PerOutput variants. We do
    // this *before* the optional delay so the user can compose the selection and then wait
    // quietly for the delay to elapse.
    selection = resolve_selection(selection, &ctx).await?;

    if let Some(delay) = args.delay {
        tokio::time::sleep(delay).await;
    }

    let capturer = WlrCapturer::new()?;
    let t0 = std::time::Instant::now();
    let images = capturer
        .capture(selection.clone(), args.cursor)
        .await
        .with_context(|| format!("capturing {:?}", selection))?;
    tracing::info!(
        elapsed_ms = t0.elapsed().as_millis() as u64,
        count = images.len(),
        "captured"
    );

    for img in &images {
        tracing::debug!(
            output = ?img.source.as_ref().map(|o| &o.name),
            width = img.width,
            height = img.height,
            stride = img.stride,
            "captured image"
        );
    }

    let sinks = if args.to.is_empty() {
        ctx.config.default_sinks()
    } else {
        args.to.clone()
    };

    // `--per-output` skips stitching entirely: each captured frame is encoded and written to its
    // own file (or copied to its own clipboard entry), with the output name interpolated into
    // the filename template. Templates that lack `{output}` get one auto-inserted to avoid the
    // multiple frames collapsing onto the same path.
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
            let t_e = std::time::Instant::now();
            let png = crate::output::encode_png(img)?;
            tracing::info!(
                elapsed_ms = t_e.elapsed().as_millis() as u64,
                bytes = png.len(),
                output = %name,
                "encoded PNG"
            );
            let t_w = std::time::Instant::now();
            let paths = outputs.write_png(&png).await?;
            tracing::info!(
                elapsed_ms = t_w.elapsed().as_millis() as u64,
                output = %name,
                "wrote sinks"
            );
            all_paths.extend(paths);
        }
        for p in &all_paths {
            println!("{}", p.display());
        }
        return Ok(());
    }

    let t1 = std::time::Instant::now();
    let stitched = crate::capture::region::stitch(&images, &selection)?;
    tracing::info!(
        elapsed_ms = t1.elapsed().as_millis() as u64,
        width = stitched.width,
        height = stitched.height,
        "stitched"
    );

    let ctx_fname = crate::config::FilenameContext {
        output: None,
        selection: Some(selection_label(&selection)),
    };
    let outputs = Outputs::from_specs(&sinks, &ctx.config, &ctx_fname)?;
    let t2 = std::time::Instant::now();
    let png = crate::output::encode_png(&stitched)?;
    tracing::info!(
        elapsed_ms = t2.elapsed().as_millis() as u64,
        bytes = png.len(),
        "encoded PNG"
    );
    let t3 = std::time::Instant::now();
    let paths = outputs.write_png(&png).await?;
    tracing::info!(elapsed_ms = t3.elapsed().as_millis() as u64, "wrote sinks");
    for p in &paths {
        println!("{}", p.display());
    }

    Ok(())
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
/// that the capture pipeline can act on directly.
///
/// - `Interactive` opens the GTK overlay and is replaced with `Region(rect)`.
/// - `Window` reads the currently active window from Hyprland and is replaced with `Region(rect)`.
/// - `Focused` reads the currently focused monitor from Hyprland and is replaced with
///   `Output(name)`.
///
/// All other variants pass through unchanged.
async fn resolve_selection(
    selection: Selection,
    _ctx: &std::sync::Arc<crate::context::Context>,
) -> Result<Selection> {
    match selection {
        Selection::Interactive => {
            #[cfg(feature = "ui")]
            {
                let rect = crate::ui::selector::pick_region(_ctx.clone())
                    .await
                    .context("interactive region selection")?;
                tracing::info!(
                    x = rect.x,
                    y = rect.y,
                    w = rect.w,
                    h = rect.h,
                    "region selected"
                );
                Ok(Selection::Region(rect))
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
            Ok(Selection::Region(rect))
        }
        Selection::Focused => {
            let name = crate::hypr::focused_monitor()
                .await
                .context("querying focused monitor from Hyprland")?;
            tracing::info!(monitor = %name, "focused monitor resolved");
            Ok(Selection::Output(name))
        }
        other => Ok(other),
    }
}

fn parse_selection(args: &Args) -> Result<Selection> {
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
    use pretty_assertions::assert_eq;
    use rstest::rstest;

    #[rstest]
    #[case("10,20,100x200", crate::capture::region::Rect { x: 10, y: 20, w: 100, h: 200 })]
    fn parses_region(#[case] s: &str, #[case] expected: crate::capture::region::Rect) {
        assert_eq!(parse_region(s).unwrap(), expected);
    }
}
