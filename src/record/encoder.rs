//! Spawns the system `ffmpeg` binary and pipes raw video frames to its stdin.
//!
//! PipeWire only transports raw frames; encoding them into an actual video file is this
//! module's job. Shelling out to `ffmpeg` (checked by `snypr doctor`) avoids linking libav
//! directly, keeping the `screencast` feature's *build-time* dependency footprint limited to
//! the capture side (`ashpd` + `pipewire`) — see AGENTS.md's "don't add dependencies
//! lightly".

use std::path::{Path, PathBuf};
use std::process::Stdio;
use std::time::Duration;

use anyhow::{Context as _, Result, bail};
use tokio::io::{AsyncBufReadExt as _, AsyncWriteExt as _, BufReader};
use tokio::process::{Child, Command};

use super::stream::RawPixelFormat;
use crate::config::VideoFormat;

/// How long [`Encoder::finish`] waits for `ffmpeg` to flush its container trailer (moov atom,
/// WebM cues, ...) after stdin closes before giving up and reporting an error.
const FINISH_TIMEOUT: Duration = Duration::from_secs(15);

/// Build the `ffmpeg` argv for encoding a raw frame stream into `path`. A pure function so
/// it's unit-testable without `ffmpeg` installed.
pub(crate) fn build_ffmpeg_args(
    width: u32,
    height: u32,
    pix_fmt: RawPixelFormat,
    fps: u32,
    format: VideoFormat,
    crf: u8,
    path: &Path,
) -> Vec<String> {
    let mut args = vec![
        "-hide_banner".to_owned(),
        "-loglevel".to_owned(),
        "error".to_owned(),
        "-y".to_owned(),
        "-f".to_owned(),
        "rawvideo".to_owned(),
        "-pix_fmt".to_owned(),
        pix_fmt.ffmpeg_pix_fmt().to_owned(),
        "-s".to_owned(),
        format!("{width}x{height}"),
        "-r".to_owned(),
        fps.to_string(),
        "-i".to_owned(),
        "pipe:0".to_owned(),
    ];
    match format {
        VideoFormat::Mp4 => args.extend([
            "-c:v".to_owned(),
            "libx264".to_owned(),
            "-pix_fmt".to_owned(),
            "yuv420p".to_owned(),
            "-crf".to_owned(),
            crf.to_string(),
            "-movflags".to_owned(),
            "+faststart".to_owned(),
        ]),
        VideoFormat::WebM => args.extend([
            "-c:v".to_owned(),
            "libvpx-vp9".to_owned(),
            "-crf".to_owned(),
            crf.to_string(),
            "-b:v".to_owned(),
            "0".to_owned(),
        ]),
        // Single-pass in v1 — no palette optimization (see the plan's "Known v1
        // limitations"). `-loop 0` makes the GIF loop indefinitely, matching every other
        // screen-recording tool's default.
        VideoFormat::Gif => args.extend(["-loop".to_owned(), "0".to_owned()]),
    }
    args.push(path.display().to_string());
    args
}

/// A running `ffmpeg` encoder child process, fed one raw frame at a time via its stdin.
pub(crate) struct Encoder {
    child: Child,
}

impl Encoder {
    /// Spawn `ffmpeg` with argv built from the given parameters, creating `path`'s parent
    /// directory if needed. `ffmpeg`'s stderr is drained into `tracing::debug!` on a
    /// background task so encoder diagnostics are visible under `RUST_LOG=snypr=debug`
    /// without risking a filled-pipe deadlock (ffmpeg blocks writing to stderr once its
    /// buffer fills if nobody reads it).
    pub async fn spawn(
        path: &Path,
        width: u32,
        height: u32,
        pix_fmt: RawPixelFormat,
        fps: u32,
        format: VideoFormat,
        crf: u8,
    ) -> Result<Self> {
        if let Some(parent) = path.parent()
            && !parent.as_os_str().is_empty()
        {
            tokio::fs::create_dir_all(parent)
                .await
                .with_context(|| format!("creating {}", parent.display()))?;
        }

        let args = build_ffmpeg_args(width, height, pix_fmt, fps, format, crf, path);
        let mut child = Command::new("ffmpeg")
            .args(&args)
            .stdin(Stdio::piped())
            .stdout(Stdio::null())
            .stderr(Stdio::piped())
            // `snypr record`'s foreground path stops the encoder by closing its stdin (see
            // `finish`), expecting a clean trailer flush — not by killing it. But Ctrl+C in
            // an interactive terminal sends SIGINT to every process in the *foreground
            // process group*, which by default includes this child too, killing it before
            // `finish` gets a chance to close stdin gracefully (surfaced as ffmpeg exiting
            // with a bare, unhelpful status like 255). Spawning into its own process group
            // makes the terminal's Ctrl+C only reach `snypr` itself.
            .process_group(0)
            .spawn()
            .context("spawning ffmpeg (is it installed? see `snypr doctor`)")?;

        if let Some(stderr) = child.stderr.take() {
            tokio::spawn(async move {
                let mut lines = BufReader::new(stderr).lines();
                while let Ok(Some(line)) = lines.next_line().await {
                    // No explicit `target:` override here (unlike an earlier version of
                    // this code): that pinned the target to the literal string `"ffmpeg"`,
                    // which `RUST_LOG=snypr=debug` — the exact filter this doc comment and
                    // `Encoder::finish`'s error message tell users to set — does *not*
                    // match, since env-filter directives match on target *prefix* and
                    // `"ffmpeg"` doesn't start with `"snypr"`. Leaving the target as this
                    // module's default (`snypr::record::encoder`) fixes that.
                    tracing::debug!(ffmpeg.line = %line, "ffmpeg stderr");
                }
            });
        }

        Ok(Self { child })
    }

