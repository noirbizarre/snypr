use crate::annotate::{StrokeStyle, Tool, ToolKind, impl_tool_boilerplate};
use crate::capture::region::Rect;

#[derive(Debug, Clone)]
pub struct RectTool {
    pub bounds: Rect,
    pub stroke: [f32; 4],
    pub stroke_width: f32,
    pub stroke_style: StrokeStyle,
}

/// Default stroke color for a freshly created Rect tool: opaque red. Mirrored by
/// [`crate::config::AnnotateColors::default`], the documented source of truth for
/// user-configurable defaults.
pub const DEFAULT_STROKE: [f32; 4] = [1.0, 0.0, 0.0, 1.0];
/// Default stroke width, in logical pixels.
pub const DEFAULT_STROKE_WIDTH: f32 = 2.0;

impl RectTool {
    pub fn new(bounds: Rect) -> Self {
        Self {
            bounds,
            stroke: DEFAULT_STROKE,
            stroke_width: DEFAULT_STROKE_WIDTH,
            stroke_style: StrokeStyle::Solid,
        }
    }
}

impl Tool for RectTool {
    fn kind(&self) -> ToolKind {
        ToolKind::Rect
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

    fn tool() -> RectTool {
        RectTool::new(Rect {
            x: 10,
            y: 20,
            w: 30,
            h: 40,
        })
    }

    /// Closed on every edge, unlike `Rect::contains`: a click exactly on the right or
    /// bottom border is a click on the shape as far as the user is concerned.
    #[rstest]
    #[case::inside(15.0, 25.0, true)]
    #[case::right_edge(40.0, 25.0, true)]
    #[case::bottom_edge(15.0, 60.0, true)]
    #[case::just_outside(40.1, 25.0, false)]
    fn hit_test_uses_closed_bounds(#[case] x: f64, #[case] y: f64, #[case] expected: bool) {
        assert_eq!(tool().hit_test(x, y), expected);
    }

    #[test]
    fn translate_shifts_bounds() {
        let mut t = RectTool::new(Rect {
            x: 10,
            y: 20,
            w: 30,
            h: 40,
        });
        t.translate(5.0, -7.0);
        assert_eq!(
            t.bounds,
            Rect {
                x: 15,
                y: 13,
                w: 30,
                h: 40
            }
        );
    }
}
