use crate::annotate::{Tool, ToolKind};
use crate::capture::region::Rect;

#[derive(Debug, Clone)]
pub struct RedactTool {
    pub bounds: Rect,
}

impl RedactTool {
    pub fn new(bounds: Rect) -> Self {
        Self { bounds }
    }
}

impl Tool for RedactTool {
    fn kind(&self) -> ToolKind {
        ToolKind::Redact
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

    fn tool() -> RedactTool {
        RedactTool {
            bounds: Rect {
                x: 10,
                y: 20,
                w: 30,
                h: 40,
            },
        }
    }

    #[test]
    fn reports_its_kind_and_bounds() {
        let t = tool();
        assert_eq!(t.kind(), ToolKind::Redact);
        assert_eq!(t.bounds(), t.bounds);
    }

    /// Closed bounds on the right/bottom edges, unlike the half-open [`Rect::contains`].
    #[rstest]
    #[case::inside(20.0, 40.0, true)]
    #[case::top_left_corner(10.0, 20.0, true)]
    #[case::bottom_right_corner(40.0, 60.0, true)]
    #[case::just_left(9.9, 40.0, false)]
    #[case::just_below(20.0, 60.1, false)]
    fn hit_test_uses_closed_bounds(#[case] x: f64, #[case] y: f64, #[case] expected: bool) {
        assert_eq!(tool().hit_test(x, y), expected);
    }

    #[test]
    fn translate_shifts_bounds_and_preserves_size() {
        let mut t = tool();
        t.translate(-10.0, 5.0);
        assert_eq!(
            t.bounds,
            Rect {
                x: 0,
                y: 25,
                w: 30,
                h: 40
            }
        );
    }

    #[test]
    fn clone_box_preserves_bounds() {
        let mut t = tool();
        assert!(t.as_any_mut().downcast_mut::<RedactTool>().is_some());
        let cloned = t.clone_box();
        assert_eq!(cloned.bounds(), tool().bounds);
    }
}
