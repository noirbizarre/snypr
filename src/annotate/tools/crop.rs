use crate::annotate::{Tool, ToolKind};
use crate::capture::region::Rect;

/// `Crop` is applied at export time only — it doesn't produce a render node.
#[derive(Debug, Clone)]
pub struct CropTool {
    pub bounds: Rect,
}

impl CropTool {
    pub fn new(bounds: Rect) -> Self {
        Self { bounds }
    }
}

impl Tool for CropTool {
    fn kind(&self) -> ToolKind {
        ToolKind::Crop
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
    fn clone_box(&self) -> Box<dyn Tool> {
        Box::new(self.clone())
    }
    fn as_any(&self) -> &dyn std::any::Any {
        self
    }
    fn as_any_mut(&mut self) -> &mut dyn std::any::Any {
        self
    }
}

#[cfg(test)]
mod tests {
    use super::*;
    use pretty_assertions::assert_eq;
    use rstest::rstest;

    fn tool() -> CropTool {
        CropTool::new(Rect {
            x: 10,
            y: 20,
            w: 30,
            h: 40,
        })
    }

    #[test]
    fn new_keeps_the_bounds_it_was_given() {
        let t = tool();
        assert_eq!(t.bounds.x, 10);
        assert_eq!(t.bounds.w, 30);
    }

    #[test]
    fn reports_its_kind_and_bounds() {
        let t = tool();
        assert_eq!(t.kind(), ToolKind::Crop);
        assert_eq!(t.bounds(), t.bounds);
    }

    /// Box-shaped tools hit-test with *closed* bounds (`x <= right()`), unlike
    /// [`Rect::contains`] which is deliberately half-open. Pin that divergence.
    #[rstest]
    #[case::inside(20.0, 40.0, true)]
    #[case::top_left_corner(10.0, 20.0, true)]
    #[case::bottom_right_corner(40.0, 60.0, true)]
    #[case::just_left(9.9, 40.0, false)]
    #[case::just_above(20.0, 19.9, false)]
    #[case::just_right(40.1, 40.0, false)]
    #[case::just_below(20.0, 60.1, false)]
    fn hit_test_uses_closed_bounds(#[case] x: f64, #[case] y: f64, #[case] expected: bool) {
        assert_eq!(tool().hit_test(x, y), expected);
        assert!(
            !tool().bounds.contains(40, 60),
            "Rect::contains is half-open on the right/bottom edges"
        );
    }

    #[test]
    fn translate_shifts_bounds_and_preserves_size() {
        let mut t = tool();
        t.translate(5.4, -7.6);
        assert_eq!(
            t.bounds,
            Rect {
                x: 15,
                y: 12,
                w: 30,
                h: 40
            }
        );
    }

    #[test]
    fn clone_box_preserves_bounds() {
        let cloned = tool().clone_box();
        assert_eq!(cloned.kind(), ToolKind::Crop);
        assert_eq!(cloned.bounds(), tool().bounds);
        assert!(cloned.as_any().downcast_ref::<CropTool>().is_some());
    }
}
