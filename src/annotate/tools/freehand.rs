use crate::annotate::{Tool, ToolKind};
use crate::capture::region::Rect;

#[derive(Debug, Clone)]
pub struct FreehandTool {
    pub points: Vec<(f64, f64)>,
    pub stroke: [f32; 4],
    pub stroke_width: f32,
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
    fn hit_test(&self, _x: f64, _y: f64) -> bool {
        false
    }
    fn clone_box(&self) -> Box<dyn Tool> {
        Box::new(self.clone())
    }
}
