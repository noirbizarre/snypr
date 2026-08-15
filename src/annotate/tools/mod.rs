//! Annotation tools — each is a `Tool` impl.

use crate::capture::region::Rect;

pub mod arrow;
pub mod blur;
pub mod crop;
pub mod ellipse;
pub mod freehand;
pub mod highlight;
pub mod line;
pub mod number;
pub mod rect;
pub mod redact;
pub mod text;

/// Point-in-rectangle test used by every box-shaped tool's `hit_test`.
///
/// Deliberately **closed** on all four edges, unlike [`Rect::contains`], which is half-open.
/// A click exactly on the right or bottom edge of a shape is a click on the shape as far as
/// the user is concerned; the half-open form would make the edge unselectable.
pub fn rect_hit_test(r: Rect, x: f64, y: f64) -> bool {
    x >= r.x as f64 && x <= r.right() as f64 && y >= r.y as f64 && y <= r.bottom() as f64
}

#[cfg(test)]
mod tests {
    use super::*;
    use pretty_assertions::assert_eq;
    use rstest::rstest;

    #[rstest]
    #[case::inside(15.0, 25.0, true)]
    #[case::top_left_corner(10.0, 20.0, true)]
    #[case::bottom_right_corner(30.0, 50.0, true)]
    #[case::right_edge(30.0, 25.0, true)]
    #[case::bottom_edge(15.0, 50.0, true)]
    #[case::just_outside_right(30.1, 25.0, false)]
    #[case::just_outside_bottom(15.0, 50.1, false)]
    #[case::outside(0.0, 0.0, false)]
    fn rect_hit_test_is_closed_on_every_edge(
        #[case] x: f64,
        #[case] y: f64,
        #[case] expected: bool,
    ) {
        let r = Rect {
            x: 10,
            y: 20,
            w: 20,
            h: 30,
        };
        assert_eq!(rect_hit_test(r, x, y), expected);
    }
}
