//! Annotation document model, tool trait, and rendering helpers.
//!
//! UI integration lives in [`crate::ui::canvas`] and [`crate::ui::overlay`]. This module is
//! kept UI-free so the document model can be unit-tested without GTK.

pub mod render;
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
    fn clone_box(&self) -> Box<dyn Tool>;
    /// Downcast escape hatch so the UI layer can pick the concrete tool struct (and its
    /// type-specific fields like `FreehandTool::points` or `NumberTool::value`) without baking
    /// rendering knowledge into the trait itself.
    fn as_any(&self) -> &dyn std::any::Any;
}

#[derive(Copy, Clone, Debug, PartialEq, Eq, Hash)]
pub enum ToolKind {
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
}
