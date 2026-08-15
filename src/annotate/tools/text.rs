use crate::annotate::{Tool, ToolKind};
use crate::capture::region::Rect;

/// Static text annotation. Built once at commit time by the canvas's WYSIWYG editor
/// (see `commit_pending_text` in `src/ui/canvas.rs`); `bounds_cache` is the Pango-measured
/// pixel size of the laid-out glyphs at commit time so [`Tool::bounds`] can return the real
/// extent without needing a `pango::Context`.
#[derive(Debug, Clone)]
pub struct TextTool {
    pub origin: (f64, f64),
    pub text: String,
    pub size_pt: f32,
    pub color: [f32; 4],
    /// Optional word-wrap width in document pixels. `None` means the text lays out at its
    /// natural width (line breaks only at explicit `\n`). `Some(w)` makes Pango wrap long
    /// lines at `w` — driven by the Select tool's east/west resize handles.
    pub wrap_width: Option<f64>,
    /// Pixel `(width, height)` of the laid-out text at the chosen `size_pt` (and `wrap_width`),
    /// captured at commit / resize time. Used purely as the layer's reported [`Tool::bounds`];
    /// the rendered glyphs are re-laid out from `text` on every snapshot.
    pub bounds_cache: (u32, u32),
}

impl TextTool {
    pub fn new(
        origin: (f64, f64),
        text: String,
        size_pt: f32,
        color: [f32; 4],
        bounds_cache: (u32, u32),
    ) -> Self {
        Self {
            origin,
            text,
            size_pt,
            color,
            wrap_width: None,
            bounds_cache,
        }
    }
}

impl Tool for TextTool {
    fn kind(&self) -> ToolKind {
        ToolKind::Text
    }
    fn bounds(&self) -> Rect {
        Rect {
            x: self.origin.0 as i32,
            y: self.origin.1 as i32,
            w: self.bounds_cache.0,
            h: self.bounds_cache.1,
        }
    }
    fn hit_test(&self, x: f64, y: f64) -> bool {
        super::rect_hit_test(self.bounds(), x, y)
    }
    fn translate(&mut self, dx: f64, dy: f64) {
        self.origin = (self.origin.0 + dx, self.origin.1 + dy);
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

    fn sample() -> TextTool {
        TextTool::new(
            (10.0, 20.0),
            "hi".into(),
            18.0,
            [1.0, 1.0, 1.0, 1.0],
            (40, 24),
        )
    }

    #[test]
    fn hit_test_inside_cached_bounds() {
        let t = sample();
        assert!(t.hit_test(30.0, 30.0));
        assert!(t.hit_test(10.0, 20.0));
        assert!(t.hit_test(50.0, 44.0));
    }

    #[test]
    fn hit_test_outside_misses() {
        let t = sample();
        assert!(!t.hit_test(5.0, 30.0));
        assert!(!t.hit_test(60.0, 30.0));
    }

    #[test]
    fn translate_moves_origin() {
        let mut t = sample();
        t.translate(3.0, 4.0);
        assert_eq!(t.origin, (13.0, 24.0));
    }
}
