use crate::annotate::{Tool, ToolKind};
use crate::capture::region::Rect;

#[derive(Debug, Clone)]
pub struct BlurTool {
    pub bounds: Rect,
    pub radius: f32,
    /// When `true`, the blur is applied to everything *outside* `bounds` and the inside
    /// stays sharp. Used by SHIFT+drag in the editor and overlay so users can "focus" a
    /// region instead of obscure it. Default `false` preserves the historical behaviour
    /// (blur is contained to `bounds`).
    pub invert: bool,
}

/// Default Gaussian blur radius, in logical pixels. Chosen to make small text unreadable
/// at typical screenshot scales without smearing the region beyond recognition.
pub const DEFAULT_RADIUS: f32 = 12.0;

impl BlurTool {
    pub fn new(bounds: Rect, invert: bool) -> Self {
        Self {
            bounds,
            radius: DEFAULT_RADIUS,
            invert,
        }
    }
}

impl Tool for BlurTool {
    fn kind(&self) -> ToolKind {
        ToolKind::Blur
    }
    fn bounds(&self) -> Rect {
        self.bounds
    }
    fn hit_test(&self, x: f64, y: f64) -> bool {
        let inside = super::rect_hit_test(self.bounds, x, y);
        // For an inverted blur the *outside* is the affected area; clicks outside the
        // selection rect should still hit the layer (so users can pick it from the canvas
        // to delete or otherwise act on it). Clicks inside the rect — which renders sharp
        // — should fall through to whatever is underneath.
        if self.invert { !inside } else { inside }
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

    fn rect() -> Rect {
        Rect {
            x: 10,
            y: 20,
            w: 100,
            h: 80,
        }
    }

    #[test]
    fn clone_box_preserves_invert() {
        let t = BlurTool {
            bounds: rect(),
            radius: 12.0,
            invert: true,
        };
        let cloned = t.clone_box();
        let cloned = cloned.as_any().downcast_ref::<BlurTool>().unwrap();
        assert!(cloned.invert);
        assert_eq!(cloned.bounds, t.bounds);
        assert_eq!(cloned.radius, t.radius);
    }

    #[test]
    fn hit_test_normal_hits_inside_only() {
        let t = BlurTool {
            bounds: rect(),
            radius: 12.0,
            invert: false,
        };
        assert!(t.hit_test(50.0, 50.0));
        assert!(!t.hit_test(500.0, 500.0));
    }

    #[test]
    fn hit_test_inverted_hits_outside_only() {
        let t = BlurTool {
            bounds: rect(),
            radius: 12.0,
            invert: true,
        };
        assert!(!t.hit_test(50.0, 50.0));
        assert!(t.hit_test(500.0, 500.0));
    }
}
