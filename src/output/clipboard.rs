//! Clipboard sink — publish PNG bytes as `image/png` via `wl-clipboard-rs`.

use std::path::PathBuf;

use anyhow::{Context as _, Result};
use async_trait::async_trait;

use super::OutputSink;

pub struct ClipboardSink;

impl ClipboardSink {
    pub fn new() -> Self {
        Self
    }
}

impl Default for ClipboardSink {
    fn default() -> Self {
        Self::new()
    }
}

#[async_trait]
impl OutputSink for ClipboardSink {
    async fn write_png(&self, bytes: &[u8]) -> Result<Option<PathBuf>> {
        let bytes = bytes.to_vec();
        tokio::task::spawn_blocking(move || copy_png(&bytes))
            .await
            .map_err(|e| anyhow::anyhow!("clipboard task panicked: {e}"))??;
        Ok(None)
    }
}

fn copy_png(bytes: &[u8]) -> Result<()> {
    use wl_clipboard_rs::copy::{MimeType, Options, Source};

    let opts = Options::new();
    opts.copy(
        Source::Bytes(bytes.to_vec().into()),
        MimeType::Specific("image/png".to_owned()),
    )
    .context("publishing image/png to wayland clipboard")?;
    tracing::info!(bytes = bytes.len(), "copied PNG to clipboard");
    Ok(())
}
