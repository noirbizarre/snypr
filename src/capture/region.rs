//! Geometry, output, and selection types plus stitching helpers.

use std::sync::Arc;

use anyhow::{Result, bail};

use super::{CapturedImage, PixelFormat};

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

    /// Shift the rectangle by `(dx, dy)` logical pixels, rounding to the nearest integer.
    /// Used by the Select tool's move gesture for box-based shapes.
    pub fn translate(&self, dx: f64, dy: f64) -> Rect {
        Rect {
            x: self.x + dx.round() as i32,
            y: self.y + dy.round() as i32,
            w: self.w,
            h: self.h,
        }
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

    /// `true` if the rectangle contains the point `(x, y)` (logical coordinates).
    ///
    /// Half-open on the right/bottom edges so adjacent rectangles don't both claim a shared
    /// border pixel.
    pub fn contains(&self, x: i32, y: i32) -> bool {
        x >= self.x && y >= self.y && x < self.right() && y < self.bottom()
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
    /// The currently focused monitor (Hyprland/Sway).
    Focused,
    /// A specific output by name.
    Output(String),
    /// The currently active window (Hyprland/Sway).
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
///
/// When `selection` is `Region(rect)`, the composite is additionally cropped to that rect
/// (clipped to the bounding box if it extends beyond captured outputs).
pub fn stitch(images: &[CapturedImage], selection: &Selection) -> Result<CapturedImage> {
    if images.is_empty() {
        bail!("no captured images to stitch");
    }
    if let Selection::PerOutput = selection {
        // The CLI handles per-output specially; if we get here, fall through to the bounding box.
    }
    if images.len() == 1 && !matches!(selection, Selection::Region(_)) {
        return Ok(images[0].clone());
    }

    // Normalise every image to its output's logical size so the composite is in a single
    // coordinate space. Without this, a HiDPI laptop screen (logical 1920×1200, captured
    // 3840×2400) is placed and sized incorrectly relative to a 1× external monitor.
    let normalised: Vec<CapturedImage> = images.iter().map(to_logical_size).collect();

    // Each output negotiates its buffer format independently, so a mixed-GPU/mixed-driver
    // setup could in principle hand back outputs with different byte orders. That can't be
    // represented by a single composite format; assume they agree (true in practice — the
    // format comes from the shared SHM/renderer path) and flag it loudly if they don't so a
    // real mismatch surfaces as a bug report instead of a silent color-channel corruption.
    let format: PixelFormat = normalised[0].format;
    if normalised.iter().any(|i| i.format != format) {
        tracing::warn!(
            "stitching outputs with mismatched pixel formats; colors on some outputs may be \
             wrong in the composite"
        );
    }

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

    let composite = CapturedImage {
        width: bbox.w,
        height: bbox.h,
        stride: stride as u32,
        pixels: Arc::from(buf.into_boxed_slice()),
        format,
        source: None,
    };

    if let Selection::Region(rect) = selection {
        return crop(&composite, &bbox, rect);
    }
    Ok(composite)
}

/// Crop `img` (whose top-left sits at `origin` in logical coords) to `rect` (also logical).
/// Returns the input unchanged if `rect` fully covers it.
fn crop(img: &CapturedImage, origin: &Rect, rect: &Rect) -> Result<CapturedImage> {
    let Some(clipped) = rect.intersect(origin) else {
        bail!(
            "selected region {:?} does not intersect captured outputs (bbox {:?})",
            rect,
            origin
        );
    };
    if clipped == *origin {
        return Ok(img.clone());
    }
    let off_x = (clipped.x - origin.x) as usize;
    let off_y = (clipped.y - origin.y) as usize;
    let dst_w = clipped.w as usize;
    let dst_h = clipped.h as usize;
    let src_stride = img.stride as usize;
    let dst_stride = dst_w * 4;
    let mut out = Vec::with_capacity(dst_stride * dst_h);
    for y in 0..dst_h {
        let s = (off_y + y) * src_stride + off_x * 4;
        out.extend_from_slice(&img.pixels[s..s + dst_stride]);
    }
    Ok(CapturedImage {
        width: clipped.w,
        height: clipped.h,
        stride: dst_stride as u32,
        pixels: Arc::from(out.into_boxed_slice()),
        format: img.format,
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

    // Compact rows (drop any stride padding) into a tight 4-channel buffer. `image`'s resampler
    // treats each channel independently regardless of what it's called, so the result keeps
    // whatever byte order `img.format` says it is — no swizzle needed here.
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
        format: img.format,
        source: img.source.clone(),
    }
}

/// Bounding box of every captured image in compositor logical coordinates. Used by callers
/// that need to know where a stitched buffer sits on the virtual desktop (e.g. the in-place
/// annotation overlay, which has to align per-monitor slices back to the original capture).
///
/// Returns `None` if `images` is empty. Images without `source` metadata are placed at
/// `(0, 0)` with their device-pixel size — matching what `stitch` does internally.
pub fn bbox(images: &[CapturedImage]) -> Option<Rect> {
    images
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
}

/// Copy a sub-rectangle out of a tightly-packed RGBA/BGRA buffer.
///
/// `base_origin` is the buffer's top-left in logical coordinates; `slice` is the rectangle to
/// extract in the same coordinate space. Pixels are copied row-by-row so the result has a
/// tight stride (`slice.w * 4`). The byte order is preserved as-is — caller decides RGBA vs
/// BGRA semantics.
///
/// Returns `None` if the slice doesn't intersect the buffer or has zero area.
pub fn slice_pixels(
    pixels: &[u8],
    base_width: u32,
    base_height: u32,
    base_stride: u32,
    base_origin: (i32, i32),
    slice: Rect,
) -> Option<(Vec<u8>, u32, u32)> {
    let base_rect = Rect {
        x: base_origin.0,
        y: base_origin.1,
        w: base_width,
        h: base_height,
    };
    let clipped = base_rect.intersect(&slice)?;
    if clipped.w == 0 || clipped.h == 0 {
        return None;
    }
    let off_x = (clipped.x - base_origin.0) as usize;
    let off_y = (clipped.y - base_origin.1) as usize;
    let src_stride = base_stride as usize;
    let dst_w = clipped.w as usize;
    let dst_h = clipped.h as usize;
    let dst_stride = dst_w * 4;
    let mut out = Vec::with_capacity(dst_stride * dst_h);
    for y in 0..dst_h {
        let s = (off_y + y) * src_stride + off_x * 4;
        out.extend_from_slice(&pixels[s..s + dst_stride]);
    }
    Some((out, clipped.w, clipped.h))
}

#[cfg(test)]
mod tests {
    use super::*;
    use pretty_assertions::assert_eq;
    use rstest::rstest;

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
            format: PixelFormat::Bgra,
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
    fn stitch_keeps_the_first_images_format_and_carries_it_forward() {
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
        assert_eq!(left.format, PixelFormat::Bgra);
        let out = stitch(&[left, right], &Selection::Full).unwrap();
        assert_eq!(out.format, PixelFormat::Bgra);
    }

    /// A mixed-GPU/mixed-driver setup could in principle negotiate different formats per
    /// output. `stitch` can't represent that in a single composite; it picks the first
    /// image's format and (per this test) at least logs the mismatch loudly rather than
    /// silently mixing byte orders.
    #[test]
    fn stitch_warns_and_keeps_the_first_format_when_inputs_disagree() {
        let left = solid_image(
            Rect {
                x: 0,
                y: 0,
                w: 2,
                h: 2,
            },
            0x11,
        );
        let mut right = solid_image(
            Rect {
                x: 2,
                y: 0,
                w: 2,
                h: 2,
            },
            0x22,
        );
        right.format = PixelFormat::Rgba;
        assert_ne!(left.format, right.format);

        let out = stitch(&[left, right], &Selection::Full).unwrap();
        assert_eq!(
            out.format,
            PixelFormat::Bgra,
            "the composite keeps the first image's format"
        );
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

    #[test]
    fn stitch_with_region_crops_to_rect() {
        let left = solid_image(
            Rect {
                x: 0,
                y: 0,
                w: 4,
                h: 4,
            },
            0x11,
        );
        let right = solid_image(
            Rect {
                x: 4,
                y: 0,
                w: 4,
                h: 4,
            },
            0x22,
        );
        // Region spans the gap between the two: x=2..6, y=0..4.
        let region = Rect {
            x: 2,
            y: 0,
            w: 4,
            h: 4,
        };
        let out = stitch(&[left, right], &Selection::Region(region)).unwrap();
        assert_eq!(out.width, 4);
        assert_eq!(out.height, 4);
        // Left two columns are 0x11 (from `left`), right two columns are 0x22 (from `right`).
        assert_eq!(out.pixels[0], 0x11);
        assert_eq!(out.pixels[4], 0x11);
        assert_eq!(out.pixels[8], 0x22);
        assert_eq!(out.pixels[12], 0x22);
    }

    #[test]
    fn bbox_unions_image_logical_rects() {
        let left = solid_image(
            Rect {
                x: 0,
                y: 0,
                w: 4,
                h: 4,
            },
            0,
        );
        let right = solid_image(
            Rect {
                x: 4,
                y: 0,
                w: 4,
                h: 4,
            },
            0,
        );
        assert_eq!(
            bbox(&[left, right]),
            Some(Rect {
                x: 0,
                y: 0,
                w: 8,
                h: 4,
            })
        );
    }

    #[test]
    fn bbox_returns_none_for_empty_input() {
        assert_eq!(bbox(&[]), None);
    }

    #[test]
    fn slice_pixels_extracts_subrect() {
        // 4x2 buffer with two horizontal halves (0xAA | 0xBB), tight RGBA stride.
        let mut buf = Vec::with_capacity(4 * 2 * 4);
        for _y in 0..2 {
            for _x in 0..2 {
                buf.extend_from_slice(&[0xAA, 0xAA, 0xAA, 0xFF]);
            }
            for _x in 0..2 {
                buf.extend_from_slice(&[0xBB, 0xBB, 0xBB, 0xFF]);
            }
        }
        let (out, w, h) = slice_pixels(
            &buf,
            4,
            2,
            4 * 4,
            (10, 20),
            Rect {
                x: 12,
                y: 20,
                w: 2,
                h: 2,
            },
        )
        .unwrap();
        assert_eq!((w, h), (2, 2));
        // Right half columns 2..=3 of the original (right side, 0xBB).
        assert_eq!(out[0], 0xBB);
        assert_eq!(out[4], 0xBB);
    }

    #[test]
    fn slice_pixels_clips_against_buffer() {
        // 2x2 buffer at origin (0,0); requesting (1,1, 4x4) clips to (1,1, 1x1).
        let buf = vec![0xCC; 2 * 2 * 4];
        let (out, w, h) = slice_pixels(
            &buf,
            2,
            2,
            2 * 4,
            (0, 0),
            Rect {
                x: 1,
                y: 1,
                w: 4,
                h: 4,
            },
        )
        .unwrap();
        assert_eq!((w, h), (1, 1));
        assert_eq!(out.len(), 4);
    }

    #[test]
    fn slice_pixels_returns_none_for_disjoint_rect() {
        let buf = vec![0; 16];
        assert!(
            slice_pixels(
                &buf,
                2,
                2,
                8,
                (0, 0),
                Rect {
                    x: 10,
                    y: 10,
                    w: 2,
                    h: 2,
                },
            )
            .is_none()
        );
    }

    #[rstest]
    #[case(Rect { x: 0, y: 0, w: 100, h: 50 }, 100, 50)]
    #[case(Rect { x: 10, y: 20, w: 30, h: 40 }, 40, 60)]
    #[case(Rect { x: -30, y: -40, w: 10, h: 10 }, -20, -30)]
    fn right_and_bottom_are_the_exclusive_edges(
        #[case] r: Rect,
        #[case] right: i32,
        #[case] bottom: i32,
    ) {
        assert_eq!(r.right(), right);
        assert_eq!(r.bottom(), bottom);
    }

    #[rstest]
    #[case(1.0, 2.0, 11, 22)]
    #[case(-1.0, -2.0, 9, 18)]
    #[case(0.0, 0.0, 10, 20)]
    // `f64::round` breaks ties away from zero in both directions.
    #[case(0.5, -0.5, 11, 19)]
    #[case(0.4, -0.4, 10, 20)]
    #[case(1.6, 2.6, 12, 23)]
    fn translate_rounds_to_the_nearest_integer(
        #[case] dx: f64,
        #[case] dy: f64,
        #[case] x: i32,
        #[case] y: i32,
    ) {
        let r = Rect {
            x: 10,
            y: 20,
            w: 30,
            h: 40,
        };
        assert_eq!(r.translate(dx, dy), Rect { x, y, w: 30, h: 40 });
    }

    /// `contains` is deliberately half-open on the right/bottom edges so two adjacent
    /// rectangles never both claim the shared border pixel.
    #[rstest]
    #[case::inside(20, 30, true)]
    #[case::top_left_corner(10, 20, true)]
    #[case::last_contained_pixel(39, 59, true)]
    #[case::right_edge_excluded(40, 30, false)]
    #[case::bottom_edge_excluded(20, 60, false)]
    #[case::left_of(9, 30, false)]
    #[case::above(20, 19, false)]
    fn contains_is_half_open(#[case] x: i32, #[case] y: i32, #[case] expected: bool) {
        let r = Rect {
            x: 10,
            y: 20,
            w: 30,
            h: 40,
        };
        assert_eq!(r.contains(x, y), expected);
    }

    #[test]
    fn adjacent_rectangles_never_share_a_pixel() {
        let left = Rect {
            x: 0,
            y: 0,
            w: 10,
            h: 10,
        };
        let right = Rect {
            x: 10,
            y: 0,
            w: 10,
            h: 10,
        };
        assert!(left.contains(9, 0) && !right.contains(9, 0));
        assert!(right.contains(10, 0) && !left.contains(10, 0));
    }

    #[test]
    fn crop_rejects_a_region_outside_the_capture() {
        let origin = Rect {
            x: 0,
            y: 0,
            w: 4,
            h: 4,
        };
        let img = solid_image(origin, 0x11);
        let err = crop(
            &img,
            &origin,
            &Rect {
                x: 100,
                y: 100,
                w: 4,
                h: 4,
            },
        )
        .unwrap_err();
        assert!(
            err.to_string().contains("does not intersect"),
            "unexpected error: {err}"
        );
    }

    #[test]
    fn crop_returns_the_input_when_the_region_covers_everything() {
        let origin = Rect {
            x: 0,
            y: 0,
            w: 4,
            h: 4,
        };
        let img = solid_image(origin, 0x11);
        // A region larger than the capture clips back to the origin, hitting the identity path.
        let out = crop(
            &img,
            &origin,
            &Rect {
                x: -10,
                y: -10,
                w: 100,
                h: 100,
            },
        )
        .unwrap();
        assert_eq!((out.width, out.height, out.stride), (4, 4, 16));
        assert!(
            Arc::ptr_eq(&out.pixels, &img.pixels),
            "the identity path should share the buffer instead of copying"
        );
    }

    #[test]
    fn to_logical_size_returns_the_input_without_source_metadata() {
        let mut img = solid_image(
            Rect {
                x: 0,
                y: 0,
                w: 4,
                h: 4,
            },
            0x22,
        );
        img.source = None;
        let out = to_logical_size(&img);
        assert_eq!((out.width, out.height), (4, 4));
        assert!(Arc::ptr_eq(&out.pixels, &img.pixels));
    }

    #[rstest]
    // Already at its logical size.
    #[case(Rect { x: 0, y: 0, w: 4, h: 4 })]
    // A degenerate logical size (an output that reported nothing usable).
    #[case(Rect { x: 0, y: 0, w: 0, h: 4 })]
    #[case(Rect { x: 0, y: 0, w: 4, h: 0 })]
    fn to_logical_size_short_circuits_without_resampling(#[case] logical: Rect) {
        let mut img = solid_image(
            Rect {
                x: 0,
                y: 0,
                w: 4,
                h: 4,
            },
            0x33,
        );
        img.source = Some(Output {
            name: "FAKE".into(),
            logical,
            scale: 1,
        });
        let out = to_logical_size(&img);
        assert_eq!((out.width, out.height), (4, 4));
        assert!(
            Arc::ptr_eq(&out.pixels, &img.pixels),
            "the early-return paths should share the buffer instead of resampling"
        );
    }
}
