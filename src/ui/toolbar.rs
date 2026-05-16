//! Unified toolbar widget shared by the selector, editor, and draw overlay.
//!
//! A single `gtk4::Box` styled with `.hyprsnap-toolbar` hosts whichever combination of buttons
//! the caller asks for via [`ToolbarSpec`]. Actions are emitted as [`ToolbarAction`] values
//! through a callback registered with [`Toolbar::connect`]. Keyboard shortcuts mirror the
//! on-screen buttons so external key handlers don't need their own dispatch tables — see
//! [`Toolbar::install_shortcuts`].
//!
//! Design notes:
//!
//! * The toolbar owns no domain state — it just toggles its own UI and forwards actions. The
//!   caller is responsible for applying the action to a canvas / capture pipeline / selector.
//! * Tool and Mode buttons are radio-style (one active at a time) and share a `set_group` chain.
//! * `set_tool` / `set_mode` / `set_cursor` / `set_passthrough` exist so external state changes
//!   (key shortcuts, command-line defaults) can keep the visible toggles in lockstep.

use std::cell::RefCell;
use std::rc::Rc;

use gtk4::prelude::*;

use crate::annotate::ToolKind;

/// High-level mode picker used by the interactive selector. Resolved to a concrete
/// `Selection` by the caller after the user clicks Capture (or commits).
#[derive(Copy, Clone, Debug, PartialEq, Eq, Hash, Default)]
pub enum ModeKind {
    Full,
    Screen,
    Window,
    #[default]
    Region,
}

/// Description of a single tool button. Static so the same definition can be shared between
/// editor and overlay specs.
#[derive(Copy, Clone, Debug)]
pub struct ToolEntry {
    pub kind: ToolKind,
    pub label: &'static str,
    pub key: gdk4::Key,
    /// Freedesktop icon name shown next to the label. Falls back to label-only if the icon
    /// theme can't resolve it.
    pub icon: &'static str,
}

/// Description of a single mode button.
#[derive(Copy, Clone, Debug)]
pub struct ModeEntry {
    pub kind: ModeKind,
    pub label: &'static str,
    pub key: gdk4::Key,
    pub icon: &'static str,
}

/// All annotation tools available in the editor (capture + standalone annotate flows).
pub const EDITOR_TOOLS: &[ToolEntry] = &[
    ToolEntry {
        kind: ToolKind::Rect,
        label: "Rect",
        key: gdk4::Key::r,
        icon: "view-paged-symbolic",
    },
    ToolEntry {
        kind: ToolKind::Arrow,
        label: "Arrow",
        key: gdk4::Key::a,
        icon: "mail-forward-symbolic",
    },
    ToolEntry {
        kind: ToolKind::Highlight,
        label: "Highlight",
        key: gdk4::Key::h,
        icon: "marker-symbolic",
    },
    ToolEntry {
        kind: ToolKind::Freehand,
        label: "Freehand",
        key: gdk4::Key::f,
        icon: "document-edit-symbolic",
    },
    ToolEntry {
        kind: ToolKind::Number,
        label: "Number",
        key: gdk4::Key::n,
        icon: "zoom-original-symbolic",
    },
    ToolEntry {
        kind: ToolKind::Text,
        label: "Text",
        key: gdk4::Key::t,
        icon: "format-text-italic-symbolic",
    },
    ToolEntry {
        kind: ToolKind::Blur,
        label: "Blur",
        key: gdk4::Key::b,
        icon: "view-reveal-symbolic",
    },
    ToolEntry {
        kind: ToolKind::Redact,
        label: "Redact",
        key: gdk4::Key::x,
        icon: "view-conceal-symbolic",
    },
    ToolEntry {
        kind: ToolKind::Crop,
        label: "Crop",
        key: gdk4::Key::c,
        icon: "image-crop-symbolic",
    },
];

