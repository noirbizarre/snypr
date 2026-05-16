//! Output sinks (file, clipboard) and PNG encoding.

pub mod clipboard;
pub mod file;

use anyhow::{Context as _, Result};
use async_trait::async_trait;
use std::path::PathBuf;

use crate::capture::CapturedImage;
use crate::cli::SinkSpec;
use crate::config::{Config, FilenameContext};

#[async_trait]
pub trait OutputSink: Send + Sync {
    async fn write_png(&self, bytes: &[u8]) -> Result<Option<PathBuf>>;
}

pub struct Outputs(pub Vec<Box<dyn OutputSink>>);

impl Outputs {
    /// Build the sink list from CLI/config specs, expanding `{output}`/`{selection}` tokens in
    /// the configured filename template against `ctx`. Callers that want per-image filenames
    /// (e.g. `--per-output`) rebuild `Outputs` once per image with the appropriate context.
    pub fn from_specs(
        specs: &[SinkSpec],
        config: &Config,
        ctx: &FilenameContext<'_>,
    ) -> Result<Self> {
        Self::from_specs_with(specs, config, ctx, false)
    }

    /// Like [`Self::from_specs`] but uses [`Config::expand_filename_per_output`] which guarantees
    /// the basename varies with the output name (auto-inserting `-{output}` when the user's
    /// template lacks it).
    pub fn from_specs_per_output(
        specs: &[SinkSpec],
        config: &Config,
        ctx: &FilenameContext<'_>,
    ) -> Result<Self> {
        Self::from_specs_with(specs, config, ctx, true)
    }

    fn from_specs_with(
        specs: &[SinkSpec],
        config: &Config,
        ctx: &FilenameContext<'_>,
        per_output: bool,
    ) -> Result<Self> {
        let mut sinks: Vec<Box<dyn OutputSink>> = Vec::new();
        for spec in specs {
            match spec {
                SinkSpec::File(path) => {
                    let p = if let Some(path) = path {
                        path.clone()
                    } else {
                        let dir = config.save_directory();
                        std::fs::create_dir_all(&dir)
                            .with_context(|| format!("creating {}", dir.display()))?;
                        let name = if per_output {
                            config.expand_filename_per_output(ctx)
                        } else {
                            config.expand_filename(ctx)
                        };
                        dir.join(name)
                    };
                    sinks.push(Box::new(file::FileSink::new(p)));
                }
                SinkSpec::Clipboard => {
                    sinks.push(Box::new(clipboard::ClipboardSink::new()));
                }
            }
        }
        Ok(Self(sinks))
    }

    pub async fn write_png(&self, bytes: &[u8]) -> Result<Vec<PathBuf>> {
        let mut paths = Vec::new();
        for sink in &self.0 {
            if let Some(p) = sink.write_png(bytes).await? {
                paths.push(p);
            }
        }
        Ok(paths)
    }
}

/// Encode a `CapturedImage` to PNG bytes (BGRA → RGBA swizzle).
pub fn encode_png(img: &CapturedImage) -> Result<Vec<u8>> {
    let width = img.width as usize;
    let height = img.height as usize;
    let row_bytes = width * 4;
    let stride = img.stride as usize;

    // Tight RGBA buffer with the BGRA→RGBA swizzle baked in. Reading `u32`s and rotating bytes
    // is several times faster than a `chunks_exact(4)` + `extend_from_slice` per pixel.
    let mut rgba = vec![0u8; row_bytes * height];
    for y in 0..height {
        let src = &img.pixels[y * stride..y * stride + row_bytes];
        let dst = &mut rgba[y * row_bytes..(y + 1) * row_bytes];
        let (sp, src_u32, ss) = bytemuck::pod_align_to::<u8, u32>(src);
        let (dp, dst_u32, ds) = bytemuck::pod_align_to_mut::<u8, u32>(dst);
        if sp.is_empty()
            && ss.is_empty()
            && dp.is_empty()
            && ds.is_empty()
            && src_u32.len() == width
            && dst_u32.len() == width
        {
            // Little-endian: bytes [B, G, R, A] -> u32 0xAARRGGBB; we want bytes [R, G, B, A]
            // -> u32 0xAABBGGRR. Swap the bottom two bytes (R<->B) with a mask + shifts.
            for (s, d) in src_u32.iter().zip(dst_u32.iter_mut()) {
                let v = *s;
                *d = (v & 0xFF00_FF00) | ((v & 0x00FF_0000) >> 16) | ((v & 0x0000_00FF) << 16);
            }
        } else {
            // Fallback if alignment didn't line up (unusual).
            for (s, d) in src.chunks_exact(4).zip(dst.chunks_exact_mut(4)) {
                d[0] = s[2];
                d[1] = s[1];
                d[2] = s[0];
                d[3] = s[3];
            }
        }
    }

    let mut out = Vec::with_capacity(rgba.len() / 4);
    {
        use image::codecs::png::{CompressionType, FilterType, PngEncoder};
        use image::{ExtendedColorType, ImageEncoder};
        // `NoFilter` skips per-row filter heuristics that would otherwise scan every byte
        // five times; combined with `CompressionType::Fast` this brings a 19 MP screenshot
        // encode to sub-second territory in release builds, ~1-2 s in dev (provided the
        // image/png/miniz_oxide packages are built at -O3 via [profile.dev.package.*]).
        let encoder =
            PngEncoder::new_with_quality(&mut out, CompressionType::Fast, FilterType::NoFilter);
        encoder
            .write_image(&rgba, img.width, img.height, ExtendedColorType::Rgba8)
            .context("encoding PNG")?;
    }
    Ok(out)
}

#[cfg(test)]
mod tests {
    use super::*;
    use std::sync::Arc;

    fn pixel_image() -> CapturedImage {
        // 2x2 image, all white pixels (BGRA: 0xFF FF FF FF).
        let pixels: Arc<[u8]> = Arc::from(vec![0xFFu8; 16].into_boxed_slice());
        CapturedImage {
            width: 2,
            height: 2,
            stride: 8,
            pixels,
            source: None,
        }
    }

    #[test]
    fn encodes_png_with_correct_header() {
        let png = encode_png(&pixel_image()).unwrap();
        assert!(png.starts_with(&[0x89, b'P', b'N', b'G']));
    }
}
