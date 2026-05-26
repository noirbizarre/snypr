//! Pure-math rendering helpers shared by the GTK canvas.

use crate::capture::region::Rect;

/// Compute the two arrowhead points for an arrow segment.
///
/// Returns `(left, right)` — coordinates of the wing tips.
pub fn arrowhead(from: (f64, f64), to: (f64, f64), size: f64) -> ((f64, f64), (f64, f64)) {
    let dx = to.0 - from.0;
    let dy = to.1 - from.1;
    let len = (dx * dx + dy * dy).sqrt().max(1.0);
    let ux = dx / len;
    let uy = dy / len;
    // Perpendicular unit vector.
    let px = -uy;
    let py = ux;
    let base_x = to.0 - ux * size;
    let base_y = to.1 - uy * size;
    let half = size * 0.5;
    (
        (base_x + px * half, base_y + py * half),
        (base_x - px * half, base_y - py * half),
    )
}

/// Normalise a drag (origin, end) into a non-negative-extent rectangle.
pub fn drag_rect(a: (f64, f64), b: (f64, f64)) -> Rect {
    let x = a.0.min(b.0).floor() as i32;
    let y = a.1.min(b.1).floor() as i32;
    let w = (a.0 - b.0).abs().ceil() as u32;
    let h = (a.1 - b.1).abs().ceil() as u32;
    Rect { x, y, w, h }
}

/// Like [`drag_rect`], but forces a square bounding box anchored at `a`.
///
/// The side length is `max(|dx|, |dy|)` and the sign of each axis is preserved
/// so the rectangle stays under the quadrant the cursor is dragging into.
/// Used by the Rect/Ellipse annotate tools when SHIFT is held to constrain
/// to a square / circle.
pub fn drag_square(a: (f64, f64), b: (f64, f64)) -> Rect {
    let dx = b.0 - a.0;
    let dy = b.1 - a.1;
    let side = dx.abs().max(dy.abs());
    let bx = a.0 + side.copysign(dx);
    let by = a.1 + side.copysign(dy);
    drag_rect(a, (bx, by))
}

#[cfg(test)]
mod tests {
    use super::*;
    use pretty_assertions::assert_eq;
    use rstest::rstest;

    #[test]
    fn horizontal_arrowhead_is_symmetric() {
        let (l, r) = arrowhead((0.0, 0.0), (10.0, 0.0), 4.0);
        // Wings sit at x=6, y=±2.
        assert!((l.0 - 6.0).abs() < 1e-9);
        assert!((r.0 - 6.0).abs() < 1e-9);
        assert!((l.1 + r.1).abs() < 1e-9);
    }

    #[test]
    fn drag_rect_normalises_negative_extent() {
        let rect = drag_rect((10.0, 20.0), (4.0, 8.0));
        assert_eq!(
            rect,
            Rect {
                x: 4,
                y: 8,
                w: 6,
                h: 12
            }
        );
    }

    #[rstest]
    // South-East quadrant: side = max(20, 10) = 20, anchored at (0, 0).
    #[case((0.0, 0.0), (20.0, 10.0), Rect { x: 0, y: 0, w: 20, h: 20 })]
    // South-East with |dy| > |dx|: side picks the larger axis.
    #[case((0.0, 0.0), (5.0, 30.0), Rect { x: 0, y: 0, w: 30, h: 30 })]
    // North-West quadrant: square grows up-and-left of the anchor.
    #[case((50.0, 50.0), (40.0, 20.0), Rect { x: 20, y: 20, w: 30, h: 30 })]
    // North-East quadrant: positive dx, negative dy.
    #[case((10.0, 100.0), (60.0, 80.0), Rect { x: 10, y: 50, w: 50, h: 50 })]
    // South-West quadrant: negative dx, positive dy.
    #[case((100.0, 10.0), (80.0, 60.0), Rect { x: 50, y: 10, w: 50, h: 50 })]
    // Degenerate: zero delta collapses to a zero-extent rect at the anchor.
    #[case((7.0, 9.0), (7.0, 9.0), Rect { x: 7, y: 9, w: 0, h: 0 })]
    fn drag_square_anchors_at_origin(
        #[case] a: (f64, f64),
        #[case] b: (f64, f64),
        #[case] expected: Rect,
    ) {
        assert_eq!(drag_square(a, b), expected);
    }
}
