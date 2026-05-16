use crate::annotate::{Tool, ToolKind};
use crate::capture::region::Rect;

#[derive(Debug, Clone)]
pub struct ArrowTool {
    pub from: (f64, f64),
    pub to: (f64, f64),
    pub stroke: [f32; 4],
    pub stroke_width: f32,
}

impl ArrowTool {
    pub fn new(from: (f64, f64), to: (f64, f64)) -> Self {
        Self {
            from,
            to,
            stroke: [1.0, 0.0, 0.0, 1.0],
            stroke_width: 3.0,
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
    fn hit_test(&self, _x: f64, _y: f64) -> bool {
        false
    }
    fn clone_box(&self) -> Box<dyn Tool> {
        Box::new(self.clone())
    }
}
