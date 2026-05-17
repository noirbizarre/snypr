use crate::annotate::{StrokeStyle, Tool, ToolKind};
use crate::capture::region::Rect;

/// Outline ellipse inscribed in `bounds`. Same defaults as [`super::rect::RectTool`] — a
/// 2px red stroke — so the two shape tools behave symmetrically. Hit-testing uses the
/// bounding rectangle (matches `RectTool`) which is plenty for the current selection /
/// hover model; switching to a true point-in-ellipse test is a one-liner if we ever need
/// finer-grained picking.
#[derive(Debug, Clone)]
pub struct EllipseTool {
    pub bounds: Rect,
    pub stroke: [f32; 4],
    pub stroke_width: f32,
    pub stroke_style: StrokeStyle,
}

impl EllipseTool {
    pub fn new(bounds: Rect) -> Self {
        Self {
            bounds,
            stroke: [1.0, 0.0, 0.0, 1.0],
            stroke_width: 2.0,
            stroke_style: StrokeStyle::Solid,
        }
    }
}

impl Tool for EllipseTool {
    fn kind(&self) -> ToolKind {
        ToolKind::Ellipse
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
