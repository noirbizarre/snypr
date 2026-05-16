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
    fn clone_box(&self) -> Box<dyn Tool> {
        Box::new(self.clone())
    }
}
