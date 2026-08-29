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

use crate::annotate::render::{arrowhead, drag_rect, drag_square};
use crate::annotate::select::{
    self, BoxHandle, Endpoint, HANDLE_DRAW, box_handle_at, box_handle_point, box_handle_points,
    point_handle_hit,
};
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
use crate::capture::region::Rect;
use crate::capture::{CapturedImage, PixelFormat};

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
        imp.hidden_base.set(false);
        // Drop any in-progress text edit when the document changes; the new doc has its
        // own coordinate space and the in-flight buffer is meaningless against it. The
        // caret-blink timer self-terminates as soon as it sees `pending_text` is None.
        imp.pending_text.replace(None);
        imp.next_number.set(1);
        imp.selection.set(None);
        imp.manip.replace(None);
        imp.reedit_restore.replace(None);
        self.queue_resize();
        self.queue_draw();
    }

    /// Currently active tool used when the user starts a new drag.
    pub fn set_tool(&self, kind: ToolKind) {
        // Switching to any non-Text tool while a text edit is in progress commits whatever the
        // user has typed so far — a natural "I'm done with this label" gesture. This covers
        // both the Text tool and a re-edit started from Select mode (where `current_tool` is
        // already Select, so we key off the pending edit, not the previous tool).
        if kind != ToolKind::Text && self.imp().pending_text.borrow().is_some() {
            commit_pending_text(self);
        }
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

    /// Replace the built-in per-tool color defaults with values from the user's
    /// [`crate::config::AnnotateColors`] table. Called once per canvas right after
    /// construction so the toolbar's color picker (and any subsequent fresh strokes)
    /// pick up the configured colors. Tools without a user-controllable color
    /// (Blur / Crop / Redact) are untouched.
    pub fn apply_color_defaults(&self, colors: &crate::config::AnnotateColors) {
        let mut map = self.imp().tool_colors.borrow_mut();
        map.insert(ToolKind::Rect, colors.rect.to_f32_array());
        map.insert(ToolKind::Ellipse, colors.ellipse.to_f32_array());
        map.insert(ToolKind::Arrow, colors.arrow.to_f32_array());
        map.insert(ToolKind::Line, colors.line.to_f32_array());
        map.insert(ToolKind::Freehand, colors.freehand.to_f32_array());
        map.insert(ToolKind::Highlight, colors.highlight.to_f32_array());
        map.insert(ToolKind::Number, colors.number.to_f32_array());
        map.insert(ToolKind::Text, colors.text.to_f32_array());
    }

    /// Override the color stored for `kind`. Has no effect on already-committed layers;
    /// only affects subsequent drags / clicks that produce a fresh tool instance.
    /// When a text edit is currently in progress for this kind, the in-flight
    /// `PendingText` color is updated live so the user sees the new color immediately.
    pub fn set_tool_color(&self, kind: ToolKind, color: [f32; 4]) {
        self.imp().tool_colors.borrow_mut().insert(kind, color);
        if matches!(kind, ToolKind::Text)
            && let Some(pt) = self.imp().pending_text.borrow_mut().as_mut()
        {
            pt.color = color;
            self.queue_draw();
        }
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

    /// Font size (in points) for tools that render text. Returns `None` for tools whose
    /// appearance isn't text-based, so the toolbar can hide/disable its size picker
    /// for them.
    pub fn tool_font_size(&self, kind: ToolKind) -> Option<f32> {
        self.imp().tool_font_sizes.borrow().get(&kind).copied()
    }

    /// Override the font size for `kind`. If a text edit is currently in progress for
    /// this same kind, the in-flight `PendingText` is updated live so the user sees the
    /// new size immediately.
    pub fn set_tool_font_size(&self, kind: ToolKind, size: f32) {
        let size = size.max(1.0);
        self.imp().tool_font_sizes.borrow_mut().insert(kind, size);
        if matches!(kind, ToolKind::Text)
            && let Some(pt) = self.imp().pending_text.borrow_mut().as_mut()
        {
            pt.size_pt = size;
            self.queue_draw();
        }
    }

    /// Render with a transparent background when no base image is loaded. The annotation editor
    /// keeps the default (dark fill) so an unloaded canvas is visible, but the live overlay
    /// flips this on so clicks/strokes appear directly on top of the desktop.
    pub fn set_transparent(&self, transparent: bool) {
        self.imp().transparent_background.set(transparent);
        self.queue_draw();
    }

    /// Register a callback invoked when a drawing action finishes and the overlay should
    /// return to Select mode: a drag-end layer push, a Number placement, a Text commit, or a
    /// Text edit cancelled with Escape. Lets each tool act as a one-shot. Replaces any
    /// previous callback.
    pub fn set_on_commit<F: Fn() + 'static>(&self, f: F) {
        self.imp().on_commit.replace(Some(Rc::new(f)));
    }

    /// Register a callback invoked whenever the selection or text-edit state changes, so the
    /// overlay can recompute toolbar picker sensitivity. Replaces any previous callback.
    pub fn set_on_ui_state<F: Fn() + 'static>(&self, f: F) {
        self.imp().on_ui_state.replace(Some(Rc::new(f)));
    }

    /// `true` while an in-canvas text edit (new or re-edit) is active.
    pub fn is_editing_text(&self) -> bool {
        self.imp().pending_text.borrow().is_some()
    }

    /// [`ToolKind`] of the currently selected layer in Select mode, or `None` if nothing is
    /// selected. Used by the overlay to enable per-shape toolbar pickers.
    pub fn selected_kind(&self) -> Option<ToolKind> {
        let i = self.imp().selection.get()?;
        let doc_rc = self.imp().doc.borrow().clone()?;
        let doc = doc_rc.borrow();
        doc.layer(i).map(|t| t.kind())
    }

    /// Apply a font-size change to the active text target, if any: the in-progress text edit
    /// (live), or a selected committed `TextTool` (re-measured). Returns `true` if it consumed
    /// the change, so the caller can skip mutating the tool's default size.
    pub fn apply_font_size_to_target(&self, size: f32) -> bool {
        if self.is_editing_text() {
            // Reuses the live-update path: updates `PendingText.size_pt` and redraws.
            self.set_tool_font_size(ToolKind::Text, size);
            return true;
        }
        let Some(i) = self.imp().selection.get() else {
            return false;
        };
        let Some(doc_rc) = self.imp().doc.borrow().clone() else {
            return false;
        };
        let pango_ctx = self.create_pango_context();
        let mut changed = false;
        {
            let mut doc = doc_rc.borrow_mut();
            if let Some(layer) = doc.layer_mut(i)
                && let Some(t) = layer.as_any_mut().downcast_mut::<TextTool>()
            {
                t.size_pt = size.max(1.0);
                t.bounds_cache = measure_text_bounds(&t.text, t.size_pt, t.wrap_width, &pango_ctx);
                changed = true;
            }
        }
        if changed {
            self.queue_draw();
        }
        changed
    }

    /// Apply a color change to the selected layer (any colorable kind), if one is selected.
    /// Returns `true` if it consumed the change.
    pub fn apply_color_to_target(&self, color: [f32; 4]) -> bool {
        if self.is_editing_text() {
            self.set_tool_color(ToolKind::Text, color);
            return true;
        }
        let Some(i) = self.imp().selection.get() else {
            return false;
        };
        let Some(doc_rc) = self.imp().doc.borrow().clone() else {
            return false;
        };
        let mut changed = false;
        {
            let mut doc = doc_rc.borrow_mut();
            if let Some(layer) = doc.layer_mut(i) {
                changed = set_layer_color(layer, color);
            }
        }
        if changed {
            self.queue_draw();
        }
        changed
    }

    /// Apply a stroke-style change to the selected layer (outline kinds), if one is selected.
    /// Returns `true` if it consumed the change.
    pub fn apply_style_to_target(&self, style: StrokeStyle) -> bool {
        let Some(i) = self.imp().selection.get() else {
            return false;
        };
        let Some(doc_rc) = self.imp().doc.borrow().clone() else {
            return false;
        };
        let mut changed = false;
        {
            let mut doc = doc_rc.borrow_mut();
            if let Some(layer) = doc.layer_mut(i) {
                changed = set_layer_style(layer, style);
            }
        }
        if changed {
            self.queue_draw();
        }
        changed
    }

    /// Replace the current document with an empty one of the given logical size. Used by the
    /// live overlay to spin up a canvas sized to a monitor without owning any pixels.
    pub fn set_empty(&self, size: (u32, u32)) {
        self.set_document(Document::empty(size));
    }

    /// Attach a [`DocumentBase`] to the current document without exposing it on screen.
    ///
    /// Used by the draw-mode overlay: when the user picks the Blur tool we capture the
    /// underlying desktop into this hidden base so [`snapshot_blur`] has real pixels to
    /// sample. The base is **not** painted as a background (the canvas stays in
    /// `transparent_background = true` mode) — it only feeds the blur GSK subtree. Existing
    /// layers and crop are preserved.
    pub fn set_hidden_base(&self, base: DocumentBase) {
        let imp = self.imp();
        let tex = build_base_texture(&base).ok();
        imp.base_texture.replace(tex);
        if let Some(doc_rc) = imp.doc.borrow().clone() {
            doc_rc.borrow_mut().base = Some(base);
        } else {
            imp.doc
                .replace(Some(Rc::new(RefCell::new(Document::with_base(base)))));
        }
        imp.hidden_base.set(true);
        self.queue_draw();
    }

    /// `true` once a base image (visible or hidden) is attached to the current document.
    /// Cheap pre-flight check used by the overlay to skip redundant desktop captures.
    pub fn has_base(&self) -> bool {
        self.imp()
            .doc
            .borrow()
            .as_ref()
            .map(|d| d.borrow().base.is_some())
            .unwrap_or(false)
    }

    /// Drop every committed layer (used by the overlay's "clear" shortcut).
    pub fn clear_layers(&self) {
        if let Some(doc_rc) = self.imp().doc.borrow().clone() {
            let mut doc = doc_rc.borrow_mut();
            doc.layers.clear();
            doc.crop = None;
            drop(doc);
            self.imp().selection.set(None);
            self.imp().manip.replace(None);
            self.queue_draw();
        }
    }

    /// Pop the most recently committed layer, if any. Falls back to clearing an active crop when
    /// there are no layers left — otherwise crops would be undoable only by closing the editor.
    pub fn undo(&self) -> bool {
        let Some(doc_rc) = self.imp().doc.borrow().clone() else {
            return false;
        };
        // Any structural change invalidates the selected index; clear it up front.
        self.imp().selection.set(None);
        self.imp().manip.replace(None);
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
            format: PixelFormat::Bgra,
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
        // `Select` is an interaction mode, never a committed layer.
        ToolKind::Select => {}
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
    apply_wrap(&layout, t.wrap_width);
    layout
}

/// Apply an optional word-wrap width (document px) to a Pango layout. `None` leaves the
/// layout at its natural width (breaks only at explicit `\n`); `Some(w)` wraps long lines at
/// `w` on word boundaries (falling back to mid-word for unbreakable runs).
fn apply_wrap(layout: &pango::Layout, wrap_width: Option<f64>) {
    if let Some(w) = wrap_width {
        layout.set_width((w.max(1.0) * pango::SCALE as f64) as i32);
        layout.set_wrap(pango::WrapMode::WordChar);
    }
}

fn snapshot_text(snap: &gtk4::Snapshot, t: &TextTool, pango_ctx: &pango::Context) {
    let layout = text_layout(t, pango_ctx);
    snap.save();
    snap.translate(&graphene::Point::new(t.origin.0 as f32, t.origin.1 as f32));
    snap.append_layout(&layout, &rgba(t.color));
    snap.restore();
}

/// Build a Pango layout for the in-progress text editor. Mirrors `text_layout` so the
/// preview pixels match what gets committed.
fn pending_text_layout(pt: &PendingText, pango_ctx: &pango::Context) -> pango::Layout {
    let layout = pango::Layout::new(pango_ctx);
    layout.set_text(&pt.buffer);
    let mut desc = pango::FontDescription::from_string("Sans");
    desc.set_size((pt.size_pt as f64 * pango::SCALE as f64) as i32);
    layout.set_font_description(Some(&desc));
    apply_wrap(&layout, pt.wrap_width);
    layout
}

/// Render the in-progress text edit at its current state: the laid-out glyphs plus a
/// blinking caret. Uses the same Pango/GSK path as `snapshot_text`, so the on-screen
/// preview is pixel-identical to the eventual committed layer.
fn snapshot_pending_text(snap: &gtk4::Snapshot, pt: &PendingText, pango_ctx: &pango::Context) {
    let layout = pending_text_layout(pt, pango_ctx);
    snap.save();
    snap.translate(&graphene::Point::new(
        pt.origin.0 as f32,
        pt.origin.1 as f32,
    ));
    if !pt.buffer.is_empty() {
        snap.append_layout(&layout, &rgba(pt.color));
    }
    if pt.caret_visible {
        // Pango reports the caret position in Pango units (1024 per pixel). The "strong"
        // cursor pos is the natural visual location at the byte caret; we render it as a
        // ~1.5 px vertical bar in the text color so it stays visible against any base.
        let (strong, _weak) = layout.cursor_pos(pt.caret as i32);
        let scale = pango::SCALE as f32;
        let cx = strong.x() as f32 / scale;
        let cy = strong.y() as f32 / scale;
        let ch = (strong.height() as f32 / scale).max(pt.size_pt);
        let caret_rect = graphene::Rect::new(cx, cy, 1.5, ch);
        snap.append_color(&rgba(pt.color), &caret_rect);
    }
    snap.restore();
}

/// Blur the base image inside the tool's bounds, leaving other layers untouched. The push_blur
/// node applies a Gaussian blur to everything drawn in the active subtree; we wrap that subtree
/// in a clip rect so only pixels inside `t.bounds` are affected.
///
/// When `t.invert` is set, the roles flip: we render the *full* blurred texture first, then
/// overlay the un-blurred texture clipped to `t.bounds` so the selection stays sharp and
/// everything around it is blurred. GSK's `push_clip` only takes a rect, so this two-pass
/// approach is simpler than building an even-odd path-clip for the inverse region.
fn snapshot_blur(snap: &gtk4::Snapshot, t: &BlurTool, base: Option<&DocumentBase>) {
    let Some(base) = base else {
        // Fall back to a translucent grey rectangle when there's no base to blur (overlay path
        // before the lazy desktop capture lands). Mirrors the inverse/normal split visually.
        if t.invert {
            // Best-effort outside-veil: without a doc size here we can only hint at the rect.
            snap.append_color(&rgba([0.5, 0.5, 0.5, 0.25]), &rect_to_graphene(&t.bounds));
        } else {
            snap.append_color(&rgba([0.5, 0.5, 0.5, 0.45]), &rect_to_graphene(&t.bounds));
        }
        return;
    };
    let Ok(tex) = build_base_texture(base) else {
        return;
    };
    let clip = rect_to_graphene(&t.bounds);
    let full = graphene::Rect::new(0.0, 0.0, base.width as f32, base.height as f32);
    if t.invert {
        // Blur the entire base, then re-paint the sharp original inside the selection rect.
        snap.push_blur(t.radius as f64);
        snap.append_texture(&tex, &full);
        snap.pop(); // blur
        snap.push_clip(&clip);
        snap.append_texture(&tex, &full);
        snap.pop(); // clip
    } else {
        snap.push_clip(&clip);
        snap.push_blur(t.radius as f64);
        snap.append_texture(&tex, &full);
        snap.pop(); // blur
        snap.pop(); // clip
    }
}

