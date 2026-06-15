use crate::annotate::{Tool, ToolKind};
use crate::capture::region::Rect;

/// `Crop` is applied at export time only — it doesn't produce a render node.
#[derive(Debug, Clone)]
pub struct CropTool {
    pub bounds: Rect,
}

impl Tool for CropTool {
    fn kind(&self) -> ToolKind {
        ToolKind::Crop
    }
    fn bounds(&self) -> Rect {
        self.bounds
    }
    fn hit_test(&self, x: f64, y: f64) -> bool {
        let r = self.bounds;
        x >= r.x as f64 && x <= r.right() as f64 && y >= r.y as f64 && y <= r.bottom() as f64
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