/// Tools surfaced in the live draw overlay. Crop and Blur don't make sense on an ephemeral
/// surface (no underlying pixels to crop or blur), so they're omitted.
pub const OVERLAY_TOOLS: &[ToolEntry] = &[
    ToolEntry {
        kind: ToolKind::Rect,
        label: "Rect",
        key: gdk4::Key::r,
        icon: "edit-select-all-symbolic",
        // icon: "view-paged-symbolic",
    },
    ToolEntry {
        kind: ToolKind::Arrow,
        label: "Arrow",
        key: gdk4::Key::a,
        icon: "mail-forward-symbolic",
    },
    ToolEntry {
        kind: ToolKind::Highlight,
        label: "Highlight",
        key: gdk4::Key::h,
        icon: "checkbox-symbolic",
    },
    ToolEntry {
        kind: ToolKind::Freehand,
        label: "Freehand",
        key: gdk4::Key::f,
        icon: "document-edit-symbolic",
    },
    ToolEntry {
        kind: ToolKind::Number,
        label: "Number",
        key: gdk4::Key::n,
        icon: "zoom-original-symbolic",
    },
    ToolEntry {
        kind: ToolKind::Text,
        label: "Text",
        key: gdk4::Key::t,
        icon: "format-text-italic-symbolic",
    },
    ToolEntry {
        kind: ToolKind::Redact,
        label: "Redact",
        key: gdk4::Key::x,
        icon: "view-conceal-symbolic",
    },
];

/// Mode picker shown in the selector toolbar (bottom-center floating).
pub const SELECTOR_MODES: &[ModeEntry] = &[
    ModeEntry {
        kind: ModeKind::Full,
        label: "Full",
        key: gdk4::Key::_1,
        icon: "view-fullscreen-symbolic",
    },
    ModeEntry {
        kind: ModeKind::Screen,
        label: "Screen",
        key: gdk4::Key::_2,
        icon: "video-display-symbolic",
    },
    ModeEntry {
        kind: ModeKind::Window,
        label: "Window",
        key: gdk4::Key::_3,
        icon: "preferences-system-windows-symbolic",
    },
    ModeEntry {
        kind: ModeKind::Region,
        label: "Region",
        key: gdk4::Key::_4,
        icon: "edit-select-all-symbolic",
    },
];

/// Per-view configuration. Sections appear left-to-right in the order: modes, tools, then the
/// trailing action group (cursor toggle, passthrough toggle, undo, clear, save, capture).
#[derive(Clone, Default)]
pub struct ToolbarSpec {
    pub tools: &'static [ToolEntry],
    pub modes: &'static [ModeEntry],
    pub show_undo: bool,
    pub show_clear: bool,
    pub show_save: bool,
    pub show_capture: bool,
    pub show_cursor_toggle: bool,
    pub show_passthrough_toggle: bool,
    pub initial_tool: Option<ToolKind>,
    pub initial_mode: Option<ModeKind>,
    pub initial_cursor: bool,
    pub initial_passthrough: bool,
}

impl ToolbarSpec {
    /// Empty spec; equivalent to `Default::default()` but ergonomically suggests using `..`.
    #[allow(dead_code)]
    pub const fn empty() -> Self {
        Self {
            tools: &[],
            modes: &[],
            show_undo: false,
            show_clear: false,
            show_save: false,
            show_capture: false,
            show_cursor_toggle: false,
            show_passthrough_toggle: false,
            initial_tool: None,
            initial_mode: None,
            initial_cursor: false,
            initial_passthrough: false,
        }
    }
}

/// Action emitted by the toolbar in response to a user interaction (or a matching keyboard
/// shortcut installed via [`Toolbar::install_shortcuts`]).
#[derive(Copy, Clone, Debug, PartialEq, Eq)]
pub enum ToolbarAction {
    ToolSelected(ToolKind),
    ModeSelected(ModeKind),
    CursorToggled(bool),
    PassthroughToggled(bool),
    Undo,
    Clear,
    Save,
    Capture,
}

type Callback = Rc<RefCell<Option<Box<dyn Fn(ToolbarAction) + 'static>>>>;