fn snapshot_pending(snap: &gtk4::Snapshot, p: &PendingStroke, doc_size: (u32, u32)) {
    match p.kind {
        ToolKind::Rect => {
            let r = if p.constrain {
                drag_square(p.from, p.to)
            } else {
                drag_rect(p.from, p.to)
            };
            // Drag previews always use a marquee-style dash so the user can distinguish a
            // live drag from a committed layer regardless of the tool's chosen style.
            snap.append_stroke(
                &rect_path(&r),
                &dashed_stroke(2.0, &[6.0, 4.0]),
                &rgba(p.color),
            );
        }
        ToolKind::Ellipse => {
            let r = if p.constrain {
                drag_square(p.from, p.to)
            } else {
                drag_rect(p.from, p.to)
            };
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
            // commit the real blur on drag-end. The inverted variant (SHIFT) mirrors
            // [`snapshot_crop_veil`]: the veil covers everything *outside* the drag rect so the
            // user can see at a glance that the inside is preserved.
            let r = drag_rect(p.from, p.to);
            // Suppress the preview at drag-begin (degenerate 0-area rect). For the inverted
            // path that's critical: even-odd fill of "doc rect minus 0×0 inner rect" = full
            // doc, which would otherwise flash the entire screen the moment the user clicks.
            if r.w >= 2 && r.h >= 2 {
                if p.invert {
                    let pb = gsk::PathBuilder::new();
                    pb.add_rect(&graphene::Rect::new(
                        0.0,
                        0.0,
                        doc_size.0 as f32,
                        doc_size.1 as f32,
                    ));
                    pb.add_rect(&rect_to_graphene(&r));
                    snap.append_fill(&pb.to_path(), gsk::FillRule::EvenOdd, &rgba(p.color));
                } else {
                    snap.append_color(&rgba(p.color), &rect_to_graphene(&r));
                }
                snap.append_stroke(
                    &rect_path(&r),
                    &dashed_stroke(1.0, &[4.0, 3.0]),
                    &rgba([1.0, 1.0, 1.0, 0.9]),
                );
            }
        }
        ToolKind::Number | ToolKind::Text | ToolKind::Select => {}
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

/// Accent color for selection chrome (marquee + handles).
const SELECT_ACCENT: [f32; 4] = [0.20, 0.60, 1.0, 0.95];

/// Draw a single resize handle: a filled white square with an accent border, centred on
/// `(cx, cy)` in document coords.
fn append_handle(snap: &gtk4::Snapshot, cx: f64, cy: f64) {
    let r = graphene::Rect::new(
        (cx - HANDLE_DRAW / 2.0) as f32,
        (cy - HANDLE_DRAW / 2.0) as f32,
        HANDLE_DRAW as f32,
        HANDLE_DRAW as f32,
    );
    snap.append_color(&rgba([1.0, 1.0, 1.0, 1.0]), &r);
    let pb = gsk::PathBuilder::new();
    pb.add_rect(&r);
    snap.append_stroke(&pb.to_path(), &solid_stroke(1.0), &rgba(SELECT_ACCENT));
}

/// Draw the selection marquee + handles for the selected layer. Box shapes get a dashed
/// bounds marquee plus eight handles; Arrow/Line get endpoint handles; Number gets a circle
/// marquee plus an east radius grip; Text/Freehand get a move marquee only (no resize).
fn snapshot_selection(snap: &gtk4::Snapshot, tool: &dyn Tool) {
    let marquee = |snap: &gtk4::Snapshot, r: &Rect| {
        snap.append_stroke(
            &rect_path(r),
            &dashed_stroke(1.0, &[4.0, 3.0]),
            &rgba(SELECT_ACCENT),
        );
    };
    match tool.kind() {
        ToolKind::Rect
        | ToolKind::Ellipse
        | ToolKind::Highlight
        | ToolKind::Blur
        | ToolKind::Redact => {
            let r = tool.bounds();
            marquee(snap, &r);
            for (_, (hx, hy)) in box_handle_points(r) {
                append_handle(snap, hx, hy);
            }
        }
        ToolKind::Arrow => {
            if let Some(t) = tool.as_any().downcast_ref::<ArrowTool>() {
                append_handle(snap, t.from.0, t.from.1);
                append_handle(snap, t.to.0, t.to.1);
            }
        }
        ToolKind::Line => {
            if let Some(t) = tool.as_any().downcast_ref::<LineTool>() {
                append_handle(snap, t.from.0, t.from.1);
                append_handle(snap, t.to.0, t.to.1);
            }
        }
        ToolKind::Number => {
            if let Some(t) = tool.as_any().downcast_ref::<NumberTool>() {
                let pb = gsk::PathBuilder::new();
                pb.add_circle(
                    &graphene::Point::new(t.center.0 as f32, t.center.1 as f32),
                    t.radius as f32,
                );
                snap.append_stroke(
                    &pb.to_path(),
                    &dashed_stroke(1.0, &[4.0, 3.0]),
                    &rgba(SELECT_ACCENT),
                );
                append_handle(snap, t.center.0 + t.radius, t.center.1);
            }
        }
        ToolKind::Text => {
            let r = tool.bounds();
            marquee(snap, &r);
            for (_, (hx, hy)) in text_handles(r) {
                append_handle(snap, hx, hy);
            }
        }
        ToolKind::Freehand => {
            marquee(snap, &tool.bounds());
        }
        ToolKind::Crop | ToolKind::Select => {}
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
    /// Color the preview should render in. Mirrors the color the committed layer will
    /// receive at drag-end; populated from [`AnnotationCanvas::tool_color`] at drag-begin
    /// (with a kind-specific fallback for colorless tools).
    color: [f32; 4],
    /// Stroke dash style the preview (and committed layer) should use, seeded from
    /// [`AnnotationCanvas::tool_style`] at drag-begin. Defaults to `Solid` for tools
    /// that aren't styleable; the preview itself uses a fixed marquee dash for the
    /// bounded shape tools and only honours this for line-like tools.
    style: StrokeStyle,
    /// Modifier state captured once at drag-begin. Currently only the Blur tool reads
    /// this — when SHIFT is held the blur applies to everything *outside* the selection
    /// (a.k.a. reverse blur / focus mode). Sampled once and locked for the stroke so
    /// the preview can't flicker if the user releases SHIFT mid-drag.
    invert: bool,
    /// Refreshed on every drag-update from the live SHIFT modifier state. Only the
    /// Rect/Ellipse tools consult it: when set, the preview and committed bounds use
    /// [`drag_square`] instead of [`drag_rect`] so the user gets a perfect square /
    /// circle. Unlike `invert`, this is *not* latched at drag-begin — the user can
    /// press and release SHIFT freely during the drag and the preview tracks it.
    constrain: bool,
}

/// An in-progress WYSIWYG text edit. Lives in `imp.pending_text` while the user types;
/// rendered live by [`snapshot_pending_text`] using the same Pango helpers as the
/// committed [`TextTool`], so the user sees exactly what the final annotation will look
/// like. The caret toggles `caret_visible` every ~530 ms via a `glib::timeout_add_local`
/// timer that self-terminates when this slot becomes `None`.
#[derive(Clone, Debug)]
pub struct PendingText {
    /// Top-left of the text block on the document, in document coords (same convention
    /// as [`TextTool::origin`]).
    pub origin: (f64, f64),
    /// UTF-8 text content; may contain `\n` for explicit newlines (Shift+Return).
    pub buffer: String,
    /// Byte offset into `buffer` for the insertion caret. Always sits on a UTF-8
    /// character boundary; never indexed by char count.
    pub caret: usize,
    pub color: [f32; 4],
    pub size_pt: f32,
    /// Optional word-wrap width (document px), carried through from / back to the committed
    /// [`TextTool::wrap_width`] so re-editing a wrapped block keeps wrapping in the preview.
    pub wrap_width: Option<f64>,
    /// Toggled by the blink timer; drives whether `snapshot_pending_text` draws the caret.
    pub caret_visible: bool,
}

/// An in-progress Select-tool manipulation (move or resize). Created at drag-begin when a
/// press lands on the selected layer's body or one of its handles; consumed at drag-end. The
/// layer is mutated live on each drag-update so the shape follows the cursor.
#[derive(Clone, Debug)]
pub struct Manipulation {
    /// Index into `doc.layers` of the layer being manipulated.
    pub layer: usize,
    /// What part of the shape was grabbed.
    pub grab: Grab,
    /// Geometry snapshot at grab time. Live updates recompute absolute geometry from this plus
    /// the total cursor delta, so there's no cumulative rounding drift across frames.
    pub origin: GrabGeometry,
    /// Pointer position (document coords) where the grab started. Total move/resize deltas are
    /// measured against this.
    pub start: (f64, f64),
    /// Pointer position at the most recent update, used only by the Freehand body-move path
    /// (which translates incrementally to avoid cloning the point list).
    pub last: (f64, f64),
}

/// Which part of a selected shape the user grabbed.
#[derive(Copy, Clone, Debug, PartialEq, Eq)]
pub enum Grab {
    /// Move the whole shape.
    Body,
    /// Resize a box shape via one of its eight handles.
    BoxHandle(BoxHandle),
    /// Drag one endpoint of a 2-point shape (Arrow / Line).
    Endpoint(Endpoint),
    /// Drag the Number radius grip.
    NumberRadius,
}

/// Geometry of the grabbed layer captured at grab time, one variant per storage family.
#[derive(Copy, Clone, Debug)]
pub enum GrabGeometry {
    Box(Rect),
    Pair {
        from: (f64, f64),
        to: (f64, f64),
    },
    Center {
        center: (f64, f64),
        radius: f64,
    },
    /// Freehand only supports move; we snapshot the move origin.
    Origin((f64, f64)),
    /// Text supports move plus resize (corner handles scale `size_pt`, side handles set
    /// `wrap_width`). We snapshot enough to recompute both from the grab-time state.
    Text {
        origin: (f64, f64),
        size_pt: f32,
        wrap_width: Option<f64>,
        bounds: Rect,
    },
}

mod imp {
    use super::*;

    pub struct AnnotationCanvas {
        pub doc: RefCell<Option<Rc<RefCell<Document>>>>,
        pub base_texture: RefCell<Option<gdk::MemoryTexture>>,
        pub current_tool: Cell<ToolKind>,
        pub pending: RefCell<Option<PendingStroke>>,
        /// In-progress WYSIWYG text edit. When `Some`, the canvas is in text-editing mode:
        /// keystrokes mutate the buffer/caret, the layout is rendered live in `snapshot()`,
        /// and the caret blinks on a timer. Mutually exclusive with normal click-to-place
        /// behavior: clicking with the Text tool active commits the pending edit (if any)
        /// before starting a new one. See `commit_pending_text` / `cancel_pending_text`.
        pub pending_text: RefCell<Option<PendingText>>,
        /// Auto-increment counter used by the Number tool. Resets when a new document is loaded.
        pub next_number: Cell<u32>,
        /// When true, baseless documents render with a transparent background instead of the
        /// editor's dark fill — used by the live overlay so strokes float over the desktop.
        pub transparent_background: Cell<bool>,
        /// `true` when the `DocumentBase` attached to the current document is for blur
        /// sampling only and must not be painted as a canvas background. Set by
        /// [`AnnotationCanvas::set_hidden_base`] (Draw-mode lazy capture). Distinct from
        /// `transparent_background` because Edit mode also sets `transparent_background`
        /// but *does* want its base painted (it's the captured image the user is editing).
        pub hidden_base: Cell<bool>,
        /// Per-tool color overrides driven by the toolbar's color picker. Keys are only
        /// inserted for tools whose appearance is user-controlled — Blur, Crop and Redact
        /// stay hardcoded so the picker can disable itself for them.
        pub tool_colors: RefCell<HashMap<ToolKind, [f32; 4]>>,
        /// Per-tool stroke style overrides driven by the toolbar's style picker. Same
        /// model as `tool_colors`: only line-rendering tools (Rect/Ellipse/Arrow/Line/
        /// Freehand) get entries; everything else stays implicit-Solid.
        pub tool_styles: RefCell<HashMap<ToolKind, StrokeStyle>>,
        /// Per-tool font sizes (in points). Currently only meaningful for `ToolKind::Text`,
        /// but kept as a map so future text-bearing tools can plug in. Driven by the
        /// toolbar's font-size spinner when the Text tool is active.
        pub tool_font_sizes: RefCell<HashMap<ToolKind, f32>>,
        /// Source id of the active caret-blink timer (see `start_caret_blink`). Used
        /// to cancel the previous timer when a fresh text edit is started so we don't
        /// leak a timer per click.
        pub caret_timer: RefCell<Option<glib::SourceId>>,
        /// Index into `doc.layers` of the currently selected layer when the Select tool has a
        /// shape picked. `None` = nothing selected. Treated as a *hint*: any structural change
        /// to `layers` (undo, clear, delete, document swap) clears it, and every read goes
        /// through `doc.layers.get(i)` so a stale index fails safe.
        pub selection: Cell<Option<usize>>,
        /// In-flight Select-tool move/resize gesture. `Some` only between drag-begin and
        /// drag-end while manipulating the selected layer. Mutually exclusive with `pending`.
        pub manip: RefCell<Option<Manipulation>>,
        /// Original `TextTool` snapshot stashed while its text is being re-edited (the layer is
        /// removed from the document during the edit). Restored on cancel / empty-commit so an
        /// accidental Escape doesn't lose the annotation. See `try_reedit_text`.
        pub reedit_restore: RefCell<Option<TextTool>>,
        /// Name of the cursor currently set on the widget (Select-mode hover feedback), so the
        /// motion handler only calls `set_cursor_from_name` when it actually changes.
        pub hover_cursor: Cell<Option<&'static str>>,
        /// Invoked when a drawing action finishes (drag-end push, number placement, text
        /// commit, or a text edit cancelled with Escape). The overlay uses this to auto-return
        /// to Select mode. `None` = no observer.
        pub on_commit: RefCell<Option<Rc<dyn Fn()>>>,
        /// Invoked whenever the selection or text-edit state changes, so the overlay can
        /// recompute toolbar picker sensitivity (e.g. enable the font-size control while a text
        /// layer is selected or being edited). `None` = no observer.
        pub on_ui_state: RefCell<Option<Rc<dyn Fn()>>>,
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
            let mut font_sizes = HashMap::new();
            font_sizes.insert(ToolKind::Text, 18.0);
            Self {
                doc: RefCell::new(None),
                base_texture: RefCell::new(None),
                current_tool: Cell::new(ToolKind::Rect),
                pending: RefCell::new(None),
                pending_text: RefCell::new(None),
                next_number: Cell::new(1),
                transparent_background: Cell::new(false),
                hidden_base: Cell::new(false),
                tool_colors: RefCell::new(colors),
                tool_styles: RefCell::new(styles),
                tool_font_sizes: RefCell::new(font_sizes),
                caret_timer: RefCell::new(None),
                selection: Cell::new(None),
                manip: RefCell::new(None),
                reedit_restore: RefCell::new(None),
                hover_cursor: Cell::new(None),
                on_commit: RefCell::new(None),
                on_ui_state: RefCell::new(None),
            }
        }
    }

    #[glib::object_subclass]
    impl ObjectSubclass for AnnotationCanvas {
        const NAME: &'static str = "SnyprAnnotationCanvas";
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
            install_text_input(&obj);
            install_motion(&obj);
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
            // all for the transparent overlay path. When the base is `hidden` (Draw-mode
            // lazy capture used purely as a blur source) we skip painting it too — the
            // captured pixels stay reachable through `doc.base` for `snapshot_blur` only.
            if doc.base.is_none() {
                if !self.transparent_background.get() {
                    snapshot.append_color(&gdk::RGBA::new(0.1, 0.1, 0.12, 1.0), &bounds);
                }
            } else if !self.hidden_base.get()
                && let Some(tex) = self.base_texture.borrow().as_ref()
            {
                snapshot.append_texture(tex, &bounds);
            }

            let pango_ctx = self.obj().create_pango_context();
            let base = doc.base.clone();
            for layer in &doc.layers {
                snapshot_tool(snapshot, layer.as_ref(), &pango_ctx, base.as_ref());
            }
            if let Some(p) = self.pending.borrow().as_ref() {
                snapshot_pending(snapshot, p, doc.size);
            }
            if let Some(pt) = self.pending_text.borrow().as_ref() {
                snapshot_pending_text(snapshot, pt, &pango_ctx);
            }
            // Selection chrome sits on top of all layers so handles stay grabbable. Only drawn
            // in Select mode; the index is treated as a hint (`get`, never `[]`).
            if self.current_tool.get() == ToolKind::Select
                && let Some(i) = self.selection.get()
                && let Some(tool) = doc.layers.get(i)
            {
                snapshot_selection(snapshot, tool.as_ref());
            }
            if let Some(c) = doc.crop {
                snapshot_crop_veil(snapshot, doc.size, c);
            }
        }
    }
}

/// True when SHIFT is held during the in-flight gesture event. Uses
/// [`gtk4::EventController::current_event_state`] which is cheaper than the
/// `Display → Seat → Keyboard` roundtrip and reflects the modifier state at
/// the moment of the dispatched event — perfect for live-tracking SHIFT
/// during a drag.
fn shift_held(g: &gtk4::GestureDrag) -> bool {
    use gtk4::prelude::EventControllerExt;
    g.current_event_state()
        .contains(gdk4::ModifierType::SHIFT_MASK)
}

fn install_drag(canvas: &AnnotationCanvas) {
    let drag = gtk4::GestureDrag::new();

    {
        let weak = canvas.downgrade();
        drag.connect_drag_begin(move |_, x, y| {
            let Some(c) = weak.upgrade() else { return };
            let kind = c.tool();
            // Select drives its own hit-test → selection + manipulation, never a PendingStroke.
            if kind == ToolKind::Select {
                begin_select(&c, x, y);
                return;
            }
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
            // Sample SHIFT once at drag-begin and lock it on the stroke. Today only Blur
            // consumes this (reverse blur), so we skip the gdk roundtrip for every other
            // tool.
            let invert = if matches!(kind, ToolKind::Blur) {
                gdk4::Display::default()
                    .and_then(|d| d.default_seat())
                    .and_then(|s| s.keyboard())
                    .map(|k| k.modifier_state().contains(gdk4::ModifierType::SHIFT_MASK))
                    .unwrap_or(false)
            } else {
                false
            };
            c.imp().pending.replace(Some(PendingStroke {
                kind,
                from: (x, y),
                to: (x, y),
                points,
                color,
                style,
                invert,
                constrain: false,
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
            if c.tool() == ToolKind::Select {
                update_select(&c, (sx + dx, sy + dy));
                return;
            }
            let mut p = c.imp().pending.borrow_mut();
            if let Some(stroke) = p.as_mut() {
                stroke.to = (sx + dx, sy + dy);
                if matches!(stroke.kind, ToolKind::Freehand) {
                    stroke.points.push(stroke.to);
                }
                // Rect/Ellipse honour SHIFT live: refresh on every update so the
                // preview snaps/unsnaps to a square as the user presses/releases the
                // modifier mid-drag. Other tools skip the lookup.
                if matches!(stroke.kind, ToolKind::Rect | ToolKind::Ellipse) {
                    stroke.constrain = shift_held(g);
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
            if c.tool() == ToolKind::Select {
                // Geometry was applied live during updates; just clear the in-flight grab.
                c.imp().manip.replace(None);
                c.queue_draw();
                return;
            }
            let stroke = c.imp().pending.borrow_mut().take();
            let Some(mut stroke) = stroke else { return };
            stroke.to = (sx + dx, sy + dy);
            // Sample SHIFT one last time so a press right before mouse-up takes effect.
            // Only Rect/Ellipse consume this; the existing `invert` field is locked at
            // drag-begin and serves a different (Blur-specific) purpose.
            if matches!(stroke.kind, ToolKind::Rect | ToolKind::Ellipse) {
                stroke.constrain = shift_held(g);
            }
            let Some(doc_rc) = c.imp().doc.borrow().clone() else {
                return;
            };
            let mut doc = doc_rc.borrow_mut();
            // Track whether this drag actually produced something, so we only auto-return to
            // Select when a shape/crop was committed (not on a tiny sub-threshold drag).
            let layers_before = doc.layer_count();
            let crop_before = doc.crop.is_some();
            match stroke.kind {
                ToolKind::Rect => {
                    let r = if stroke.constrain {
                        drag_square(stroke.from, stroke.to)
                    } else {
                        drag_rect(stroke.from, stroke.to)
                    };
                    if r.w >= 2 && r.h >= 2 {
                        let mut t = RectTool::new(r);
                        t.stroke = c
                            .tool_color(ToolKind::Rect)
                            .unwrap_or(crate::annotate::tools::rect::DEFAULT_STROKE);
                        t.stroke_style = stroke.style;
                        doc.push_layer(Box::new(t));
                    }
                }
                ToolKind::Ellipse => {
                    let r = if stroke.constrain {
                        drag_square(stroke.from, stroke.to)
                    } else {
                        drag_rect(stroke.from, stroke.to)
                    };
                    if r.w >= 2 && r.h >= 2 {
                        let mut t = EllipseTool::new(r);
                        t.stroke = c
                            .tool_color(ToolKind::Ellipse)
                            .unwrap_or(crate::annotate::tools::rect::DEFAULT_STROKE);
                        t.stroke_style = stroke.style;
                        doc.push_layer(Box::new(t));
                    }
                }
                ToolKind::Arrow => {
                    let dx = stroke.to.0 - stroke.from.0;
                    let dy = stroke.to.1 - stroke.from.1;
                    if (dx * dx + dy * dy) >= 16.0 {
                        let mut t = ArrowTool::new(stroke.from, stroke.to);
                        t.stroke = c
                            .tool_color(ToolKind::Arrow)
                            .unwrap_or(crate::annotate::tools::arrow::DEFAULT_STROKE);
                        t.stroke_style = stroke.style;
                        doc.push_layer(Box::new(t));
                    }
                }
                ToolKind::Line => {
                    let dx = stroke.to.0 - stroke.from.0;
                    let dy = stroke.to.1 - stroke.from.1;
                    if (dx * dx + dy * dy) >= 16.0 {
                        let mut t = LineTool::new(stroke.from, stroke.to);
                        t.stroke = c
                            .tool_color(ToolKind::Line)
                            .unwrap_or(crate::annotate::tools::arrow::DEFAULT_STROKE);
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
                        doc.push_layer(Box::new(HighlightTool::new(r, color)));
                    }
                }
                ToolKind::Redact => {
                    let r = drag_rect(stroke.from, stroke.to);
                    if r.w >= 2 && r.h >= 2 {
                        doc.push_layer(Box::new(RedactTool::new(r)));
                    }
                }
                ToolKind::Freehand => {
                    if stroke.points.len() >= 2 {
                        let stroke_color = c
                            .tool_color(ToolKind::Freehand)
                            .unwrap_or(crate::annotate::tools::arrow::DEFAULT_STROKE);
                        doc.push_layer(Box::new(FreehandTool::new(
                            stroke.points,
                            stroke_color,
                            stroke.style,
                        )));
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
                        doc.push_layer(Box::new(BlurTool::new(clamped, stroke.invert)));
                    }
                }
                ToolKind::Number | ToolKind::Text | ToolKind::Select => {}
            }
            let resize = matches!(stroke.kind, ToolKind::Crop);
            let committed = doc.layer_count() != layers_before || doc.crop.is_some() != crop_before;
            drop(doc);
            if resize {
                c.queue_resize();
            }
            c.queue_draw();
            if committed {
                notify_commit(&c);
            }
        });
    }
    canvas.add_controller(drag);
}

fn install_click(canvas: &AnnotationCanvas) {
    let click = gtk4::GestureClick::new();
    click.set_button(gdk4::BUTTON_PRIMARY);
    let weak = canvas.downgrade();
    click.connect_released(move |_, n_press, x, y| {
        let Some(c) = weak.upgrade() else { return };
        match c.tool() {
            // Number/Text place on a single click; ignore the synthetic 2nd press of a double.
            ToolKind::Number if n_press == 1 => place_number(&c, x, y),
            ToolKind::Text => {
                if n_press == 2 {
                    try_reedit_text(&c, x, y);
                } else if n_press == 1 {
                    start_or_commit_text(&c, x, y);
                }
            }
            // Select: single-click selection is handled by the drag gesture (drag-begin fires
            // on press). A double-click on a text layer re-opens its editor.
            ToolKind::Select if n_press == 2 => try_reedit_text(&c, x, y),
            _ => {}
        }
    });
    canvas.add_controller(click);
}

/// Install a motion controller that drives the pointer cursor in Select mode: a directional
/// resize cursor over a handle, a move cursor over a grabbable body, and the default cursor
/// elsewhere. No-op (and resets any custom cursor) for every other tool so it never interferes
/// with the drawing tools' crosshair.
fn install_motion(canvas: &AnnotationCanvas) {
    let motion = gtk4::EventControllerMotion::new();
    let weak = canvas.downgrade();
    motion.connect_motion(move |_, x, y| {
        let Some(c) = weak.upgrade() else { return };
        let name = if c.tool() == ToolKind::Select {
            select_cursor_at(&c, x, y)
        } else {
            None
        };
        set_canvas_cursor(&c, name);
    });
    let weak_leave = canvas.downgrade();
    motion.connect_leave(move |_| {
        if let Some(c) = weak_leave.upgrade() {
            set_canvas_cursor(&c, None);
        }
    });
    canvas.add_controller(motion);
}

/// Cursor name for the Select tool at `(x, y)`: a resize cursor over the selected layer's
/// handle / endpoint, `move` over any grabbable body, else `None` (default cursor).
fn select_cursor_at(canvas: &AnnotationCanvas, x: f64, y: f64) -> Option<&'static str> {
    let doc_rc = canvas.imp().doc.borrow().clone()?;
    let doc = doc_rc.borrow();
    // Selected layer's handles take priority.
    if let Some(i) = canvas.imp().selection.get()
        && let Some(tool) = doc.layer(i)
        && let Some(grab) = grab_at(tool, x, y)
    {
        return Some(match grab {
            Grab::Body => "move",
            Grab::NumberRadius => "ew-resize",
            Grab::Endpoint(_) => "crosshair",
            Grab::BoxHandle(h) => box_handle_cursor(h),
        });
    }
    // Otherwise a body hit anywhere shows the move cursor.
    if doc.layers.iter().rev().any(|t| t.hit_test(x, y)) {
        return Some("move");
    }
    None
}

/// Map a box resize handle to its directional CSS cursor name.
fn box_handle_cursor(h: BoxHandle) -> &'static str {
    match h {
        BoxHandle::NW | BoxHandle::SE => "nwse-resize",
        BoxHandle::NE | BoxHandle::SW => "nesw-resize",
        BoxHandle::N | BoxHandle::S => "ns-resize",
        BoxHandle::E | BoxHandle::W => "ew-resize",
    }
}

/// Set the widget cursor by name, only when it changes (avoids per-motion churn). `None`
/// clears any custom cursor back to the default.
fn set_canvas_cursor(canvas: &AnnotationCanvas, name: Option<&'static str>) {
    if canvas.imp().hover_cursor.get() == name {
        return;
    }
    canvas.imp().hover_cursor.set(name);
    match name {
        Some(n) => canvas.set_cursor_from_name(Some(n)),
        None => canvas.set_cursor(None),
    }
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
    doc_rc
        .borrow_mut()
        .push_layer(Box::new(NumberTool::new((x, y), value, fill)));
    c.queue_draw();
    notify_commit(c);
}

/// Invoke the registered "drawing done" callback, if any. Called when a shape is committed
/// or a text edit ends (commit or Escape-cancel) so the overlay can return to Select mode.
fn notify_commit(canvas: &AnnotationCanvas) {
    let cb = canvas.imp().on_commit.borrow().clone();
    if let Some(cb) = cb {
        cb();
    }
}

/// Invoke the registered UI-state callback, if any. Called whenever the selection or text-edit
/// state changes so the overlay can refresh toolbar picker sensitivity.
fn notify_ui_state(canvas: &AnnotationCanvas) {
    let cb = canvas.imp().on_ui_state.borrow().clone();
    if let Some(cb) = cb {
        cb();
    }
}

/// Snapshot the grabbed layer's geometry for [`Manipulation::origin`].
fn grab_geometry(tool: &dyn Tool) -> GrabGeometry {
    if let Some(t) = tool.as_any().downcast_ref::<ArrowTool>() {
        GrabGeometry::Pair {
            from: t.from,
            to: t.to,
        }
    } else if let Some(t) = tool.as_any().downcast_ref::<LineTool>() {
        GrabGeometry::Pair {
            from: t.from,
            to: t.to,
        }
    } else if let Some(t) = tool.as_any().downcast_ref::<NumberTool>() {
        GrabGeometry::Center {
            center: t.center,
            radius: t.radius,
        }
    } else if let Some(t) = tool.as_any().downcast_ref::<TextTool>() {
        GrabGeometry::Text {
            origin: t.origin,
            size_pt: t.size_pt,
            wrap_width: t.wrap_width,
            bounds: t.bounds(),
        }
    } else if let Some(t) = tool.as_any().downcast_ref::<FreehandTool>() {
        // Freehand body-move translates incrementally; origin is unused but kept for symmetry.
        GrabGeometry::Origin(t.points.first().copied().unwrap_or((0.0, 0.0)))
    } else {
        GrabGeometry::Box(tool.bounds())
    }
}

/// Decide what `(x, y)` grabs on `tool`: a resize handle / endpoint / radius grip, or the
/// body. Returns `None` if the point misses the shape entirely (so the caller can fall
/// through to hit-testing other layers or deselecting).
fn grab_at(tool: &dyn Tool, x: f64, y: f64) -> Option<Grab> {
    match tool.kind() {
        ToolKind::Rect
        | ToolKind::Ellipse
        | ToolKind::Highlight
        | ToolKind::Blur
        | ToolKind::Redact => {
            if let Some(h) = box_handle_at(tool.bounds(), x, y) {
                return Some(Grab::BoxHandle(h));
            }
            tool.hit_test(x, y).then_some(Grab::Body)
        }
        ToolKind::Arrow | ToolKind::Line => {
            let (from, to) = endpoints(tool)?;
            if point_handle_hit(from, x, y) {
                return Some(Grab::Endpoint(Endpoint::From));
            }
            if point_handle_hit(to, x, y) {
                return Some(Grab::Endpoint(Endpoint::To));
            }
            tool.hit_test(x, y).then_some(Grab::Body)
        }
        ToolKind::Number => {
            if let Some(t) = tool.as_any().downcast_ref::<NumberTool>() {
                let grip = (t.center.0 + t.radius, t.center.1);
                if point_handle_hit(grip, x, y) {
                    return Some(Grab::NumberRadius);
                }
            }
            tool.hit_test(x, y).then_some(Grab::Body)
        }
        ToolKind::Text => {
            // Corners scale the font; E/W edges set the wrap width. N/S are omitted.
            for (h, (hx, hy)) in text_handles(tool.bounds()) {
                if point_handle_hit((hx, hy), x, y) {
                    return Some(Grab::BoxHandle(h));
                }
            }
            tool.hit_test(x, y).then_some(Grab::Body)
        }
        ToolKind::Freehand => tool.hit_test(x, y).then_some(Grab::Body),
        ToolKind::Crop | ToolKind::Select => None,
    }
}

/// Resize handles shown for a Text layer: the four corners (which scale the font size) and the
/// east / west edge midpoints (which set the wrap width). North / south are intentionally
/// omitted — vertical-only resize has no meaning for text.
fn text_handles(r: Rect) -> [(BoxHandle, (f64, f64)); 6] {
    [
        BoxHandle::NW,
        BoxHandle::NE,
        BoxHandle::SE,
        BoxHandle::SW,
        BoxHandle::E,
        BoxHandle::W,
    ]
    .map(|h| (h, box_handle_point(r, h)))
}

/// Endpoints of a 2-point tool (Arrow / Line), or `None` for other kinds.
fn endpoints(tool: &dyn Tool) -> Option<((f64, f64), (f64, f64))> {
    if let Some(t) = tool.as_any().downcast_ref::<ArrowTool>() {
        Some((t.from, t.to))
    } else {
        tool.as_any()
            .downcast_ref::<LineTool>()
            .map(|t| (t.from, t.to))
    }
}

/// Handle a Select-tool press: grab a handle of the already-selected layer, else select the
/// top-most layer under the cursor (and grab its body), else deselect. Leaves a
/// [`Manipulation`] in `imp.manip` when a grab started.
fn begin_select(c: &AnnotationCanvas, x: f64, y: f64) {
    let Some(doc_rc) = c.imp().doc.borrow().clone() else {
        return;
    };

    // 1) If something is selected, its handles take priority over other layers' bodies.
    let selected = c.imp().selection.get();
    if let Some(i) = selected {
        let grab = {
            let doc = doc_rc.borrow();
            doc.layer(i).and_then(|t| grab_at(t, x, y))
        };
        if let Some(grab) = grab {
            let origin = {
                let doc = doc_rc.borrow();
                grab_geometry(doc.layer(i).expect("layer present"))
            };
            c.imp().manip.replace(Some(Manipulation {
                layer: i,
                grab,
                origin,
                start: (x, y),
                last: (x, y),
            }));
            return;
        }
    }

    // 2) Hit-test bodies top-most first; select + grab body on the first hit.
    let hit = {
        let doc = doc_rc.borrow();
        doc.layers
            .iter()
            .enumerate()
            .rev()
            .find(|(_, t)| t.hit_test(x, y))
            .map(|(i, _)| i)
    };
    if let Some(i) = hit {
        let origin = {
            let doc = doc_rc.borrow();
            grab_geometry(doc.layer(i).expect("layer present"))
        };
        c.imp().selection.set(Some(i));
        c.imp().manip.replace(Some(Manipulation {
            layer: i,
            grab: Grab::Body,
            origin,
            start: (x, y),
            last: (x, y),
        }));
        c.queue_draw();
        notify_ui_state(c);
        return;
    }

    // 3) Empty space → deselect.
    if c.imp().selection.get().is_some() {
        c.imp().selection.set(None);
        c.queue_draw();
        notify_ui_state(c);
    }
}

/// Apply the in-flight Select manipulation to its layer, recomputing absolute geometry from
/// the grab-time snapshot plus the current cursor position `cur` (document coords).
fn update_select(c: &AnnotationCanvas, cur: (f64, f64)) {
    let Some(m) = c.imp().manip.borrow().clone() else {
        return;
    };
    let Some(doc_rc) = c.imp().doc.borrow().clone() else {
        return;
    };
    // Built eagerly but only read by the Text-resize arm, which needs to re-measure glyph
    // extents.
    let pango_ctx = c.create_pango_context();
    {
        let mut doc = doc_rc.borrow_mut();
        let Some(layer) = doc.layer_mut(m.layer) else {
            return;
        };
        apply_manipulation(layer, &m, cur, Some(&pango_ctx));
    }
    // Track the latest cursor for the incremental Freehand path.
    if let Some(m) = c.imp().manip.borrow_mut().as_mut() {
        m.last = cur;
    }
    c.queue_draw();
}

/// The geometry core of [`update_select`], split out so the resize / move / reshape math is
/// unit-testable without a widget. Mutates `layer` in place; performs no redraw and does not
/// advance `m.last` (the caller owns both).
///
/// `pango_ctx` is only consulted by the Text corner/edge resize arm, which must re-measure
/// glyph extents. Pass `None` from tests that exercise any other arm; a `None` with a Text
/// resize grab is a no-op rather than a panic, since a missing display must not corrupt the
/// document.
fn apply_manipulation(
    layer: &mut Box<dyn Tool>,
    m: &Manipulation,
    cur: (f64, f64),
    pango_ctx: Option<&pango::Context>,
) {
    match (m.grab, m.origin) {
        (Grab::Body, GrabGeometry::Box(orig)) => {
            let (dx, dy) = (cur.0 - m.start.0, cur.1 - m.start.1);
            if let Some(t) = layer.as_any_mut().downcast_mut::<RectTool>() {
                t.bounds = orig.translate(dx, dy);
            } else if let Some(t) = layer.as_any_mut().downcast_mut::<EllipseTool>() {
                t.bounds = orig.translate(dx, dy);
            } else if let Some(t) = layer.as_any_mut().downcast_mut::<HighlightTool>() {
                t.bounds = orig.translate(dx, dy);
            } else if let Some(t) = layer.as_any_mut().downcast_mut::<BlurTool>() {
                t.bounds = orig.translate(dx, dy);
            } else if let Some(t) = layer.as_any_mut().downcast_mut::<RedactTool>() {
                t.bounds = orig.translate(dx, dy);
            }
        }
        (Grab::Body, GrabGeometry::Pair { from, to }) => {
            let (dx, dy) = (cur.0 - m.start.0, cur.1 - m.start.1);
            let nf = (from.0 + dx, from.1 + dy);
            let nt = (to.0 + dx, to.1 + dy);
            set_pair(layer, nf, nt);
        }
        (Grab::Body, GrabGeometry::Center { center, .. }) => {
            let (dx, dy) = (cur.0 - m.start.0, cur.1 - m.start.1);
            if let Some(t) = layer.as_any_mut().downcast_mut::<NumberTool>() {
                t.center = (center.0 + dx, center.1 + dy);
            }
        }
        (Grab::Body, GrabGeometry::Origin(_)) => {
            // Freehand body-move: translate incrementally from the last cursor position
            // (avoids cloning the point list into the grab snapshot).
            let (dx, dy) = (cur.0 - m.last.0, cur.1 - m.last.1);
            layer.translate(dx, dy);
        }
        (
            Grab::Body,
            GrabGeometry::Text {
                origin: orig_origin,
                ..
            },
        ) => {
            if let Some(t) = layer.as_any_mut().downcast_mut::<TextTool>() {
                let (dx, dy) = (cur.0 - m.start.0, cur.1 - m.start.1);
                t.origin = (orig_origin.0 + dx, orig_origin.1 + dy);
            }
        }
        (Grab::BoxHandle(h), GrabGeometry::Box(orig)) => {
            let nr = select::resize_box(orig, h, cur.0, cur.1);
            set_box_bounds(layer, nr);
        }
        (
            Grab::BoxHandle(h),
            GrabGeometry::Text {
                origin: orig_origin,
                size_pt,
                wrap_width,
                bounds,
            },
        ) => {
            if let Some(pango_ctx) = pango_ctx
                && let Some(t) = layer.as_any_mut().downcast_mut::<TextTool>()
            {
                resize_text(
                    t,
                    h,
                    orig_origin,
                    size_pt,
                    wrap_width,
                    bounds,
                    cur,
                    pango_ctx,
                );
            }
        }
        (Grab::Endpoint(which), GrabGeometry::Pair { from, to }) => {
            let (nf, nt) = select::set_endpoint(from, to, which, cur.0, cur.1);
            set_pair(layer, nf, nt);
        }
        (Grab::NumberRadius, GrabGeometry::Center { center, .. }) => {
            if let Some(t) = layer.as_any_mut().downcast_mut::<NumberTool>() {
                t.radius = select::new_radius(center, cur.0, cur.1);
            }
        }
        _ => {}
    }
}

/// Resize a Text layer in place during a Select drag. Corner handles scale `size_pt` by the
/// ratio of the dragged box height to the original height (keeping the top-left anchored);
/// the east / west edge handles set `wrap_width` from the new box width. `bounds_cache` is
/// re-measured so the marquee and handles track the new extent.
#[allow(clippy::too_many_arguments)]
fn resize_text(
    t: &mut TextTool,
    handle: BoxHandle,
    orig_origin: (f64, f64),
    orig_size_pt: f32,
    orig_wrap: Option<f64>,
    orig_bounds: Rect,
    cur: (f64, f64),
    pango_ctx: &pango::Context,
) {
    match handle {
        BoxHandle::E | BoxHandle::W => {
            // Set the wrap width from the new box width, keeping the opposite edge anchored.
            let (left, right) = match handle {
                BoxHandle::W => (cur.0, orig_bounds.right() as f64),
                _ => (orig_bounds.x as f64, cur.0),
            };
            let width = (right - left).abs().max(select::MIN_BOX);
            t.origin = (left.min(right), orig_origin.1);
            t.wrap_width = Some(width);
        }
        _ => {
            // Corner: scale the font size by the vertical drag ratio against the original box.
            let new_box = select::resize_box(orig_bounds, handle, cur.0, cur.1);
            let ratio = if orig_bounds.h > 0 {
                (new_box.h as f64 / orig_bounds.h as f64).clamp(0.1, 20.0)
            } else {
                1.0
            };
            let new_size = ((orig_size_pt as f64 * ratio).round() as f32).clamp(6.0, 200.0);
            t.size_pt = new_size;
            t.wrap_width = orig_wrap;
            // Anchor the resized block at the new box's top-left so it grows toward the cursor.
            t.origin = (new_box.x as f64, new_box.y as f64);
        }
    }
    t.bounds_cache = measure_text_bounds(&t.text, t.size_pt, t.wrap_width, pango_ctx);
}

/// Set the stroke / fill color of a colorable layer in place. Returns `true` if the layer was
/// a colorable kind. Blur / Crop / Redact have no user color and return `false`.
fn set_layer_color(layer: &mut Box<dyn Tool>, color: [f32; 4]) -> bool {
    if let Some(t) = layer.as_any_mut().downcast_mut::<RectTool>() {
        t.stroke = color;
    } else if let Some(t) = layer.as_any_mut().downcast_mut::<EllipseTool>() {
        t.stroke = color;
    } else if let Some(t) = layer.as_any_mut().downcast_mut::<ArrowTool>() {
        t.stroke = color;
    } else if let Some(t) = layer.as_any_mut().downcast_mut::<LineTool>() {
        t.stroke = color;
    } else if let Some(t) = layer.as_any_mut().downcast_mut::<FreehandTool>() {
        t.stroke = color;
    } else if let Some(t) = layer.as_any_mut().downcast_mut::<HighlightTool>() {
        t.color = color;
    } else if let Some(t) = layer.as_any_mut().downcast_mut::<NumberTool>() {
        t.fill = color;
    } else if let Some(t) = layer.as_any_mut().downcast_mut::<TextTool>() {
        t.color = color;
    } else {
        return false;
    }
    true
}

/// Set the stroke dash style of an outline layer in place. Returns `true` if the layer has a
/// styleable outline (Rect / Ellipse / Arrow / Line / Freehand).
fn set_layer_style(layer: &mut Box<dyn Tool>, style: StrokeStyle) -> bool {
    if let Some(t) = layer.as_any_mut().downcast_mut::<RectTool>() {
        t.stroke_style = style;
    } else if let Some(t) = layer.as_any_mut().downcast_mut::<EllipseTool>() {
        t.stroke_style = style;
    } else if let Some(t) = layer.as_any_mut().downcast_mut::<ArrowTool>() {
        t.stroke_style = style;
    } else if let Some(t) = layer.as_any_mut().downcast_mut::<LineTool>() {
        t.stroke_style = style;
    } else if let Some(t) = layer.as_any_mut().downcast_mut::<FreehandTool>() {
        t.stroke_style = style;
    } else {
        return false;
    }
    true
}

/// Write `bounds` onto whichever box-family tool `layer` is.
fn set_box_bounds(layer: &mut Box<dyn Tool>, bounds: Rect) {
    if let Some(t) = layer.as_any_mut().downcast_mut::<RectTool>() {
        t.bounds = bounds;
    } else if let Some(t) = layer.as_any_mut().downcast_mut::<EllipseTool>() {
        t.bounds = bounds;
    } else if let Some(t) = layer.as_any_mut().downcast_mut::<HighlightTool>() {
        t.bounds = bounds;
    } else if let Some(t) = layer.as_any_mut().downcast_mut::<BlurTool>() {
        t.bounds = bounds;
    } else if let Some(t) = layer.as_any_mut().downcast_mut::<RedactTool>() {
        t.bounds = bounds;
    }
}

/// Write `(from, to)` onto whichever 2-point tool `layer` is.
fn set_pair(layer: &mut Box<dyn Tool>, from: (f64, f64), to: (f64, f64)) {
    if let Some(t) = layer.as_any_mut().downcast_mut::<ArrowTool>() {
        t.from = from;
        t.to = to;
    } else if let Some(t) = layer.as_any_mut().downcast_mut::<LineTool>() {
        t.from = from;
        t.to = to;
    }
}

/// Click-with-Text-tool handler. If a text edit is already in progress, commits it and
/// starts a fresh one at the new click point — the natural "I'm done with that label;
/// place another here" gesture. Otherwise opens a new in-canvas WYSIWYG text editor at
/// the click location: a [`PendingText`] is stored on the canvas, the caret-blink timer
/// is started, focus is grabbed so keystrokes route to our `install_text_input`
/// controller, and the live preview is rendered from the next `snapshot()` onwards.
fn start_or_commit_text(canvas: &AnnotationCanvas, x: f64, y: f64) {
    if canvas.imp().pending_text.borrow().is_some() {
        commit_pending_text(canvas);
    }
    let color = canvas
        .tool_color(ToolKind::Text)
        .unwrap_or([1.0, 0.95, 0.2, 1.0]);
    let size_pt = canvas.tool_font_size(ToolKind::Text).unwrap_or(18.0);
    canvas.imp().pending_text.replace(Some(PendingText {
        origin: (x, y),
        buffer: String::new(),
        caret: 0,
        color,
        size_pt,
        wrap_width: None,
        caret_visible: true,
    }));
    let _ = canvas.grab_focus();
    start_caret_blink(canvas);
    canvas.queue_draw();
    notify_ui_state(canvas);
}

/// Commit the in-progress text edit (if any) as a `TextTool` layer on the document.
/// An empty buffer cancels instead of pushing a zero-glyph layer. Either way, clears
/// `pending_text` and stops the blink timer.
///
/// When this edit was a *re-edit* of an existing layer (an original is stashed in
/// `reedit_restore`), an empty buffer restores the original rather than deleting it — losing
/// text to an accidental select-all+Enter is worse than the alternative.
fn commit_pending_text(canvas: &AnnotationCanvas) {
    let Some(pt) = canvas.imp().pending_text.borrow_mut().take() else {
        return;
    };
    stop_caret_blink(canvas);
    let restore = canvas.imp().reedit_restore.replace(None);
    if pt.buffer.is_empty() {
        // Nothing to commit. If we were re-editing, put the original back.
        if let (Some(orig), Some(doc_rc)) = (restore, canvas.imp().doc.borrow().clone()) {
            doc_rc.borrow_mut().push_layer(Box::new(orig));
        }
        canvas.queue_draw();
        notify_ui_state(canvas);
        return;
    }
    if let Some(doc_rc) = canvas.imp().doc.borrow().clone() {
        // Measure with a fresh Pango context so the cached `Tool::bounds` matches the
        // pixels `snapshot_text` will render at this size / wrap width.
        let pango_ctx = canvas.create_pango_context();
        let bounds_cache = measure_text_bounds(&pt.buffer, pt.size_pt, pt.wrap_width, &pango_ctx);
        let mut t = TextTool::new(pt.origin, pt.buffer, pt.size_pt, pt.color, bounds_cache);
        t.wrap_width = pt.wrap_width;
        doc_rc.borrow_mut().push_layer(Box::new(t));
    }
    canvas.queue_draw();
    notify_ui_state(canvas);
}

/// Pango-measure a text block's pixel `(width, height)` at `size_pt` and optional `wrap_width`.
/// When wrapping, the reported width is the wrap width (so the layer's bounds box matches the
/// resize handles) rather than the shorter "ink" width Pango actually used.
fn measure_text_bounds(
    text: &str,
    size_pt: f32,
    wrap_width: Option<f64>,
    pango_ctx: &pango::Context,
) -> (u32, u32) {
    let tmp = TextTool {
        origin: (0.0, 0.0),
        text: text.to_string(),
        size_pt,
        color: [0.0, 0.0, 0.0, 1.0],
        wrap_width,
        bounds_cache: (0, 0),
    };
    let layout = text_layout(&tmp, pango_ctx);
    let (w, h) = layout.pixel_size();
    let width = match wrap_width {
        Some(ww) => ww.max(w as f64).round() as u32,
        None => w.max(0) as u32,
    };
    (width, h.max(0) as u32)
}

/// Drop the in-progress text edit without committing. Pairs with the Escape key. If the edit
/// was a re-edit of an existing layer, the stashed original is restored so Escape means
/// "cancel my changes" rather than "delete the annotation".
fn cancel_pending_text(canvas: &AnnotationCanvas) {
    if canvas.imp().pending_text.borrow().is_some() {
        canvas.imp().pending_text.replace(None);
        stop_caret_blink(canvas);
        if let (Some(orig), Some(doc_rc)) = (
            canvas.imp().reedit_restore.replace(None),
            canvas.imp().doc.borrow().clone(),
        ) {
            doc_rc.borrow_mut().push_layer(Box::new(orig));
        }
        canvas.queue_draw();
        notify_ui_state(canvas);
    }
}

/// Re-open the WYSIWYG editor for the top-most `TextTool` under `(x, y)`. The committed layer
/// is removed and stashed in `reedit_restore` (so its glyphs don't double-draw under the live
/// preview, and so Escape / empty-commit can restore it). Seeded with the original text /
/// origin / size / color. No-op if no text layer is hit.
fn try_reedit_text(canvas: &AnnotationCanvas, x: f64, y: f64) {
    // Finish any other edit first.
    if canvas.imp().pending_text.borrow().is_some() {
        commit_pending_text(canvas);
    }
    let Some(doc_rc) = canvas.imp().doc.borrow().clone() else {
        return;
    };
    let idx = {
        let doc = doc_rc.borrow();
        doc.layers
            .iter()
            .enumerate()
            .rev()
            .find(|(_, t)| t.kind() == ToolKind::Text && t.hit_test(x, y))
            .map(|(i, _)| i)
    };
    let Some(i) = idx else { return };
    let removed = doc_rc.borrow_mut().remove_layer(i);
    let Some(removed) = removed else { return };
    let Some(text) = removed.as_any().downcast_ref::<TextTool>().cloned() else {
        // Shouldn't happen (we filtered on kind); put it back to be safe.
        doc_rc.borrow_mut().push_layer(removed);
        return;
    };
    canvas.imp().selection.set(None);
    canvas.imp().manip.replace(None);
    let buffer = text.text.clone();
    let caret = buffer.len();
    canvas.imp().pending_text.replace(Some(PendingText {
        origin: text.origin,
        buffer,
        caret,
        color: text.color,
        size_pt: text.size_pt,
        wrap_width: text.wrap_width,
        caret_visible: true,
    }));
    canvas.imp().reedit_restore.replace(Some(text));
    let _ = canvas.grab_focus();
    start_caret_blink(canvas);
    canvas.queue_draw();
    notify_ui_state(canvas);
}

/// Start (or restart) the caret-blink timer. Any previous timer is removed first so
/// clicking around with the Text tool doesn't accumulate timers. The closure
/// self-terminates as soon as `pending_text` becomes `None` or the canvas is dropped,
/// so commit/cancel/document-swap all naturally stop the blink without further work.
fn start_caret_blink(canvas: &AnnotationCanvas) {
    stop_caret_blink(canvas);
    let weak = canvas.downgrade();
    let source = glib::timeout_add_local(std::time::Duration::from_millis(530), move || {
        let Some(c) = weak.upgrade() else {
            return glib::ControlFlow::Break;
        };
        let mut slot = c.imp().pending_text.borrow_mut();
        let Some(pt) = slot.as_mut() else {
            // Clear the stored handle so we don't try to `remove` an already-finished
            // source later — `SourceId::remove` panics on a stale id.
            drop(slot);
            c.imp().caret_timer.replace(None);
            return glib::ControlFlow::Break;
        };
        pt.caret_visible = !pt.caret_visible;
        drop(slot);
        c.queue_draw();
        glib::ControlFlow::Continue
    });
    canvas.imp().caret_timer.replace(Some(source));
}

/// Remove the caret-blink timer if one is active. Safe to call when no timer exists.
fn stop_caret_blink(canvas: &AnnotationCanvas) {
    if let Some(id) = canvas.imp().caret_timer.replace(None) {
        id.remove();
    }
}

/// Install a key-event controller that drives the in-canvas WYSIWYG text editor.
///
/// Runs in `PropagationPhase::Capture` so that, while a text edit is in progress, we
/// intercept Return/Backspace/etc. *before* the toolbar's window-level shortcut
/// dispatcher (which binds `Return` to Save). When no text edit is active, every key
/// is allowed to propagate, so global shortcuts behave exactly as before.
fn install_text_input(canvas: &AnnotationCanvas) {
    let key = gtk4::EventControllerKey::new();
    key.set_propagation_phase(gtk4::PropagationPhase::Capture);
    let weak = canvas.downgrade();
    key.connect_key_pressed(move |_, keyval, _keycode, state| {
        let Some(c) = weak.upgrade() else {
            return glib::Propagation::Proceed;
        };
        if c.imp().pending_text.borrow().is_none() {
            // Not editing text. The Select tool consumes Delete / Escape / arrow-nudge for the
            // selected layer; everything else propagates so global shortcuts behave as before.
            return handle_select_key(&c, keyval, state);
        }
        // Ignore modifier-only chords other than Shift — let Ctrl+S / Ctrl+Z etc. through
        // so the toolbar shortcuts keep working even mid-edit (Undo on the most recent
        // commit, Save on the document, etc.).
        let mods = state
            & (gdk4::ModifierType::CONTROL_MASK
                | gdk4::ModifierType::ALT_MASK
                | gdk4::ModifierType::SUPER_MASK);
        if !mods.is_empty() {
            return glib::Propagation::Proceed;
        }
        let shift = state.contains(gdk4::ModifierType::SHIFT_MASK);
        handle_text_key(&c, keyval, shift)
    });
    canvas.add_controller(key);
}

/// What a key press means to the Select tool, decided without touching the widget.
///
/// Split out of [`handle_select_key`] so the modifier and arrow-key policy is testable as a
/// table; the caller owns every side effect.
#[derive(Copy, Clone, Debug, PartialEq)]
enum SelectKeyAction {
    /// Not a Select action — let the key propagate.
    Ignore,
    /// Remove the selected layer.
    Delete,
    /// Clear the selection.
    Deselect,
    /// Translate the selected layer by this document-space delta.
    Nudge(f64, f64),
}

/// Distance an arrow key moves the selection, in document px. Shift makes it coarse.
const NUDGE_STEP: f64 = 1.0;
const NUDGE_STEP_COARSE: f64 = 10.0;

/// Decide what `keyval` does for the Select tool.
///
/// `has_selection` mirrors `imp.selection.is_some()`: with nothing selected there is nothing
/// to delete, deselect or nudge, so every key propagates. Ctrl/Alt/Super chords are reserved
/// for global accelerators (Undo, Save, …) and are never consumed here — note that Shift is
/// deliberately *not* in that set, since it is the coarse-nudge modifier.
fn select_key_action(
    tool: ToolKind,
    keyval: gdk4::Key,
    state: gdk4::ModifierType,
    has_selection: bool,
) -> SelectKeyAction {
    if tool != ToolKind::Select || !has_selection {
        return SelectKeyAction::Ignore;
    }
    let chord = state
        & (gdk4::ModifierType::CONTROL_MASK
            | gdk4::ModifierType::ALT_MASK
            | gdk4::ModifierType::SUPER_MASK);
    if !chord.is_empty() {
        return SelectKeyAction::Ignore;
    }
    let step = if state.contains(gdk4::ModifierType::SHIFT_MASK) {
        NUDGE_STEP_COARSE
    } else {
        NUDGE_STEP
    };
    match keyval {
        gdk4::Key::BackSpace | gdk4::Key::Delete => SelectKeyAction::Delete,
        gdk4::Key::Escape => SelectKeyAction::Deselect,
        gdk4::Key::Left => SelectKeyAction::Nudge(-step, 0.0),
        gdk4::Key::Right => SelectKeyAction::Nudge(step, 0.0),
        gdk4::Key::Up => SelectKeyAction::Nudge(0.0, -step),
        gdk4::Key::Down => SelectKeyAction::Nudge(0.0, step),
        _ => SelectKeyAction::Ignore,
    }
}

/// Keyboard handling for the Select tool when no text edit is active: Delete/Backspace removes
/// the selected layer, Escape deselects, and arrow keys nudge it (Shift = 10px). Returns
/// `Proceed` whenever the key isn't a Select action (or nothing is selected) so global
/// shortcuts keep working.
fn handle_select_key(
    canvas: &AnnotationCanvas,
    keyval: gdk4::Key,
    state: gdk4::ModifierType,
) -> glib::Propagation {
    let selection = canvas.imp().selection.get();
    let action = select_key_action(canvas.tool(), keyval, state, selection.is_some());
    let (dx, dy) = match action {
        SelectKeyAction::Ignore => return glib::Propagation::Proceed,
        SelectKeyAction::Delete => {
            delete_selected(canvas);
            return glib::Propagation::Stop;
        }
        SelectKeyAction::Deselect => {
            canvas.imp().selection.set(None);
            canvas.queue_draw();
            notify_ui_state(canvas);
            return glib::Propagation::Stop;
        }
        SelectKeyAction::Nudge(dx, dy) => (dx, dy),
    };
    // `select_key_action` only returns Nudge when something is selected.
    let i = selection.expect("nudge implies a selection");
    if let Some(doc_rc) = canvas.imp().doc.borrow().clone() {
        let mut doc = doc_rc.borrow_mut();
        if let Some(layer) = doc.layer_mut(i) {
            layer.translate(dx, dy);
            drop(doc);
            canvas.queue_draw();
            return glib::Propagation::Stop;
        }
    }
    glib::Propagation::Proceed
}

/// Remove the currently selected layer and clear the selection.
fn delete_selected(canvas: &AnnotationCanvas) {
    let Some(i) = canvas.imp().selection.get() else {
        return;
    };
    if let Some(doc_rc) = canvas.imp().doc.borrow().clone() {
        doc_rc.borrow_mut().remove_layer(i);
    }
    canvas.imp().selection.set(None);
    canvas.imp().manip.replace(None);
    canvas.queue_draw();
    notify_ui_state(canvas);
}

/// A key press, reduced to the vocabulary the text editor understands.
///
/// This is the single GTK boundary of the text-editing path: [`text_key_from`] does the
/// `gdk4::Key` translation, and everything downstream is pure.
#[derive(Copy, Clone, Debug, PartialEq)]
enum TextKey {
    Escape,
    Enter,
    Backspace,
    Delete,
    Left,
    Right,
    Home,
    End,
    Up,
    Down,
    /// A printable character to insert.
    Char(char),
    /// Not meaningful to the editor (Tab, function keys, bare modifiers, …).
    Other,
}

/// What [`apply_text_key`] did, and what the caller still owes.
#[derive(Copy, Clone, Debug, PartialEq)]
enum TextKeyOutcome {
    /// Buffer and/or caret changed; redraw and consume the key.
    Handled,
    /// Nothing to do; let the key propagate.
    Unhandled,
    /// Abandon the edit (restoring the original layer if re-editing).
    Cancel,
    /// Finish the edit and commit it as a layer.
    Commit,
}

/// Translate a `gdk4::Key` into the editor's [`TextKey`] vocabulary.
///
/// `to_unicode` already returns the shifted character when Shift is in the keyval's effective
/// group, so ASCII typing works without a manual case shift. Control characters are rejected
/// here rather than inserted as invisible garbage.
fn text_key_from(keyval: gdk4::Key) -> TextKey {
    match keyval {
        gdk4::Key::Escape => TextKey::Escape,
        gdk4::Key::Return | gdk4::Key::KP_Enter => TextKey::Enter,
        gdk4::Key::BackSpace => TextKey::Backspace,
        gdk4::Key::Delete => TextKey::Delete,
        gdk4::Key::Left => TextKey::Left,
        gdk4::Key::Right => TextKey::Right,
        gdk4::Key::Home => TextKey::Home,
        gdk4::Key::End => TextKey::End,
        gdk4::Key::Up => TextKey::Up,
        gdk4::Key::Down => TextKey::Down,
        other => match other.to_unicode() {
            Some(ch) if !ch.is_control() => TextKey::Char(ch),
            _ => TextKey::Other,
        },
    }
}

/// Apply one key to the in-progress text edit. Pure: mutates `pt` and reports what the caller
/// must do about it.
///
/// `shift` only matters for [`TextKey::Enter`], where it means "insert a newline" rather than
/// "finish editing".
fn apply_text_key(pt: &mut PendingText, key: TextKey, shift: bool) -> TextKeyOutcome {
    match key {
        // Escape always exits text editing, never leaves the Text tool armed for another
        // placement.
        TextKey::Escape => return TextKeyOutcome::Cancel,
        TextKey::Enter => {
            if shift {
                insert_at_caret(pt, "\n");
            } else {
                return TextKeyOutcome::Commit;
            }
        }
        TextKey::Backspace => {
            if pt.caret > 0 {
                let prev = prev_char_boundary(&pt.buffer, pt.caret);
                pt.buffer.replace_range(prev..pt.caret, "");
                pt.caret = prev;
            }
        }
        TextKey::Delete => {
            if pt.caret < pt.buffer.len() {
                let next = next_char_boundary(&pt.buffer, pt.caret);
                pt.buffer.replace_range(pt.caret..next, "");
            }
        }
        TextKey::Left => pt.caret = prev_char_boundary(&pt.buffer, pt.caret),
        TextKey::Right => pt.caret = next_char_boundary(&pt.buffer, pt.caret),
        TextKey::Home => pt.caret = line_start(&pt.buffer, pt.caret),
        TextKey::End => pt.caret = line_end(&pt.buffer, pt.caret),
        TextKey::Up => pt.caret = move_caret_vertically(&pt.buffer, pt.caret, -1),
        TextKey::Down => pt.caret = move_caret_vertically(&pt.buffer, pt.caret, 1),
        TextKey::Char(ch) => {
            let mut buf = [0u8; 4];
            insert_at_caret(pt, ch.encode_utf8(&mut buf));
        }
        TextKey::Other => return TextKeyOutcome::Unhandled,
    }
    // Restart the visible portion of the blink cycle so the caret is always shown
    // immediately after typing — feels more responsive than waiting for the next tick.
    pt.caret_visible = true;
    TextKeyOutcome::Handled
}

/// Apply a single key press to the in-progress text edit. Returns `Stop` whenever the
/// key was meaningful to the editor (printable insertion, navigation, commit, cancel)
/// so toolbar accelerators don't double-fire; returns `Proceed` for keys we don't
/// handle (e.g. Tab, function keys) so they remain available for other controllers.
fn handle_text_key(canvas: &AnnotationCanvas, keyval: gdk4::Key, shift: bool) -> glib::Propagation {
    let outcome = {
        let mut pt_borrow = canvas.imp().pending_text.borrow_mut();
        let Some(pt) = pt_borrow.as_mut() else {
            return glib::Propagation::Proceed;
        };
        apply_text_key(pt, text_key_from(keyval), shift)
    };
    match outcome {
        TextKeyOutcome::Handled => {
            canvas.queue_draw();
            glib::Propagation::Stop
        }
        TextKeyOutcome::Unhandled => glib::Propagation::Proceed,
        TextKeyOutcome::Cancel => {
            cancel_pending_text(canvas);
            // Explicit "done editing" → return to Select mode (one-shot tool).
            notify_commit(canvas);
            glib::Propagation::Stop
        }
        TextKeyOutcome::Commit => {
            commit_pending_text(canvas);
            notify_commit(canvas);
            glib::Propagation::Stop
        }
    }
}

fn insert_at_caret(pt: &mut PendingText, s: &str) {
    pt.buffer.insert_str(pt.caret, s);
    pt.caret += s.len();
}

/// Walk back from `idx` to the previous UTF-8 character boundary; returns 0 if `idx`
/// already sits at the start of the string.
fn prev_char_boundary(s: &str, idx: usize) -> usize {
    if idx == 0 {
        return 0;
    }
    let mut i = idx - 1;
    while i > 0 && !s.is_char_boundary(i) {
        i -= 1;
    }
    i
}

/// Walk forward from `idx` to the next UTF-8 character boundary; returns `s.len()` if
/// `idx` already sits at the end of the string.
fn next_char_boundary(s: &str, idx: usize) -> usize {
    if idx >= s.len() {
        return s.len();
    }
    let mut i = idx + 1;
    while i < s.len() && !s.is_char_boundary(i) {
        i += 1;
    }
    i
}

/// Byte offset of the start of the line that contains the byte position `caret`.
fn line_start(s: &str, caret: usize) -> usize {
    s[..caret].rfind('\n').map(|i| i + 1).unwrap_or(0)
}

/// Byte offset of the end of the line that contains `caret` (i.e. just before the next
/// `\n`, or `s.len()` if there's no trailing newline).
fn line_end(s: &str, caret: usize) -> usize {
    s[caret..]
        .find('\n')
        .map(|i| caret + i)
        .unwrap_or_else(|| s.len())
}

/// Move the caret one line up (`delta = -1`) or down (`delta = 1`), preserving the
/// column position measured as the number of `char`s from the start of the current
/// line. Snaps to the end of the destination line when the column exceeds its length.
fn move_caret_vertically(s: &str, caret: usize, delta: i32) -> usize {
    let cur_start = line_start(s, caret);
    let column_chars = s[cur_start..caret].chars().count();
    let dest_start = match delta {
        d if d < 0 => {
            if cur_start == 0 {
                return caret;
            }
            // Previous line: scan back from cur_start - 1 (which is the '\n') to its line start.
            line_start(s, cur_start - 1)
        }
        _ => {
            let cur_end = line_end(s, caret);
            if cur_end >= s.len() {
                return caret;
            }
            cur_end + 1
        }
    };
    let dest_end = line_end(s, dest_start);
    let dest_line = &s[dest_start..dest_end];
    for (taken, (i, _)) in dest_line.char_indices().enumerate() {
        if taken >= column_chars {
            return dest_start + i;
        }
    }
    // Ran out of characters before reaching the target column → snap to line end.
    dest_end
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
    use pretty_assertions::assert_eq;
    use rstest::rstest;

    // --- fixtures ---------------------------------------------------------

    fn rect(x: i32, y: i32, w: u32, h: u32) -> Rect {
        Rect { x, y, w, h }
    }

    fn pending(buffer: &str, caret: usize) -> PendingText {
        PendingText {
            origin: (0.0, 0.0),
            buffer: buffer.to_owned(),
            caret,
            color: [1.0, 1.0, 1.0, 1.0],
            size_pt: 16.0,
            wrap_width: None,
            caret_visible: false,
        }
    }

    fn text_tool(text: &str) -> TextTool {
        TextTool::new((10.0, 20.0), text.to_owned(), 16.0, [1.0; 4], (100, 40))
    }

    fn manip(grab: Grab, origin: GrabGeometry, start: (f64, f64)) -> Manipulation {
        Manipulation {
            layer: 0,
            grab,
            origin,
            start,
            last: start,
        }
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

    #[test]
    fn set_layer_color_recolors_colorable_kinds() {
        let mut rect: Box<dyn Tool> = Box::new(RectTool::new(Rect {
            x: 0,
            y: 0,
            w: 10,
            h: 10,
        }));
        assert!(set_layer_color(&mut rect, [0.0, 1.0, 0.0, 1.0]));
        let r = rect.as_any().downcast_ref::<RectTool>().unwrap();
        assert_eq!(r.stroke, [0.0, 1.0, 0.0, 1.0]);

        // Redact has no user color → returns false, unchanged.
        let mut redact: Box<dyn Tool> = Box::new(RedactTool {
            bounds: Rect {
                x: 0,
                y: 0,
                w: 5,
                h: 5,
            },
        });
        assert!(!set_layer_color(&mut redact, [1.0, 1.0, 1.0, 1.0]));
    }

    #[test]
    fn set_layer_style_only_affects_outline_kinds() {
        let mut line: Box<dyn Tool> = Box::new(LineTool::new((0.0, 0.0), (5.0, 5.0)));
        assert!(set_layer_style(&mut line, StrokeStyle::Dashed));
        let l = line.as_any().downcast_ref::<LineTool>().unwrap();
        assert_eq!(l.stroke_style, StrokeStyle::Dashed);

        // Highlight is a filled rect with no outline style → false.
        let mut hl: Box<dyn Tool> = Box::new(HighlightTool {
            bounds: Rect {
                x: 0,
                y: 0,
                w: 5,
                h: 5,
            },
            color: [1.0, 1.0, 0.0, 0.35],
        });
        assert!(!set_layer_style(&mut hl, StrokeStyle::Dotted));
    }

    // --- caret / buffer helpers -------------------------------------------
    //
    // These index `buffer` by *byte* offset, so every one of them is a UTF-8
    // boundary hazard. The tests below deliberately mix ASCII, 2-byte (é),
    // 3-byte (—) and 4-byte (emoji) sequences.

    #[test]
    fn insert_at_caret_splices_at_the_byte_offset_and_advances_the_caret() {
        let mut pt = pending("hello world", 5);
        insert_at_caret(&mut pt, ",");
        assert_eq!(pt.buffer, "hello, world");
        assert_eq!(pt.caret, 6);
    }

    #[test]
    fn insert_at_caret_advances_by_byte_length_not_char_count() {
        let mut pt = pending("", 0);
        insert_at_caret(&mut pt, "é");
        // 'é' is two bytes; a char-count caret would land mid-sequence.
        assert_eq!(pt.caret, 2);
        assert!(pt.buffer.is_char_boundary(pt.caret));
    }

    #[test]
    fn insert_at_caret_appends_at_the_end_of_the_buffer() {
        let mut pt = pending("ab", 2);
        insert_at_caret(&mut pt, "c");
        assert_eq!(pt.buffer, "abc");
        assert_eq!(pt.caret, 3);
    }

    #[rstest]
    // ASCII: one byte back.
    #[case("abc", 3, 2)]
    #[case("abc", 1, 0)]
    // Already at the start.
    #[case("abc", 0, 0)]
    #[case("", 0, 0)]
    // 2-byte 'é' — must skip the whole sequence, not land on the continuation byte.
    #[case("aé", 3, 1)]
    // 3-byte em dash.
    #[case("a—", 4, 1)]
    // 4-byte emoji.
    #[case("a😀", 5, 1)]
    // Newlines are ordinary one-byte characters here.
    #[case("a\nb", 2, 1)]
    fn prev_char_boundary_lands_on_a_boundary(
        #[case] s: &str,
        #[case] idx: usize,
        #[case] expected: usize,
    ) {
        let got = prev_char_boundary(s, idx);
        assert_eq!(got, expected);
        assert!(
            s.is_char_boundary(got),
            "{got} is not a char boundary of {s:?}"
        );
    }

    #[rstest]
    #[case("abc", 0, 1)]
    #[case("abc", 2, 3)]
    // Already at (or past) the end.
    #[case("abc", 3, 3)]
    #[case("", 0, 0)]
    #[case("aé", 1, 3)]
    #[case("a—", 1, 4)]
    #[case("a😀", 1, 5)]
    #[case("a\nb", 1, 2)]
    fn next_char_boundary_lands_on_a_boundary(
        #[case] s: &str,
        #[case] idx: usize,
        #[case] expected: usize,
    ) {
        let got = next_char_boundary(s, idx);
        assert_eq!(got, expected);
        assert!(
            s.is_char_boundary(got),
            "{got} is not a char boundary of {s:?}"
        );
    }

    #[test]
    fn char_boundary_walks_are_inverses_across_a_mixed_width_string() {
        let s = "aé—😀z";
        // Walk forward collecting every boundary, then walk back and expect the mirror.
        let mut forward = vec![0usize];
        let mut i = 0;
        while i < s.len() {
            i = next_char_boundary(s, i);
            forward.push(i);
        }
        let mut backward = vec![s.len()];
        let mut j = s.len();
        while j > 0 {
            j = prev_char_boundary(s, j);
            backward.push(j);
        }
        backward.reverse();
        assert_eq!(forward, backward);
        assert_eq!(forward.len(), s.chars().count() + 1);
    }

    #[rstest]
    // Single line: always 0.
    #[case("hello", 0, 0)]
    #[case("hello", 5, 0)]
    // Caret on the second line.
    #[case("ab\ncd", 3, 3)]
    #[case("ab\ncd", 5, 3)]
    // Caret exactly on the newline belongs to the *first* line.
    #[case("ab\ncd", 2, 0)]
    // Empty line between two others.
    #[case("a\n\nb", 2, 2)]
    #[case("", 0, 0)]
    fn line_start_finds_the_byte_after_the_preceding_newline(
        #[case] s: &str,
        #[case] caret: usize,
        #[case] expected: usize,
    ) {
        assert_eq!(line_start(s, caret), expected);
    }

    #[rstest]
    #[case("hello", 0, 5)]
    #[case("hello", 5, 5)]
    #[case("ab\ncd", 0, 2)]
    #[case("ab\ncd", 2, 2)]
    // Second line runs to the end of the buffer (no trailing newline).
    #[case("ab\ncd", 3, 5)]
    #[case("a\n\nb", 2, 2)]
    #[case("", 0, 0)]
    fn line_end_stops_before_the_next_newline(
        #[case] s: &str,
        #[case] caret: usize,
        #[case] expected: usize,
    ) {
        assert_eq!(line_end(s, caret), expected);
    }

    #[test]
    fn home_and_end_bracket_a_multi_byte_line() {
        // "aé—" is a(0..1) é(1..3) —(3..6), so 3 is the boundary between é and the em dash.
        let s = "aé—\nxy";
        let caret = 3;
        assert_eq!(line_start(s, caret), 0);
        // Line 1 is 'a'(1) + 'é'(2) + '—'(3) = 6 bytes.
        assert_eq!(line_end(s, caret), 6);
    }

    // --- vertical caret movement ------------------------------------------

    #[test]
    fn move_caret_up_preserves_the_column_in_characters() {
        let s = "abcd\nefgh";
        // Caret after 'g' on line 2 → column 3.
        let caret = 5 + 3;
        assert_eq!(move_caret_vertically(s, caret, -1), 3);
    }

    #[test]
    fn move_caret_down_preserves_the_column_in_characters() {
        let s = "abcd\nefgh";
        assert_eq!(move_caret_vertically(s, 3, 1), 5 + 3);
    }

    #[test]
    fn move_caret_column_is_measured_in_chars_not_bytes() {
        // Line 1 is three chars but seven bytes; line 2 is plain ASCII.
        let s = "aé—\nxyz";
        let caret = 6; // end of line 1 → column 3
        let down = move_caret_vertically(s, caret, 1);
        // Column 3 on "xyz" is its end, byte 7 + 3.
        assert_eq!(down, 7 + 3);
        assert_eq!(&s[7..down], "xyz");
    }

    #[test]
    fn move_caret_snaps_to_the_end_of_a_shorter_destination_line() {
        let s = "abcdef\nxy";
        let caret = 5; // column 5 on the long line
        let down = move_caret_vertically(s, caret, 1);
        // "xy" has only two chars → snap to its end.
        assert_eq!(down, s.len());
    }

    #[test]
    fn move_caret_up_from_the_first_line_is_a_no_op() {
        let s = "abc\ndef";
        assert_eq!(move_caret_vertically(s, 2, -1), 2);
    }

    #[test]
    fn move_caret_down_from_the_last_line_is_a_no_op() {
        let s = "abc\ndef";
        assert_eq!(move_caret_vertically(s, 6, 1), 6);
    }

    #[test]
    fn move_caret_traverses_an_empty_line() {
        let s = "abc\n\ndef";
        // Down from column 2 of line 1 onto the empty line snaps to its (zero-length) end.
        let mid = move_caret_vertically(s, 2, 1);
        assert_eq!(mid, 4);
        // Down again lands at column 0 of line 3 — the column was clamped, not remembered.
        assert_eq!(move_caret_vertically(s, mid, 1), 5);
    }

    #[test]
    fn move_caret_always_lands_on_a_char_boundary() {
        let s = "aé—😀\nz😀é\nqq";
        for caret in (0..=s.len()).filter(|i| s.is_char_boundary(*i)) {
            for delta in [-1, 1] {
                let got = move_caret_vertically(s, caret, delta);
                assert!(
                    s.is_char_boundary(got),
                    "caret {caret} delta {delta} → {got}, not a boundary of {s:?}"
                );
            }
        }
    }

    // --- text key translation ---------------------------------------------

    #[rstest]
    #[case(TextKey::Backspace, "abc", 3, "ab", 2)]
    // Backspace at the start of the buffer is a no-op, not an underflow.
    #[case(TextKey::Backspace, "abc", 0, "abc", 0)]
    // Deletes a whole multi-byte character.
    #[case(TextKey::Backspace, "aé", 3, "a", 1)]
    #[case(TextKey::Delete, "abc", 0, "bc", 0)]
    // Delete at the end of the buffer is a no-op.
    #[case(TextKey::Delete, "abc", 3, "abc", 3)]
    #[case(TextKey::Delete, "aé", 1, "a", 1)]
    #[case(TextKey::Left, "abc", 2, "abc", 1)]
    #[case(TextKey::Left, "abc", 0, "abc", 0)]
    #[case(TextKey::Right, "abc", 1, "abc", 2)]
    #[case(TextKey::Right, "abc", 3, "abc", 3)]
    #[case(TextKey::Home, "ab\ncd", 5, "ab\ncd", 3)]
    #[case(TextKey::End, "ab\ncd", 0, "ab\ncd", 2)]
    #[case(TextKey::Char('x'), "ab", 1, "axb", 2)]
    #[case(TextKey::Char('é'), "ab", 1, "aéb", 3)]
    fn apply_text_key_edits_the_buffer(
        #[case] key: TextKey,
        #[case] before: &str,
        #[case] caret: usize,
        #[case] after: &str,
        #[case] after_caret: usize,
    ) {
        let mut pt = pending(before, caret);
        assert_eq!(apply_text_key(&mut pt, key, false), TextKeyOutcome::Handled);
        assert_eq!(pt.buffer, after);
        assert_eq!(pt.caret, after_caret);
    }

    #[test]
    fn escape_cancels_the_edit_without_touching_the_buffer() {
        let mut pt = pending("abc", 1);
        assert_eq!(
            apply_text_key(&mut pt, TextKey::Escape, false),
            TextKeyOutcome::Cancel
        );
        assert_eq!(pt.buffer, "abc");
        assert_eq!(pt.caret, 1);
    }

    #[test]
    fn plain_enter_commits_but_shift_enter_inserts_a_newline() {
        let mut pt = pending("ab", 1);
        assert_eq!(
            apply_text_key(&mut pt, TextKey::Enter, false),
            TextKeyOutcome::Commit
        );
        assert_eq!(pt.buffer, "ab", "commit must not mutate the buffer");

        let mut pt = pending("ab", 1);
        assert_eq!(
            apply_text_key(&mut pt, TextKey::Enter, true),
            TextKeyOutcome::Handled
        );
        assert_eq!(pt.buffer, "a\nb");
        assert_eq!(pt.caret, 2);
    }

    #[test]
    fn shift_only_matters_for_enter() {
        // Every other key ignores it, so a shifted arrow still just moves the caret.
        for key in [TextKey::Left, TextKey::Right, TextKey::Up, TextKey::Down] {
            let mut shifted = pending("ab\ncd", 4);
            let mut plain = pending("ab\ncd", 4);
            assert_eq!(
                apply_text_key(&mut shifted, key, true),
                apply_text_key(&mut plain, key, false)
            );
            assert_eq!(shifted.caret, plain.caret);
        }
    }

    #[test]
    fn unhandled_keys_leave_the_buffer_and_blink_state_alone() {
        let mut pt = pending("abc", 1);
        pt.caret_visible = false;
        assert_eq!(
            apply_text_key(&mut pt, TextKey::Other, false),
            TextKeyOutcome::Unhandled
        );
        assert_eq!(pt.buffer, "abc");
        assert_eq!(pt.caret, 1);
        assert!(
            !pt.caret_visible,
            "an unhandled key must not restart the blink cycle"
        );
    }

    #[test]
    fn every_handled_key_restarts_the_caret_blink() {
        for key in [
            TextKey::Backspace,
            TextKey::Delete,
            TextKey::Left,
            TextKey::Right,
            TextKey::Home,
            TextKey::End,
            TextKey::Up,
            TextKey::Down,
            TextKey::Char('x'),
        ] {
            let mut pt = pending("ab\ncd", 4);
            pt.caret_visible = false;
            assert_eq!(apply_text_key(&mut pt, key, false), TextKeyOutcome::Handled);
            assert!(pt.caret_visible, "{key:?} did not make the caret visible");
        }
    }

    #[test]
    fn apply_text_key_never_leaves_the_caret_off_a_boundary() {
        let mut pt = pending("aé—😀\nz😀", 0);
        for key in [
            TextKey::Right,
            TextKey::Right,
            TextKey::Down,
            TextKey::End,
            TextKey::Backspace,
            TextKey::Left,
            TextKey::Delete,
            TextKey::Home,
            TextKey::Up,
            TextKey::Char('é'),
        ] {
            apply_text_key(&mut pt, key, false);
            assert!(
                pt.buffer.is_char_boundary(pt.caret),
                "caret {} off boundary in {:?} after {key:?}",
                pt.caret,
                pt.buffer
            );
        }
    }

    #[test]
    fn typing_a_word_then_deleting_it_returns_to_the_empty_buffer() {
        let mut pt = pending("", 0);
        for ch in "héllo".chars() {
            apply_text_key(&mut pt, TextKey::Char(ch), false);
        }
        assert_eq!(pt.buffer, "héllo");
        assert_eq!(pt.caret, pt.buffer.len());
        for _ in 0..5 {
            apply_text_key(&mut pt, TextKey::Backspace, false);
        }
        assert_eq!(pt.buffer, "");
        assert_eq!(pt.caret, 0);
    }

    // --- select key table -------------------------------------------------

    #[rstest]
    #[case(gdk4::Key::Left, -NUDGE_STEP, 0.0)]
    #[case(gdk4::Key::Right, NUDGE_STEP, 0.0)]
    #[case(gdk4::Key::Up, 0.0, -NUDGE_STEP)]
    #[case(gdk4::Key::Down, 0.0, NUDGE_STEP)]
    fn arrow_keys_nudge_the_selection(#[case] key: gdk4::Key, #[case] dx: f64, #[case] dy: f64) {
        let action = select_key_action(ToolKind::Select, key, gdk4::ModifierType::empty(), true);
        assert_eq!(action, SelectKeyAction::Nudge(dx, dy));
    }

    #[rstest]
    #[case(gdk4::Key::Left, -NUDGE_STEP_COARSE, 0.0)]
    #[case(gdk4::Key::Down, 0.0, NUDGE_STEP_COARSE)]
    fn shift_makes_the_nudge_coarse(#[case] key: gdk4::Key, #[case] dx: f64, #[case] dy: f64) {
        let action = select_key_action(ToolKind::Select, key, gdk4::ModifierType::SHIFT_MASK, true);
        assert_eq!(action, SelectKeyAction::Nudge(dx, dy));
    }

    #[rstest]
    #[case(gdk4::Key::BackSpace, SelectKeyAction::Delete)]
    #[case(gdk4::Key::Delete, SelectKeyAction::Delete)]
    #[case(gdk4::Key::Escape, SelectKeyAction::Deselect)]
    // Not a Select binding.
    #[case(gdk4::Key::Tab, SelectKeyAction::Ignore)]
    #[case(gdk4::Key::a, SelectKeyAction::Ignore)]
    #[case(gdk4::Key::F1, SelectKeyAction::Ignore)]
    fn select_key_action_maps_the_editing_keys(
        #[case] key: gdk4::Key,
        #[case] expected: SelectKeyAction,
    ) {
        let action = select_key_action(ToolKind::Select, key, gdk4::ModifierType::empty(), true);
        assert_eq!(action, expected);
    }

    #[rstest]
    #[case(gdk4::ModifierType::CONTROL_MASK)]
    #[case(gdk4::ModifierType::ALT_MASK)]
    #[case(gdk4::ModifierType::SUPER_MASK)]
    #[case(gdk4::ModifierType::CONTROL_MASK | gdk4::ModifierType::SHIFT_MASK)]
    fn chords_are_reserved_for_global_accelerators(#[case] state: gdk4::ModifierType) {
        // Ctrl+Z / Ctrl+S / … must reach the toolbar, so the Select tool never eats them —
        // not even for keys it otherwise binds.
        for key in [
            gdk4::Key::Left,
            gdk4::Key::Delete,
            gdk4::Key::Escape,
            gdk4::Key::Down,
        ] {
            assert_eq!(
                select_key_action(ToolKind::Select, key, state, true),
                SelectKeyAction::Ignore,
                "{key:?} with {state:?} should propagate"
            );
        }
    }

    #[test]
    fn select_keys_do_nothing_without_a_selection() {
        for key in [gdk4::Key::Left, gdk4::Key::Delete, gdk4::Key::Escape] {
            assert_eq!(
                select_key_action(ToolKind::Select, key, gdk4::ModifierType::empty(), false),
                SelectKeyAction::Ignore
            );
        }
    }

    #[rstest]
    #[case(ToolKind::Rect)]
    #[case(ToolKind::Text)]
    #[case(ToolKind::Freehand)]
    #[case(ToolKind::Crop)]
    fn select_keys_only_apply_to_the_select_tool(#[case] tool: ToolKind) {
        assert_eq!(
            select_key_action(tool, gdk4::Key::Delete, gdk4::ModifierType::empty(), true),
            SelectKeyAction::Ignore
        );
    }

    // --- grab geometry ----------------------------------------------------

    #[rstest]
    #[case(BoxHandle::NW, "nwse-resize")]
    #[case(BoxHandle::SE, "nwse-resize")]
    #[case(BoxHandle::NE, "nesw-resize")]
    #[case(BoxHandle::SW, "nesw-resize")]
    #[case(BoxHandle::N, "ns-resize")]
    #[case(BoxHandle::S, "ns-resize")]
    #[case(BoxHandle::E, "ew-resize")]
    #[case(BoxHandle::W, "ew-resize")]
    fn box_handle_cursor_names_the_drag_axis(#[case] handle: BoxHandle, #[case] expected: &str) {
        assert_eq!(box_handle_cursor(handle), expected);
    }

    #[test]
    fn grab_geometry_snapshots_a_box_tool_as_its_bounds() {
        let tool: Box<dyn Tool> = Box::new(RectTool::new(rect(1, 2, 30, 40)));
        match grab_geometry(tool.as_ref()) {
            GrabGeometry::Box(r) => assert_eq!(r, rect(1, 2, 30, 40)),
            other => panic!("expected Box, got {other:?}"),
        }
    }

    #[test]
    fn grab_geometry_snapshots_a_two_point_tool_as_a_pair() {
        let tool: Box<dyn Tool> = Box::new(LineTool::new((1.0, 2.0), (3.0, 4.0)));
        match grab_geometry(tool.as_ref()) {
            GrabGeometry::Pair { from, to } => {
                assert_eq!(from, (1.0, 2.0));
                assert_eq!(to, (3.0, 4.0));
            }
            other => panic!("expected Pair, got {other:?}"),
        }
    }

    #[test]
    fn grab_geometry_snapshots_a_number_as_a_center_and_radius() {
        let tool: Box<dyn Tool> = Box::new(NumberTool::new((50.0, 60.0), 1, [1.0; 4]));
        match grab_geometry(tool.as_ref()) {
            GrabGeometry::Center { center, radius } => {
                assert_eq!(center, (50.0, 60.0));
                assert!(radius > 0.0);
            }
            other => panic!("expected Center, got {other:?}"),
        }
    }

    #[test]
    fn grab_geometry_snapshots_text_with_everything_needed_to_resize() {
        let tool: Box<dyn Tool> = Box::new(text_tool("hi"));
        match grab_geometry(tool.as_ref()) {
            GrabGeometry::Text {
                origin,
                size_pt,
                wrap_width,
                bounds,
            } => {
                assert_eq!(origin, (10.0, 20.0));
                assert_eq!(size_pt, 16.0);
                assert_eq!(wrap_width, None);
                assert_eq!(bounds, rect(10, 20, 100, 40));
            }
            other => panic!("expected Text, got {other:?}"),
        }
    }

    #[test]
    fn grab_geometry_snapshots_freehand_as_its_first_point() {
        let tool: Box<dyn Tool> = Box::new(FreehandTool::new(
            vec![(7.0, 8.0), (9.0, 10.0)],
            [1.0; 4],
            StrokeStyle::Solid,
        ));
        match grab_geometry(tool.as_ref()) {
            GrabGeometry::Origin(p) => assert_eq!(p, (7.0, 8.0)),
            other => panic!("expected Origin, got {other:?}"),
        }
    }

    #[test]
    fn grab_geometry_tolerates_an_empty_freehand_stroke() {
        let tool: Box<dyn Tool> = Box::new(FreehandTool::new(vec![], [1.0; 4], StrokeStyle::Solid));
        match grab_geometry(tool.as_ref()) {
            GrabGeometry::Origin(p) => assert_eq!(p, (0.0, 0.0)),
            other => panic!("expected Origin, got {other:?}"),
        }
    }

    #[test]
    fn endpoints_are_reported_for_two_point_tools_only() {
        let line: Box<dyn Tool> = Box::new(LineTool::new((0.0, 0.0), (5.0, 5.0)));
        assert_eq!(endpoints(line.as_ref()), Some(((0.0, 0.0), (5.0, 5.0))));

        let arrow: Box<dyn Tool> = Box::new(ArrowTool::new((1.0, 1.0), (2.0, 2.0)));
        assert_eq!(endpoints(arrow.as_ref()), Some(((1.0, 1.0), (2.0, 2.0))));

        let r: Box<dyn Tool> = Box::new(RectTool::new(rect(0, 0, 10, 10)));
        assert_eq!(endpoints(r.as_ref()), None);
    }

    #[test]
    fn text_handles_are_the_four_corners_plus_the_two_side_midpoints() {
        let handles = text_handles(rect(0, 0, 100, 50));
        let kinds: Vec<BoxHandle> = handles.iter().map(|(h, _)| *h).collect();
        assert_eq!(
            kinds,
            vec![
                BoxHandle::NW,
                BoxHandle::NE,
                BoxHandle::SE,
                BoxHandle::SW,
                BoxHandle::E,
                BoxHandle::W,
            ]
        );
        // N/S are deliberately absent: vertical-only resize has no meaning for text.
        assert!(!kinds.contains(&BoxHandle::N));
        assert!(!kinds.contains(&BoxHandle::S));
    }

    #[test]
    fn text_handle_points_sit_on_the_bounding_box() {
        let r = rect(10, 20, 100, 50);
        for (h, (x, y)) in text_handles(r) {
            assert!(
                (r.x as f64..=r.right() as f64).contains(&x)
                    && (r.y as f64..=r.bottom() as f64).contains(&y),
                "{h:?} at ({x}, {y}) is outside {r:?}"
            );
        }
    }

    #[test]
    fn grab_at_prefers_a_resize_handle_over_the_body() {
        let tool: Box<dyn Tool> = Box::new(RectTool::new(rect(0, 0, 100, 100)));
        // Dead on the north-west corner.
        assert_eq!(
            grab_at(tool.as_ref(), 0.0, 0.0),
            Some(Grab::BoxHandle(BoxHandle::NW))
        );
        // Well inside → body move.
        assert_eq!(grab_at(tool.as_ref(), 50.0, 50.0), Some(Grab::Body));
        // Far outside → no grab at all, so the caller can fall through to other layers.
        assert_eq!(grab_at(tool.as_ref(), 500.0, 500.0), None);
    }

    #[test]
    fn grab_at_returns_endpoints_for_two_point_tools() {
        let tool: Box<dyn Tool> = Box::new(LineTool::new((0.0, 0.0), (100.0, 100.0)));
        assert_eq!(
            grab_at(tool.as_ref(), 0.0, 0.0),
            Some(Grab::Endpoint(Endpoint::From))
        );
        assert_eq!(
            grab_at(tool.as_ref(), 100.0, 100.0),
            Some(Grab::Endpoint(Endpoint::To))
        );
        // On the line but away from both ends.
        assert_eq!(grab_at(tool.as_ref(), 50.0, 50.0), Some(Grab::Body));
    }

    #[test]
    fn grab_at_finds_the_number_radius_grip() {
        let n = NumberTool::new((100.0, 100.0), 1, [1.0; 4]);
        let grip = (n.center.0 + n.radius, n.center.1);
        let tool: Box<dyn Tool> = Box::new(n);
        assert_eq!(
            grab_at(tool.as_ref(), grip.0, grip.1),
            Some(Grab::NumberRadius)
        );
        assert_eq!(grab_at(tool.as_ref(), 100.0, 100.0), Some(Grab::Body));
    }

    #[rstest]
    #[case(ToolKind::Crop)]
    #[case(ToolKind::Select)]
    fn non_layer_tools_are_never_grabbable(#[case] _kind: ToolKind) {
        // Crop and Select are modes, not layers — they never appear in `doc.layers`, so
        // `grab_at` must decline them rather than fabricating a Body grab.
        let tool: Box<dyn Tool> = Box::new(crate::annotate::tools::crop::CropTool::new(rect(
            0, 0, 10, 10,
        )));
        assert_eq!(grab_at(tool.as_ref(), 5.0, 5.0), None);
    }

    // --- layer mutation helpers -------------------------------------------

    #[test]
    fn set_box_bounds_resizes_every_box_backed_kind() {
        let target = rect(5, 6, 70, 80);
        let mut layers: Vec<Box<dyn Tool>> = vec![
            Box::new(RectTool::new(rect(0, 0, 10, 10))),
            Box::new(EllipseTool::new(rect(0, 0, 10, 10))),
            Box::new(HighlightTool {
                bounds: rect(0, 0, 10, 10),
                color: [1.0, 1.0, 0.0, 0.35],
            }),
            Box::new(RedactTool {
                bounds: rect(0, 0, 10, 10),
            }),
        ];
        for layer in &mut layers {
            set_box_bounds(layer, target);
            assert_eq!(layer.bounds(), target, "{:?} was not resized", layer.kind());
        }
    }

    #[test]
    fn set_box_bounds_ignores_kinds_with_no_box() {
        let mut line: Box<dyn Tool> = Box::new(LineTool::new((0.0, 0.0), (10.0, 10.0)));
        let before = line.bounds();
        set_box_bounds(&mut line, rect(100, 100, 5, 5));
        assert_eq!(line.bounds(), before);
    }

    #[test]
    fn set_pair_moves_both_endpoints_of_a_two_point_tool() {
        for mut layer in [
            Box::new(LineTool::new((0.0, 0.0), (1.0, 1.0))) as Box<dyn Tool>,
            Box::new(ArrowTool::new((0.0, 0.0), (1.0, 1.0))) as Box<dyn Tool>,
        ] {
            set_pair(&mut layer, (10.0, 20.0), (30.0, 40.0));
            assert_eq!(
                endpoints(layer.as_ref()),
                Some(((10.0, 20.0), (30.0, 40.0))),
                "{:?} endpoints were not set",
                layer.kind()
            );
        }
    }

    #[test]
    fn set_pair_ignores_kinds_without_endpoints() {
        let mut r: Box<dyn Tool> = Box::new(RectTool::new(rect(0, 0, 10, 10)));
        let before = r.bounds();
        set_pair(&mut r, (100.0, 100.0), (200.0, 200.0));
        assert_eq!(r.bounds(), before);
    }

    // --- apply_manipulation -----------------------------------------------
    //
    // The highest-risk logic in the canvas: every resize / move arm, exercised without a
    // widget. `pango_ctx` is None throughout; only the Text *resize* arm needs one.

    #[test]
    fn body_drag_translates_a_box_layer_by_the_total_delta() {
        let orig = rect(10, 10, 50, 50);
        let mut layer: Box<dyn Tool> = Box::new(RectTool::new(orig));
        let m = manip(Grab::Body, GrabGeometry::Box(orig), (0.0, 0.0));
        apply_manipulation(&mut layer, &m, (25.0, -5.0), None);
        assert_eq!(layer.bounds(), rect(35, 5, 50, 50));
    }

    #[test]
    fn body_drag_recomputes_from_the_snapshot_so_it_never_accumulates_drift() {
        let orig = rect(0, 0, 20, 20);
        let mut layer: Box<dyn Tool> = Box::new(RectTool::new(orig));
        let m = manip(Grab::Body, GrabGeometry::Box(orig), (0.0, 0.0));
        // Many intermediate positions, ending back where we started.
        for cur in [(3.0, 3.0), (17.0, 2.0), (100.0, 100.0), (0.0, 0.0)] {
            apply_manipulation(&mut layer, &m, cur, None);
        }
        assert_eq!(layer.bounds(), orig);
    }

    #[test]
    fn body_drag_translates_a_two_point_layer() {
        let mut layer: Box<dyn Tool> = Box::new(LineTool::new((0.0, 0.0), (10.0, 10.0)));
        let m = manip(
            Grab::Body,
            GrabGeometry::Pair {
                from: (0.0, 0.0),
                to: (10.0, 10.0),
            },
            (0.0, 0.0),
        );
        apply_manipulation(&mut layer, &m, (5.0, 7.0), None);
        assert_eq!(
            endpoints(layer.as_ref()),
            Some(((5.0, 7.0), (15.0, 17.0))),
            "both endpoints move together, preserving length and angle"
        );
    }

    #[test]
    fn body_drag_moves_a_number_center_without_changing_its_radius() {
        let n = NumberTool::new((50.0, 50.0), 3, [1.0; 4]);
        let radius = n.radius;
        let mut layer: Box<dyn Tool> = Box::new(n);
        let m = manip(
            Grab::Body,
            GrabGeometry::Center {
                center: (50.0, 50.0),
                radius,
            },
            (0.0, 0.0),
        );
        apply_manipulation(&mut layer, &m, (10.0, 20.0), None);
        let n = layer.as_any().downcast_ref::<NumberTool>().unwrap();
        assert_eq!(n.center, (60.0, 70.0));
        assert_eq!(n.radius, radius);
    }

    #[test]
    fn freehand_body_drag_translates_incrementally_from_the_last_position() {
        let mut layer: Box<dyn Tool> = Box::new(FreehandTool::new(
            vec![(0.0, 0.0), (10.0, 0.0)],
            [1.0; 4],
            StrokeStyle::Solid,
        ));
        // `last` — not `start` — is the reference for this arm, because the point list is
        // never snapshotted.
        let mut m = manip(Grab::Body, GrabGeometry::Origin((0.0, 0.0)), (0.0, 0.0));
        m.last = (0.0, 0.0);
        apply_manipulation(&mut layer, &m, (5.0, 5.0), None);
        m.last = (5.0, 5.0);
        apply_manipulation(&mut layer, &m, (8.0, 5.0), None);
        let f = layer.as_any().downcast_ref::<FreehandTool>().unwrap();
        assert_eq!(f.points, vec![(8.0, 5.0), (18.0, 5.0)]);
    }

    #[test]
    fn body_drag_moves_a_text_origin() {
        let mut layer: Box<dyn Tool> = Box::new(text_tool("hi"));
        let m = manip(
            Grab::Body,
            GrabGeometry::Text {
                origin: (10.0, 20.0),
                size_pt: 16.0,
                wrap_width: None,
                bounds: rect(10, 20, 100, 40),
            },
            (0.0, 0.0),
        );
        apply_manipulation(&mut layer, &m, (5.0, 5.0), None);
        let t = layer.as_any().downcast_ref::<TextTool>().unwrap();
        assert_eq!(t.origin, (15.0, 25.0));
    }

    #[rstest]
    // Dragging the SE corner outward grows the box; the NW corner stays put.
    #[case(BoxHandle::SE, (120.0, 130.0), rect(10, 10, 110, 120))]
    // Dragging the NW corner inward shrinks it; the SE corner stays put.
    #[case(BoxHandle::NW, (30.0, 40.0), rect(30, 40, 30, 20))]
    // A side handle only moves one axis.
    #[case(BoxHandle::E, (80.0, 999.0), rect(10, 10, 70, 50))]
    #[case(BoxHandle::N, (999.0, 20.0), rect(10, 20, 50, 40))]
    fn box_handle_drag_resizes_against_the_opposite_edge(
        #[case] handle: BoxHandle,
        #[case] cur: (f64, f64),
        #[case] expected: Rect,
    ) {
        let orig = rect(10, 10, 50, 50);
        let mut layer: Box<dyn Tool> = Box::new(RectTool::new(orig));
        let m = manip(
            Grab::BoxHandle(handle),
            GrabGeometry::Box(orig),
            (10.0, 10.0),
        );
        apply_manipulation(&mut layer, &m, cur, None);
        assert_eq!(layer.bounds(), expected);
    }

    #[test]
    fn dragging_a_handle_past_the_opposite_edge_flips_the_box_instead_of_inverting_it() {
        let orig = rect(10, 10, 50, 50);
        let mut layer: Box<dyn Tool> = Box::new(RectTool::new(orig));
        let m = manip(
            Grab::BoxHandle(BoxHandle::SE),
            GrabGeometry::Box(orig),
            (60.0, 60.0),
        );
        // Drag the SE corner far past the NW one.
        apply_manipulation(&mut layer, &m, (-100.0, -100.0), None);
        let b = layer.bounds();
        // Width/height are unsigned, so a correct implementation normalises rather than
        // underflowing.
        assert!(b.w > 0, "flipped box collapsed: {b:?}");
        assert!(b.h > 0, "flipped box collapsed: {b:?}");
        assert!(b.x < 10, "box did not flip past the anchor: {b:?}");
    }

    #[test]
    fn endpoint_drag_moves_only_the_grabbed_end() {
        let mut layer: Box<dyn Tool> = Box::new(LineTool::new((0.0, 0.0), (10.0, 10.0)));
        let geom = GrabGeometry::Pair {
            from: (0.0, 0.0),
            to: (10.0, 10.0),
        };
        let m = manip(Grab::Endpoint(Endpoint::To), geom, (10.0, 10.0));
        apply_manipulation(&mut layer, &m, (99.0, 3.0), None);
        let (from, to) = endpoints(layer.as_ref()).unwrap();
        assert_eq!(from, (0.0, 0.0), "the anchored end must not move");
        assert_eq!(to, (99.0, 3.0));

        let m = manip(Grab::Endpoint(Endpoint::From), geom, (0.0, 0.0));
        apply_manipulation(&mut layer, &m, (-4.0, -6.0), None);
        let (from, to) = endpoints(layer.as_ref()).unwrap();
        assert_eq!(from, (-4.0, -6.0));
        assert_eq!(to, (10.0, 10.0));
    }

    #[test]
    fn number_radius_drag_resizes_the_badge_without_moving_it() {
        let n = NumberTool::new((50.0, 50.0), 1, [1.0; 4]);
        let mut layer: Box<dyn Tool> = Box::new(n);
        let m = manip(
            Grab::NumberRadius,
            GrabGeometry::Center {
                center: (50.0, 50.0),
                radius: 10.0,
            },
            (60.0, 50.0),
        );
        apply_manipulation(&mut layer, &m, (90.0, 50.0), None);
        let n = layer.as_any().downcast_ref::<NumberTool>().unwrap();
        assert_eq!(n.center, (50.0, 50.0));
        assert_eq!(n.radius, select::new_radius((50.0, 50.0), 90.0, 50.0));
    }

    #[test]
    fn text_resize_without_a_pango_context_is_a_no_op() {
        // A missing display must leave the document untouched rather than panic or write a
        // stale bounds cache.
        let mut layer: Box<dyn Tool> = Box::new(text_tool("hello"));
        let before = format!("{layer:?}");
        let m = manip(
            Grab::BoxHandle(BoxHandle::SE),
            GrabGeometry::Text {
                origin: (10.0, 20.0),
                size_pt: 16.0,
                wrap_width: None,
                bounds: rect(10, 20, 100, 40),
            },
            (110.0, 60.0),
        );
        apply_manipulation(&mut layer, &m, (200.0, 200.0), None);
        assert_eq!(format!("{layer:?}"), before);
    }

    #[test]
    fn a_grab_that_does_not_match_its_geometry_is_ignored() {
        // Defensive: a NumberRadius grab against a Box snapshot is a wiring bug, and must
        // not corrupt the layer.
        let orig = rect(0, 0, 10, 10);
        let mut layer: Box<dyn Tool> = Box::new(RectTool::new(orig));
        let m = manip(Grab::NumberRadius, GrabGeometry::Box(orig), (0.0, 0.0));
        apply_manipulation(&mut layer, &m, (100.0, 100.0), None);
        assert_eq!(layer.bounds(), orig);
    }
}
