use super::rect::{DEFAULT_STROKE, DEFAULT_STROKE_WIDTH};
use crate::annotate::{StrokeStyle, Tool, ToolKind, impl_tool_boilerplate};
use crate::capture::region::Rect;

/// Outline ellipse inscribed in `bounds`. Reuses [`super::rect::RectTool`]'s defaults — a
/// 2px red stroke — so the two shape tools behave symmetrically. Hit-testing uses the
/// bounding rectangle (matches `RectTool`) which is plenty for the current selection /
/// hover model; switching to a true point-in-ellipse test is a one-liner if we ever need
/// finer-grained picking.
#[derive(Debug, Clone)]
pub struct EllipseTool {
    pub bounds: Rect,
    pub stroke: [f32; 4],
    pub stroke_width: f32,
    pub stroke_style: StrokeStyle,
}

impl EllipseTool {
    pub fn new(bounds: Rect) -> Self {
        Self {
            bounds,
            stroke: DEFAULT_STROKE,
            stroke_width: DEFAULT_STROKE_WIDTH,
            stroke_style: StrokeStyle::Solid,
        }
    }
}

impl Tool for EllipseTool {
    fn kind(&self) -> ToolKind {
        ToolKind::Ellipse
    }
    fn bounds(&self) -> Rect {
        self.bounds
    }
    fn hit_test(&self, x: f64, y: f64) -> bool {
        super::rect_hit_test(self.bounds, x, y)
    }
    fn translate(&mut self, dx: f64, dy: f64) {
        self.bounds = self.bounds.translate(dx, dy);
    }
    impl_tool_boilerplate!();
}

#[cfg(test)]
mod tests {
    use super::*;
    use pretty_assertions::assert_eq;
    use rstest::rstest;

    fn bounds() -> Rect {
        Rect {
            x: 10,
            y: 20,
            w: 30,
            h: 40,
        }
    }

    #[test]
    fn new_matches_the_rect_tool_defaults() {
        let t = EllipseTool::new(bounds());
        assert_eq!(t.kind(), ToolKind::Ellipse);
        assert_eq!(t.bounds(), bounds());
        assert_eq!(t.stroke, [1.0, 0.0, 0.0, 1.0]);
        assert_eq!(t.stroke_width, 2.0);
        assert_eq!(t.stroke_style, StrokeStyle::Solid);
    }

    /// Hit-testing uses the bounding rectangle with *closed* edges (documented on the type),
    /// not a true point-in-ellipse test — so the box corners count as hits.
    #[rstest]
    #[case::centre(25.0, 40.0, true)]
    #[case::top_left_corner(10.0, 20.0, true)]
    #[case::bottom_right_corner(40.0, 60.0, true)]
    #[case::just_left(9.9, 40.0, false)]
    #[case::just_below(25.0, 60.1, false)]
    fn hit_test_uses_the_closed_bounding_box(
        #[case] x: f64,
        #[case] y: f64,
        #[case] expected: bool,
    ) {
        assert_eq!(EllipseTool::new(bounds()).hit_test(x, y), expected);
    }

    #[test]
    fn translate_shifts_bounds_and_preserves_size() {
        let mut t = EllipseTool::new(bounds());
        // `f64::round` breaks ties away from zero, so -2.5 becomes -3, not -2.
        t.translate(5.5, -2.5);
        assert_eq!(
            t.bounds,
            Rect {
                x: 16,
                y: 17,
                w: 30,
                h: 40
            }
        );
    }

    #[test]
    fn clone_box_preserves_the_stroke() {
        let mut t = EllipseTool::new(bounds());
        t.stroke_width = 7.0;
        assert!(t.as_any_mut().downcast_mut::<EllipseTool>().is_some());
        let cloned = t.clone_box();
        assert_eq!(
            cloned
                .as_any()
                .downcast_ref::<EllipseTool>()
                .unwrap()
                .stroke_width,
            7.0
        );
    }
}