/// State shared between the widget, signal handlers, and the public setter API. We keep the
/// `ToggleButton`s in `Vec`s so `set_tool` / `set_mode` can flip them without re-emitting
/// `ToolbarAction`s (we use `block_signal` for that path).
struct ToolbarState {
    tools: Vec<(ToolKind, gtk4::ToggleButton, glib::SignalHandlerId)>,
    modes: Vec<(ModeKind, gtk4::ToggleButton, glib::SignalHandlerId)>,
    cursor: Option<(gtk4::ToggleButton, glib::SignalHandlerId)>,
    passthrough: Option<(gtk4::ToggleButton, glib::SignalHandlerId)>,
    shortcuts: Vec<Shortcut>,
    callback: Callback,
}

/// Lightweight description of a key shortcut so `install_shortcuts` can replay button clicks
/// from an external `EventControllerKey` (e.g. on the canvas widget).
struct Shortcut {
    key: gdk4::Key,
    action: ShortcutAction,
}

enum ShortcutAction {
    Tool(ToolKind),
    Mode(ModeKind),
    #[allow(dead_code)] // reserved for a future cursor-toggle key binding
    Cursor,
    Passthrough,
    Undo,
    Clear,
    Save,
    Capture,
}

/// Reusable toolbar widget. Cheap to clone — it holds an `Rc` to its internal state.
#[derive(Clone)]
pub struct Toolbar {
    widget: gtk4::Box,
    state: Rc<ToolbarState>,
}