    /// Write one raw frame's bytes to `ffmpeg`'s stdin.
    pub async fn write_frame(&mut self, pixels: &[u8]) -> Result<()> {
        let stdin = self
            .child
            .stdin
            .as_mut()
            .context("ffmpeg stdin is already closed")?;
        stdin
            .write_all(pixels)
            .await
            .context("writing a frame to ffmpeg")?;
        Ok(())
    }

    /// Close `ffmpeg`'s stdin and wait for it to finish muxing, returning `path` once it has
    /// exited successfully.
    pub async fn finish(mut self, path: PathBuf) -> Result<PathBuf> {
        // Dropping the stdin handle closes the pipe, which is `ffmpeg`'s cue to flush the
        // container trailer and exit — it never exits on its own while rawvideo keeps
        // "arriving".
        drop(self.child.stdin.take());
        let status = tokio::time::timeout(FINISH_TIMEOUT, self.child.wait())
            .await
            .context("timed out waiting for ffmpeg to finish encoding")?
            .context("waiting for ffmpeg to exit")?;
        if !status.success() {
            bail!(
                "ffmpeg exited with {status}; rerun with RUST_LOG=snypr=debug to see its \
                 stderr output"
            );
        }
        Ok(path)
    }
}

#[cfg(test)]
mod tests {
    use pretty_assertions::assert_eq;
    use rstest::rstest;

    use super::*;

    #[test]
    fn mp4_args_include_libx264_and_faststart() {
        let args = build_ffmpeg_args(
            1920,
            1080,
            RawPixelFormat::Bgrx,
            30,
            VideoFormat::Mp4,
            23,
            Path::new("/tmp/out.mp4"),
        );
        assert_eq!(
            args,
            vec![
                "-hide_banner",
                "-loglevel",
                "error",
                "-y",
                "-f",
                "rawvideo",
                "-pix_fmt",
                "bgr0",
                "-s",
                "1920x1080",
                "-r",
                "30",
                "-i",
                "pipe:0",
                "-c:v",
                "libx264",
                "-pix_fmt",
                "yuv420p",
                "-crf",
                "23",
                "-movflags",
                "+faststart",
                "/tmp/out.mp4",
            ]
        );
    }

    #[test]
    fn webm_args_include_libvpx_vp9() {
        let args = build_ffmpeg_args(
            640,
            480,
            RawPixelFormat::Rgba,
            60,
            VideoFormat::WebM,
            30,
            Path::new("/tmp/out.webm"),
        );
        assert_eq!(
            args,
            vec![
                "-hide_banner",
                "-loglevel",
                "error",
                "-y",
                "-f",
                "rawvideo",
                "-pix_fmt",
                "rgba",
                "-s",
                "640x480",
                "-r",
                "60",
                "-i",
                "pipe:0",
                "-c:v",
                "libvpx-vp9",
                "-crf",
                "30",
                "-b:v",
                "0",
                "/tmp/out.webm",
            ]
        );
    }

    #[test]
    fn gif_args_loop_forever_and_skip_crf() {
        let args = build_ffmpeg_args(
            320,
            240,
            RawPixelFormat::Argb,
            15,
            VideoFormat::Gif,
            23,
            Path::new("/tmp/out.gif"),
        );
        assert_eq!(
            args,
            vec![
                "-hide_banner",
                "-loglevel",
                "error",
                "-y",
                "-f",
                "rawvideo",
                "-pix_fmt",
                "argb",
                "-s",
                "320x240",
                "-r",
                "15",
                "-i",
                "pipe:0",
                "-loop",
                "0",
                "/tmp/out.gif",
            ]
        );
    }

    #[rstest]
    #[case(RawPixelFormat::Rgbx, "rgb0")]
    #[case(RawPixelFormat::Bgrx, "bgr0")]
    #[case(RawPixelFormat::XRgb, "0rgb")]
    #[case(RawPixelFormat::XBgr, "0bgr")]
    #[case(RawPixelFormat::Rgba, "rgba")]
    #[case(RawPixelFormat::Bgra, "bgra")]
    #[case(RawPixelFormat::Argb, "argb")]
    #[case(RawPixelFormat::Abgr, "abgr")]
    fn every_pixel_format_maps_to_a_pix_fmt(#[case] fmt: RawPixelFormat, #[case] expected: &str) {
        let args = build_ffmpeg_args(2, 2, fmt, 30, VideoFormat::Mp4, 23, Path::new("/tmp/o.mp4"));
        let idx = args.iter().position(|a| a == "-pix_fmt").unwrap();
        assert_eq!(args[idx + 1], expected);
    }
}
