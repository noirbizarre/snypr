use crate::annotate::{Tool, ToolKind};
use crate::capture::region::Rect;

#[derive(Debug, Clone)]
pub struct NumberTool {
    pub center: (f64, f64),
    pub radius: f64,
    pub value: u32,
    pub fill: [f32; 4],
    pub text_color: [f32; 4],
}

impl Tool for NumberTool {
    fn kind(&self) -> ToolKind {
        ToolKind::Number
    }
    fn bounds(&self) -> Rect {
        Rect {
            x: (self.center.0 - self.radius).floor() as i32,
            y: (self.center.1 - self.radius).floor() as i32,
            w: (self.radius * 2.0).ceil() as u32,
            h: (self.radius * 2.0).ceil() as u32,
        }
    }
    fn hit_test(&self, x: f64, y: f64) -> bool {
        let dx = x - self.center.0;
        let dy = y - self.center.1;
        (dx * dx + dy * dy).sqrt() <= self.radius
    }
    fn clone_box(&self) -> Box<dyn Tool> {
        Box::new(self.clone())
    }
}
