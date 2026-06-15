//! Annotation document model, tool trait, and rendering helpers.
//!
//! UI integration lives in [`crate::ui::canvas`] and [`crate::ui::overlay`]. This module is
//! kept UI-free so the document model can be unit-tested without GTK.

pub mod render;
pub mod select;
pub mod tools;

use crate::capture::region::Rect;

/// A single annotation document — a base image plus an ordered list of [`Tool`] layers.
pub struct Document {
    pub base: Option<DocumentBase>,
    pub size: (u32, u32),
    pub layers: Vec<Box<dyn Tool>>,
    pub crop: Option<Rect>,
}

#[derive(Clone)]
pub struct DocumentBase {
    pub pixels: std::sync::Arc<[u8]>,
    pub width: u32,
    pub height: u32,
    pub stride: u32,
}

impl Document {
    pub fn empty(size: (u32, u32)) -> Self {
        Self {
            base: None,
            size,
            layers: Vec::new(),
            crop: None,
        }
    }

    pub fn with_base(base: DocumentBase) -> Self {
        let size = (base.width, base.height);
        Self {
            base: Some(base),
            size,
            layers: Vec::new(),
            crop: None,
        }
    }

    pub fn push_layer(&mut self, tool: Box<dyn Tool>) {
        self.layers.push(tool);
    }

    pub fn pop_layer(&mut self) -> Option<Box<dyn Tool>> {
        self.layers.pop()
    }

    /// Immutable access to the layer at `i`, if it exists.
    pub fn layer(&self, i: usize) -> Option<&dyn Tool> {
        self.layers.get(i).map(|b| b.as_ref())
    }

    /// Mutable access to the boxed layer at `i`, if it exists. Used by the Select tool to
    /// translate / resize a layer in place (via `Tool::translate` or an `as_any_mut` downcast).
    pub fn layer_mut(&mut self, i: usize) -> Option<&mut Box<dyn Tool>> {
        self.layers.get_mut(i)
    }

    /// Remove and return the layer at `i`, or `None` if out of range. Shifts later layers down
    /// by one (their indices change) — callers tracking a selected index must treat it as stale
    /// afterwards.
    pub fn remove_layer(&mut self, i: usize) -> Option<Box<dyn Tool>> {
        if i < self.layers.len() {
            Some(self.layers.remove(i))
        } else {
            None
        }
    }

    pub fn layer_count(&self) -> usize {
        self.layers.len()
    }

    pub fn bounds(&self) -> Rect {
        self.crop.unwrap_or(Rect {
            x: 0,
            y: 0,
            w: self.size.0,
            h: self.size.1,
        })
    }
}

pub trait Tool: std::fmt::Debug + Send + Sync {
    fn kind(&self) -> ToolKind;
    fn bounds(&self) -> Rect;
    fn hit_test(&self, x: f64, y: f64) -> bool;
    /// Translate the shape by `(dx, dy)` document-space pixels in place. Pure geometry — each
    /// concrete tool moves its own coordinate storage. Used by the Select tool's move gesture
    /// and keyboard nudge. Does not touch color / style / text.
    fn translate(&mut self, dx: f64, dy: f64);
    fn clone_box(&self) -> Box<dyn Tool>;
    /// Downcast escape hatch so the UI layer can pick the concrete tool struct (and its
    /// type-specific fields like `FreehandTool::points` or `NumberTool::value`) without baking
    /// rendering knowledge into the trait itself.
    fn as_any(&self) -> &dyn std::any::Any;
    /// Mutable counterpart to [`Self::as_any`], so the Select tool can resize a layer in place
    /// (e.g. set a `RectTool::bounds` or an `ArrowTool` endpoint) without per-kind trait methods.
    fn as_any_mut(&mut self) -> &mut dyn std::any::Any;
}

#[derive(Copy, Clone, Debug, PartialEq, Eq, Hash)]
pub enum ToolKind {
    /// Pointer mode: select an existing layer, then move / resize / re-edit it. Never produces
    /// a layer of its own.
    Select,
    Arrow,
    Line,
    Rect,
    Ellipse,
    Text,
    Blur,
    Highlight,
    Freehand,
    Number,
    Redact,
    Crop,
}

/// Per-tool line dash pattern. Applies to outline-rendering tools
/// (Rect / Ellipse / Arrow / Line / Freehand); ignored by fill-only
/// tools (Highlight) and hardcoded tools (Blur, Crop, Redact). The
/// toolbar's style picker exposes one of these per tool.
#[derive(Copy, Clone, Debug, PartialEq, Eq, Hash, Default)]
pub enum StrokeStyle {
    #[default]
    Solid,
    Dashed,
    Dotted,
}

#[cfg(test)]
mod tests {
    use super::*;
    use crate::annotate::tools::rect::RectTool;
    use pretty_assertions::assert_eq;

    #[test]
    fn push_and_pop_layer() {
        let mut doc = Document::empty((100, 100));
        doc.push_layer(Box::new(RectTool::new(Rect {
            x: 0,
            y: 0,
            w: 10,
            h: 10,
        })));
        assert_eq!(doc.layers.len(), 1);
        assert!(doc.pop_layer().is_some());
        assert!(doc.layers.is_empty());
    }

    #[test]
    fn remove_layer_shifts_indices() {
        let mut doc = Document::empty((100, 100));
        for i in 0..3 {
            doc.push_layer(Box::new(RectTool::new(Rect {
                x: i,
                y: 0,
                w: 10,
                h: 10,
            })));
        }
        // Remove the middle layer; the third shifts down to index 1.
        let removed = doc.remove_layer(1).expect("layer present");
        assert_eq!(removed.bounds().x, 1);
        assert_eq!(doc.layer_count(), 2);
        assert_eq!(doc.layer(1).unwrap().bounds().x, 2);
        // Out-of-range removal is a no-op.
        assert!(doc.remove_layer(5).is_none());
    }

    #[test]
    fn layer_mut_translates_in_place() {
        let mut doc = Document::empty((100, 100));
        doc.push_layer(Box::new(RectTool::new(Rect {
            x: 0,
            y: 0,
            w: 10,
            h: 10,
        })));
        doc.layer_mut(0).unwrap().translate(5.0, 7.0);
        assert_eq!(doc.layer(0).unwrap().bounds().x, 5);
        assert_eq!(doc.layer(0).unwrap().bounds().y, 7);
    }
}
