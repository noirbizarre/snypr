use crate::annotate::{StrokeStyle, Tool, ToolKind};
use crate::capture::region::Rect;

#[derive(Debug, Clone)]
pub struct RectTool {
    pub bounds: Rect,
    pub stroke: [f32; 4],
    pub stroke_width: f32,
    pub stroke_style: StrokeStyle,
}

impl RectTool {
    pub fn new(bounds: Rect) -> Self {
        Self {
            bounds,
            stroke: [1.0, 0.0, 0.0, 1.0],
            stroke_width: 2.0,
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
        let r = self.bounds;
        x >= r.x as f64 && x <= r.right() as f64 && y >= r.y as f64 && y <= r.bottom() as f64
    }
    fn clone_box(&self) -> Box<dyn Tool> {
        Box::new(self.clone())
    }
    fn as_any(&self) -> &dyn std::any::Any {
        self
    }
}
