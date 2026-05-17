//! Annotation canvas — a `GtkWidget` subclass that draws a [`Document`] via GSK render nodes.
//!
//! The on-screen `snapshot()` path builds `gsk::Path` / `gsk::Stroke` nodes and pushes them
//! onto the [`gtk4::Snapshot`] so GTK's GL renderer can rasterise everything on the GPU.
//! [`AnnotationCanvas::compose_png`] uses the same render-node tree: it renders into a
//! `gdk::Texture` through the widget's native [`gsk::Renderer`], then downloads pixels via
//! [`gdk::TextureDownloader`]. One rendering codepath, identical pixels on-screen and on disk.

use std::cell::{Cell, RefCell};
use std::collections::HashMap;
use std::rc::Rc;

use anyhow::{Result, anyhow};
use gtk4::gdk;
use gtk4::glib;
use gtk4::graphene;
use gtk4::gsk;
use gtk4::pango;
use gtk4::prelude::*;
use gtk4::subclass::prelude::*;

use crate::annotate::render::{arrowhead, drag_rect};
use crate::annotate::tools::arrow::ArrowTool;
use crate::annotate::tools::blur::BlurTool;
use crate::annotate::tools::ellipse::EllipseTool;
use crate::annotate::tools::freehand::FreehandTool;
use crate::annotate::tools::highlight::HighlightTool;
use crate::annotate::tools::line::LineTool;
use crate::annotate::tools::number::NumberTool;
use crate::annotate::tools::rect::RectTool;
use crate::annotate::tools::redact::RedactTool;
use crate::annotate::tools::text::TextTool;
use crate::annotate::{Document, DocumentBase, StrokeStyle, Tool, ToolKind};
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

    /// Color the given tool should use when the next layer of that kind is committed.
    /// Returns `None` for tools that have no user-facing color (Blur, Crop, Redact —
    /// their appearance is hardcoded).
    pub fn tool_color(&self, kind: ToolKind) -> Option<[f32; 4]> {
        self.imp().tool_colors.borrow().get(&kind).copied()
    }

    /// Override the color stored for `kind`. Has no effect on already-committed layers;
    /// only affects subsequent drags / clicks that produce a fresh tool instance.
    pub fn set_tool_color(&self, kind: ToolKind, color: [f32; 4]) {
        self.imp().tool_colors.borrow_mut().insert(kind, color);
    }

    /// Stroke dash style the given tool should use on the next drag commit. Returns
    /// `None` for tools whose appearance isn't outline-driven (Highlight, Number,
    /// Text, Blur, Crop, Redact) so the toolbar can disable its style picker for them.
    pub fn tool_style(&self, kind: ToolKind) -> Option<StrokeStyle> {
        self.imp().tool_styles.borrow().get(&kind).copied()
    }

    /// Override the stroke style stored for `kind`. Same persistence model as
    /// [`Self::set_tool_color`] — affects future drags only, not committed layers.
    pub fn set_tool_style(&self, kind: ToolKind, style: StrokeStyle) {
        self.imp().tool_styles.borrow_mut().insert(kind, style);
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
    /// `output::encode_png`). The render-node tree mirrors what `snapshot()` builds for the
    /// on-screen path; we just route it through `gsk::Renderer::render_texture` and download
    /// the resulting `gdk::Texture` as BGRA bytes via `gdk::TextureDownloader`.
    ///
    /// Must be called after the canvas widget has been realised inside a `gtk4::Native` (i.e.
    /// from inside the editor window's GTK callbacks) so we can borrow its `gsk::Renderer`.
    pub fn compose(&self) -> Result<CapturedImage> {
        let Some(doc_rc) = self.imp().doc.borrow().clone() else {
            return Err(anyhow!("no document loaded"));
        };
        let doc = doc_rc.borrow();
        let crop = doc.bounds();
        let (w, h) = (crop.w, crop.h);
        if w == 0 || h == 0 {
            return Err(anyhow!("cannot compose empty document"));
        }

        // Build the render tree against the *document* coordinate space (tools store their
        // absolute coords), then ask `render_texture` to clip to the crop viewport. This
        // matches the on-screen snapshot exactly minus the editor's neutral background fill
        // and selection veil — neither belongs in the exported PNG.
        let snap = gtk4::Snapshot::new();
        let doc_bounds = graphene::Rect::new(0.0, 0.0, doc.size.0 as f32, doc.size.1 as f32);
        if let Some(tex) = self.imp().base_texture.borrow().as_ref() {
            snap.append_texture(tex, &doc_bounds);
        }
        let pango_ctx = self.create_pango_context();
        for layer in &doc.layers {
            snapshot_tool(&snap, layer.as_ref(), &pango_ctx, doc.base.as_ref());
        }

        let node = snap
            .to_node()
            .ok_or_else(|| anyhow!("nothing to compose: document has no base and no layers"))?;

        // `Native::renderer()` returns the renderer GTK already realised for the editor's
        // surface, so we don't have to instantiate (and realise) a fresh one per save.
        let renderer = self.native().and_then(|n| n.renderer()).ok_or_else(|| {
            anyhow!("canvas has no native renderer; compose must run from inside a realised editor")
        })?;
        let viewport = graphene::Rect::new(crop.x as f32, crop.y as f32, w as f32, h as f32);
        let texture = renderer.render_texture(node, Some(&viewport));

        // Force BGRA8 (premultiplied) so the byte layout matches what `output::encode_png`
        // already expects — its R<->B swizzle would otherwise produce a colour-swapped PNG.
        let mut downloader = gdk::TextureDownloader::new(&texture);
        downloader.set_format(gdk::MemoryFormat::B8g8r8a8Premultiplied);
        let (bytes, stride) = downloader.download_bytes();
        let pixels: std::sync::Arc<[u8]> = std::sync::Arc::from(bytes.to_vec().into_boxed_slice());
        Ok(CapturedImage {
            width: w,
            height: h,
            stride: stride as u32,
            pixels,
            source: None,
        })
    }

    /// Convenience: compose + PNG-encode the document in one go using the supplied
    /// compression preset.
    pub fn compose_png(&self, compression: crate::config::PngCompression) -> Result<Vec<u8>> {
        let img = self.compose()?;
        crate::output::encode_png(&img, compression)
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

/// Path builder helper for an outlined ellipse inscribed in `r`. GSK exposes
/// `add_circle` but no native ellipse, so we stitch one out of four cubic Bézier arcs
/// using the standard 0.5522847 corner constant — accurate to ~0.027 % of the radius.
fn ellipse_path(r: &Rect) -> gsk::Path {
    const K: f32 = 0.552_284_8;
    let cx = r.x as f32 + r.w as f32 / 2.0;
    let cy = r.y as f32 + r.h as f32 / 2.0;
    let rx = r.w as f32 / 2.0;
    let ry = r.h as f32 / 2.0;
    let ox = rx * K;
    let oy = ry * K;
    let pb = gsk::PathBuilder::new();
    pb.move_to(cx + rx, cy);
    pb.cubic_to(cx + rx, cy + oy, cx + ox, cy + ry, cx, cy + ry);
    pb.cubic_to(cx - ox, cy + ry, cx - rx, cy + oy, cx - rx, cy);
    pb.cubic_to(cx - rx, cy - oy, cx - ox, cy - ry, cx, cy - ry);
    pb.cubic_to(cx + ox, cy - ry, cx + rx, cy - oy, cx + rx, cy);
    pb.close();
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

/// Build a `gsk::Stroke` honouring a [`StrokeStyle`].
///
/// * `Solid` skips dashing entirely.
/// * `Dashed` uses width-relative on/off lengths (3w on, 2w off) so thick lines stay
///   visually proportional rather than turning into a continuous line.
/// * `Dotted` uses a `[0.0, gap]` dash pattern combined with the underlying round line
///   cap: a 0-length segment with round caps rasterises as a single round dot, repeated
///   every `gap`. The cap radius equals half the stroke width, so dot-to-dot spacing of
///   `2w` reads as roughly equal-sized dots and gaps.
fn styled_stroke(width: f64, style: StrokeStyle) -> gsk::Stroke {
    let s = solid_stroke(width);
    match style {
        StrokeStyle::Solid => {}
        StrokeStyle::Dashed => {
            let w = width as f32;
            s.set_dash(&[w * 3.0, w * 2.0]);
        }
        StrokeStyle::Dotted => {
            let w = width as f32;
            s.set_dash(&[0.0, w * 2.0]);
        }
    }
    s
}

/// Append a committed [`Tool`] layer to the snapshot.
fn snapshot_tool(
    snap: &gtk4::Snapshot,
    tool: &dyn Tool,
    pango_ctx: &pango::Context,
    base: Option<&DocumentBase>,
) {
    match tool.kind() {
        ToolKind::Rect => {
            if let Some(t) = tool.as_any().downcast_ref::<RectTool>() {
                snap.append_stroke(
                    &rect_path(&t.bounds),
                    &styled_stroke(t.stroke_width as f64, t.stroke_style),
                    &rgba(t.stroke),
                );
            }
        }
        ToolKind::Ellipse => {
            if let Some(t) = tool.as_any().downcast_ref::<EllipseTool>() {
                snap.append_stroke(
                    &ellipse_path(&t.bounds),
                    &styled_stroke(t.stroke_width as f64, t.stroke_style),
                    &rgba(t.stroke),
                );
            }
        }
        ToolKind::Arrow => {
            if let Some(t) = tool.as_any().downcast_ref::<ArrowTool>() {
                let color = rgba(t.stroke);
                // Only the shaft honours `stroke_style`; the arrowhead always renders solid
                // so a dashed/dotted arrow still terminates in a recognisable pointer.
                snap.append_stroke(
                    &line_path(t.from, t.to),
                    &styled_stroke(t.stroke_width as f64, t.stroke_style),
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
        ToolKind::Line => {
            if let Some(t) = tool.as_any().downcast_ref::<LineTool>() {
                snap.append_stroke(
                    &line_path(t.from, t.to),
                    &styled_stroke(t.stroke_width as f64, t.stroke_style),
                    &rgba(t.stroke),
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
                    &styled_stroke(t.stroke_width as f64, t.stroke_style),
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
        ToolKind::Text => {
            if let Some(t) = tool.as_any().downcast_ref::<TextTool>() {
                snapshot_text(snap, t, pango_ctx);
            }
        }
        ToolKind::Blur => {
            if let Some(t) = tool.as_any().downcast_ref::<BlurTool>() {
                snapshot_blur(snap, t, base);
            }
        }
        // `Crop` is applied at compose time via `doc.crop`; no on-screen layer rendering.
        ToolKind::Crop => {}
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

fn text_layout(t: &TextTool, pango_ctx: &pango::Context) -> pango::Layout {
    let layout = pango::Layout::new(pango_ctx);
    layout.set_text(&t.text);
    let mut desc = pango::FontDescription::from_string("Sans");
    desc.set_size((t.size_pt as f64 * pango::SCALE as f64) as i32);
    layout.set_font_description(Some(&desc));
    layout
}

fn snapshot_text(snap: &gtk4::Snapshot, t: &TextTool, pango_ctx: &pango::Context) {
    let layout = text_layout(t, pango_ctx);
    snap.save();
    snap.translate(&graphene::Point::new(t.origin.0 as f32, t.origin.1 as f32));
    snap.append_layout(&layout, &rgba(t.color));
    snap.restore();
}

/// Blur the base image inside the tool's bounds, leaving other layers untouched. The push_blur
/// node applies a Gaussian blur to everything drawn in the active subtree; we wrap that subtree
/// in a clip rect so only pixels inside `t.bounds` are affected.
fn snapshot_blur(snap: &gtk4::Snapshot, t: &BlurTool, base: Option<&DocumentBase>) {
    let Some(base) = base else {
        // Fall back to a translucent grey rectangle when there's no base to blur (overlay path).
        snap.append_color(&rgba([0.5, 0.5, 0.5, 0.45]), &rect_to_graphene(&t.bounds));
        return;
    };
    let Ok(tex) = build_base_texture(base) else {
        return;
    };
    let clip = rect_to_graphene(&t.bounds);
    let full = graphene::Rect::new(0.0, 0.0, base.width as f32, base.height as f32);
    snap.push_clip(&clip);
    snap.push_blur(t.radius as f64);
    snap.append_texture(&tex, &full);
    snap.pop(); // blur
    snap.pop(); // clip
}

fn snapshot_pending(snap: &gtk4::Snapshot, p: &PendingStroke) {
    match p.kind {
        ToolKind::Rect => {
            let r = drag_rect(p.from, p.to);
            // Drag previews always use a marquee-style dash so the user can distinguish a
            // live drag from a committed layer regardless of the tool's chosen style.
            snap.append_stroke(
                &rect_path(&r),
                &dashed_stroke(2.0, &[6.0, 4.0]),
                &rgba(p.color),
            );
        }
        ToolKind::Ellipse => {
            let r = drag_rect(p.from, p.to);
            snap.append_stroke(
                &ellipse_path(&r),
                &dashed_stroke(2.0, &[6.0, 4.0]),
                &rgba(p.color),
            );
        }
        ToolKind::Arrow => {
            let color = rgba(p.color);
            snap.append_stroke(
                &line_path(p.from, p.to),
                &styled_stroke(3.0, p.style),
                &color,
            );
            snap.append_fill(
                &arrowhead_path(p.from, p.to, 15.0),
                gsk::FillRule::Winding,
                &color,
            );
        }
        ToolKind::Line => {
            snap.append_stroke(
                &line_path(p.from, p.to),
                &styled_stroke(3.0, p.style),
                &rgba(p.color),
            );
        }
        ToolKind::Highlight => {
            let r = drag_rect(p.from, p.to);
            snap.append_color(&rgba(p.color), &rect_to_graphene(&r));
        }
        ToolKind::Redact => {
            let r = drag_rect(p.from, p.to);
            snap.append_color(&rgba(p.color), &rect_to_graphene(&r));
        }
        ToolKind::Freehand => {
            if let Some(path) = polyline_path(&p.points) {
                snap.append_stroke(&path, &styled_stroke(3.0, p.style), &rgba(p.color));
            }
        }
        ToolKind::Crop => {
            let r = drag_rect(p.from, p.to);
            snap.append_stroke(
                &rect_path(&r),
                &dashed_stroke(1.5, &[8.0, 4.0]),
                &rgba(p.color),
            );
        }
        ToolKind::Blur => {
            // Dashed outline with a faint fill — actually applying the blur every drag-update
            // would re-upload the texture every frame, so we settle for a marker preview and
            // commit the real blur on drag-end.
            let r = drag_rect(p.from, p.to);
            snap.append_color(&rgba(p.color), &rect_to_graphene(&r));
            snap.append_stroke(
                &rect_path(&r),
                &dashed_stroke(1.0, &[4.0, 3.0]),
                &rgba([1.0, 1.0, 1.0, 0.9]),
            );
        }
        ToolKind::Number | ToolKind::Text => {}
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

/// A drag-in-progress preview that hasn't been committed to the document yet.
#[derive(Clone, Debug)]
pub struct PendingStroke {
    kind: ToolKind,
    from: (f64, f64),
    to: (f64, f64),
    /// Populated for Freehand; empty for two-point tools.
    points: Vec<(f64, f64)>,
    /// Color the preview should render in. Mirrors the color the committed layer will
    /// receive at drag-end; populated from [`AnnotationCanvas::tool_color`] at drag-begin
    /// (with a kind-specific fallback for colorless tools).
    color: [f32; 4],
    /// Stroke dash style the preview (and committed layer) should use, seeded from
    /// [`AnnotationCanvas::tool_style`] at drag-begin. Defaults to `Solid` for tools
    /// that aren't styleable; the preview itself uses a fixed marquee dash for the
    /// bounded shape tools and only honours this for line-like tools.
    style: StrokeStyle,
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
        /// Per-tool color overrides driven by the toolbar's color picker. Keys are only
        /// inserted for tools whose appearance is user-controlled — Blur, Crop and Redact
        /// stay hardcoded so the picker can disable itself for them.
        pub tool_colors: RefCell<HashMap<ToolKind, [f32; 4]>>,
        /// Per-tool stroke style overrides driven by the toolbar's style picker. Same
        /// model as `tool_colors`: only line-rendering tools (Rect/Ellipse/Arrow/Line/
        /// Freehand) get entries; everything else stays implicit-Solid.
        pub tool_styles: RefCell<HashMap<ToolKind, StrokeStyle>>,
    }

    impl Default for AnnotationCanvas {
        fn default() -> Self {
            let mut colors = HashMap::new();
            colors.insert(ToolKind::Rect, [1.0, 0.0, 0.0, 1.0]);
            colors.insert(ToolKind::Ellipse, [1.0, 0.0, 0.0, 1.0]);
            colors.insert(ToolKind::Arrow, [1.0, 0.0, 0.0, 1.0]);
            colors.insert(ToolKind::Line, [1.0, 0.0, 0.0, 1.0]);
            colors.insert(ToolKind::Freehand, [1.0, 0.0, 0.0, 1.0]);
            colors.insert(ToolKind::Highlight, [1.0, 1.0, 0.0, 0.35]);
            colors.insert(ToolKind::Number, [0.9, 0.1, 0.1, 1.0]);
            colors.insert(ToolKind::Text, [1.0, 0.95, 0.2, 1.0]);
            let mut styles = HashMap::new();
            styles.insert(ToolKind::Rect, StrokeStyle::Solid);
            styles.insert(ToolKind::Ellipse, StrokeStyle::Solid);
            styles.insert(ToolKind::Arrow, StrokeStyle::Solid);
            styles.insert(ToolKind::Line, StrokeStyle::Solid);
            styles.insert(ToolKind::Freehand, StrokeStyle::Solid);
            Self {
                doc: RefCell::new(None),
                base_texture: RefCell::new(None),
                current_tool: Cell::new(ToolKind::Rect),
                pending: RefCell::new(None),
                next_number: Cell::new(1),
                transparent_background: Cell::new(false),
                tool_colors: RefCell::new(colors),
                tool_styles: RefCell::new(styles),
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
            let base = doc.base.clone();
            for layer in &doc.layers {
                snapshot_tool(snapshot, layer.as_ref(), &pango_ctx, base.as_ref());
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
            // Number/Text are click-driven, not drag-driven; ignore drag-begin for them.
            if matches!(kind, ToolKind::Number | ToolKind::Text) {
                return;
            }
            let points = if matches!(kind, ToolKind::Freehand) {
                vec![(x, y)]
            } else {
                Vec::new()
            };
            // For colorable tools the user's picker choice drives the preview; for the
            // hardcoded-appearance tools (Blur/Crop/Redact) we fall back to a sensible
            // preview color so `snapshot_pending` doesn't need a special case.
            let color = c.tool_color(kind).unwrap_or(match kind {
                ToolKind::Redact => [0.0, 0.0, 0.0, 0.85],
                ToolKind::Crop => [1.0, 1.0, 1.0, 0.9],
                ToolKind::Blur => [0.5, 0.5, 0.5, 0.25],
                _ => [1.0, 0.0, 0.0, 0.85],
            });
            let style = c.tool_style(kind).unwrap_or_default();
            c.imp().pending.replace(Some(PendingStroke {
                kind,
                from: (x, y),
                to: (x, y),
                points,
                color,
                style,
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
                        let mut t = RectTool::new(r);
                        if let Some(color) = c.tool_color(ToolKind::Rect) {
                            t.stroke = color;
                        }
                        t.stroke_style = stroke.style;
                        doc.push_layer(Box::new(t));
                    }
                }
                ToolKind::Ellipse => {
                    let r = drag_rect(stroke.from, stroke.to);
                    if r.w >= 2 && r.h >= 2 {
                        let mut t = EllipseTool::new(r);
                        if let Some(color) = c.tool_color(ToolKind::Ellipse) {
                            t.stroke = color;
                        }
                        t.stroke_style = stroke.style;
                        doc.push_layer(Box::new(t));
                    }
                }
                ToolKind::Arrow => {
                    let dx = stroke.to.0 - stroke.from.0;
                    let dy = stroke.to.1 - stroke.from.1;
                    if (dx * dx + dy * dy) >= 16.0 {
                        let mut t = ArrowTool::new(stroke.from, stroke.to);
                        if let Some(color) = c.tool_color(ToolKind::Arrow) {
                            t.stroke = color;
                        }
                        t.stroke_style = stroke.style;
                        doc.push_layer(Box::new(t));
                    }
                }
                ToolKind::Line => {
                    let dx = stroke.to.0 - stroke.from.0;
                    let dy = stroke.to.1 - stroke.from.1;
                    if (dx * dx + dy * dy) >= 16.0 {
                        let mut t = LineTool::new(stroke.from, stroke.to);
                        if let Some(color) = c.tool_color(ToolKind::Line) {
                            t.stroke = color;
                        }
                        t.stroke_style = stroke.style;
                        doc.push_layer(Box::new(t));
                    }
                }
                ToolKind::Highlight => {
                    let r = drag_rect(stroke.from, stroke.to);
                    if r.w >= 2 && r.h >= 2 {
                        let color = c
                            .tool_color(ToolKind::Highlight)
                            .unwrap_or([1.0, 1.0, 0.0, 0.35]);
                        doc.push_layer(Box::new(HighlightTool { bounds: r, color }));
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
                        let stroke_color = c
                            .tool_color(ToolKind::Freehand)
                            .unwrap_or([1.0, 0.0, 0.0, 1.0]);
                        doc.push_layer(Box::new(FreehandTool {
                            points: stroke.points,
                            stroke: stroke_color,
                            stroke_width: 3.0,
                            stroke_style: stroke.style,
                        }));
                    }
                }
                ToolKind::Crop => {
                    let r = drag_rect(stroke.from, stroke.to);
                    // Clamp to the document so a stray over-drag doesn't crop to an off-canvas
                    // rect (which would produce empty rows when compose() renders the viewport).
                    let clamped = clamp_to_doc(r, doc.size);
                    if clamped.w >= 2 && clamped.h >= 2 {
                        doc.crop = Some(clamped);
                    }
                }
                ToolKind::Blur => {
                    let r = drag_rect(stroke.from, stroke.to);
                    let clamped = clamp_to_doc(r, doc.size);
                    if clamped.w >= 2 && clamped.h >= 2 {
                        doc.push_layer(Box::new(BlurTool {
                            bounds: clamped,
                            radius: 12.0,
                        }));
                    }
                }
                ToolKind::Number | ToolKind::Text => {}
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
        match c.tool() {
            ToolKind::Number => place_number(&c, x, y),
            ToolKind::Text => prompt_text(&c, x, y),
            _ => {}
        }
    });
    canvas.add_controller(click);
}

fn place_number(c: &AnnotationCanvas, x: f64, y: f64) {
    let Some(doc_rc) = c.imp().doc.borrow().clone() else {
        return;
    };
    let value = c.imp().next_number.get();
    c.imp().next_number.set(value + 1);
    let fill = c
        .tool_color(ToolKind::Number)
        .unwrap_or([0.9, 0.1, 0.1, 1.0]);
    doc_rc.borrow_mut().push_layer(Box::new(NumberTool {
        center: (x, y),
        radius: 18.0,
        value,
        fill,
        // White stays hardcoded — the picker drives only the disc fill so digits keep their
        // contrast guarantee regardless of which color the user picks for the marker.
        text_color: [1.0, 1.0, 1.0, 1.0],
    }));
    c.queue_draw();
}

/// Pop an Entry popover anchored at `(x, y)` and commit a `TextTool` when the user presses
/// Enter (empty input cancels). The popover is unparented on close to keep GTK from leaking
/// the widget hierarchy.
fn prompt_text(canvas: &AnnotationCanvas, x: f64, y: f64) {
    let popover = gtk4::Popover::new();
    popover.set_parent(canvas);
    popover.set_autohide(true);
    let rect = gdk4::Rectangle::new(x as i32, y as i32, 1, 1);
    popover.set_pointing_to(Some(&rect));
    let entry = gtk4::Entry::new();
    entry.set_placeholder_text(Some("Text"));
    entry.set_width_chars(20);
    popover.set_child(Some(&entry));

    {
        let canvas_weak = canvas.downgrade();
        let popover_weak = popover.downgrade();
        entry.connect_activate(move |e| {
            let Some(c) = canvas_weak.upgrade() else {
                return;
            };
            let text = e.text().to_string();
            if !text.is_empty()
                && let Some(doc_rc) = c.imp().doc.borrow().clone()
            {
                let color = c
                    .tool_color(ToolKind::Text)
                    .unwrap_or([1.0, 0.95, 0.2, 1.0]);
                doc_rc.borrow_mut().push_layer(Box::new(TextTool {
                    origin: (x, y),
                    text,
                    size_pt: 18.0,
                    color,
                }));
                c.queue_draw();
            }
            if let Some(p) = popover_weak.upgrade() {
                p.popdown();
            }
        });
    }
    // GTK4 requires explicit unparent on transient popovers, otherwise the widget tree leaks.
    popover.connect_closed(|p| p.unparent());

    popover.popup();
    entry.grab_focus();
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
