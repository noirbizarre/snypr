use crate::annotate::{Tool, ToolKind};
use crate::capture::region::Rect;

#[derive(Debug, Clone)]
pub struct HighlightTool {
    pub bounds: Rect,
    pub color: [f32; 4],
}

impl Tool for HighlightTool {
    fn kind(&self) -> ToolKind {
        ToolKind::Highlight
    }
    fn bounds(&self) -> Rect {
        self.bounds
    }
    fn hit_test(&self, x: f64, y: f64) -> bool {
        let r = self.bounds;
        x >= r.x as f64 && x <= r.right() as f64 && y >= r.y as f64 && y <= r.bottom() as f64
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

    fn tool() -> HighlightTool {
        HighlightTool {
            bounds: Rect {
                x: 10,
                y: 20,
                w: 30,
                h: 40,
            },
            color: [1.0, 1.0, 0.0, 0.4],
        }
    }

    #[test]
    fn reports_its_kind_and_bounds() {
        let t = tool();
        assert_eq!(t.kind(), ToolKind::Highlight);
        assert_eq!(t.bounds(), t.bounds);
    }

    /// Closed bounds on the right/bottom edges, unlike the half-open [`Rect::contains`].
    #[rstest]
    #[case::inside(20.0, 40.0, true)]
    #[case::top_left_corner(10.0, 20.0, true)]
    #[case::bottom_right_corner(40.0, 60.0, true)]
    #[case::just_right(40.1, 40.0, false)]
    #[case::just_above(20.0, 19.9, false)]
    fn hit_test_uses_closed_bounds(#[case] x: f64, #[case] y: f64, #[case] expected: bool) {
        assert_eq!(tool().hit_test(x, y), expected);
    }

    #[test]
    fn translate_shifts_bounds_and_preserves_size() {
        let mut t = tool();
        t.translate(5.0, 5.0);
        assert_eq!(
            t.bounds,
            Rect {
                x: 15,
                y: 25,
                w: 30,
                h: 40
            }
        );
    }

    #[test]
    fn clone_box_preserves_colour() {
        let cloned = tool().clone_box();
        let same = cloned.as_any().downcast_ref::<HighlightTool>().unwrap();
        assert_eq!(same.color, [1.0, 1.0, 0.0, 0.4]);
    }
}
