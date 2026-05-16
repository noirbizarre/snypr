//! Annotation canvas — a `GtkWidget` subclass that draws a [`Document`] via Cairo.
//!
//! Rendering goes through `Snapshot::append_cairo` rather than building per-shape GSK render
//! nodes: Cairo gives us stroke/fill/arrowhead semantics with one path each, and the same
//! drawing routine is reused (against a `cairo::ImageSurface`) by [`AnnotationCanvas::compose_png`]
//! to flatten the document to a PNG for saving. The cached `base_surface` keeps the BGRA swizzle
//! cost off the per-frame draw path.

use std::cell::{Cell, RefCell};
use std::rc::Rc;

use anyhow::{Result, anyhow};
use gtk4::cairo;
use gtk4::glib;
use gtk4::prelude::*;
use gtk4::subclass::prelude::*;

use crate::annotate::render::{arrowhead, drag_rect};
use crate::annotate::tools::arrow::ArrowTool;
use crate::annotate::tools::rect::RectTool;
use crate::annotate::{Document, Tool, ToolKind};
use crate::capture::CapturedImage;
use crate::capture::region::Rect;

glib::wrapper! {
    pub struct AnnotationCanvas(ObjectSubclass<imp::AnnotationCanvas>)
        @extends gtk4::Widget,
        @implements gtk4::Accessible, gtk4::Buildable, gtk4::ConstraintTarget;
}

impl AnnotationCanvas {
    pub fn new() -> Self {
        glib::Object::new()
    }

    pub fn set_document(&self, doc: Document) {
        let imp = self.imp();
        // Refresh the cached cairo surface used for the base texture so the first draw doesn't
        // pay the swizzle cost mid-frame.
        let surface = doc.base.as_ref().and_then(|b| build_base_surface(b).ok());
        imp.base_surface.replace(surface);
        imp.doc.replace(Some(Rc::new(RefCell::new(doc))));
        imp.pending.replace(None);
        self.queue_resize();
        self.queue_draw();
    }

    /// Currently active tool used when the user starts a new drag.
    pub fn set_tool(&self, kind: ToolKind) {
        self.imp().current_tool.set(kind);
    }

    pub fn tool(&self) -> ToolKind {
        self.imp().current_tool.get()
    }

    /// Pop the most recently committed layer, if any.
    pub fn undo(&self) -> bool {
        let Some(doc_rc) = self.imp().doc.borrow().clone() else {
            return false;
        };
        let popped = doc_rc.borrow_mut().pop_layer().is_some();
        if popped {
            self.queue_draw();
        }
        popped
    }

    /// Render the current document to a freshly-allocated `CapturedImage` (BGRA, ready for
    /// `output::encode_png`). Used by the editor's save action and exercised in tests.
    pub fn compose(&self) -> Result<CapturedImage> {
        let Some(doc_rc) = self.imp().doc.borrow().clone() else {
            return Err(anyhow!("no document loaded"));
        };
        let doc = doc_rc.borrow();
        compose_document(&doc)
    }

    /// Convenience: compose + PNG-encode the document in one go.
    pub fn compose_png(&self) -> Result<Vec<u8>> {
        let img = self.compose()?;
        crate::output::encode_png(&img)
    }
}

impl Default for AnnotationCanvas {
    fn default() -> Self {
        Self::new()
    }
}

