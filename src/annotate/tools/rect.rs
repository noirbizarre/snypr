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
        super::rect_hit_test(self.bounds, x, y)
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

#[cfg(test)]
mod tests {
    use super::*;
    use pretty_assertions::assert_eq;

    #[test]
    fn translate_shifts_bounds() {
        let mut t = RectTool::new(Rect {
            x: 10,
            y: 20,
            w: 30,
            h: 40,
        });
        t.translate(5.0, -7.0);
        assert_eq!(
            t.bounds,
            Rect {
                x: 15,
                y: 13,
                w: 30,
                h: 40
            }
        );
    }
}
