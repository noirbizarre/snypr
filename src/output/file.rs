//! File sink — write PNG bytes to disk.

use std::path::PathBuf;

use anyhow::{Context as _, Result};
use async_trait::async_trait;

use super::OutputSink;

pub struct FileSink {
    pub path: PathBuf,
}

impl FileSink {
    pub fn new(path: PathBuf) -> Self {
        Self { path }
    }
}

#[async_trait]
impl OutputSink for FileSink {
    async fn write_png(&self, bytes: &[u8]) -> Result<Option<PathBuf>> {
        if let Some(parent) = self.path.parent()
            && !parent.as_os_str().is_empty()
        {
            tokio::fs::create_dir_all(parent)
                .await
                .with_context(|| format!("creating {}", parent.display()))?;
        }
        tokio::fs::write(&self.path, bytes)
            .await
            .with_context(|| format!("writing {}", self.path.display()))?;
        tracing::info!(path = %self.path.display(), bytes = bytes.len(), "wrote screenshot");
        Ok(Some(self.path.clone()))
    }
}

#[cfg(test)]
mod tests {
    use super::*;

    #[tokio::test]
    async fn writes_bytes() {
        let dir = tempfile::tempdir().unwrap();
        let path = dir.path().join("nested").join("out.png");
        let sink = FileSink::new(path.clone());
        sink.write_png(b"\x89PNG\r\n\x1a\n").await.unwrap();
        let read = tokio::fs::read(&path).await.unwrap();
        assert!(read.starts_with(&[0x89, b'P', b'N', b'G']));
    }
}