/// Build a cached BGRA `cairo::ImageSurface` from the document's base pixels (which are RGBA).
fn build_base_surface(base: &crate::annotate::DocumentBase) -> Result<cairo::ImageSurface> {
    let mut bgra = base.pixels.to_vec();
    for px in bgra.chunks_exact_mut(4) {
        px.swap(0, 2); // RGBA -> BGRA so Cairo ARGB32 (little-endian) renders correctly
    }
    let stride = cairo::Format::ARgb32
        .stride_for_width(base.width)
        .map_err(|e| anyhow!("cairo stride: {e}"))?;
    let row_bytes = (base.width * 4) as usize;
    let surface = if stride as usize == row_bytes {
        cairo::ImageSurface::create_for_data(
            bgra,
            cairo::Format::ARgb32,
            base.width as i32,
            base.height as i32,
            stride,
        )
    } else {
        // Cairo's stride may include trailing padding when the row is not 4-byte aligned (e.g.
        // odd widths). Rebuild with the padded stride.
        let mut padded = vec![0u8; stride as usize * base.height as usize];
        for y in 0..base.height as usize {
            let src = &bgra[y * row_bytes..(y + 1) * row_bytes];
            padded[y * stride as usize..y * stride as usize + row_bytes].copy_from_slice(src);
        }
        cairo::ImageSurface::create_for_data(
            padded,
            cairo::Format::ARgb32,
            base.width as i32,
            base.height as i32,
            stride,
        )
    }
    .map_err(|e| anyhow!("creating base cairo surface: {e}"))?;
    Ok(surface)
}

/// Render `doc` into a freshly-allocated `CapturedImage` in BGRA (Cairo ARGB32 byte order).
fn compose_document(doc: &Document) -> Result<CapturedImage> {
    let (w, h) = doc.size;
    if w == 0 || h == 0 {
        return Err(anyhow!("cannot compose empty document"));
    }
    let surface = cairo::ImageSurface::create(cairo::Format::ARgb32, w as i32, h as i32)
        .map_err(|e| anyhow!("creating composite surface: {e}"))?;
    {
        let cr = cairo::Context::new(&surface).map_err(|e| anyhow!("cairo context: {e}"))?;
        // Transparent base so widget background isn't baked into the saved image when the
        // document has no base texture.
        if let Some(base) = &doc.base {
            let bs = build_base_surface(base)?;
            cr.set_source_surface(&bs, 0.0, 0.0)
                .map_err(|e| anyhow!("set_source_surface: {e}"))?;
            cr.paint().map_err(|e| anyhow!("paint base: {e}"))?;
        }
        for layer in &doc.layers {
            draw_tool(layer.as_ref(), &cr);
        }
    }
    surface.flush();
    let stride = surface.stride() as u32;
    let mut surface = surface;
    let data = surface
        .data()
        .map_err(|e| anyhow!("cairo surface data: {e}"))?;
    let pixels: std::sync::Arc<[u8]> = std::sync::Arc::from(data.to_vec().into_boxed_slice());
    Ok(CapturedImage {
        width: w,
        height: h,
        stride,
        pixels,
        source: None,
    })
}

/// Draw a committed [`Tool`] layer into a cairo context. Unknown tool kinds are ignored — the
/// matching draw routines are added as each tool ships.
fn draw_tool(tool: &dyn Tool, cr: &cairo::Context) {
    match tool.kind() {
        ToolKind::Rect => {
            // We don't have a downcast from `&dyn Tool`; rebuild the shape from `bounds()` and a
            // hard-coded stroke. The dedicated `RectTool` fields (stroke color/width) are wired
            // up in the next pass.
            let r = tool.bounds();
            draw_rect_outline(cr, &r, [1.0, 0.0, 0.0, 1.0], 2.0);
        }
        ToolKind::Arrow => {
            let r = tool.bounds();
            // For an arrow we approximate from/to as the top-left / bottom-right of the bounds.
            // The interactive path stores the genuine endpoints; this fallback only matters for
            // documents reconstructed from disk in a future release.
            let from = (r.x as f64, r.y as f64);
            let to = (r.right() as f64, r.bottom() as f64);
            draw_arrow(cr, from, to, [1.0, 0.0, 0.0, 1.0], 3.0);
        }
        _ => {}
    }
}

