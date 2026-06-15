use crate::annotate::{StrokeStyle, Tool, ToolKind};
use crate::capture::region::Rect;

#[derive(Debug, Clone)]
pub struct ArrowTool {
    pub from: (f64, f64),
    pub to: (f64, f64),
    pub stroke: [f32; 4],
    pub stroke_width: f32,
    pub stroke_style: StrokeStyle,
}

impl ArrowTool {
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

impl Tool for ArrowTool {
    fn kind(&self) -> ToolKind {
        ToolKind::Arrow
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

    #[test]
    fn hit_test_near_shaft_hits() {
        let t = ArrowTool::new((0.0, 0.0), (100.0, 0.0));
        // On the shaft and within stroke/slop tolerance.
        assert!(t.hit_test(50.0, 0.0));
        assert!(t.hit_test(50.0, 4.0));
        // Far from the segment misses.
        assert!(!t.hit_test(50.0, 40.0));
    }

    #[test]
    fn hit_test_near_endpoint_hits() {
        let t = ArrowTool::new((0.0, 0.0), (100.0, 0.0));
        assert!(t.hit_test(100.0, 2.0));
    }

    #[test]
    fn translate_moves_both_endpoints() {
        let mut t = ArrowTool::new((0.0, 0.0), (10.0, 10.0));
        t.translate(5.0, -5.0);
        assert_eq!(t.from, (5.0, -5.0));
        assert_eq!(t.to, (15.0, 5.0));
    }
}
