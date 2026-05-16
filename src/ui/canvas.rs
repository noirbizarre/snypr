//! Annotation canvas — a `GtkWidget` subclass that draws a [`Document`] via GSK render nodes.
//!
//! This widget is intentionally thin: all renderable math lives in [`crate::annotate::render`].

use std::cell::RefCell;
use std::rc::Rc;

use gtk4::glib;
use gtk4::prelude::*;
use gtk4::subclass::prelude::*;

use crate::annotate::Document;

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
        self.imp().doc.replace(Some(Rc::new(RefCell::new(doc))));
        self.queue_draw();
    }

    pub fn with_document<R>(&self, f: impl FnOnce(&mut Document) -> R) -> Option<R> {
        let doc = self.imp().doc.borrow();
        doc.as_ref().map(|d| f(&mut d.borrow_mut()))
    }
}

impl Default for AnnotationCanvas {
    fn default() -> Self {
        Self::new()
    }
}

mod imp {
    use super::*;

    #[derive(Default)]
    pub struct AnnotationCanvas {
        pub doc: RefCell<Option<Rc<RefCell<Document>>>>,
    }

    #[glib::object_subclass]
    impl ObjectSubclass for AnnotationCanvas {
        const NAME: &'static str = "HyprSnapAnnotationCanvas";
        type Type = super::AnnotationCanvas;
        type ParentType = gtk4::Widget;
    }

    impl ObjectImpl for AnnotationCanvas {}

    impl WidgetImpl for AnnotationCanvas {
        fn snapshot(&self, snapshot: &gtk4::Snapshot) {
            let Some(doc_rc) = self.doc.borrow().clone() else {
                return;
            };
            let doc = doc_rc.borrow();
            let width = doc.size.0 as f32;
            let height = doc.size.1 as f32;
            // Background — neutral gray when no base image is set.
            snapshot.append_color(
                &gdk4::RGBA::new(0.1, 0.1, 0.12, 1.0),
                &gtk4::graphene::Rect::new(0.0, 0.0, width, height),
            );
            // TODO: render base texture + each tool layer to the snapshot. The current shipped
            // code only paints the background so the editor at least opens; tool-specific GSK
            // nodes will be added as each tool is wired up.
        }
    }
}