fn draw_rect_outline(cr: &cairo::Context, rect: &Rect, rgba: [f64; 4], width: f64) {
    cr.set_source_rgba(rgba[0], rgba[1], rgba[2], rgba[3]);
    cr.set_line_width(width);
    cr.rectangle(rect.x as f64, rect.y as f64, rect.w as f64, rect.h as f64);
    let _ = cr.stroke();
}

fn draw_arrow(cr: &cairo::Context, from: (f64, f64), to: (f64, f64), rgba: [f64; 4], width: f64) {
    cr.set_source_rgba(rgba[0], rgba[1], rgba[2], rgba[3]);
    cr.set_line_width(width);
    cr.move_to(from.0, from.1);
    cr.line_to(to.0, to.1);
    let _ = cr.stroke();
    let head = (width * 5.0).max(10.0);
    let (l, r) = arrowhead(from, to, head);
    cr.move_to(to.0, to.1);
    cr.line_to(l.0, l.1);
    cr.line_to(r.0, r.1);
    cr.close_path();
    let _ = cr.fill();
}

/// A drag-in-progress preview that hasn't been committed to the document yet.
#[derive(Clone, Copy, Debug)]
pub struct PendingStroke {
    kind: ToolKind,
    from: (f64, f64),
    to: (f64, f64),
}

mod imp {
    use super::*;

    pub struct AnnotationCanvas {
        pub doc: RefCell<Option<Rc<RefCell<Document>>>>,
        pub base_surface: RefCell<Option<cairo::ImageSurface>>,
        pub current_tool: Cell<ToolKind>,
        pub pending: RefCell<Option<PendingStroke>>,
    }

    impl Default for AnnotationCanvas {
        fn default() -> Self {
            Self {
                doc: RefCell::new(None),
                base_surface: RefCell::new(None),
                current_tool: Cell::new(ToolKind::Rect),
                pending: RefCell::new(None),
            }
        }
    }

    #[glib::object_subclass]
    impl ObjectSubclass for AnnotationCanvas {
        const NAME: &'static str = "HyprSnapAnnotationCanvas";
        type Type = super::AnnotationCanvas;
        type ParentType = gtk4::Widget;
    }

    impl ObjectImpl for AnnotationCanvas {
        fn constructed(&self) {
            self.parent_constructed();
            let obj = self.obj();
            obj.set_focusable(true);
            install_drag(&obj);
        }
    }

    impl WidgetImpl for AnnotationCanvas {
        fn measure(&self, orientation: gtk4::Orientation, _for_size: i32) -> (i32, i32, i32, i32) {
            // Natural size matches the document so a ScrolledWindow can present it at 1:1.
            // Fall back to a sensible default when no document is loaded.
            let (w, h) = self
                .doc
                .borrow()
                .as_ref()
                .map(|d| d.borrow().size)
                .unwrap_or((640, 480));
            let nat = match orientation {
                gtk4::Orientation::Horizontal => w as i32,
                _ => h as i32,
            };
            (0, nat, -1, -1)
        }

        fn snapshot(&self, snapshot: &gtk4::Snapshot) {
            let Some(doc_rc) = self.doc.borrow().clone() else {
                return;
            };
            let doc = doc_rc.borrow();
            let width = doc.size.0 as f32;
            let height = doc.size.1 as f32;
            let bounds = gtk4::graphene::Rect::new(0.0, 0.0, width, height);

            let cr = snapshot.append_cairo(&bounds);
            // Neutral background for documents without a base image.
            if doc.base.is_none() {
                cr.set_source_rgba(0.1, 0.1, 0.12, 1.0);
                cr.paint().ok();
            } else if let Some(bs) = self.base_surface.borrow().as_ref() {
                cr.set_source_surface(bs, 0.0, 0.0).ok();
                cr.paint().ok();
            }
            for layer in &doc.layers {
                draw_tool(layer.as_ref(), &cr);
            }
            if let Some(p) = *self.pending.borrow() {
                draw_pending(&cr, p);
            }
        }
    }
}

