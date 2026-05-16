//! Geometry, output, and selection types plus stitching helpers.

use std::sync::Arc;

use anyhow::{Result, bail};

use super::CapturedImage;

/// A pixel rectangle (logical coordinates unless otherwise noted).
#[derive(Clone, Copy, Debug, PartialEq, Eq)]
pub struct Rect {
    pub x: i32,
    pub y: i32,
    pub w: u32,
    pub h: u32,
}

impl Rect {
    pub fn right(&self) -> i32 {
        self.x + self.w as i32
    }
    pub fn bottom(&self) -> i32 {
        self.y + self.h as i32
    }

    /// Smallest rectangle containing both `self` and `other`.
    pub fn union(&self, other: &Rect) -> Rect {
        let x = self.x.min(other.x);
        let y = self.y.min(other.y);
        let r = self.right().max(other.right());
        let b = self.bottom().max(other.bottom());
        Rect {
            x,
            y,
            w: (r - x) as u32,
            h: (b - y) as u32,
        }
    }

    /// Intersection of two rectangles. `None` if they don't overlap.
    pub fn intersect(&self, other: &Rect) -> Option<Rect> {
        let x = self.x.max(other.x);
        let y = self.y.max(other.y);
        let r = self.right().min(other.right());
        let b = self.bottom().min(other.bottom());
        if r <= x || b <= y {
            None
        } else {
            Some(Rect {
                x,
                y,
                w: (r - x) as u32,
                h: (b - y) as u32,
            })
        }
    }
}

#[derive(Clone, Debug, PartialEq, Eq)]
pub struct Output {
    pub name: String,
    /// Logical position and size of this output in the compositor's coordinate space.
    pub logical: Rect,
    /// HiDPI scale factor (usually 1 or 2).
    pub scale: i32,
}

#[derive(Clone, Debug, PartialEq, Eq)]
pub enum Selection {
    /// Stitch every output into a single image (bounding box).
    Full,
    /// Return one image per output.
    PerOutput,
    /// The currently focused monitor (Hyprland).
    Focused,
    /// A specific output by name.
    Output(String),
    /// The currently active window (Hyprland).
    Window,
    /// An explicit rectangle in compositor logical coordinates.
    Region(Rect),
    /// Open the interactive selector UI.
    Interactive,
}

/// Stitch one or more captured images into the final buffer for the given selection.
///
/// For `PerOutput`, the original list is returned. For everything else, images are composited
/// into a single bounding-box buffer with transparent gutters.
///
/// On mixed-DPI setups (e.g. a HiDPI laptop next to a 1× external) each captured frame is
/// downscaled from its native device pixels to its output's logical size, so the resulting
/// composite is a single coherent logical-coordinate canvas matching what the user sees.
pub fn stitch(images: &[CapturedImage], selection: &Selection) -> Result<CapturedImage> {
    if images.is_empty() {
        bail!("no captured images to stitch");
    }
    if let Selection::PerOutput = selection {
        // The CLI handles per-output specially; if we get here, fall through to the bounding box.
    }
    if images.len() == 1 {
        return Ok(images[0].clone());
    }

    // Normalise every image to its output's logical size so the composite is in a single
    // coordinate space. Without this, a HiDPI laptop screen (logical 1920×1200, captured
    // 3840×2400) is placed and sized incorrectly relative to a 1× external monitor.
    let normalised: Vec<CapturedImage> = images.iter().map(to_logical_size).collect();

    let bbox = normalised
        .iter()
        .map(|i| {
            let (x, y, w, h) = i
                .source
                .as_ref()
                .map(|o| (o.logical.x, o.logical.y, o.logical.w, o.logical.h))
                .unwrap_or((0, 0, i.width, i.height));
            Rect { x, y, w, h }
        })
        .reduce(|a, b| a.union(&b))
        .expect("at least one image");

    let stride = (bbox.w * 4) as usize;
    let mut buf = vec![0u8; stride * bbox.h as usize];

    for img in &normalised {
        let (logical_x, logical_y) = img
            .source
            .as_ref()
            .map(|o| (o.logical.x, o.logical.y))
            .unwrap_or((0, 0));
        let off_x = (logical_x - bbox.x).max(0) as usize;
        let off_y = (logical_y - bbox.y).max(0) as usize;
        let src_stride = img.stride as usize;
        let row_bytes_full = (img.width as usize) * 4;

        // Clip to destination bounds so a misreported logical size can never panic.
        let copy_w_px = (img.width as usize).min(bbox.w as usize - off_x);
        let copy_h = (img.height as usize).min(bbox.h as usize - off_y);
        let copy_bytes = copy_w_px * 4;

        for y in 0..copy_h {
            let src_off = y * src_stride;
            let src = &img.pixels[src_off..src_off + row_bytes_full.min(src_stride)];
            let dst_start = (off_y + y) * stride + off_x * 4;
            buf[dst_start..dst_start + copy_bytes].copy_from_slice(&src[..copy_bytes]);
        }
    }

    Ok(CapturedImage {
        width: bbox.w,
        height: bbox.h,
        stride: stride as u32,
        pixels: buf.into(),
        source: None,
    })
}