impl Toolbar {
    pub fn new(spec: ToolbarSpec) -> Self {
        let widget = gtk4::Box::new(gtk4::Orientation::Horizontal, 6);
        widget.add_css_class("hyprsnap-toolbar");

        let callback: Callback = Rc::new(RefCell::new(None));
        let mut tools = Vec::new();
        let mut modes = Vec::new();
        let mut shortcuts = Vec::new();
        let mut cursor = None;
        let mut passthrough = None;

        // Mode buttons (left section).
        let mut mode_group: Option<gtk4::ToggleButton> = None;
        for entry in spec.modes {
            let btn = gtk4::ToggleButton::new();
            btn.set_child(Some(&icon_only(entry.icon)));
            btn.set_tooltip_text(Some(entry.label));
            make_unfocusable(&btn);
            if let Some(first) = &mode_group {
                btn.set_group(Some(first));
            } else {
                mode_group = Some(btn.clone());
            }
            if Some(entry.kind) == spec.initial_mode {
                btn.set_active(true);
            }
            let cb = callback.clone();
            let kind = entry.kind;
            let id = btn.connect_toggled(move |b| {
                if b.is_active()
                    && let Some(f) = cb.borrow().as_ref()
                {
                    f(ToolbarAction::ModeSelected(kind));
                }
            });
            widget.append(&btn);
            shortcuts.push(Shortcut {
                key: entry.key,
                action: ShortcutAction::Mode(entry.kind),
            });
            modes.push((entry.kind, btn, id));
        }

        if !spec.modes.is_empty() && !spec.tools.is_empty() {
            widget.append(&separator());
        }

        // Tool buttons (middle section).
        let mut tool_group: Option<gtk4::ToggleButton> = None;
        for entry in spec.tools {
            let btn = gtk4::ToggleButton::new();
            btn.set_child(Some(&icon_only(entry.icon)));
            btn.set_tooltip_text(Some(entry.label));
            make_unfocusable(&btn);
            if let Some(first) = &tool_group {
                btn.set_group(Some(first));
            } else {
                tool_group = Some(btn.clone());
            }
            if Some(entry.kind) == spec.initial_tool {
                btn.set_active(true);
            }
            let cb = callback.clone();
            let kind = entry.kind;
            let id = btn.connect_toggled(move |b| {
                if b.is_active()
                    && let Some(f) = cb.borrow().as_ref()
                {
                    f(ToolbarAction::ToolSelected(kind));
                }
            });
            widget.append(&btn);
            shortcuts.push(Shortcut {
                key: entry.key,
                action: ShortcutAction::Tool(entry.kind),
            });
            tools.push((entry.kind, btn, id));
        }

        // Trailing actions: separator + spacer + toggles + buttons.
        let trailing = spec.show_undo
            || spec.show_clear
            || spec.show_save
            || spec.show_capture
            || spec.show_cursor_toggle
            || spec.show_passthrough_toggle;
        if trailing && (!spec.modes.is_empty() || !spec.tools.is_empty()) {
            let spacer = gtk4::Box::new(gtk4::Orientation::Horizontal, 0);
            spacer.set_hexpand(true);
            widget.append(&spacer);
        }

        if spec.show_undo {
            let btn = gtk4::Button::new();
            btn.set_child(Some(&icon_only("edit-undo-symbolic")));
            btn.set_tooltip_text(Some("Undo (Ctrl+Z)"));
            make_unfocusable(&btn);
            let cb = callback.clone();
            btn.connect_clicked(move |_| {
                if let Some(f) = cb.borrow().as_ref() {
                    f(ToolbarAction::Undo);
                }
            });
            widget.append(&btn);
            shortcuts.push(Shortcut {
                key: gdk4::Key::z,
                action: ShortcutAction::Undo,
            });
        }

        if spec.show_clear {
            let btn = gtk4::Button::new();
            btn.set_child(Some(&icon_only("edit-clear-all-symbolic")));
            btn.set_tooltip_text(Some("Clear (Ctrl+L)"));
            make_unfocusable(&btn);
            let cb = callback.clone();
            btn.connect_clicked(move |_| {
                if let Some(f) = cb.borrow().as_ref() {
                    f(ToolbarAction::Clear);
                }
            });
            widget.append(&btn);
            shortcuts.push(Shortcut {
                key: gdk4::Key::l,
                action: ShortcutAction::Clear,
            });
        }

        if spec.show_cursor_toggle {
            let btn = gtk4::ToggleButton::new();
            btn.set_child(Some(&icon_only("input-mouse-symbolic")));
            btn.set_tooltip_text(Some("Include cursor in capture"));
            make_unfocusable(&btn);
            btn.set_active(spec.initial_cursor);
            let cb = callback.clone();
            let id = btn.connect_toggled(move |b| {
                if let Some(f) = cb.borrow().as_ref() {
                    f(ToolbarAction::CursorToggled(b.is_active()));
                }
            });
            widget.append(&btn);
            cursor = Some((btn, id));
        }

        if spec.show_passthrough_toggle {
            let btn = gtk4::ToggleButton::new();
            btn.set_child(Some(&icon_only("input-touchpad-symbolic")));
            btn.set_tooltip_text(Some("Toggle pointer passthrough (P)"));
            make_unfocusable(&btn);
            btn.set_active(spec.initial_passthrough);
            let cb = callback.clone();
            let id = btn.connect_toggled(move |b| {
                if let Some(f) = cb.borrow().as_ref() {
                    f(ToolbarAction::PassthroughToggled(b.is_active()));
                }
            });
            widget.append(&btn);
            shortcuts.push(Shortcut {
                key: gdk4::Key::p,
                action: ShortcutAction::Passthrough,
            });
            passthrough = Some((btn, id));
        }

        if spec.show_save {
            let btn = gtk4::Button::new();
            btn.set_child(Some(&icon_only("document-save-symbolic")));
            btn.set_tooltip_text(Some("Save (Ctrl+S)"));
            make_unfocusable(&btn);
            let cb = callback.clone();
            btn.connect_clicked(move |_| {
                if let Some(f) = cb.borrow().as_ref() {
                    f(ToolbarAction::Save);
                }
            });
            widget.append(&btn);
            // Ctrl+S is the conventional shortcut; install_shortcuts checks the modifier
            // for Save specifically (see install_shortcuts).
            shortcuts.push(Shortcut {
                key: gdk4::Key::s,
                action: ShortcutAction::Save,
            });
        }

        if spec.show_capture {
            let btn = gtk4::Button::new();
            btn.set_child(Some(&icon_only("camera-photo-symbolic")));
            btn.set_tooltip_text(Some("Capture (Enter)"));
            btn.add_css_class("suggested-action");
            make_unfocusable(&btn);
            let cb = callback.clone();
            btn.connect_clicked(move |_| {
                if let Some(f) = cb.borrow().as_ref() {
                    f(ToolbarAction::Capture);
                }
            });
            widget.append(&btn);
            shortcuts.push(Shortcut {
                key: gdk4::Key::Return,
                action: ShortcutAction::Capture,
            });
        }

        let state = Rc::new(ToolbarState {
            tools,
            modes,
            cursor,
            passthrough,
            shortcuts,
            callback,
        });

        Self { widget, state }
    }