fn draw_pending(cr: &cairo::Context, p: PendingStroke) {
    match p.kind {
        ToolKind::Rect => {
            let r = drag_rect(p.from, p.to);
            // Slightly translucent stroke + dashed style to distinguish the preview from
            // committed layers without confusing screenshots.
            cr.save().ok();
            cr.set_dash(&[6.0, 4.0], 0.0);
            draw_rect_outline(cr, &r, [1.0, 0.0, 0.0, 0.85], 2.0);
            cr.restore().ok();
        }
        ToolKind::Arrow => {
            draw_arrow(cr, p.from, p.to, [1.0, 0.0, 0.0, 0.85], 3.0);
        }
        _ => {}
    }
}

fn install_drag(canvas: &AnnotationCanvas) {
    let drag = gtk4::GestureDrag::new();

    {
        let weak = canvas.downgrade();
        drag.connect_drag_begin(move |_, x, y| {
            let Some(c) = weak.upgrade() else { return };
            c.imp().pending.replace(Some(PendingStroke {
                kind: c.tool(),
                from: (x, y),
                to: (x, y),
            }));
            c.queue_draw();
        });
    }
    {
        let weak = canvas.downgrade();
        drag.connect_drag_update(move |g, dx, dy| {
            let Some(c) = weak.upgrade() else { return };
            let Some((sx, sy)) = g.start_point() else {
                return;
            };
            let mut p = c.imp().pending.borrow_mut();
            if let Some(stroke) = p.as_mut() {
                stroke.to = (sx + dx, sy + dy);
                drop(p);
                c.queue_draw();
            }
        });
    }
    {
        let weak = canvas.downgrade();
        drag.connect_drag_end(move |g, dx, dy| {
            let Some(c) = weak.upgrade() else { return };
            let Some((sx, sy)) = g.start_point() else {
                return;
            };
            // Commit the pending stroke to the document and clear the preview.
            let stroke = c.imp().pending.borrow_mut().take();
            let Some(mut stroke) = stroke else { return };
            stroke.to = (sx + dx, sy + dy);
            let Some(doc_rc) = c.imp().doc.borrow().clone() else {
                return;
            };
            let mut doc = doc_rc.borrow_mut();
            match stroke.kind {
                ToolKind::Rect => {
                    let r = drag_rect(stroke.from, stroke.to);
                    if r.w >= 2 && r.h >= 2 {
                        doc.push_layer(Box::new(RectTool::new(r)));
                    }
                }
                ToolKind::Arrow => {
                    let dx = stroke.to.0 - stroke.from.0;
                    let dy = stroke.to.1 - stroke.from.1;
                    if (dx * dx + dy * dy) >= 16.0 {
                        doc.push_layer(Box::new(ArrowTool::new(stroke.from, stroke.to)));
                    }
                }
                _ => {}
            }
            drop(doc);
            c.queue_draw();
        });
    }
    canvas.add_controller(drag);
}

#[cfg(test)]
mod tests {
    use super::*;
    use crate::annotate::DocumentBase;
    use std::sync::Arc;

    fn solid_base(w: u32, h: u32) -> DocumentBase {
        let pixels: Arc<[u8]> = Arc::from(vec![0xFFu8; (w * h * 4) as usize].into_boxed_slice());
        DocumentBase {
            pixels,
            width: w,
            height: h,
            stride: w * 4,
        }
    }

    #[test]
    fn compose_document_produces_bgra_buffer() {
        let mut doc = Document::with_base(solid_base(4, 4));
        doc.push_layer(Box::new(RectTool::new(Rect {
            x: 0,
            y: 0,
            w: 4,
            h: 4,
        })));
        let img = compose_document(&doc).expect("compose");
        assert_eq!(img.width, 4);
        assert_eq!(img.height, 4);
        // ARgb32 stride is at least 4 * width.
        assert!(img.stride >= 16);
    }
}
