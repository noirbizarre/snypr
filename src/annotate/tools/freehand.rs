use crate::annotate::{StrokeStyle, Tool, ToolKind};
use crate::capture::region::Rect;

#[derive(Debug, Clone)]
pub struct FreehandTool {
    pub points: Vec<(f64, f64)>,
    pub stroke: [f32; 4],
    pub stroke_width: f32,
    pub stroke_style: StrokeStyle,
}

/// Default stroke width, matching [`arrow::DEFAULT_STROKE_WIDTH`](super::arrow::DEFAULT_STROKE_WIDTH)
/// and [`line::DEFAULT_STROKE_WIDTH`](super::line::DEFAULT_STROKE_WIDTH): freehand strokes read
/// as pen marks rather than as the thinner geometric outlines used by Rect / Ellipse.
pub const DEFAULT_STROKE_WIDTH: f32 = 3.0;

impl FreehandTool {
    pub fn new(points: Vec<(f64, f64)>, stroke: [f32; 4], stroke_style: StrokeStyle) -> Self {
        Self {
            points,
            stroke,
            stroke_width: DEFAULT_STROKE_WIDTH,
            stroke_style,
        }
    }
}

impl Tool for FreehandTool {
    fn kind(&self) -> ToolKind {
        ToolKind::Freehand
    }
    fn bounds(&self) -> Rect {
        if self.points.is_empty() {
            return Rect {
                x: 0,
                y: 0,
                w: 0,
                h: 0,
            };
        }
        let (mut x0, mut y0) = (f64::INFINITY, f64::INFINITY);
        let (mut x1, mut y1) = (f64::NEG_INFINITY, f64::NEG_INFINITY);
        for &(x, y) in &self.points {
            x0 = x0.min(x);
            y0 = y0.min(y);
            x1 = x1.max(x);
            y1 = y1.max(y);
        }
        Rect {
            x: x0.floor() as i32,
            y: y0.floor() as i32,
            w: (x1 - x0).ceil() as u32,
            h: (y1 - y0).ceil() as u32,
        }
    }
    fn hit_test(&self, x: f64, y: f64) -> bool {
        if self.points.is_empty() {
            return false;
        }
        super::rect_hit_test(self.bounds(), x, y)
    }
    fn translate(&mut self, dx: f64, dy: f64) {
        for p in &mut self.points {
            p.0 += dx;
            p.1 += dy;
        }
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

    #[test]
    fn new_applies_the_default_stroke_width() {
        let t = FreehandTool::new(
            vec![(0.0, 0.0), (10.0, 5.0)],
            [0.0, 1.0, 0.0, 1.0],
            StrokeStyle::Dashed,
        );
        assert_eq!(t.points.len(), 2);
        assert_eq!(t.stroke, [0.0, 1.0, 0.0, 1.0]);
        assert_eq!(t.stroke_width, DEFAULT_STROKE_WIDTH);
        assert_eq!(t.stroke_style, StrokeStyle::Dashed);
    }

    #[test]
    fn translate_shifts_every_point() {
        let mut t = FreehandTool {
            points: vec![(0.0, 0.0), (10.0, 5.0)],
            stroke: [1.0, 0.0, 0.0, 1.0],
            stroke_width: 3.0,
            stroke_style: StrokeStyle::Solid,
        };
        t.translate(2.0, 3.0);
        assert_eq!(t.points, vec![(2.0, 3.0), (12.0, 8.0)]);
    }

    #[test]
    fn hit_test_uses_bounding_box() {
        let t = FreehandTool {
            points: vec![(10.0, 10.0), (40.0, 30.0)],
            stroke: [1.0, 0.0, 0.0, 1.0],
            stroke_width: 3.0,
            stroke_style: StrokeStyle::Solid,
        };
        assert!(t.hit_test(20.0, 20.0));
        assert!(!t.hit_test(100.0, 100.0));
    }
}