    pub fn widget(&self) -> &gtk4::Widget {
        self.widget.upcast_ref()
    }

    pub fn connect<F>(&self, f: F)
    where
        F: Fn(ToolbarAction) + 'static,
    {
        self.state.callback.replace(Some(Box::new(f)));
    }

    /// Update the visible tool radio without firing a `ToolSelected` action.
    pub fn set_tool(&self, kind: ToolKind) {
        for (k, btn, id) in &self.state.tools {
            btn.block_signal(id);
            btn.set_active(*k == kind);
            btn.unblock_signal(id);
        }
    }

    /// Update the visible mode radio without firing a `ModeSelected` action.
    #[allow(dead_code)]
    pub fn set_mode(&self, kind: ModeKind) {
        for (k, btn, id) in &self.state.modes {
            btn.block_signal(id);
            btn.set_active(*k == kind);
            btn.unblock_signal(id);
        }
    }

    /// Update the cursor toggle without firing `CursorToggled`.
    #[allow(dead_code)]
    pub fn set_cursor(&self, on: bool) {
        if let Some((btn, id)) = &self.state.cursor {
            btn.block_signal(id);
            btn.set_active(on);
            btn.unblock_signal(id);
        }
    }

    /// Update the passthrough toggle without firing `PassthroughToggled`.
    pub fn set_passthrough(&self, on: bool) {
        if let Some((btn, id)) = &self.state.passthrough {
            btn.block_signal(id);
            btn.set_active(on);
            btn.unblock_signal(id);
        }
    }

    /// Install keyboard shortcuts on `target`. Each shortcut emits the same `ToolbarAction` as
    /// clicking the matching button — and, for radio-style sections, flips the active toggle
    /// so the on-screen state stays in sync with the keyboard.
    ///
    /// Special-cased modifier rules:
    /// * `Save` requires Ctrl.
    /// * `Undo` requires Ctrl.
    /// * `Clear` requires Ctrl (matches the existing overlay `Ctrl+L`).
    pub fn install_shortcuts(&self, target: &impl IsA<gtk4::Widget>) {
        let key = gtk4::EventControllerKey::new();
        let toolbar = self.clone();
        key.connect_key_pressed(move |_, k, _, state| {
            let ctrl = state.contains(gdk4::ModifierType::CONTROL_MASK);
            for sc in &toolbar.state.shortcuts {
                if !key_matches(sc.key, k) {
                    continue;
                }
                let needs_ctrl = matches!(
                    sc.action,
                    ShortcutAction::Save | ShortcutAction::Undo | ShortcutAction::Clear
                );
                if needs_ctrl != ctrl {
                    continue;
                }
                if toolbar.dispatch_shortcut(&sc.action) {
                    return glib::Propagation::Stop;
                }
            }
            glib::Propagation::Proceed
        });
        target.add_controller(key);
    }

