//! Straight-line tool. Identical drag semantics to [`super::arrow::ArrowTool`] but
//! renders only the line segment — no arrowhead. Sibling of Arrow so the user can pick
//! one or the other from the toolbar depending on whether they want a pointer or a plain
//! ruler.

use crate::annotate::{StrokeStyle, Tool, ToolKind};
use crate::capture::region::Rect;

#[derive(Debug, Clone)]
pub struct LineTool {
    pub from: (f64, f64),
    pub to: (f64, f64),
    pub stroke: [f32; 4],
    pub stroke_width: f32,
    pub stroke_style: StrokeStyle,
}

impl LineTool {
    pub fn new(from: (f64, f64), to: (f64, f64)) -> Self {
        Self {
            from,
            to,
            stroke: [1.0, 0.0, 0.0, 1.0],
            stroke_width: 3.0,
            stroke_style: StrokeStyle::Solid,
        }
    }
}

impl Tool for LineTool {
    fn kind(&self) -> ToolKind {
        ToolKind::Line
    }
    fn bounds(&self) -> Rect {
        crate::annotate::render::drag_rect(self.from, self.to)
    }
    fn hit_test(&self, x: f64, y: f64) -> bool {
        let tol = (self.stroke_width as f64 / 2.0).max(crate::annotate::select::HANDLE_HALF_HIT);
        crate::annotate::render::dist_point_segment((x, y), self.from, self.to) <= tol
    }
    fn translate(&mut self, dx: f64, dy: f64) {
        self.from = (self.from.0 + dx, self.from.1 + dy);
        self.to = (self.to.0 + dx, self.to.1 + dy);
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

    #[test]
    fn defaults_match_arrow_minus_head() {
        let t = LineTool::new((0.0, 0.0), (10.0, 10.0));
        assert_eq!(t.kind(), ToolKind::Line);
        assert_eq!(t.stroke, [1.0, 0.0, 0.0, 1.0]);
        assert_eq!(t.stroke_width, 3.0);
        assert_eq!(t.stroke_style, StrokeStyle::Solid);
    }

    #[rstest]
    #[case((0.0, 0.0), (10.0, 20.0))]
    #[case((10.0, 20.0), (0.0, 0.0))]
    #[case((-5.0, 5.0), (5.0, -5.0))]
    fn bounds_are_axis_aligned(#[case] from: (f64, f64), #[case] to: (f64, f64)) {
        // Mirrors `drag_rect`'s normalisation: bounds should be order-insensitive.
        let a = LineTool::new(from, to).bounds();
        let b = LineTool::new(to, from).bounds();
        assert_eq!((a.x, a.y, a.w, a.h), (b.x, b.y, b.w, b.h));
    }
}
