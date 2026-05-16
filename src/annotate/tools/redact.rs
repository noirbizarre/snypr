use crate::annotate::{Tool, ToolKind};
use crate::capture::region::Rect;

#[derive(Debug, Clone)]
pub struct RedactTool {
    pub bounds: Rect,
}

impl Tool for RedactTool {
    fn kind(&self) -> ToolKind {
        ToolKind::Redact
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