/// Return a copy of `img` resampled to its output's logical (compositor) size.
///
/// If the image has no source metadata, or already matches its logical size, the original is
/// returned cheaply (the underlying pixel buffer is `Arc`-shared).
fn to_logical_size(img: &CapturedImage) -> CapturedImage {
    let Some(src) = img.source.as_ref() else {
        return img.clone();
    };
    let target_w = src.logical.w;
    let target_h = src.logical.h;
    if target_w == 0 || target_h == 0 || (target_w == img.width && target_h == img.height) {
        return img.clone();
    }

    // Compact rows (drop any stride padding) into a tight RGBA-shaped buffer. The byte order is
    // actually BGRA but `image`'s resampler treats each channel independently, so the result
    // remains BGRA — no swizzle needed here.
    let row_bytes = img.width as usize * 4;
    let stride = img.stride as usize;
    let mut tight = Vec::with_capacity(row_bytes * img.height as usize);
    for y in 0..img.height as usize {
        tight.extend_from_slice(&img.pixels[y * stride..y * stride + row_bytes]);
    }
    let source_buf = match image::RgbaImage::from_raw(img.width, img.height, tight) {
        Some(b) => b,
        None => return img.clone(),
    };
    let resized = image::imageops::resize(
        &source_buf,
        target_w,
        target_h,
        image::imageops::FilterType::Triangle,
    );
    let pixels: Arc<[u8]> = Arc::from(resized.into_raw().into_boxed_slice());
    CapturedImage {
        width: target_w,
        height: target_h,
        stride: target_w * 4,
        pixels,
        source: img.source.clone(),
    }
}

#[cfg(test)]
mod tests {
    use super::*;
    use pretty_assertions::assert_eq;

    #[test]
    fn union_combines_two_rectangles() {
        let a = Rect {
            x: 0,
            y: 0,
            w: 100,
            h: 100,
        };
        let b = Rect {
            x: 100,
            y: 0,
            w: 200,
            h: 100,
        };
        assert_eq!(
            a.union(&b),
            Rect {
                x: 0,
                y: 0,
                w: 300,
                h: 100
            }
        );
    }

    #[test]
    fn intersect_returns_none_when_disjoint() {
        let a = Rect {
            x: 0,
            y: 0,
            w: 10,
            h: 10,
        };
        let b = Rect {
            x: 20,
            y: 0,
            w: 10,
            h: 10,
        };
        assert_eq!(a.intersect(&b), None);
    }

    #[test]
    fn intersect_returns_overlap() {
        let a = Rect {
            x: 0,
            y: 0,
            w: 20,
            h: 20,
        };
        let b = Rect {
            x: 10,
            y: 10,
            w: 20,
            h: 20,
        };
        assert_eq!(
            a.intersect(&b),
            Some(Rect {
                x: 10,
                y: 10,
                w: 10,
                h: 10
            })
        );
    }

    fn solid_image(rect: Rect, byte: u8) -> CapturedImage {
        let stride = rect.w * 4;
        let pixels = vec![byte; (stride * rect.h) as usize];
        CapturedImage {
            width: rect.w,
            height: rect.h,
            stride,
            pixels: pixels.into(),
            source: Some(Output {
                name: format!("FAKE{}", byte),
                logical: rect,
                scale: 1,
            }),
        }
    }

    #[test]
    fn stitch_single_image_returns_as_is() {
        let img = solid_image(
            Rect {
                x: 0,
                y: 0,
                w: 4,
                h: 4,
            },
            0xAB,
        );
        let out = stitch(std::slice::from_ref(&img), &Selection::Full).unwrap();
        assert_eq!(out.width, 4);
        assert_eq!(out.height, 4);
    }

    #[test]
    fn stitch_side_by_side_produces_bounding_box() {
        let left = solid_image(
            Rect {
                x: 0,
                y: 0,
                w: 2,
                h: 2,
            },
            0x11,
        );
        let right = solid_image(
            Rect {
                x: 2,
                y: 0,
                w: 2,
                h: 2,
            },
            0x22,
        );
        let out = stitch(&[left, right], &Selection::Full).unwrap();
        assert_eq!(out.width, 4);
        assert_eq!(out.height, 2);
        // Left column = 0x11, right column = 0x22.
        assert_eq!(out.pixels[0], 0x11);
        assert_eq!(out.pixels[8], 0x22);
    }

    #[test]
    fn stitch_downscales_hidpi_capture_to_logical_size() {
        // Simulate a 2× HiDPI output: logical 2×2, captured at 4×4 device pixels (filled 0x11),
        // sat next to a 1× external (logical 2×2 at x=2, filled 0x22 at native size).
        let mut hidpi = solid_image(
            Rect {
                x: 0,
                y: 0,
                w: 4,
                h: 4,
            },
            0x11,
        );
        // Override the source so logical size disagrees with the captured device-pixel size.
        hidpi.source = Some(Output {
            name: "eDP-1".to_owned(),
            logical: Rect {
                x: 0,
                y: 0,
                w: 2,
                h: 2,
            },
            scale: 2,
        });
        let external = solid_image(
            Rect {
                x: 2,
                y: 0,
                w: 2,
                h: 2,
            },
            0x22,
        );
        let out = stitch(&[hidpi, external], &Selection::Full).unwrap();
        // Composite must be in logical space: 2 (downscaled hidpi) + 2 (external) = 4 wide, 2 tall.
        assert_eq!(out.width, 4);
        assert_eq!(out.height, 2);
        // Left half (hidpi) is 0x11, right half (external) is 0x22.
        assert_eq!(out.pixels[0], 0x11);
        assert_eq!(out.pixels[2 * 4], 0x22);
    }
}
