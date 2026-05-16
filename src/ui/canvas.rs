//! Annotation canvas — a `GtkWidget` subclass that draws a [`Document`] via GSK render nodes.
//!
//! The on-screen `snapshot()` path builds `gsk::Path` / `gsk::Stroke` nodes and pushes them
//! onto the [`gtk4::Snapshot`] so GTK's GL renderer can rasterise everything on the GPU.
//! [`AnnotationCanvas::compose_png`] still flattens through Cairo because it produces a
//! pixel buffer for PNG encoding, which is exactly Cairo's domain — but the GPU path no
//! longer pays the BGRA swizzle on every frame: we cache the base image as a
//! [`gdk::MemoryTexture`] in RGBA and let GSK upload it once.

use std::cell::{Cell, RefCell};
use std::rc::Rc;

use anyhow::{Result, anyhow};
use gtk4::cairo;
use gtk4::gdk;
use gtk4::glib;
use gtk4::graphene;
use gtk4::gsk;
use gtk4::pango;
use gtk4::prelude::*;
use gtk4::subclass::prelude::*;

use crate::annotate::render::{arrowhead, drag_rect};
use crate::annotate::tools::arrow::ArrowTool;
use crate::annotate::tools::freehand::FreehandTool;
use crate::annotate::tools::highlight::HighlightTool;
use crate::annotate::tools::number::NumberTool;
use crate::annotate::tools::rect::RectTool;
use crate::annotate::tools::redact::RedactTool;
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
        // Cache a gdk::MemoryTexture for the on-screen path so GSK can upload it once and
        // sample it on the GPU — avoids the per-frame BGRA swizzle that Cairo needed.
        let tex = doc.base.as_ref().and_then(|b| build_base_texture(b).ok());
        imp.base_texture.replace(tex);
        imp.doc.replace(Some(Rc::new(RefCell::new(doc))));
        imp.pending.replace(None);
        imp.next_number.set(1);
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

    /// Render with a transparent background when no base image is loaded. The annotation editor
    /// keeps the default (dark fill) so an unloaded canvas is visible, but the live overlay
    /// flips this on so clicks/strokes appear directly on top of the desktop.
    pub fn set_transparent(&self, transparent: bool) {
        self.imp().transparent_background.set(transparent);
        self.queue_draw();
    }

    /// Replace the current document with an empty one of the given logical size. Used by the
    /// live overlay to spin up a canvas sized to a monitor without owning any pixels.
    pub fn set_empty(&self, size: (u32, u32)) {
        self.set_document(Document::empty(size));
    }

    /// Drop every committed layer (used by the overlay's "clear" shortcut).
    pub fn clear_layers(&self) {
        if let Some(doc_rc) = self.imp().doc.borrow().clone() {
            let mut doc = doc_rc.borrow_mut();
            doc.layers.clear();
            doc.crop = None;
            drop(doc);
            self.queue_draw();
        }
    }

    /// Pop the most recently committed layer, if any. Falls back to clearing an active crop when
    /// there are no layers left — otherwise crops would be undoable only by closing the editor.
    pub fn undo(&self) -> bool {
        let Some(doc_rc) = self.imp().doc.borrow().clone() else {
            return false;
        };
        let mut doc = doc_rc.borrow_mut();
        if doc.pop_layer().is_some() {
            drop(doc);
            self.queue_draw();
            return true;
        }
        if doc.crop.is_some() {
            doc.crop = None;
            drop(doc);
            self.queue_resize();
            self.queue_draw();
            return true;
        }
        false
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

// ---------------------------------------------------------------------------
// GSK on-screen rendering
// ---------------------------------------------------------------------------

/// Build a `gdk::MemoryTexture` from the document's RGBA base pixels. The texture is uploaded
/// to the GPU on first use and cached on the canvas for the lifetime of the document.
fn build_base_texture(base: &crate::annotate::DocumentBase) -> Result<gdk::MemoryTexture> {
    let bytes = glib::Bytes::from(base.pixels.as_ref());
    let tex = gdk::MemoryTexture::new(
        base.width as i32,
        base.height as i32,
        gdk::MemoryFormat::R8g8b8a8,
        &bytes,
        base.stride as usize,
    );
    Ok(tex)
}

fn rgba(c: [f32; 4]) -> gdk::RGBA {
    gdk::RGBA::new(c[0], c[1], c[2], c[3])
}

fn rect_to_graphene(r: &Rect) -> graphene::Rect {
    graphene::Rect::new(r.x as f32, r.y as f32, r.w as f32, r.h as f32)
}

/// Path builder helper for an outlined rectangle.
fn rect_path(r: &Rect) -> gsk::Path {
    let pb = gsk::PathBuilder::new();
    pb.add_rect(&rect_to_graphene(r));
    pb.to_path()
}

fn line_path(from: (f64, f64), to: (f64, f64)) -> gsk::Path {
    let pb = gsk::PathBuilder::new();
    pb.move_to(from.0 as f32, from.1 as f32);
    pb.line_to(to.0 as f32, to.1 as f32);
    pb.to_path()
}

fn arrowhead_path(from: (f64, f64), to: (f64, f64), size: f64) -> gsk::Path {
    let (l, r) = arrowhead(from, to, size);
    let pb = gsk::PathBuilder::new();
    pb.move_to(to.0 as f32, to.1 as f32);
    pb.line_to(l.0 as f32, l.1 as f32);
    pb.line_to(r.0 as f32, r.1 as f32);
    pb.close();
    pb.to_path()
}

fn polyline_path(points: &[(f64, f64)]) -> Option<gsk::Path> {
    if points.is_empty() {
        return None;
    }
    let pb = gsk::PathBuilder::new();
    let (x0, y0) = points[0];
    pb.move_to(x0 as f32, y0 as f32);
    if points.len() == 1 {
        // Single tap → render a tiny dash so the user gets feedback.
        pb.line_to(x0 as f32 + 0.01, y0 as f32);
    } else {
        for &(x, y) in &points[1..] {
            pb.line_to(x as f32, y as f32);
        }
    }
    Some(pb.to_path())
}

fn solid_stroke(width: f64) -> gsk::Stroke {
    let s = gsk::Stroke::new(width as f32);
    s.set_line_cap(gsk::LineCap::Round);
    s.set_line_join(gsk::LineJoin::Round);
    s
}

fn dashed_stroke(width: f64, dash: &[f32]) -> gsk::Stroke {
    let s = solid_stroke(width);
    s.set_dash(dash);
    s
}

/// Append a committed [`Tool`] layer to the snapshot.
fn snapshot_tool(snap: &gtk4::Snapshot, tool: &dyn Tool, pango_ctx: &pango::Context) {
    match tool.kind() {
        ToolKind::Rect => {
            if let Some(t) = tool.as_any().downcast_ref::<RectTool>() {
                snap.append_stroke(
                    &rect_path(&t.bounds),
                    &solid_stroke(t.stroke_width as f64),
                    &rgba(t.stroke),
                );
            }
        }
        ToolKind::Arrow => {
            if let Some(t) = tool.as_any().downcast_ref::<ArrowTool>() {
                let color = rgba(t.stroke);
                snap.append_stroke(
                    &line_path(t.from, t.to),
                    &solid_stroke(t.stroke_width as f64),
                    &color,
                );
                let head = (t.stroke_width as f64 * 5.0).max(10.0);
                snap.append_fill(
                    &arrowhead_path(t.from, t.to, head),
                    gsk::FillRule::Winding,
                    &color,
                );
            }
        }
        ToolKind::Highlight => {
            if let Some(t) = tool.as_any().downcast_ref::<HighlightTool>() {
                snap.append_color(&rgba(t.color), &rect_to_graphene(&t.bounds));
            }
        }
        ToolKind::Freehand => {
            if let Some(t) = tool.as_any().downcast_ref::<FreehandTool>()
                && let Some(path) = polyline_path(&t.points)
            {
                snap.append_stroke(
                    &path,
                    &solid_stroke(t.stroke_width as f64),
                    &rgba(t.stroke),
                );
            }
        }
        ToolKind::Redact => {
            if let Some(t) = tool.as_any().downcast_ref::<RedactTool>() {
                snap.append_color(&gdk::RGBA::BLACK, &rect_to_graphene(&t.bounds));
            }
        }
        ToolKind::Number => {
            if let Some(t) = tool.as_any().downcast_ref::<NumberTool>() {
                snapshot_number(snap, t, pango_ctx);
            }
        }
        // `Crop` is applied at compose time via `doc.crop`; no on-screen layer rendering.
        // Text/Blur land in a follow-up commit (text-entry popover + live region blur).
        ToolKind::Crop | ToolKind::Text | ToolKind::Blur => {}
    }
}

fn snapshot_number(snap: &gtk4::Snapshot, t: &NumberTool, pango_ctx: &pango::Context) {
    let (cx, cy) = (t.center.0 as f32, t.center.1 as f32);
    // Filled disc as a circular path.
    let pb = gsk::PathBuilder::new();
    pb.add_circle(&graphene::Point::new(cx, cy), t.radius as f32);
    snap.append_fill(&pb.to_path(), gsk::FillRule::Winding, &rgba(t.fill));

    // Centered Pango label. We measure the layout's pixel size and translate so the rendered
    // glyphs sit centred over the disc.
    let layout = pango::Layout::new(pango_ctx);
    layout.set_text(&t.value.to_string());
    let mut desc = pango::FontDescription::from_string("Sans Bold");
    desc.set_size(((t.radius * 1.2) * pango::SCALE as f64) as i32);
    layout.set_font_description(Some(&desc));
    let (lw, lh) = layout.pixel_size();
    snap.save();
    snap.translate(&graphene::Point::new(
        cx - lw as f32 / 2.0,
        cy - lh as f32 / 2.0,
    ));
    snap.append_layout(&layout, &rgba(t.text_color));
    snap.restore();
}

fn snapshot_pending(snap: &gtk4::Snapshot, p: &PendingStroke) {
    match p.kind {
        ToolKind::Rect => {
            let r = drag_rect(p.from, p.to);
            snap.append_stroke(
                &rect_path(&r),
                &dashed_stroke(2.0, &[6.0, 4.0]),
                &rgba([1.0, 0.0, 0.0, 0.85]),
            );
        }
        ToolKind::Arrow => {
            let color = rgba([1.0, 0.0, 0.0, 0.85]);
            snap.append_stroke(&line_path(p.from, p.to), &solid_stroke(3.0), &color);
            snap.append_fill(
                &arrowhead_path(p.from, p.to, 15.0),
                gsk::FillRule::Winding,
                &color,
            );
        }
        ToolKind::Highlight => {
            let r = drag_rect(p.from, p.to);
            snap.append_color(&rgba([1.0, 1.0, 0.0, 0.35]), &rect_to_graphene(&r));
        }
        ToolKind::Redact => {
            let r = drag_rect(p.from, p.to);
            snap.append_color(&rgba([0.0, 0.0, 0.0, 0.85]), &rect_to_graphene(&r));
        }
        ToolKind::Freehand => {
            if let Some(path) = polyline_path(&p.points) {
                snap.append_stroke(&path, &solid_stroke(3.0), &rgba([1.0, 0.0, 0.0, 0.85]));
            }
        }
        ToolKind::Crop => {
            let r = drag_rect(p.from, p.to);
            snap.append_stroke(
                &rect_path(&r),
                &dashed_stroke(1.5, &[8.0, 4.0]),
                &rgba([1.0, 1.0, 1.0, 0.9]),
            );
        }
        ToolKind::Number | ToolKind::Text | ToolKind::Blur => {}
    }
}

fn snapshot_crop_veil(snap: &gtk4::Snapshot, doc_size: (u32, u32), crop: Rect) {
    // Even-odd fill on a path with the document rect AND the crop rect: outside is filled with
    // a translucent black veil so the user sees what gets exported.
    let pb = gsk::PathBuilder::new();
    pb.add_rect(&graphene::Rect::new(
        0.0,
        0.0,
        doc_size.0 as f32,
        doc_size.1 as f32,
    ));
    pb.add_rect(&rect_to_graphene(&crop));
    snap.append_fill(
        &pb.to_path(),
        gsk::FillRule::EvenOdd,
        &rgba([0.0, 0.0, 0.0, 0.35]),
    );
    // Dashed border around the crop rect.
    snap.append_stroke(
        &rect_path(&crop),
        &dashed_stroke(1.0, &[6.0, 4.0]),
        &rgba([1.0, 1.0, 1.0, 0.9]),
    );
}

// ---------------------------------------------------------------------------
// Cairo off-screen composition (PNG export)
// ---------------------------------------------------------------------------

/// Render `doc` into a freshly-allocated `CapturedImage` in BGRA (Cairo ARGB32 byte order).
/// Cairo is the right tool here: we're producing a pixel buffer for `output::encode_png`, and
/// keeping it CPU-side avoids round-tripping through `GskRenderer::render_texture`.
///
/// When `doc.crop` is set the output is the cropped region only — annotation coordinates are
/// preserved by translating the cairo origin so clipped tools still render correctly.
fn compose_document(doc: &Document) -> Result<CapturedImage> {
    let crop = doc.bounds();
    let (w, h) = (crop.w, crop.h);
    if w == 0 || h == 0 {
        return Err(anyhow!("cannot compose empty document"));
    }
    let surface = cairo::ImageSurface::create(cairo::Format::ARgb32, w as i32, h as i32)
        .map_err(|e| anyhow!("creating composite surface: {e}"))?;
    {
        let cr = cairo::Context::new(&surface).map_err(|e| anyhow!("cairo context: {e}"))?;
        if doc.crop.is_some() {
            cr.translate(-(crop.x as f64), -(crop.y as f64));
        }
        if let Some(base) = &doc.base {
            let bs = build_base_cairo_surface(base)?;
            cr.set_source_surface(&bs, 0.0, 0.0)
                .map_err(|e| anyhow!("set_source_surface: {e}"))?;
            cr.paint().map_err(|e| anyhow!("paint base: {e}"))?;
        }
        for layer in &doc.layers {
            cairo_draw_tool(layer.as_ref(), &cr);
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

/// Build an ARgb32 cairo surface from the document's RGBA base pixels, swizzling channels and
/// handling stride padding. Only allocated when composing for PNG export.
fn build_base_cairo_surface(base: &crate::annotate::DocumentBase) -> Result<cairo::ImageSurface> {
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

fn cairo_rgba(c: [f32; 4]) -> [f64; 4] {
    [c[0] as f64, c[1] as f64, c[2] as f64, c[3] as f64]
}

fn cairo_draw_tool(tool: &dyn Tool, cr: &cairo::Context) {
    match tool.kind() {
        ToolKind::Rect => {
            if let Some(t) = tool.as_any().downcast_ref::<RectTool>() {
                cairo_rect_outline(cr, &t.bounds, cairo_rgba(t.stroke), t.stroke_width as f64);
            }
        }
        ToolKind::Arrow => {
            if let Some(t) = tool.as_any().downcast_ref::<ArrowTool>() {
                cairo_arrow(
                    cr,
                    t.from,
                    t.to,
                    cairo_rgba(t.stroke),
                    t.stroke_width as f64,
                );
            }
        }
        ToolKind::Highlight => {
            if let Some(t) = tool.as_any().downcast_ref::<HighlightTool>() {
                cairo_filled_rect(cr, &t.bounds, cairo_rgba(t.color));
            }
        }
        ToolKind::Freehand => {
            if let Some(t) = tool.as_any().downcast_ref::<FreehandTool>() {
                cairo_polyline(cr, &t.points, cairo_rgba(t.stroke), t.stroke_width as f64);
            }
        }
        ToolKind::Redact => {
            if let Some(t) = tool.as_any().downcast_ref::<RedactTool>() {
                cairo_filled_rect(cr, &t.bounds, [0.0, 0.0, 0.0, 1.0]);
            }
        }
        ToolKind::Number => {
            if let Some(t) = tool.as_any().downcast_ref::<NumberTool>() {
                cairo_number(cr, t);
            }
        }
        ToolKind::Crop | ToolKind::Text | ToolKind::Blur => {}
    }
}

fn cairo_rect_outline(cr: &cairo::Context, rect: &Rect, rgba: [f64; 4], width: f64) {
    cr.set_source_rgba(rgba[0], rgba[1], rgba[2], rgba[3]);
    cr.set_line_width(width);
    cr.rectangle(rect.x as f64, rect.y as f64, rect.w as f64, rect.h as f64);
    let _ = cr.stroke();
}

fn cairo_filled_rect(cr: &cairo::Context, rect: &Rect, rgba: [f64; 4]) {
    cr.set_source_rgba(rgba[0], rgba[1], rgba[2], rgba[3]);
    cr.rectangle(rect.x as f64, rect.y as f64, rect.w as f64, rect.h as f64);
    let _ = cr.fill();
}

fn cairo_arrow(cr: &cairo::Context, from: (f64, f64), to: (f64, f64), rgba: [f64; 4], width: f64) {
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

fn cairo_polyline(cr: &cairo::Context, points: &[(f64, f64)], rgba: [f64; 4], width: f64) {
    if points.is_empty() {
        return;
    }
    cr.set_source_rgba(rgba[0], rgba[1], rgba[2], rgba[3]);
    cr.set_line_width(width);
    cr.set_line_cap(cairo::LineCap::Round);
    cr.set_line_join(cairo::LineJoin::Round);
    let (x0, y0) = points[0];
    cr.move_to(x0, y0);
    if points.len() == 1 {
        cr.line_to(x0 + 0.01, y0);
    } else {
        for &(x, y) in &points[1..] {
            cr.line_to(x, y);
        }
    }
    let _ = cr.stroke();
}

fn cairo_number(cr: &cairo::Context, t: &NumberTool) {
    let (cx, cy) = t.center;
    cr.set_source_rgba(
        t.fill[0] as f64,
        t.fill[1] as f64,
        t.fill[2] as f64,
        t.fill[3] as f64,
    );
    cr.arc(cx, cy, t.radius, 0.0, std::f64::consts::TAU);
    let _ = cr.fill();
    let label = t.value.to_string();
    cr.set_source_rgba(
        t.text_color[0] as f64,
        t.text_color[1] as f64,
        t.text_color[2] as f64,
        t.text_color[3] as f64,
    );
    cr.select_font_face("sans", cairo::FontSlant::Normal, cairo::FontWeight::Bold);
    cr.set_font_size(t.radius * 1.2);
    if let Ok(ext) = cr.text_extents(&label) {
        let tx = cx - ext.width() / 2.0 - ext.x_bearing();
        let ty = cy - ext.height() / 2.0 - ext.y_bearing();
        cr.move_to(tx, ty);
        let _ = cr.show_text(&label);
    }
}

/// A drag-in-progress preview that hasn't been committed to the document yet.
#[derive(Clone, Debug)]
pub struct PendingStroke {
    kind: ToolKind,
    from: (f64, f64),
    to: (f64, f64),
    /// Populated for Freehand; empty for two-point tools.
    points: Vec<(f64, f64)>,
}

mod imp {
    use super::*;

    pub struct AnnotationCanvas {
        pub doc: RefCell<Option<Rc<RefCell<Document>>>>,
        pub base_texture: RefCell<Option<gdk::MemoryTexture>>,
        pub current_tool: Cell<ToolKind>,
        pub pending: RefCell<Option<PendingStroke>>,
        /// Auto-increment counter used by the Number tool. Resets when a new document is loaded.
        pub next_number: Cell<u32>,
        /// When true, baseless documents render with a transparent background instead of the
        /// editor's dark fill — used by the live overlay so strokes float over the desktop.
        pub transparent_background: Cell<bool>,
    }

    impl Default for AnnotationCanvas {
        fn default() -> Self {
            Self {
                doc: RefCell::new(None),
                base_texture: RefCell::new(None),
                current_tool: Cell::new(ToolKind::Rect),
                pending: RefCell::new(None),
                next_number: Cell::new(1),
                transparent_background: Cell::new(false),
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
            install_click(&obj);
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
            let bounds = graphene::Rect::new(0.0, 0.0, width, height);

            // Background: neutral dark fill for baseless documents in the editor; the base
            // texture (uploaded once via GdkMemoryTexture) for loaded documents; nothing at
            // all for the transparent overlay path.
            if doc.base.is_none() {
                if !self.transparent_background.get() {
                    snapshot.append_color(&gdk::RGBA::new(0.1, 0.1, 0.12, 1.0), &bounds);
                }
            } else if let Some(tex) = self.base_texture.borrow().as_ref() {
                snapshot.append_texture(tex, &bounds);
            }

            let pango_ctx = self.obj().create_pango_context();
            for layer in &doc.layers {
                snapshot_tool(snapshot, layer.as_ref(), &pango_ctx);
            }
            if let Some(p) = self.pending.borrow().as_ref() {
                snapshot_pending(snapshot, p);
            }
            if let Some(c) = doc.crop {
                snapshot_crop_veil(snapshot, doc.size, c);
            }
        }
    }
}

fn install_drag(canvas: &AnnotationCanvas) {
    let drag = gtk4::GestureDrag::new();

    {
        let weak = canvas.downgrade();
        drag.connect_drag_begin(move |_, x, y| {
            let Some(c) = weak.upgrade() else { return };
            let kind = c.tool();
            // Number is click-driven, not drag-driven; ignore drag-begin for it.
            if matches!(kind, ToolKind::Number | ToolKind::Text | ToolKind::Blur) {
                return;
            }
            let points = if matches!(kind, ToolKind::Freehand) {
                vec![(x, y)]
            } else {
                Vec::new()
            };
            c.imp().pending.replace(Some(PendingStroke {
                kind,
                from: (x, y),
                to: (x, y),
                points,
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
                if matches!(stroke.kind, ToolKind::Freehand) {
                    stroke.points.push(stroke.to);
                }
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
                ToolKind::Highlight => {
                    let r = drag_rect(stroke.from, stroke.to);
                    if r.w >= 2 && r.h >= 2 {
                        doc.push_layer(Box::new(HighlightTool {
                            bounds: r,
                            color: [1.0, 1.0, 0.0, 0.35],
                        }));
                    }
                }
                ToolKind::Redact => {
                    let r = drag_rect(stroke.from, stroke.to);
                    if r.w >= 2 && r.h >= 2 {
                        doc.push_layer(Box::new(RedactTool { bounds: r }));
                    }
                }
                ToolKind::Freehand => {
                    if stroke.points.len() >= 2 {
                        doc.push_layer(Box::new(FreehandTool {
                            points: stroke.points,
                            stroke: [1.0, 0.0, 0.0, 1.0],
                            stroke_width: 3.0,
                        }));
                    }
                }
                ToolKind::Crop => {
                    let r = drag_rect(stroke.from, stroke.to);
                    // Clamp to the document so a stray over-drag doesn't crop to an off-canvas
                    // rect (which would produce empty rows in compose_document).
                    let clamped = clamp_to_doc(r, doc.size);
                    if clamped.w >= 2 && clamped.h >= 2 {
                        doc.crop = Some(clamped);
                    }
                }
                ToolKind::Number | ToolKind::Text | ToolKind::Blur => {}
            }
            let resize = matches!(stroke.kind, ToolKind::Crop);
            drop(doc);
            if resize {
                c.queue_resize();
            }
            c.queue_draw();
        });
    }
    canvas.add_controller(drag);
}

fn install_click(canvas: &AnnotationCanvas) {
    let click = gtk4::GestureClick::new();
    click.set_button(gdk4::BUTTON_PRIMARY);
    let weak = canvas.downgrade();
    click.connect_released(move |_, n_press, x, y| {
        // Only react to single-clicks; doubles would otherwise drop two numbers on the same spot.
        if n_press != 1 {
            return;
        }
        let Some(c) = weak.upgrade() else { return };
        if !matches!(c.tool(), ToolKind::Number) {
            return;
        }
        let Some(doc_rc) = c.imp().doc.borrow().clone() else {
            return;
        };
        let value = c.imp().next_number.get();
        c.imp().next_number.set(value + 1);
        doc_rc.borrow_mut().push_layer(Box::new(NumberTool {
            center: (x, y),
            radius: 18.0,
            value,
            fill: [0.9, 0.1, 0.1, 1.0],
            text_color: [1.0, 1.0, 1.0, 1.0],
        }));
        c.queue_draw();
    });
    canvas.add_controller(click);
}

fn clamp_to_doc(r: Rect, size: (u32, u32)) -> Rect {
    let x0 = r.x.max(0);
    let y0 = r.y.max(0);
    let x1 = (r.right()).min(size.0 as i32).max(x0);
    let y1 = (r.bottom()).min(size.1 as i32).max(y0);
    Rect {
        x: x0,
        y: y0,
        w: (x1 - x0) as u32,
        h: (y1 - y0) as u32,
    }
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

    #[test]
    fn compose_document_honours_crop() {
        let mut doc = Document::with_base(solid_base(10, 8));
        doc.crop = Some(Rect {
            x: 2,
            y: 1,
            w: 5,
            h: 4,
        });
        let img = compose_document(&doc).expect("compose");
        assert_eq!(img.width, 5);
        assert_eq!(img.height, 4);
    }

    #[test]
    fn clamp_to_doc_constrains_overflow() {
        let r = Rect {
            x: -3,
            y: -1,
            w: 100,
            h: 100,
        };
        let c = clamp_to_doc(r, (20, 10));
        assert_eq!(c.x, 0);
        assert_eq!(c.y, 0);
        assert_eq!(c.w, 20);
        assert_eq!(c.h, 10);
    }
}
