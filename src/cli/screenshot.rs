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

    let selection = parse_selection(&args)?;

    if matches!(selection, Selection::Interactive) {
        tracing::warn!(
            "interactive selector overlay is not yet implemented; capturing all outputs as a fallback"
        );
    }

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

    let t1 = std::time::Instant::now();
    let stitched = crate::capture::region::stitch(&images, &selection)?;
    tracing::info!(
        elapsed_ms = t1.elapsed().as_millis() as u64,
        width = stitched.width,
        height = stitched.height,
        "stitched"
    );

    let sinks = if args.to.is_empty() {
        ctx.config.default_sinks()
    } else {
        args.to.clone()
    };

    let outputs = Outputs::from_specs(&sinks, &ctx.config)?;
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
