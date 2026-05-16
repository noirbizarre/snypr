use crate::annotate::{Tool, ToolKind};
use crate::capture::region::Rect;

#[derive(Debug, Clone)]
pub struct TextTool {
    pub origin: (f64, f64),
    pub text: String,
    pub size_pt: f32,
    pub color: [f32; 4],
}

impl Tool for TextTool {
    fn kind(&self) -> ToolKind {
        ToolKind::Text
    }
    fn bounds(&self) -> Rect {
        Rect {
            x: self.origin.0 as i32,
            y: self.origin.1 as i32,
            w: (self.text.len() as u32) * (self.size_pt as u32),
            h: self.size_pt as u32,
        }
    }
    fn hit_test(&self, _x: f64, _y: f64) -> bool {
        false
    }
    fn clone_box(&self) -> Box<dyn Tool> {
        Box::new(self.clone())
    }
}