    fn dispatch_shortcut(&self, action: &ShortcutAction) -> bool {
        match action {
            ShortcutAction::Tool(kind) => {
                // Activate the matching button: GTK will fire `toggled`, which forwards the
                // `ToolSelected` action through the callback for free.
                if let Some((_, btn, _)) = self.state.tools.iter().find(|(k, _, _)| k == kind) {
                    btn.set_active(true);
                    true
                } else {
                    false
                }
            }
            ShortcutAction::Mode(kind) => {
                if let Some((_, btn, _)) = self.state.modes.iter().find(|(k, _, _)| k == kind) {
                    btn.set_active(true);
                    true
                } else {
                    false
                }
            }
            ShortcutAction::Cursor => {
                if let Some((btn, _)) = &self.state.cursor {
                    btn.set_active(!btn.is_active());
                    true
                } else {
                    false
                }
            }
            ShortcutAction::Passthrough => {
                if let Some((btn, _)) = &self.state.passthrough {
                    btn.set_active(!btn.is_active());
                    true
                } else {
                    false
                }
            }
            ShortcutAction::Undo => self.emit(ToolbarAction::Undo),
            ShortcutAction::Clear => self.emit(ToolbarAction::Clear),
            ShortcutAction::Save => self.emit(ToolbarAction::Save),
            ShortcutAction::Capture => self.emit(ToolbarAction::Capture),
        }
    }

    fn emit(&self, action: ToolbarAction) -> bool {
        if let Some(f) = self.state.callback.borrow().as_ref() {
            f(action);
            true
        } else {
            false
        }
    }
}

/// Case-insensitive key match for letter shortcuts so Shift / CapsLock don't prevent the
/// toolbar from reacting. The `Return` shortcut also matches `KP_Enter`.
fn key_matches(expected: gdk4::Key, actual: gdk4::Key) -> bool {
    if expected == actual {
        return true;
    }
    if expected == gdk4::Key::Return && actual == gdk4::Key::KP_Enter {
        return true;
    }
    match (expected.to_unicode(), actual.to_unicode()) {
        (Some(a), Some(b)) => a.eq_ignore_ascii_case(&b),
        _ => false,
    }
}

fn separator() -> gtk4::Separator {
    let s = gtk4::Separator::new(gtk4::Orientation::Vertical);
    s.set_margin_start(2);
    s.set_margin_end(2);
    s
}

/// Build an icon-only child for a Button/ToggleButton. Labels live in the button's tooltip
/// (set by the caller). Buttons are also marked non-focusable so the toolbar never steals
/// keyboard focus from the window-level shortcut dispatcher.
fn icon_only(icon_name: &str) -> gtk4::Image {
    let image = gtk4::Image::from_icon_name(icon_name);
    image.set_icon_size(gtk4::IconSize::Normal);
    image
}

/// Strip focus traversal from a toolbar button so pressing Enter on the surface is dispatched
/// by the toolbar's installed `EventControllerKey` (or the window's own key handler) instead
/// of activating whichever button GTK happened to focus on present.
fn make_unfocusable<W: IsA<gtk4::Widget>>(w: &W) {
    w.set_focusable(false);
    w.set_can_focus(false);
}

#[cfg(test)]
mod tests {
    use super::*;
    use pretty_assertions::assert_eq;

    #[test]
    fn mode_kind_default_is_region() {
        assert_eq!(ModeKind::default(), ModeKind::Region);
    }

    #[test]
    fn editor_tool_table_covers_every_tool_kind() {
        // If a new ToolKind variant is added, this test reminds us to wire it into the editor
        // toolbar (or explicitly exclude it).
        let kinds: std::collections::HashSet<_> = EDITOR_TOOLS.iter().map(|e| e.kind).collect();
        for kind in [
            ToolKind::Rect,
            ToolKind::Arrow,
            ToolKind::Highlight,
            ToolKind::Freehand,
            ToolKind::Number,
            ToolKind::Text,
            ToolKind::Blur,
            ToolKind::Redact,
            ToolKind::Crop,
        ] {
            assert!(kinds.contains(&kind), "EDITOR_TOOLS missing {kind:?}");
        }
    }

    #[test]
    fn overlay_excludes_blur_and_crop() {
        let kinds: std::collections::HashSet<_> = OVERLAY_TOOLS.iter().map(|e| e.kind).collect();
        assert!(!kinds.contains(&ToolKind::Blur));
        assert!(!kinds.contains(&ToolKind::Crop));
    }
}
