//! Output sinks (file, clipboard) and PNG encoding.

pub mod clipboard;
pub mod file;

use anyhow::{Context as _, Result};
use async_trait::async_trait;
use std::path::PathBuf;

use crate::capture::CapturedImage;
use crate::cli::SinkSpec;
use crate::config::{FilenameContext, PngCompression};
use crate::context::Ctx;

#[async_trait]
pub trait OutputSink: Send + Sync {
    async fn write_png(&self, bytes: &[u8]) -> Result<Option<PathBuf>>;
}

pub struct Outputs(pub Vec<Box<dyn OutputSink>>);

impl Outputs {
    /// Build the sink list from CLI/config specs, expanding `{output}`/`{selection}` tokens in
    /// the configured filename template against `ctx`. Callers that want per-image filenames
    /// (e.g. `--per-output`) rebuild `Outputs` once per image with the appropriate context.
    ///
    /// `app_ctx` is the shared application context; the only field used here is
    /// [`Ctx::running_as_daemon`], which lets the clipboard sink choose between forking
    /// (one-shot CLI) and staying in-process (daemon).
    pub fn from_specs(
        specs: &[SinkSpec],
        app_ctx: &Ctx,
        ctx: &FilenameContext<'_>,
    ) -> Result<Self> {
        Self::from_specs_with(specs, app_ctx, ctx, false)
    }

    /// Like [`Self::from_specs`] but uses [`crate::config::Config::expand_filename_per_output`] which guarantees
    /// the basename varies with the output name (auto-inserting `-{output}` when the user's
    /// template lacks it).
    pub fn from_specs_per_output(
        specs: &[SinkSpec],
        app_ctx: &Ctx,
        ctx: &FilenameContext<'_>,
    ) -> Result<Self> {
        Self::from_specs_with(specs, app_ctx, ctx, true)
    }

    fn from_specs_with(
        specs: &[SinkSpec],
        app_ctx: &Ctx,
        ctx: &FilenameContext<'_>,
        per_output: bool,
    ) -> Result<Self> {
        let config = &app_ctx.config;
        let default_kind = config.clipboard.default_kind;
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
                SinkSpec::Clipboard(kind) => {
                    // Defence-in-depth: by the time we reach `from_specs` the kind has been
                    // resolved by the CLI / config layer. If a `None` still slips through
                    // (e.g. a future code path forgot to resolve it), fall back to the
                    // config default rather than panicking.
                    let kind = kind.unwrap_or(default_kind);
                    sinks.push(Box::new(clipboard::ClipboardSink::new(
                        kind,
                        app_ctx.running_as_daemon,
                    )));
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

/// Encode a `CapturedImage` to PNG bytes (BGRA → RGBA swizzle) using the supplied
/// compression preset. See [`PngCompression`] for the speed/size trade-offs.
pub fn encode_png(img: &CapturedImage, compression: PngCompression) -> Result<Vec<u8>> {
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
        // Trade-off picked by the config preset. `Fast` skips per-row filter heuristics and
        // uses the lowest deflate level — the original behaviour, fastest but largest. The
        // other presets let miniz_oxide pick filters/levels for substantially smaller files
        // at the cost of more CPU. Even `Best` typically encodes a 4K screenshot in a few
        // seconds in release builds (the image/png/miniz_oxide packages are built at -O3 via
        // `[profile.dev.package.*]` so dev builds stay tolerable too).
        let (ctype, ftype) = match compression {
            PngCompression::Fast => (CompressionType::Fast, FilterType::NoFilter),
            PngCompression::Balanced => (CompressionType::Default, FilterType::Adaptive),
            PngCompression::Best => (CompressionType::Best, FilterType::Adaptive),
        };
        let encoder = PngEncoder::new_with_quality(&mut out, ctype, ftype);
        encoder
            .write_image(&rgba, img.width, img.height, ExtendedColorType::Rgba8)
            .context("encoding PNG")?;
    }
    Ok(out)
}

#[cfg(test)]
mod tests {
    use super::*;
    use pretty_assertions::assert_eq;
    use rstest::rstest;
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

    /// Build a `width`x`height` image whose pixel `(x, y)` carries distinct per-channel
    /// values, laid out BGRA with `padding` extra bytes at the end of every row.
    fn distinct_image(width: u32, height: u32, padding: u32) -> CapturedImage {
        let stride = width * 4 + padding;
        let mut pixels = vec![0u8; (stride * height) as usize];
        for y in 0..height {
            for x in 0..width {
                let i = (y * stride + x * 4) as usize;
                pixels[i] = 10 + x as u8; // B
                pixels[i + 1] = 20 + x as u8; // G
                pixels[i + 2] = 30 + x as u8; // R
                pixels[i + 3] = 40 + y as u8; // A
            }
            // Poison the padding so any code that fails to skip it produces wrong pixels.
            for p in 0..padding {
                pixels[(y * stride + width * 4 + p) as usize] = 0xAB;
            }
        }
        CapturedImage {
            width,
            height,
            stride,
            pixels: Arc::from(pixels.into_boxed_slice()),
            source: None,
        }
    }

    #[test]
    fn encodes_png_with_correct_header() {
        let png = encode_png(&pixel_image(), PngCompression::Fast).unwrap();
        assert!(png.starts_with(&[0x89, b'P', b'N', b'G']));
    }

    /// The BGRA -> RGBA swizzle is invisible to an all-white fixture, so decode the PNG back
    /// and assert the channel order pixel by pixel. `padding` of 0 exercises the fast aligned
    /// `u32` path; a non-zero padding forces the scalar fallback *and* proves the row padding
    /// is dropped rather than encoded.
    #[rstest]
    #[case(PngCompression::Fast, 0)]
    #[case(PngCompression::Fast, 8)]
    #[case(PngCompression::Balanced, 0)]
    #[case(PngCompression::Balanced, 8)]
    #[case(PngCompression::Best, 0)]
    #[case(PngCompression::Best, 8)]
    fn encode_png_swizzles_bgra_to_rgba(#[case] compression: PngCompression, #[case] padding: u32) {
        let img = distinct_image(3, 2, padding);
        let png = encode_png(&img, compression).unwrap();
        let decoded = image::load_from_memory(&png).unwrap().to_rgba8();

        assert_eq!(decoded.dimensions(), (3, 2));
        for y in 0..2u32 {
            for x in 0..3u32 {
                assert_eq!(
                    decoded.get_pixel(x, y).0,
                    [30 + x as u8, 20 + x as u8, 10 + x as u8, 40 + y as u8],
                    "pixel ({x}, {y}) has the wrong channel order"
                );
            }
        }
    }

    #[test]
    fn balanced_is_smaller_than_fast() {
        // A 256x256 gradient compresses very differently across presets.
        let mut pixels = vec![0u8; 256 * 256 * 4];
        for y in 0..256u32 {
            for x in 0..256u32 {
                let i = ((y * 256 + x) * 4) as usize;
                pixels[i] = x as u8; // B
                pixels[i + 1] = y as u8; // G
                pixels[i + 2] = ((x + y) / 2) as u8; // R
                pixels[i + 3] = 0xFF;
            }
        }
        let img = CapturedImage {
            width: 256,
            height: 256,
            stride: 256 * 4,
            pixels: Arc::from(pixels.into_boxed_slice()),
            source: None,
        };
        let fast = encode_png(&img, PngCompression::Fast).unwrap();
        let balanced = encode_png(&img, PngCompression::Balanced).unwrap();
        assert!(
            balanced.len() < fast.len(),
            "balanced ({}) should be smaller than fast ({})",
            balanced.len(),
            fast.len()
        );
    }
}
