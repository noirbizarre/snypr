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
    /// Pixel `(width, height)` of the laid-out text at the chosen `size_pt`, captured at
    /// commit time. Used purely as the layer's reported [`Tool::bounds`]; the rendered
    /// glyphs are re-laid out from `text` on every snapshot.
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
    fn hit_test(&self, _x: f64, _y: f64) -> bool {
        false
    }
    fn clone_box(&self) -> Box<dyn Tool> {
        Box::new(self.clone())
    }
    fn as_any(&self) -> &dyn std::any::Any {
        self
    }
}
