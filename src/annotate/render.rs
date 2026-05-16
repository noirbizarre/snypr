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

#[cfg(test)]
mod tests {
    use super::*;
    use pretty_assertions::assert_eq;

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
}
