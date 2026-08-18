//! Unified toolbar widget shared by the selector, editor, and draw overlay.
//!
//! A single `gtk4::Box` styled with `.snypr-toolbar` hosts whichever combination of buttons
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

use std::cell::{Cell, RefCell};
use std::rc::Rc;

use gtk4::prelude::*;

use crate::annotate::{StrokeStyle, ToolKind};
use crate::i18n::fl;
use crate::output::OutputMode;

/// Localized tooltip label for an annotation tool.
fn tool_label(kind: ToolKind) -> String {
    match kind {
        ToolKind::Select => fl!("toolbar-tool-select"),
        ToolKind::Rect => fl!("toolbar-tool-rect"),
        ToolKind::Ellipse => fl!("toolbar-tool-ellipse"),
        ToolKind::Arrow => fl!("toolbar-tool-arrow"),
        ToolKind::Line => fl!("toolbar-tool-line"),
        ToolKind::Highlight => fl!("toolbar-tool-highlight"),
        ToolKind::Freehand => fl!("toolbar-tool-freehand"),
        ToolKind::Number => fl!("toolbar-tool-number"),
        ToolKind::Text => fl!("toolbar-tool-text"),
        ToolKind::Blur => fl!("toolbar-tool-blur"),
        ToolKind::Redact => fl!("toolbar-tool-redact"),
        ToolKind::Crop => fl!("toolbar-tool-crop"),
    }
}

/// Localized tooltip label for a selector mode.
fn mode_label(kind: ModeKind) -> String {
    match kind {
        ModeKind::Full => fl!("toolbar-mode-full"),
        ModeKind::Screen => fl!("toolbar-mode-screen"),
        ModeKind::Window => fl!("toolbar-mode-window"),
        ModeKind::Region => fl!("toolbar-mode-region"),
    }
}

/// Localized tooltip label for an output destination.
fn output_mode_label(mode: OutputMode) -> String {
    match mode {
        OutputMode::File => fl!("toolbar-output-file"),
        OutputMode::Clipboard => fl!("toolbar-output-clipboard"),
        OutputMode::Both => fl!("toolbar-output-both"),
    }
}

/// High-level mode picker used by the interactive selector. Resolved to a concrete
/// `Selection` by the caller after the user clicks Capture (or commits).
#[derive(Copy, Clone, Debug, PartialEq, Eq, Hash, Default)]
pub enum ModeKind {
    Full,
    #[default]
    Screen,
    Window,
    Region,
}

impl From<crate::config::InitialMode> for ModeKind {
    fn from(value: crate::config::InitialMode) -> Self {
        match value {
            crate::config::InitialMode::Full => ModeKind::Full,
            crate::config::InitialMode::Screen => ModeKind::Screen,
            crate::config::InitialMode::Window => ModeKind::Window,
            crate::config::InitialMode::Region => ModeKind::Region,
        }
    }
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
/// `Select` mode has no button — it's the implicit "no tool active" state.
pub const EDITOR_TOOLS: &[ToolEntry] = &[
    ToolEntry {
        kind: ToolKind::Rect,
        label: "Rectangle",
        key: gdk4::Key::r,
        icon: "draw-rectangle-symbolic",
    },
    ToolEntry {
        kind: ToolKind::Ellipse,
        label: "Ellipse",
        key: gdk4::Key::o,
        icon: "draw-oval2-symbolic",
    },
    ToolEntry {
        kind: ToolKind::Arrow,
        label: "Arrow",
        key: gdk4::Key::a,
        icon: "arrow1-top-right-symbolic",
    },
    ToolEntry {
        kind: ToolKind::Line,
        label: "Line",
        key: gdk4::Key::l,
        icon: "draw-line-symbolic",
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
        icon: "lang-define-symbolic",
    },
    ToolEntry {
        kind: ToolKind::Text,
        label: "Text",
        key: gdk4::Key::t,
        icon: "text-insert-symbolic",
    },
    ToolEntry {
        kind: ToolKind::Blur,
        label: "Blur",
        key: gdk4::Key::b,
        icon: "blend-tool-symbolic",
    },
    ToolEntry {
        kind: ToolKind::Redact,
        label: "Redact",
        key: gdk4::Key::x,
        icon: "screen-privacy7-symbolic",
    },
    ToolEntry {
        kind: ToolKind::Crop,
        label: "Crop",
        key: gdk4::Key::c,
        icon: "ui-crop-to-selection-symbolic",
    },
];

/// Tools surfaced in the live draw overlay. Crop is omitted — it has no meaning without a
/// captured base (the overlay is ephemeral and saves via a fresh compositor capture). Blur is
/// included: when first used, the overlay grabs the underlying desktop into a hidden base so
/// the GSK blur node has real pixels to sample (see `AnnotationCanvas::set_hidden_base`).
pub const OVERLAY_TOOLS: &[ToolEntry] = &[
    ToolEntry {
        kind: ToolKind::Rect,
        label: "Rectangle",
        key: gdk4::Key::r,
        icon: "draw-rectangle-symbolic",
    },
    ToolEntry {
        kind: ToolKind::Ellipse,
        label: "Ellipse",
        key: gdk4::Key::o,
        icon: "draw-oval2-symbolic",
    },
    ToolEntry {
        kind: ToolKind::Arrow,
        label: "Arrow",
        key: gdk4::Key::a,
        icon: "arrow1-top-right-symbolic",
    },
    ToolEntry {
        kind: ToolKind::Line,
        label: "Line",
        key: gdk4::Key::l,
        icon: "draw-line-symbolic",
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
        icon: "lang-define-symbolic",
    },
    ToolEntry {
        kind: ToolKind::Text,
        label: "Text",
        key: gdk4::Key::t,
        icon: "text-insert-symbolic",
    },
    ToolEntry {
        kind: ToolKind::Blur,
        label: "Blur",
        key: gdk4::Key::b,
        icon: "blend-tool-symbolic",
    },
    ToolEntry {
        kind: ToolKind::Redact,
        label: "Redact",
        key: gdk4::Key::x,
        icon: "screen-privacy7-symbolic",
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
        icon: "overlapping-windows-symbolic",
    },
    ModeEntry {
        kind: ModeKind::Region,
        label: "Region",
        key: gdk4::Key::_4,
        icon: "tool-select-rectangle-symbolic",
    },
];

/// Per-view configuration. Sections appear left-to-right in the order: modes, tools, then the
/// trailing action group (cursor toggle, passthrough toggle, undo, clear, save, capture).
#[derive(Clone)]
pub struct ToolbarSpec {
    pub tools: &'static [ToolEntry],
    pub modes: &'static [ModeEntry],
    pub show_undo: bool,
    pub show_clear: bool,
    pub show_save: bool,
    /// Show a "Capture" action button. Shift+click (or Shift+Enter) routes the action through
    /// [`ToolbarAction::Annotate`] instead of [`ToolbarAction::Capture`]; the on-screen icon
    /// reflects the held Shift state live (only when shortcuts have been installed on a target
    /// window via [`Toolbar::install_shortcuts`]).
    pub show_capture: bool,
    /// When `show_capture` is true, control whether the Capture button honors the Shift
    /// modifier to switch to [`ToolbarAction::Annotate`]. Set to `false` from contexts where
    /// the captured region will not be re-annotated (e.g. the selector popped by the draw
    /// overlay's Save action — the user is already on an annotation surface). Defaults to
    /// `true`, matching the historical selector behavior.
    pub capture_shift_annotates: bool,
    pub show_cursor_toggle: bool,
    pub show_passthrough_toggle: bool,
    /// Show a numeric delay spinner (seconds) next to the cursor toggle. The toolbar emits
    /// [`ToolbarAction::DelayChanged`] when the user changes the value; the caller is
    /// responsible for storing the choice and applying it before the actual capture.
    pub show_delay_spinner: bool,
    /// Show a color-picker button (with alpha) that drives the color of the currently
    /// selected tool. The toolbar emits [`ToolbarAction::ColorChanged`] when the user
    /// picks a new color; the caller is responsible for storing the choice on whatever
    /// canvas state it owns.
    pub show_color_picker: bool,
    /// Show a stroke-style picker (Solid / Dashed / Dotted) that drives the dash
    /// pattern of the currently selected tool. The toolbar emits
    /// [`ToolbarAction::StrokeStyleChanged`] when the user picks a new style; the
    /// caller is responsible for storing the choice on whatever canvas state it owns.
    pub show_style_picker: bool,
    /// Show a font-size spinner that drives the point size of the currently selected
    /// tool, when that tool renders text (Text, currently). The toolbar emits
    /// [`ToolbarAction::FontSizeChanged`] when the user picks a new size; the caller
    /// is responsible for forwarding it to the canvas via `set_tool_font_size`.
    pub show_font_size_picker: bool,
    /// Show a cycling output-destination button (file → clipboard → both) next to Save. The
    /// toolbar emits [`ToolbarAction::OutputModeChanged`]; the caller owns the sink list and
    /// is responsible for applying the new destination before the next save. Only meaningful
    /// alongside `show_save`.
    pub show_output_switcher: bool,
    pub initial_tool: Option<ToolKind>,
    pub initial_mode: Option<ModeKind>,
    pub initial_cursor: bool,
    pub initial_passthrough: bool,
    /// Initial value (in whole seconds) for the delay spinner. Ignored when
    /// `show_delay_spinner` is false. Common values are 0, 3, and 10.
    pub initial_delay_secs: u32,
    /// Initial state of the output switcher, derived by the caller from the sinks resolved
    /// from `--to` / `[output].default_sinks`. Ignored when `show_output_switcher` is false.
    pub initial_output_mode: OutputMode,
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
            capture_shift_annotates: true,
            show_cursor_toggle: false,
            show_passthrough_toggle: false,
            show_delay_spinner: false,
            show_color_picker: false,
            show_style_picker: false,
            show_font_size_picker: false,
            show_output_switcher: false,
            initial_tool: None,
            initial_mode: None,
            initial_cursor: false,
            initial_passthrough: false,
            initial_delay_secs: 0,
            initial_output_mode: OutputMode::File,
        }
    }
}

impl Default for ToolbarSpec {
    // `capture_shift_annotates` defaults to `true` to preserve the historical selector
    // behavior where Shift+click / Shift+Enter on Capture opens the annotation editor.
    // Callers that don't want that (the draw overlay's Save → selector flow) flip it to
    // `false` explicitly.
    fn default() -> Self {
        Self::empty()
    }
}

/// Action emitted by the toolbar in response to a user interaction (or a matching keyboard
/// shortcut installed via [`Toolbar::install_shortcuts`]).
#[derive(Copy, Clone, Debug, PartialEq)]
pub enum ToolbarAction {
    ToolSelected(ToolKind),
    ModeSelected(ModeKind),
    CursorToggled(bool),
    PassthroughToggled(bool),
    /// User changed the delay (in whole seconds) on the selector toolbar's spinner.
    DelayChanged(u32),
    Undo,
    Clear,
    Save,
    Capture,
    /// Variant of `Capture` emitted when the user holds Shift (Shift-click on the Capture
    /// button, or Shift+Enter / Shift+KP_Enter). Asks the caller to open the annotation
    /// editor on the captured image.
    Annotate,
    /// User picked a new color from the color picker. The caller applies it to the
    /// currently active tool (whichever it tracks).
    ColorChanged([f32; 4]),
    /// User picked a new stroke style from the style picker. The caller applies it
    /// to the currently active tool's `stroke_style` (analogous to `ColorChanged`).
    StrokeStyleChanged(StrokeStyle),
    /// User picked a new font size (in points) from the font-size spinner. The caller
    /// applies it to the currently active tool's `size_pt` (analogous to
    /// `StrokeStyleChanged`). Only meaningful for text-rendering tools.
    FontSizeChanged(f32),
    /// User cycled the output-destination switcher (button click or Ctrl+O). The caller
    /// applies the new destination to whatever sink state it owns; the toolbar has already
    /// updated its own icon and tooltip.
    OutputModeChanged(OutputMode),
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
    /// Capture-delay spinner. Populated only when `show_delay_spinner` is true. We keep the
    /// `value-changed` handler id so external state updates ([`Toolbar::set_delay`]) can
    /// `block_signal` while writing the new value to avoid re-emitting `DelayChanged`.
    delay: Option<DelaySpinnerUi>,
    /// Capture-button visuals + live-shift tracking. Populated only when `show_capture` is
    /// true. `install_shortcuts` updates `shift_held` and re-skins the icon/tooltip as Shift
    /// is pressed/released so users get visual feedback before clicking.
    capture: Option<Rc<CaptureUi>>,
    /// Color-picker UI (button + popover + inline chooser). We use an inline
    /// `ColorChooserWidget` hosted in a `Popover` rather than the modern `ColorDialog`
    /// because `ColorDialog` opens as a new `xdg_toplevel`, which can't receive pointer
    /// or keyboard input while a layer-shell parent holds `KeyboardMode::Exclusive`.
    /// Popovers are popup surfaces of the parent layer-shell surface and work fine.
    color: Option<ColorPickerUi>,
    /// Stroke-style picker UI (button + popover with three radios). Same lifecycle
    /// model as `color`. Built only when [`ToolbarSpec::show_style_picker`] is set.
    style: Option<StylePickerUi>,
    /// Font-size spinner UI. Same lifecycle model as `color` / `style`. Built only
    /// when [`ToolbarSpec::show_font_size_picker`] is set.
    font_size: Option<FontSizePickerUi>,
    /// Output-destination switcher. Built only when [`ToolbarSpec::show_output_switcher`] is
    /// set. Unlike the toggle-based controls it is a plain `Button`, so external updates via
    /// [`Toolbar::set_output_mode`] need no signal blocking.
    output: Option<OutputSwitcherUi>,
    shortcuts: Vec<Shortcut>,
    callback: Callback,
}

/// Live state for the Capture button so [`Toolbar::install_shortcuts`] can swap its icon and
/// tooltip when Shift is held.
struct CaptureUi {
    button: gtk4::Button,
    icon: gtk4::Image,
    shift_held: Rc<Cell<bool>>,
    /// The output-destination switcher, when the same toolbar carries one. Dimmed while
    /// Shift is held — see the construction site for why.
    output: Option<gtk4::Button>,
}

/// Live state for the output-destination switcher. A single `Button` whose child is a small
/// horizontal box of icons, rebuilt on every state change: one icon for `File` / `Clipboard`,
/// two side by side for `Both`. Composing icons at runtime avoids vendoring a third
/// "file+clipboard" SVG and keeps the meaning of the combined state obvious.
struct OutputSwitcherUi {
    button: gtk4::Button,
    /// Container for the state icons. Emptied and refilled by [`OutputSwitcherUi::apply`].
    icons: gtk4::Box,
    /// Current destination. Source of truth for the control.
    mode: Rc<Cell<OutputMode>>,
}

/// Icon names shown for a destination, in display order.
fn output_mode_icons(mode: OutputMode) -> &'static [&'static str] {
    match mode {
        OutputMode::File => &["document-save-symbolic"],
        OutputMode::Clipboard => &["edit-copy-symbolic"],
        OutputMode::Both => &["document-save-symbolic", "edit-copy-symbolic"],
    }
}

/// Re-skin the switcher's button for `mode`. Free function so the click handler can call it
/// without holding a reference to the (not yet constructed) `ToolbarState`.
fn apply_output_mode(button: &gtk4::Button, icons: &gtk4::Box, mode: OutputMode) {
    while let Some(child) = icons.first_child() {
        icons.remove(&child);
    }
    for name in output_mode_icons(mode) {
        icons.append(&icon_only(name));
    }
    button.set_tooltip_text(Some(&output_mode_label(mode)));
}

/// Live state for the color picker. Stored on `ToolbarState` so external callers can update
/// the swatch silently when the active tool changes ([`Toolbar::set_color`]) or enable /
/// disable the button for tools with hardcoded appearance
/// ([`Toolbar::set_color_picker_sensitive`]).
///
/// Picking uses the modern `gtk4::ColorDialog` (full HSV picker + presets + custom-color
/// editor). The dialog opens as an `xdg_toplevel`, which on Wayland can't receive input
/// while a layer-shell parent surface holds `KeyboardMode::Exclusive` — so when the parent
/// is a layer-shell window, the button click handler temporarily switches it to
/// `KeyboardMode::OnDemand` and restores the previous mode from the dialog's completion
/// callback. Non-layer-shell parents (e.g. the standalone annotation editor) don't need
/// any of that.
struct ColorPickerUi {
    button: gtk4::Button,
    swatch: gtk4::DrawingArea,
    current: Rc<Cell<gdk4::RGBA>>,
}

/// Live state for the stroke-style picker. Three grouped `ToggleButton`s laid
/// out horizontally as an inline segmented control. We previously used a
/// `MenuButton` + `Popover` here, but popover children on layer-shell windows
/// holding `KeyboardMode::Exclusive` don't reliably receive pointer events on
/// Hyprland — the click was routed as "outside the popover", auto-closing it
/// without ever reaching the toggles. Inlining the toggles avoids the popup
/// surface entirely.
struct StylePickerUi {
    /// Container holding the three toggle buttons. Used by
    /// [`Toolbar::set_style_picker_sensitive`] to enable/disable the whole group.
    container: gtk4::Box,
    /// Three `(style, toggle, handler)` tuples — kept in declaration order so
    /// `set_stroke_style` can flip the right toggle without re-emitting the
    /// `StrokeStyleChanged` action (via `block_signal`).
    toggles: Vec<(StrokeStyle, gtk4::ToggleButton, glib::SignalHandlerId)>,
}

/// Live state for the font-size picker. An inline segmented group of three non-focusable
/// widgets `[−] [18pt] [+]`, matching [`DelaySpinnerUi`]. A `SpinButton` was avoided here for
/// the same reason: its internal `GtkText` grabs keyboard focus on a layer-shell surface,
/// which would break the in-canvas text editor (typing would stop driving the pending text).
/// The segmented group never takes focus, so the canvas keeps it throughout.
struct FontSizePickerUi {
    /// Label between the `−` / `+` buttons, displays the current value as "18pt".
    label: gtk4::Label,
    /// Current value in whole points. Source of truth for the control.
    value: Rc<Cell<u32>>,
    /// The segmented container, toggled by [`Toolbar::set_font_size_picker_sensitive`].
    container: gtk4::Box,
}

/// Live state for the capture-delay control. An inline segmented group of three
/// non-focusable widgets: `[−] [3s] [+]`. We previously tried both an inline
/// `SpinButton` (its internal `GtkText` grabbed focus on a layer-shell selector
/// surface holding `KeyboardMode::Exclusive`, wedging pointer dispatch to the
/// sibling Capture button) and a `ToggleButton` opening a popover around the
/// same `SpinButton` (popover autohide treats clicks inside the spinner as
/// outside on Hyprland layer-shell, dismissing without applying — the same
/// limitation that pushed the stroke-style picker to inline toggles). The
/// segmented group avoids both classes of bug while still giving precise
/// control: left-click steps by 1 s, right-click steps by 5 s, scroll wheel
/// adjusts smoothly. The value clamps to the same 0–60 s range the spinner
/// used.
struct DelaySpinnerUi {
    /// Label between the `−` / `+` buttons, displays the current value as "3s".
    label: gtk4::Label,
    /// Current value in whole seconds. Source of truth for the control — both
    /// buttons and the scroll handler mutate this and then refresh the label.
    value: Rc<Cell<u32>>,
}

impl CaptureUi {
    fn apply_shift(&self, shift: bool) {
        if self.shift_held.get() == shift {
            return;
        }
        self.shift_held.set(shift);
        if shift {
            self.icon.set_icon_name(Some("document-edit-symbolic"));
            self.button
                .set_tooltip_text(Some(&fl!("toolbar-annotate-tooltip")));
        } else {
            self.icon.set_icon_name(Some("camera-photo-symbolic"));
            self.button
                .set_tooltip_text(Some(&fl!("toolbar-capture-tooltip-shift")));
        }
        if let Some(output) = &self.output {
            output.set_sensitive(!shift);
        }
    }
}

/// Lightweight description of a key shortcut so `install_shortcuts` can replay button clicks
/// from an external `EventControllerKey` (e.g. on the canvas widget).
struct Shortcut {
    key: gdk4::Key,
    action: ShortcutAction,
    /// Required modifier mask. The dispatcher matches only when the pressed key's modifier
    /// state — restricted to Ctrl/Shift/Alt — equals this set exactly. Default
    /// `ModifierType::empty()` means "no modifier".
    modifiers: gdk4::ModifierType,
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
    Annotate,
    /// Cycle the output destination (Ctrl+O). Dispatched by clicking the switcher button so
    /// the visual state and the emitted action stay on a single code path.
    OutputCycle,
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
        widget.add_css_class("snypr-toolbar");

        let callback: Callback = Rc::new(RefCell::new(None));
        let mut tools = Vec::new();
        let mut modes = Vec::new();
        let mut shortcuts = Vec::new();
        let mut cursor = None;
        let mut passthrough = None;
        let mut capture = None;
        let mut delay = None;
        let mut output = None;

        // Mode buttons (left section).
        let mut mode_group: Option<gtk4::ToggleButton> = None;
        for entry in spec.modes {
            let btn = gtk4::ToggleButton::new();
            btn.set_child(Some(&icon_only(entry.icon)));
            btn.set_tooltip_text(Some(&mode_label(entry.kind)));
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
                modifiers: gdk4::ModifierType::empty(),
            });
            modes.push((entry.kind, btn, id));
        }

        if !spec.modes.is_empty() && !spec.tools.is_empty() {
            widget.append(&separator());
        }

        // Tool buttons (middle section). These are independent (non-grouped) toggle buttons
        // rather than a radio group: clicking the active tool toggles it *off*, which means
        // "no drawing tool" — i.e. Select mode. We enforce single-selection manually so at
        // most one tool button is ever active, and emit `ToolSelected(Select)` when a button
        // is toggled off (the user clicked the already-active tool).
        //
        // Built in two passes: first create every button (so each toggled handler can see all
        // siblings via a shared list), then connect the handlers and record their ids.
        let tool_btns: Rc<Vec<(ToolKind, gtk4::ToggleButton)>> = Rc::new(
            spec.tools
                .iter()
                .map(|entry| {
                    let btn = gtk4::ToggleButton::new();
                    btn.set_child(Some(&icon_only(entry.icon)));
                    btn.set_tooltip_text(Some(&tool_label(entry.kind)));
                    make_unfocusable(&btn);
                    if Some(entry.kind) == spec.initial_tool {
                        btn.set_active(true);
                    }
                    widget.append(&btn);
                    (entry.kind, btn)
                })
                .collect(),
        );
        for (entry, (kind, btn)) in spec.tools.iter().zip(tool_btns.iter()) {
            let cb = callback.clone();
            let kind = *kind;
            let siblings = tool_btns.clone();
            let id = btn.connect_toggled(move |b| {
                if b.is_active() {
                    // Deactivate every other tool so only this one stays lit. Block each
                    // sibling's handler so the deactivation doesn't recurse into an emit.
                    for (k, other) in siblings.iter() {
                        if *k != kind && other.is_active() {
                            other.set_active(false);
                        }
                    }
                    if let Some(f) = cb.borrow().as_ref() {
                        f(ToolbarAction::ToolSelected(kind));
                    }
                } else if !any_active(&siblings) {
                    // Toggled off (clicked the active tool) and nothing else lit → Select mode.
                    if let Some(f) = cb.borrow().as_ref() {
                        f(ToolbarAction::ToolSelected(ToolKind::Select));
                    }
                }
            });
            shortcuts.push(Shortcut {
                key: entry.key,
                action: ShortcutAction::Tool(entry.kind),
                modifiers: gdk4::ModifierType::empty(),
            });
            tools.push((entry.kind, btn.clone(), id));
        }

        // Color picker (optional) — sits right after the tool radios so it visually belongs
        // to the tool group it modifies.
        //
        // We use the modern `gtk4::ColorDialog` for the actual picking UI: it has the full
        // HSV picker, presets, custom-color editor, and works correctly across themes.
        // The dialog opens as a new `xdg_toplevel`; on Wayland this means it can't receive
        // input while a layer-shell parent surface holds `KeyboardMode::Exclusive`. The
        // workaround lives in the button's click handler: it walks to the root window,
        // and — if that window is a layer-shell window — temporarily switches the keyboard
        // mode to `OnDemand` for the duration of the dialog. The dialog's completion
        // callback restores the previous mode. Non-layer-shell parents (the standalone
        // annotation editor's `gtk4::Window`) skip the swap and use the dialog directly.
        //
        // The visible trigger is a plain `gtk4::Button` with a 16×16 `DrawingArea` swatch
        // as its child, sized to match the icon-only tool buttons so the toolbar stays
        // visually uniform. The swatch paints a checkerboard background + the current
        // color on top, so translucent colors are distinguishable from greys.
        let color = if spec.show_color_picker {
            if !spec.tools.is_empty() {
                widget.append(&separator());
            }
            let initial = array_to_rgba([1.0, 0.0, 0.0, 1.0]);
            let current = Rc::new(Cell::new(initial));

            // Trigger-button swatch. We pin both `content_*` (natural size) and
            // `size_request` (hard minimum) plus center alignment — without those the
            // DrawingArea expands to fill the button's allocation and renders as a
            // vertical rectangle when the button is taller than 16 px (which it is,
            // thanks to the icon-button padding inherited from the theme).
            let swatch = gtk4::DrawingArea::new();
            swatch.set_content_width(16);
            swatch.set_content_height(16);
            swatch.set_size_request(16, 16);
            swatch.set_halign(gtk4::Align::Center);
            swatch.set_valign(gtk4::Align::Center);
            swatch.set_hexpand(false);
            swatch.set_vexpand(false);
            let current_for_draw = current.clone();
            swatch.set_draw_func(move |_, cr, w, h| {
                draw_color_swatch(cr, w as f64, h as f64, current_for_draw.get());
            });

            let btn = gtk4::Button::new();
            btn.set_child(Some(&swatch));
            btn.set_tooltip_text(Some(&fl!("toolbar-color-tooltip")));
            make_unfocusable(&btn);

            // On click: walk to the root window, temporarily relax layer-shell keyboard
            // mode if applicable, open the dialog, restore the mode + emit ColorChanged in
            // the completion callback. The dialog is rebuilt per click so each open uses
            // the latest current color as its initial value.
            let btn_for_click = btn.clone();
            let current_for_click = current.clone();
            let swatch_for_click = swatch.clone();
            let cb = callback.clone();
            btn.connect_clicked(move |_| {
                open_color_dialog(
                    &btn_for_click,
                    current_for_click.clone(),
                    swatch_for_click.clone(),
                    cb.clone(),
                );
            });

            widget.append(&btn);
            Some(ColorPickerUi {
                button: btn,
                swatch,
                current,
            })
        } else {
            None
        };

        // Stroke-style picker (optional) — sits next to the color picker because it
        // shares the same "modifies the active tool's appearance" semantics.
        //
        // Three grouped `ToggleButton`s in a horizontal `linked` Box, inlined directly
        // into the toolbar (no popover). Popovers on layer-shell parents holding
        // `KeyboardMode::Exclusive` proved unreliable on Hyprland: clicks on popover
        // children were sometimes routed as "outside the popover", auto-closing it
        // without ever dispatching to the toggle. Inlining removes the popup surface
        // entirely and is also one fewer click to change style.
        let style = if spec.show_style_picker {
            // Separator only if we didn't already add one for the color picker (the
            // color picker emits its own leading separator when tools precede it).
            if color.is_none() && !spec.tools.is_empty() {
                widget.append(&separator());
            }

            let container = gtk4::Box::new(gtk4::Orientation::Horizontal, 0);
            container.add_css_class("linked");

            let mut toggles: Vec<(StrokeStyle, gtk4::ToggleButton, glib::SignalHandlerId)> =
                Vec::new();
            let mut toggle_group: Option<gtk4::ToggleButton> = None;
            for (style, tooltip_key) in [
                (StrokeStyle::Solid, "toolbar-stroke-solid"),
                (StrokeStyle::Dashed, "toolbar-stroke-dashed"),
                (StrokeStyle::Dotted, "toolbar-stroke-dotted"),
            ] {
                let toggle = gtk4::ToggleButton::new();
                let sample = gtk4::DrawingArea::new();
                // Square sample so the toggle button itself renders square (button size
                // is driven by its child + theme padding). The sample is wide enough for
                // the dash pattern to read as dashes/dots, not as one long line.
                sample.set_content_width(20);
                sample.set_content_height(20);
                sample.set_size_request(20, 20);
                sample.set_draw_func(move |_, cr, w, h| {
                    draw_style_swatch(cr, w as f64, h as f64, style);
                });
                toggle.set_child(Some(&sample));
                let tooltip = match tooltip_key {
                    "toolbar-stroke-solid" => fl!("toolbar-stroke-solid"),
                    "toolbar-stroke-dashed" => fl!("toolbar-stroke-dashed"),
                    _ => fl!("toolbar-stroke-dotted"),
                };
                toggle.set_tooltip_text(Some(&tooltip));
                make_unfocusable(&toggle);
                if let Some(first) = &toggle_group {
                    toggle.set_group(Some(first));
                } else {
                    toggle_group = Some(toggle.clone());
                }
                if style == StrokeStyle::Solid {
                    toggle.set_active(true);
                }
                let cb = callback.clone();
                let id = toggle.connect_toggled(move |t| {
                    // GtkToggleButton group semantics fire `toggled` on both the newly
                    // deactivated and newly activated buttons; only react to activation
                    // so we don't emit a spurious `StrokeStyleChanged` for the old style.
                    if !t.is_active() {
                        return;
                    }
                    if let Some(f) = cb.borrow().as_ref() {
                        f(ToolbarAction::StrokeStyleChanged(style));
                    }
                });
                container.append(&toggle);
                toggles.push((style, toggle, id));
            }

            widget.append(&container);
            Some(StylePickerUi { container, toggles })
        } else {
            None
        };

        // Font-size control (optional) — shown when the active tool renders text. Inline
        // segmented group `[−] [18pt] [+]`, mirroring the delay control. A `SpinButton` is
        // deliberately avoided: its internal `GtkText` grabs keyboard focus on a layer-shell
        // surface, which would stop the in-canvas text editor from receiving keystrokes.
        // Left-click steps by 1pt, right-click by 5pt, scroll wheel by 1pt; range 6–200pt.
        let font_size = if spec.show_font_size_picker {
            const FONT_MIN: u32 = 6;
            const FONT_MAX: u32 = 200;
            const STEP_PRIMARY: i32 = 1;
            const STEP_SECONDARY: i32 = 5;

            // Only emit a leading separator when neither the color nor the style
            // picker already provided one (they each emit their own when no earlier
            // picker has).
            if color.is_none() && style.is_none() && !spec.tools.is_empty() {
                widget.append(&separator());
            }

            let value = Rc::new(Cell::new(18u32));

            let group = gtk4::Box::new(gtk4::Orientation::Horizontal, 0);
            group.add_css_class("linked");
            group.set_tooltip_text(Some(&fl!("toolbar-font-size-tooltip")));

            let minus = gtk4::Button::new();
            minus.set_child(Some(&icon_only("list-remove-symbolic")));
            make_unfocusable(&minus);

            let label = gtk4::Label::new(Some(&format_font_size_label(value.get())));
            // Pin a width so the label doesn't reflow ("6pt" vs "200pt"); 5 chars covers
            // "200pt" comfortably.
            label.set_width_chars(5);
            label.set_xalign(0.5);
            label.set_can_target(false);

            let plus = gtk4::Button::new();
            plus.set_child(Some(&icon_only("list-add-symbolic")));
            make_unfocusable(&plus);

            group.append(&minus);
            group.append(&label);
            group.append(&plus);

            let emit = {
                let value = value.clone();
                let label = label.clone();
                let cb = callback.clone();
                move |delta: i32| {
                    let current = value.get() as i32;
                    let next = (current + delta).clamp(FONT_MIN as i32, FONT_MAX as i32) as u32;
                    if next == value.get() {
                        return;
                    }
                    value.set(next);
                    label.set_text(&format_font_size_label(next));
                    if let Some(f) = cb.borrow().as_ref() {
                        f(ToolbarAction::FontSizeChanged(next as f32));
                    }
                }
            };

            let make_gesture = |delta_primary: i32, delta_secondary: i32| {
                let gesture = gtk4::GestureClick::new();
                gesture.set_button(0);
                let emit_for_gesture = emit.clone();
                gesture.connect_pressed(move |g, _, _, _| {
                    let delta = if g.current_button() == gdk4::BUTTON_SECONDARY {
                        delta_secondary
                    } else {
                        delta_primary
                    };
                    emit_for_gesture(delta);
                });
                gesture
            };
            minus.add_controller(make_gesture(-STEP_PRIMARY, -STEP_SECONDARY));
            plus.add_controller(make_gesture(STEP_PRIMARY, STEP_SECONDARY));

            let scroll =
                gtk4::EventControllerScroll::new(gtk4::EventControllerScrollFlags::VERTICAL);
            let emit_for_scroll = emit.clone();
            scroll.connect_scroll(move |_, _dx, dy| {
                if dy < 0.0 {
                    emit_for_scroll(STEP_PRIMARY);
                } else if dy > 0.0 {
                    emit_for_scroll(-STEP_PRIMARY);
                }
                glib::Propagation::Stop
            });
            group.add_controller(scroll);

            widget.append(&group);
            Some(FontSizePickerUi {
                label,
                value,
                container: group,
            })
        } else {
            None
        };

        // Trailing actions: separator + spacer + toggles + buttons.
        let trailing = spec.show_undo
            || spec.show_clear
            || spec.show_save
            || spec.show_capture
            || spec.show_cursor_toggle
            || spec.show_passthrough_toggle
            || spec.show_delay_spinner
            || spec.show_output_switcher;
        if trailing && (!spec.modes.is_empty() || !spec.tools.is_empty()) {
            let spacer = gtk4::Box::new(gtk4::Orientation::Horizontal, 0);
            spacer.set_hexpand(true);
            widget.append(&spacer);
        }

        if spec.show_undo {
            let btn = gtk4::Button::new();
            btn.set_child(Some(&icon_only("edit-undo-symbolic")));
            btn.set_tooltip_text(Some(&fl!("toolbar-undo-tooltip")));
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
                modifiers: gdk4::ModifierType::CONTROL_MASK,
            });
        }

        if spec.show_clear {
            let btn = gtk4::Button::new();
            btn.set_child(Some(&icon_only("edit-clear-all-symbolic")));
            btn.set_tooltip_text(Some(&fl!("toolbar-clear-tooltip")));
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
                modifiers: gdk4::ModifierType::CONTROL_MASK,
            });
        }

        if spec.show_cursor_toggle {
            let btn = gtk4::ToggleButton::new();
            btn.set_child(Some(&icon_only("pointer-primary-click-symbolic")));
            btn.set_tooltip_text(Some(&fl!("toolbar-cursor-tooltip")));
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

        // Capture-delay control (selector only). Three-widget segmented group:
        // `[−] [3s] [+]`. See [`DelaySpinnerUi`] for the rationale for not using a
        // SpinButton (focus-trap on layer-shell) or a popover (autohide quirks).
        // Value range 0–60 s; left-click steps by 1 s, right-click by 5 s, scroll
        // wheel adjusts by the same step.
        if spec.show_delay_spinner {
            const DELAY_MIN: u32 = 0;
            const DELAY_MAX: u32 = 60;
            const STEP_PRIMARY: i32 = 1;
            const STEP_SECONDARY: i32 = 5;

            let initial_secs = spec.initial_delay_secs.clamp(DELAY_MIN, DELAY_MAX);
            let value = Rc::new(Cell::new(initial_secs));

            // Container is a horizontal Box styled as a single linked unit. Same
            // visual pattern as the stroke-style picker.
            let group = gtk4::Box::new(gtk4::Orientation::Horizontal, 0);
            group.add_css_class("linked");
            group.set_tooltip_text(Some(&fl!("toolbar-delay-tooltip")));

            let minus = gtk4::Button::new();
            minus.set_child(Some(&icon_only("list-remove-symbolic")));
            make_unfocusable(&minus);

            let label = gtk4::Label::new(Some(&format_delay_label(initial_secs)));
            // Pin a width so the label doesn't reflow as values change ("0s" vs
            // "60s" differ by ~1.5 char). 3 chars covers "60s" comfortably.
            label.set_width_chars(3);
            label.set_xalign(0.5);
            // The label sits on the toolbar surface but must not eat clicks — it's
            // purely informational. Without this, click-through to the underlying
            // toolbar Box is fine, but explicit is clearer.
            label.set_can_target(false);

            let plus = gtk4::Button::new();
            plus.set_child(Some(&icon_only("list-add-symbolic")));
            make_unfocusable(&plus);

            group.append(&minus);
            group.append(&label);
            group.append(&plus);

            // Shared adjustment + emit closure. `delta` is signed so a single
            // helper covers both buttons + scroll wheel + secondary-click.
            let emit = {
                let value = value.clone();
                let label = label.clone();
                let cb = callback.clone();
                move |delta: i32| {
                    let current = value.get() as i32;
                    let next = (current + delta).clamp(DELAY_MIN as i32, DELAY_MAX as i32) as u32;
                    if next == value.get() {
                        return;
                    }
                    value.set(next);
                    label.set_text(&format_delay_label(next));
                    if let Some(f) = cb.borrow().as_ref() {
                        f(ToolbarAction::DelayChanged(next));
                    }
                }
            };

            // Primary (left) click on `−` / `+` steps by 1; secondary (right) click
            // steps by 5. We use a GestureClick with `button = 0` (any) to read the
            // pressed button from the gesture, rather than two separate gestures.
            let make_gesture = |delta_primary: i32, delta_secondary: i32| {
                let gesture = gtk4::GestureClick::new();
                gesture.set_button(0);
                let emit_for_gesture = emit.clone();
                gesture.connect_pressed(move |g, _, _, _| {
                    let delta = if g.current_button() == gdk4::BUTTON_SECONDARY {
                        delta_secondary
                    } else {
                        delta_primary
                    };
                    emit_for_gesture(delta);
                });
                gesture
            };
            minus.add_controller(make_gesture(-STEP_PRIMARY, -STEP_SECONDARY));
            plus.add_controller(make_gesture(STEP_PRIMARY, STEP_SECONDARY));

            // Scroll wheel over any part of the segmented group adjusts the value.
            // Vertical-axis scrolls map to step changes (up = increase, down =
            // decrease) matching common spinner conventions.
            let scroll =
                gtk4::EventControllerScroll::new(gtk4::EventControllerScrollFlags::VERTICAL);
            let emit_for_scroll = emit.clone();
            scroll.connect_scroll(move |_, _dx, dy| {
                if dy < 0.0 {
                    emit_for_scroll(STEP_PRIMARY);
                } else if dy > 0.0 {
                    emit_for_scroll(-STEP_PRIMARY);
                }
                glib::Propagation::Stop
            });
            group.add_controller(scroll);

            widget.append(&group);
            delay = Some(DelaySpinnerUi { label, value });
        }

        if spec.show_passthrough_toggle {
            let btn = gtk4::ToggleButton::new();
            btn.set_child(Some(&icon_only("mouse-click-symbolic")));
            btn.set_tooltip_text(Some(&fl!("toolbar-passthrough-tooltip")));
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
                modifiers: gdk4::ModifierType::empty(),
            });
            passthrough = Some((btn, id));
        }

        // Output-destination switcher, immediately left of Save: the two read together as
        // "where it goes" / "send it there". A cycling button rather than three toggles keeps
        // the toolbar narrow, and avoids the popover/dropdown route that misbehaves on
        // Hyprland layer-shell surfaces (see `StylePickerUi`).
        if spec.show_output_switcher {
            let btn = gtk4::Button::new();
            let icons = gtk4::Box::new(gtk4::Orientation::Horizontal, 2);
            btn.set_child(Some(&icons));
            make_unfocusable(&btn);
            let mode = Rc::new(Cell::new(spec.initial_output_mode));
            apply_output_mode(&btn, &icons, mode.get());

            let cb = callback.clone();
            let cycle_mode = mode.clone();
            let cycle_icons = icons.clone();
            btn.connect_clicked(move |b| {
                let next = cycle_mode.get().next();
                cycle_mode.set(next);
                apply_output_mode(b, &cycle_icons, next);
                if let Some(f) = cb.borrow().as_ref() {
                    f(ToolbarAction::OutputModeChanged(next));
                }
            });
            widget.append(&btn);
            shortcuts.push(Shortcut {
                key: gdk4::Key::o,
                action: ShortcutAction::OutputCycle,
                modifiers: gdk4::ModifierType::CONTROL_MASK,
            });
            output = Some(OutputSwitcherUi {
                button: btn,
                icons,
                mode,
            });
        }

        if spec.show_save {
            let btn = gtk4::Button::new();
            btn.set_child(Some(&icon_only("document-save-symbolic")));
            btn.set_tooltip_text(Some(&fl!("toolbar-save-tooltip")));
            make_unfocusable(&btn);
            let cb = callback.clone();
            btn.connect_clicked(move |_| {
                if let Some(f) = cb.borrow().as_ref() {
                    f(ToolbarAction::Save);
                }
            });
            widget.append(&btn);
            // Ctrl+S is the conventional shortcut; Enter / KP_Enter is the quick-save used
            // by both the annotation editor (Edit mode) and the live draw overlay (Draw
            // mode). The Text tool's in-canvas WYSIWYG editor installs a key controller
            // in `PropagationPhase::Capture` on the canvas widget itself, so while a
            // text edit is in progress Return is intercepted there (committing the
            // typed text) before it ever reaches this shortcut dispatcher. When no
            // text edit is active, Enter falls through and triggers Save as before.
            shortcuts.push(Shortcut {
                key: gdk4::Key::s,
                action: ShortcutAction::Save,
                modifiers: gdk4::ModifierType::CONTROL_MASK,
            });
            shortcuts.push(Shortcut {
                key: gdk4::Key::Return,
                action: ShortcutAction::Save,
                modifiers: gdk4::ModifierType::empty(),
            });
            shortcuts.push(Shortcut {
                key: gdk4::Key::KP_Enter,
                action: ShortcutAction::Save,
                modifiers: gdk4::ModifierType::empty(),
            });
        }

        if spec.show_capture {
            // Capture commits the selection; Shift-click reroutes through Annotate so the
            // caller opens the annotation editor instead. We can't read modifier state from
            // a GestureClick attached to a `gtk4::Button` because the button's own internal
            // gesture claims the event sequence first. Instead we use `connect_clicked` and
            // consult `shift_held`, which the window-level key controller installed by
            // `install_shortcuts` keeps in sync as Shift is pressed/released. The icon and
            // tooltip also swap live from that same key controller.
            //
            // When `capture_shift_annotates` is false (selector popped by the draw overlay's
            // Save flow), Shift is meaningless here — the user is already on an annotation
            // surface — so we keep a static icon/tooltip and route every click and Enter to
            // plain `Capture` regardless of modifier state.
            let shift_annotates = spec.capture_shift_annotates;
            let btn = gtk4::Button::new();
            let icon = icon_only("camera-photo-symbolic");
            btn.set_child(Some(&icon));
            if shift_annotates {
                btn.set_tooltip_text(Some(&fl!("toolbar-capture-tooltip-shift")));
            } else {
                btn.set_tooltip_text(Some(&fl!("toolbar-capture-tooltip-plain")));
            }
            btn.add_css_class("suggested-action");
            make_unfocusable(&btn);

            let shift_held = Rc::new(Cell::new(false));
            let cb = callback.clone();
            // Use `connect_clicked` (button's native signal) rather than a GestureClick: in
            // recent gtk4-layer-shell + GTK versions a Capture-phase GestureClick on the
            // button sometimes fails to dispatch because the button's internal gesture and
            // ours both try to claim the sequence. The native click signal is reliable.
            //
            // For modifier state we consult two sources, in order:
            // 1. `shift_held`, which the EventControllerMotion below sets to the latest
            //    modifier state read off pointer motion events. This is the authoritative
            //    source when the user moves the mouse while holding Shift.
            // 2. The seat keyboard's `modifier_state()` as a fallback, for the case where
            //    the user pressed Shift first and then clicked without moving the mouse.
            let shift_for_click = shift_held.clone();
            btn.connect_clicked(move |_| {
                let action = if shift_annotates {
                    let shift = shift_for_click.get()
                        || gdk4::Display::default()
                            .and_then(|d| d.default_seat())
                            .and_then(|s| s.keyboard())
                            .map(|k| k.modifier_state().contains(gdk4::ModifierType::SHIFT_MASK))
                            .unwrap_or(false);
                    if shift {
                        ToolbarAction::Annotate
                    } else {
                        ToolbarAction::Capture
                    }
                } else {
                    ToolbarAction::Capture
                };
                if let Some(f) = cb.borrow().as_ref() {
                    f(action);
                }
            });

            widget.append(&btn);
            // Enter → Capture, Shift+Enter → Annotate (and the KP variants). The Save block
            // never coexists with show_capture (editor toolbar vs selector toolbar), so the
            // bare-Enter shortcuts don't collide. The Shift+Enter / Shift+KP_Enter shortcuts
            // are only installed when `capture_shift_annotates` is true; otherwise the
            // strict modifier match at `install_shortcuts` lets Shift+Enter fall through to
            // the window-level key controller (which also honors the same flag).
            shortcuts.push(Shortcut {
                key: gdk4::Key::Return,
                action: ShortcutAction::Capture,
                modifiers: gdk4::ModifierType::empty(),
            });
            shortcuts.push(Shortcut {
                key: gdk4::Key::KP_Enter,
                action: ShortcutAction::Capture,
                modifiers: gdk4::ModifierType::empty(),
            });
            if shift_annotates {
                shortcuts.push(Shortcut {
                    key: gdk4::Key::Return,
                    action: ShortcutAction::Annotate,
                    modifiers: gdk4::ModifierType::SHIFT_MASK,
                });
                shortcuts.push(Shortcut {
                    key: gdk4::Key::KP_Enter,
                    action: ShortcutAction::Annotate,
                    modifiers: gdk4::ModifierType::SHIFT_MASK,
                });
            }

            // Only populate `CaptureUi` when Shift can actually change the button's
            // appearance — otherwise `apply_shift` (driven by `install_shortcuts`) has
            // nothing to do.
            if shift_annotates {
                let ui = Rc::new(CaptureUi {
                    button: btn.clone(),
                    icon,
                    shift_held,
                    // Dimmed while Shift is held: Capture becomes Annotate, and the editor
                    // that opens carries its own editable switcher, so leaving this one live
                    // would mean two controls for the same value.
                    output: output.as_ref().map(|o: &OutputSwitcherUi| o.button.clone()),
                });

                // Live icon/tooltip swap as Shift is pressed/released. Pointer key events on a
                // layer-shell selector don't reliably reach `EventControllerKey` (the window may
                // not have keyboard focus, depending on compositor + layer config), so instead
                // we poll the seat keyboard's modifier state on a short GLib timer. This is
                // cheap (a handful of pointer hops every 60ms) and works regardless of focus.
                // It shares `apply_shift` with the key-controller route so the two can't drift.
                let ui_for_timer = ui.clone();
                glib::timeout_add_local(std::time::Duration::from_millis(60), move || {
                    if btn.parent().is_none() {
                        // Button was destroyed (window closed); cancel the timer.
                        return glib::ControlFlow::Break;
                    }
                    let shift = gdk4::Display::default()
                        .and_then(|d| d.default_seat())
                        .and_then(|s| s.keyboard())
                        .map(|k| k.modifier_state().contains(gdk4::ModifierType::SHIFT_MASK))
                        .unwrap_or(false);
                    ui_for_timer.apply_shift(shift);
                    glib::ControlFlow::Continue
                });

                capture = Some(ui);
            }
        }

        let state = Rc::new(ToolbarState {
            tools,
            modes,
            cursor,
            passthrough,
            delay,
            capture,
            color,
            style,
            font_size,
            output,
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

    /// Update the visible tool buttons without firing a `ToolSelected` action. Passing
    /// [`ToolKind::Select`] (which has no button) deactivates every tool — i.e. shows the
    /// "no drawing tool / Select mode" state.
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

    /// Update the delay control without emitting `DelayChanged`. Used to mirror state
    /// across per-monitor toolbars in the selector. Refreshes both the cached value and
    /// the visible label; no signal is fired because the cached value is the source of
    /// truth read by the click / scroll handlers.
    #[allow(dead_code)]
    pub fn set_delay(&self, secs: u32) {
        if let Some(ui) = &self.state.delay {
            let clamped = secs.min(60);
            ui.value.set(clamped);
            ui.label.set_text(&format_delay_label(clamped));
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

    /// Update the color-picker swatch without firing `ColorChanged`. Used by external
    /// state changes — e.g. selecting a different tool — so the swatch always reflects
    /// the active tool's color. The next time the user opens the picker, the dialog will
    /// also be seeded with this color (it reads from `current` at open time).
    pub fn set_color(&self, color: [f32; 4]) {
        if let Some(ui) = &self.state.color {
            ui.current.set(array_to_rgba(color));
            ui.swatch.queue_draw();
        }
    }

    /// Enable or disable the color-picker button (e.g. when a tool with hardcoded
    /// appearance like Blur / Crop / Redact is active). No-op when the picker isn't
    /// present in this toolbar.
    pub fn set_color_picker_sensitive(&self, sensitive: bool) {
        if let Some(ui) = &self.state.color {
            ui.button.set_sensitive(sensitive);
        }
    }

    /// Update the style-picker toggles without firing `StrokeStyleChanged`.
    /// Used by external state changes (e.g. selecting a different tool) so the
    /// picker always reflects the active tool's stored style.
    ///
    /// We block every toggle's `toggled` handler **before** touching any of them so
    /// the group's "exactly one active" invariant (which fires auto-deactivation
    /// `toggled` signals on peers) can't trigger a spurious
    /// `StrokeStyleChanged` callback. We then activate only the matching toggle —
    /// GTK auto-deactivates the others via the group, but those side-effects are
    /// silenced by the blocked handlers.
    pub fn set_stroke_style(&self, style: StrokeStyle) {
        if let Some(ui) = &self.state.style {
            for (_, toggle, id) in &ui.toggles {
                toggle.block_signal(id);
            }
            if let Some((_, toggle, _)) = ui.toggles.iter().find(|(s, _, _)| *s == style) {
                toggle.set_active(true);
            }
            for (_, toggle, id) in &ui.toggles {
                toggle.unblock_signal(id);
            }
        }
    }

    /// Set the output-destination switcher's state without emitting
    /// [`ToolbarAction::OutputModeChanged`]. No signal blocking is needed — the control is a
    /// plain `Button`, so writing the `Cell` and re-skinning is inert. No-op when the
    /// switcher was not built.
    pub fn set_output_mode(&self, mode: OutputMode) {
        if let Some(ui) = &self.state.output {
            ui.mode.set(mode);
            apply_output_mode(&ui.button, &ui.icons, mode);
        }
    }

    /// Enable or disable the entire style-picker segmented control (mirrors
    /// [`Self::set_color_picker_sensitive`] for tools whose stroke style is
    /// hardcoded — Highlight / Number / Text / Blur / Crop / Redact).
    pub fn set_style_picker_sensitive(&self, sensitive: bool) {
        if let Some(ui) = &self.state.style {
            ui.container.set_sensitive(sensitive);
        }
    }

    /// Update the font-size control without firing `FontSizeChanged`. Used by external state
    /// changes (e.g. selecting a different tool) so the control always reflects the active
    /// tool's stored size. The cached value is the source of truth read by the click / scroll
    /// handlers, so no signal is fired.
    pub fn set_font_size(&self, size: f32) {
        if let Some(ui) = &self.state.font_size {
            let clamped = (size.round() as i64).clamp(6, 200) as u32;
            ui.value.set(clamped);
            ui.label.set_text(&format_font_size_label(clamped));
        }
    }

    /// Enable or disable the font-size control (mirrors [`Self::set_color_picker_sensitive`]
    /// — only tools that render text expose a meaningful font size).
    pub fn set_font_size_picker_sensitive(&self, sensitive: bool) {
        if let Some(ui) = &self.state.font_size {
            ui.container.set_sensitive(sensitive);
        }
    }

    /// Install keyboard shortcuts on `target`. Each shortcut emits the same `ToolbarAction` as
    /// clicking the matching button — and, for radio-style sections, flips the active toggle
    /// so the on-screen state stays in sync with the keyboard.
    ///
    /// Each shortcut declares the modifier mask it requires. Only Ctrl/Shift/Alt are matched
    /// (Lock/Mod2 etc. are masked out) so CapsLock or NumLock don't suppress a shortcut.
    ///
    /// As a side-effect, the installed key controller also tracks the Shift modifier so the
    /// Capture button can swap its icon between "camera" and "annotate" in real time — giving
    /// users visual feedback that Shift-click will route through Annotate.
    pub fn install_shortcuts(&self, target: &impl IsA<gtk4::Widget>) {
        let key = gtk4::EventControllerKey::new();
        let toolbar = self.clone();
        key.connect_key_pressed(move |_, k, _, state| {
            let mods = state
                & (gdk4::ModifierType::CONTROL_MASK
                    | gdk4::ModifierType::SHIFT_MASK
                    | gdk4::ModifierType::ALT_MASK);
            // Reflect the current Shift state on the Capture button (if present). The state
            // value already includes the just-pressed modifier, so pressing Shift_L fires this
            // arm with SHIFT_MASK set.
            if let Some(cap) = &toolbar.state.capture {
                cap.apply_shift(mods.contains(gdk4::ModifierType::SHIFT_MASK));
            }
            for sc in &toolbar.state.shortcuts {
                if !key_matches(sc.key, k) {
                    continue;
                }
                if mods != sc.modifiers {
                    continue;
                }
                if toolbar.dispatch_shortcut(&sc.action) {
                    return glib::Propagation::Stop;
                }
            }
            glib::Propagation::Proceed
        });
        let toolbar_released = self.clone();
        key.connect_key_released(move |_, k, _, state| {
            // GDK reports the modifier mask *before* this key was released, so on a Shift_L
            // release `state` still contains SHIFT_MASK. Detect by inspecting the released
            // keyval itself: clear the cached Shift state when either Shift key is released
            // *and* no other Shift modifier is still held (i.e. the other Shift_R/Shift_L).
            if let Some(cap) = &toolbar_released.state.capture
                && (k == gdk4::Key::Shift_L || k == gdk4::Key::Shift_R)
            {
                // If both Shift keys were held and one is released, GDK still reports
                // SHIFT_MASK; we conservatively clear, knowing the next key event will
                // re-apply if the other Shift is still down.
                let _ = state;
                cap.apply_shift(false);
            }
        });
        target.add_controller(key);
    }

    fn dispatch_shortcut(&self, action: &ShortcutAction) -> bool {
        match action {
            ShortcutAction::Tool(kind) => {
                // Toggle the matching button: activating fires `toggled` → emits
                // `ToolSelected(kind)`; pressing the same key again deactivates it → emits
                // `ToolSelected(Select)` (the "no tool" / Select mode), matching click behavior.
                if let Some((_, btn, _)) = self.state.tools.iter().find(|(k, _, _)| k == kind) {
                    btn.set_active(!btn.is_active());
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
            ShortcutAction::Annotate => self.emit(ToolbarAction::Annotate),
            ShortcutAction::OutputCycle => {
                // Click the button rather than emitting directly, so the icon and tooltip
                // follow the keyboard route exactly as they do the pointer one.
                if let Some(ui) = &self.state.output {
                    ui.button.emit_clicked();
                    true
                } else {
                    false
                }
            }
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

/// Format a whole-seconds delay value for the toolbar's delay-trigger label.
/// Localized via the `toolbar-delay-label` Fluent message so French gets
/// the canonical "3 s" with a non-breaking space.
fn format_delay_label(secs: u32) -> String {
    fl!("toolbar-delay-label", secs = secs)
}

/// `true` if any tool button in the list is currently active. Used by the toggle handlers to
/// distinguish "switched to another tool" from "deselected the last tool" (= Select mode).
fn any_active(buttons: &[(ToolKind, gtk4::ToggleButton)]) -> bool {
    buttons.iter().any(|(_, b)| b.is_active())
}

/// Format a whole-point font size for the toolbar's font-size label. Localized via the
/// `toolbar-font-size-label` Fluent message.
fn format_font_size_label(pt: u32) -> String {
    fl!("toolbar-font-size-label", pt = pt)
}

/// `gdk::RGBA` → packed `[f32; 4]` matching the canvas's tool storage format.
fn rgba_to_array(c: &gdk4::RGBA) -> [f32; 4] {
    [c.red(), c.green(), c.blue(), c.alpha()]
}

/// Inverse of [`rgba_to_array`].
fn array_to_rgba(c: [f32; 4]) -> gdk4::RGBA {
    gdk4::RGBA::new(c[0], c[1], c[2], c[3])
}

/// Format an RGBA color as `#rrggbb` (opaque) or `#rrggbbaa` (with alpha < 1). Exported
/// (within the module) so tests can keep their parity checks; not used by the runtime
/// picker UI any more — `ColorDialog` handles its own presentation.
#[cfg(test)]
fn rgba_to_hex(c: &gdk4::RGBA) -> String {
    let to_byte = |v: f32| (v.clamp(0.0, 1.0) * 255.0).round() as u8;
    let r = to_byte(c.red());
    let g = to_byte(c.green());
    let b = to_byte(c.blue());
    let a = to_byte(c.alpha());
    if a == 255 {
        format!("#{r:02x}{g:02x}{b:02x}")
    } else {
        format!("#{r:02x}{g:02x}{b:02x}{a:02x}")
    }
}

/// Paint the alpha-aware preview used by the color-picker trigger button: a 4 px
/// checkerboard background (so translucent colors are distinguishable from opaque grey)
/// with the RGBA color on top.
fn draw_color_swatch(cr: &gtk4::cairo::Context, w: f64, h: f64, c: gdk4::RGBA) {
    let cell = 4.0_f64;
    let cols = (w / cell).ceil() as i32;
    let rows = (h / cell).ceil() as i32;
    for j in 0..rows {
        for i in 0..cols {
            let dark = (i + j) % 2 == 0;
            let s = if dark { 0.78 } else { 1.0 };
            cr.set_source_rgb(s, s, s);
            cr.rectangle(f64::from(i) * cell, f64::from(j) * cell, cell, cell);
            let _ = cr.fill();
        }
    }
    cr.set_source_rgba(
        c.red() as f64,
        c.green() as f64,
        c.blue() as f64,
        c.alpha() as f64,
    );
    cr.rectangle(0.0, 0.0, w, h);
    let _ = cr.fill();
}

/// Paint the sample line used by the stroke-style picker's trigger button. Draws
/// a 2 px horizontal line centred vertically, dashed/dotted according to `style`,
/// using the theme's foreground color so the swatch reads correctly in light and
/// dark themes alike.
fn draw_style_swatch(cr: &gtk4::cairo::Context, w: f64, h: f64, style: StrokeStyle) {
    let line_w = 2.0_f64;
    cr.set_source_rgb(0.85, 0.85, 0.85);
    cr.set_line_width(line_w);
    cr.set_line_cap(gtk4::cairo::LineCap::Round);
    match style {
        StrokeStyle::Solid => cr.set_dash(&[], 0.0),
        // Width-relative dashes mirror `styled_stroke` in `canvas.rs` so the swatch
        // preview matches what the user will get on-canvas.
        StrokeStyle::Dashed => cr.set_dash(&[line_w * 3.0, line_w * 2.0], 0.0),
        StrokeStyle::Dotted => cr.set_dash(&[0.0, line_w * 2.0], 0.0),
    }
    let y = h / 2.0;
    cr.move_to(2.0, y);
    cr.line_to(w - 2.0, y);
    let _ = cr.stroke();
}

/// Open a `gtk4::ColorDialog` so the user can pick a color, handling the layer-shell
/// stacking + keyboard issues that otherwise leave the dialog uninteractive. Caller passes
/// the shared state cells so the completion callback can publish the picked color.
///
/// Why this is non-trivial: the screenshot selector and the live draw overlay both run at
/// `Layer::Overlay`, which on Hyprland (and any wlr-layer-shell compositor) sits **above**
/// every regular `xdg_toplevel`. The dialog opens as a toplevel, so without intervention
/// it appears behind the overlay (invisible) or — if it forces itself visible — still has
/// pointer / keyboard events stolen by the layer surface above it.
///
/// Flow:
/// 1. Walk `btn` → root `Window`, then enumerate every window in the same `Application`.
/// 2. For each window that is a layer-shell window, snapshot its current `Layer` +
///    `KeyboardMode` and demote it to `Layer::Background` / `KeyboardMode::None` so the
///    dialog can sit above and grab input. We do **all** of them (not just `btn`'s root)
///    because multi-monitor setups have one layer-shell window per output; leaving even
///    one at `Overlay` would still steal events.
/// 3. Build a `ColorDialog` (alpha-enabled, modal, titled "Pick a Color") seeded with the
///    current color and call `choose_rgba` with `None` as parent — the dialog must NOT be
///    transient-for any of the (now-demoted) layer-shell surfaces.
/// 4. In the completion callback: restore every saved layer + keyboard mode regardless of
///    success / cancel. On `Ok(rgba)` push the picked color into `current`, redraw the
///    swatch, and emit `ColorChanged`. `Err` (cancel / Esc) is ignored silently.
fn open_color_dialog(
    btn: &gtk4::Button,
    current: Rc<Cell<gdk4::RGBA>>,
    swatch: gtk4::DrawingArea,
    callback: Callback,
) {
    use gtk4_layer_shell::{KeyboardMode, Layer, LayerShell};

    // Walk up to the toplevel. If the button isn't realised yet (no root) bail silently.
    let Some(root_window) = btn.root().and_then(|r| r.downcast::<gtk4::Window>().ok()) else {
        return;
    };

    // Demote every layer-shell window in this application so the dialog can sit above
    // them and receive input. Each saved tuple is (window, prior_layer, prior_keyboard).
    let saved: Vec<(gtk4::Window, Layer, KeyboardMode)> =
        if let Some(app) = root_window.application() {
            app.windows()
                .into_iter()
                .filter(|w| w.is_layer_window())
                .map(|w| {
                    let layer = w.layer();
                    let mode = w.keyboard_mode();
                    w.set_layer(Layer::Background);
                    w.set_keyboard_mode(KeyboardMode::None);
                    (w, layer, mode)
                })
                .collect()
        } else {
            Vec::new()
        };

    let dialog_title = fl!("toolbar-color-dialog-title");
    let dialog = gtk4::ColorDialog::builder()
        .with_alpha(true)
        .title(&dialog_title)
        .modal(true)
        .build();
    let initial = current.get();

    dialog.choose_rgba(
        // No parent: layer-shell surfaces aren't valid transient-for targets for xdg
        // toplevels, and we've already demoted them out of the way.
        None::<&gtk4::Window>,
        Some(&initial),
        gtk4::gio::Cancellable::NONE,
        move |result| {
            // Restore every demoted layer-shell window regardless of OK/Cancel so the
            // user gets their overlay back the moment the dialog closes.
            for (w, layer, mode) in &saved {
                w.set_layer(*layer);
                w.set_keyboard_mode(*mode);
            }
            let Ok(rgba) = result else {
                return;
            };
            current.set(rgba);
            swatch.queue_draw();
            if let Some(f) = callback.borrow().as_ref() {
                f(ToolbarAction::ColorChanged(rgba_to_array(&rgba)));
            }
        },
    );

    // GtkColorDialog owns its window internally and ships it with a stack that is
    // size-homogeneous plus internal GtkScrolledWindows. When the user clicks "+" to
    // switch to the custom-color editor, the window keeps the palette geometry and a
    // scrollbar appears instead of growing to fit. We can't influence the window at
    // creation time, but we can find it via the toplevel list after `choose_rgba` and
    // tweak its widget tree so it refits naturally on every visible-child swap.
    schedule_color_dialog_refit(dialog_title, 3);
}

/// Try to locate the just-opened color dialog and apply [`refit_color_dialog`].
/// The dialog window is created and mapped asynchronously by GTK; retry on a short
/// timeout up to `remaining` times, then give up silently.
fn schedule_color_dialog_refit(title: String, remaining: u32) {
    gtk4::glib::idle_add_local_once(move || {
        if let Some(dlg) = find_color_dialog_window(&title) {
            refit_color_dialog(&dlg);
        } else if remaining > 0 {
            gtk4::glib::timeout_add_local_once(std::time::Duration::from_millis(50), move || {
                schedule_color_dialog_refit(title, remaining - 1)
            });
        }
    });
}

/// Scan the application's toplevels for a window titled exactly `title` —
/// the title we set on the `ColorDialog`. Returns the first match.
/// The expected title is threaded through from `open_color_dialog` so the
/// match stays in sync with the localized title we set on the dialog.
fn find_color_dialog_window(title: &str) -> Option<gtk4::Window> {
    for obj in gtk4::Window::list_toplevels() {
        let Ok(win) = obj.downcast::<gtk4::Window>() else {
            continue;
        };
        if win.title().map(|t| t.as_str() == title).unwrap_or(false) {
            return Some(win);
        }
    }
    None
}

/// Walk every descendant of `root` in DFS order via the `first_child` / `next_sibling`
/// chain. Used to find the chooser's internal `GtkStack` and `GtkScrolledWindow`s.
fn walk_descendants(root: &gtk4::Widget, visit: &mut dyn FnMut(&gtk4::Widget)) {
    visit(root);
    let mut child = root.first_child();
    while let Some(c) = child {
        walk_descendants(&c, visit);
        child = c.next_sibling();
    }
}

/// Mutate the dialog window so it refits to the chooser's currently-visible page:
///
/// * `set_resizable(false)` — non-resizable GTK windows re-allocate to the child's
///   natural size on every `check-resize`.
/// * Suppress every nested `GtkScrolledWindow`: no scrollbars, and propagate the
///   child's natural size up so the window's natural size includes the full content.
/// * Make the internal `GtkStack` non-homogeneous so it reports the *visible* child's
///   natural size rather than the max of palette and custom-editor pages.
/// * Connect `notify::visible-child` as belt-and-braces: on every swap, reset the
///   default size and queue a resize so the window definitively refits.
fn refit_color_dialog(dlg: &gtk4::Window) {
    dlg.set_resizable(false);

    let mut stack: Option<gtk4::Stack> = None;
    walk_descendants(dlg.upcast_ref(), &mut |w| {
        if let Some(sw) = w.downcast_ref::<gtk4::ScrolledWindow>() {
            sw.set_policy(gtk4::PolicyType::Never, gtk4::PolicyType::Never);
            sw.set_propagate_natural_height(true);
            sw.set_propagate_natural_width(true);
        } else if stack.is_none()
            && let Some(s) = w.downcast_ref::<gtk4::Stack>()
        {
            stack = Some(s.clone());
        }
    });

    if let Some(s) = stack {
        s.set_hhomogeneous(false);
        s.set_vhomogeneous(false);
        s.set_interpolate_size(false);
        let dlg_weak = dlg.downgrade();
        s.connect_visible_child_notify(move |_| {
            if let Some(dlg) = dlg_weak.upgrade() {
                dlg.set_default_size(-1, -1);
                dlg.queue_resize();
            }
        });
    }
}

/// One per-monitor host slot: the overlay (and its window) that can parent the single shared
/// toolbar, plus the identity needed to target it from a Hyprland focus event.
struct ToolbarSlot {
    /// GDK monitor index (`display.monitors()` position). Surface code maps this back to its own
    /// per-monitor registries (selector `MonitorInfo`, overlay `MonitorCanvas`).
    index: usize,
    /// GDK `Monitor::connector()` (e.g. `DP-1`), matched against Hyprland's focused-monitor name.
    connector: Option<String>,
    overlay: gtk4::Overlay,
    window: gtk4::ApplicationWindow,
}

/// Tracks which per-monitor window currently hosts the **single** shared [`Toolbar`] and moves
/// it between monitors as Hyprland focus changes.
///
/// Snypr keeps one fullscreen layer-shell window per monitor (for dimming / canvas / capture),
/// but only one toolbar — on the focused monitor. Reparenting a GTK widget means
/// `old_overlay.remove_overlay(w)` then `new_overlay.add_overlay(w)`; this type owns that move and
/// the slot registry.
///
/// `!Send` (holds GTK widgets); lives entirely on the GTK main thread. Wrap in [`Rc`] so focus
/// event handlers can hold a clone.
pub struct ToolbarHost {
    toolbar: Toolbar,
    slots: RefCell<Vec<ToolbarSlot>>,
    /// Index into `slots` currently parenting the toolbar widget, if any.
    current: Cell<Option<usize>>,
}

impl ToolbarHost {
    /// Create a host for `toolbar`. The toolbar's alignment is fixed to bottom-center here so it
    /// renders identically regardless of which monitor's overlay currently parents it.
    pub fn new(toolbar: Toolbar) -> Rc<Self> {
        let w = toolbar.widget();
        w.set_halign(gtk4::Align::Center);
        w.set_valign(gtk4::Align::End);
        w.set_margin_bottom(24);
        Rc::new(Self {
            toolbar,
            slots: RefCell::new(Vec::new()),
            current: Cell::new(None),
        })
    }

    /// Register a monitor's overlay + window. Call once per per-monitor window, in build order.
    pub fn register(
        &self,
        index: usize,
        connector: Option<String>,
        overlay: &gtk4::Overlay,
        window: &gtk4::ApplicationWindow,
    ) {
        self.slots.borrow_mut().push(ToolbarSlot {
            index,
            connector,
            overlay: overlay.clone(),
            window: window.clone(),
        });
    }

    /// The single shared toolbar.
    pub fn toolbar(&self) -> &Toolbar {
        &self.toolbar
    }

    /// GDK monitor index of the window currently hosting the toolbar, if any. Surfaces use this to
    /// route actions (selector Screen-mode capture target, overlay Undo) to the focused monitor.
    pub fn current_index(&self) -> Option<usize> {
        let pos = self.current.get()?;
        self.slots.borrow().get(pos).map(|s| s.index)
    }

    /// Move the toolbar into the slot whose GDK monitor `index` matches. No-op if it's already
    /// there or no such slot exists.
    pub fn move_to_index(&self, index: usize) {
        let slots = self.slots.borrow();
        let Some(target) = slots.iter().position(|s| s.index == index) else {
            return;
        };
        self.move_to_slot(&slots, target);
    }

    /// Move the toolbar to the slot matching Hyprland connector `name`.
    ///
    /// Live (focus-driven) behavior is best-effort: a name with no matching slot leaves the
    /// toolbar where it is, rather than yanking it to an arbitrary monitor (e.g. when focus lands
    /// on a monitor the Edit overlay skipped because it didn't intersect the capture).
    pub fn move_to_connector(&self, name: Option<&str>) {
        let Some(name) = name else { return };
        let slots = self.slots.borrow();
        let Some(target) = slots
            .iter()
            .position(|s| s.connector.as_deref() == Some(name))
        else {
            return;
        };
        self.move_to_slot(&slots, target);
    }

    /// Initial placement before the windows are presented: the focused monitor if it has a slot,
    /// otherwise the first registered slot (the toolbar must start somewhere visible).
    pub fn place_initial(&self, focused: Option<&str>) {
        let slots = self.slots.borrow();
        if slots.is_empty() {
            return;
        }
        let target = focused
            .and_then(|name| {
                slots
                    .iter()
                    .position(|s| s.connector.as_deref() == Some(name))
            })
            .unwrap_or(0);
        self.move_to_slot(&slots, target);
    }

    /// Reparent the toolbar widget into `slots[target]`. The `remove_overlay` / `add_overlay`
    /// pair is kept synchronous and adjacent so the toolbar's children are only momentarily
    /// unparented (the Capture button's Shift-poll timer self-cancels on `parent().is_none()`,
    /// but GLib timeouts can't interleave between these two calls within one main-loop turn).
    fn move_to_slot(&self, slots: &[ToolbarSlot], target: usize) {
        if self.current.get() == Some(target) {
            return;
        }
        let widget = self.toolbar.widget();
        if let Some(old) = self.current.get()
            && let Some(slot) = slots.get(old)
        {
            slot.overlay.remove_overlay(widget);
            // Repaint the window we just left so its per-frame passthrough handler drops the
            // toolbar-bounds input region (otherwise, under passthrough, the old monitor would
            // keep a stray clickable rectangle for up to one frame).
            if let Some(s) = slot.window.surface() {
                s.queue_render();
            }
        }
        let slot = &slots[target];
        slot.overlay.add_overlay(widget);
        self.current.set(Some(target));

        // Nudge the now-hosting surface to repaint so the overlay's per-frame passthrough
        // input-region handler re-derives that this window hosts the toolbar without waiting for
        // organic damage. Harmless for the selector (no passthrough handler).
        if let Some(s) = slot.window.surface() {
            s.queue_render();
        }
    }
}

#[cfg(test)]
mod tests {
    use super::*;
    use pretty_assertions::assert_eq;
    use rstest::rstest;

    #[test]
    fn mode_kind_default_is_screen() {
        assert_eq!(ModeKind::default(), ModeKind::Screen);
    }

    #[test]
    fn mode_kind_from_initial_mode_maps_all_variants() {
        use crate::config::InitialMode;
        assert_eq!(ModeKind::from(InitialMode::Full), ModeKind::Full);
        assert_eq!(ModeKind::from(InitialMode::Screen), ModeKind::Screen);
        assert_eq!(ModeKind::from(InitialMode::Window), ModeKind::Window);
        assert_eq!(ModeKind::from(InitialMode::Region), ModeKind::Region);
        // Defaults must agree so an absent config key preserves historical behavior.
        assert_eq!(ModeKind::from(InitialMode::default()), ModeKind::default());
    }

    #[test]
    fn editor_tool_table_covers_every_drawing_tool_kind() {
        // If a new ToolKind variant is added, this test reminds us to wire it into the editor
        // toolbar (or explicitly exclude it). `Select` is intentionally excluded — it's the
        // implicit "no tool active" mode and has no button.
        let kinds: std::collections::HashSet<_> = EDITOR_TOOLS.iter().map(|e| e.kind).collect();
        assert!(
            !kinds.contains(&ToolKind::Select),
            "Select must not have a toolbar button (it is the no-tool mode)"
        );
        for kind in [
            ToolKind::Rect,
            ToolKind::Ellipse,
            ToolKind::Arrow,
            ToolKind::Line,
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
    fn overlay_draw_preset_includes_blur_excludes_crop() {
        // Blur in the overlay is backed by a lazy desktop capture (see
        // `AnnotationCanvas::set_hidden_base`); Crop has no meaning without a captured
        // base and is still omitted.
        let kinds: std::collections::HashSet<_> = OVERLAY_TOOLS.iter().map(|e| e.kind).collect();
        assert!(kinds.contains(&ToolKind::Blur));
        assert!(!kinds.contains(&ToolKind::Crop));
    }

    #[test]
    fn overlay_draw_preset_includes_line() {
        let kinds: std::collections::HashSet<_> = OVERLAY_TOOLS.iter().map(|e| e.kind).collect();
        assert!(kinds.contains(&ToolKind::Line));
    }

    #[test]
    fn overlay_edit_preset_uses_full_editor_toolset() {
        // The in-place edit overlay shares EDITOR_TOOLS so users get every annotation tool
        // (incl. Blur + Crop, which need an underlying base) when annotating a captured frame.
        let kinds: std::collections::HashSet<_> = EDITOR_TOOLS.iter().map(|e| e.kind).collect();
        for kind in [ToolKind::Blur, ToolKind::Crop] {
            assert!(
                kinds.contains(&kind),
                "Edit-mode toolbar (EDITOR_TOOLS) is missing {kind:?}"
            );
        }
    }

    #[rstest]
    #[case(OutputMode::File, &["document-save-symbolic"])]
    #[case(OutputMode::Clipboard, &["edit-copy-symbolic"])]
    #[case(
        OutputMode::Both,
        &["document-save-symbolic", "edit-copy-symbolic"]
    )]
    fn the_output_switcher_shows_one_icon_per_active_sink(
        #[case] mode: OutputMode,
        #[case] expected: &[&str],
    ) {
        // `Both` renders two icons side by side; that composition is what makes the combined
        // state legible without vendoring a third SVG.
        assert_eq!(output_mode_icons(mode), expected);
    }

    use crate::ui::require_gtk;

    /// Selector-shaped toolbar: modes + cursor + delay + the output switcher + Capture.
    fn selector_toolbar(initial: OutputMode) -> Toolbar {
        Toolbar::new(ToolbarSpec {
            modes: SELECTOR_MODES,
            show_cursor_toggle: true,
            show_delay_spinner: true,
            show_capture: true,
            show_output_switcher: true,
            initial_mode: Some(ModeKind::Screen),
            initial_output_mode: initial,
            ..Default::default()
        })
    }

    /// Drain the actions a toolbar emits into a shared vec.
    fn record(toolbar: &Toolbar) -> Rc<RefCell<Vec<ToolbarAction>>> {
        let seen = Rc::new(RefCell::new(Vec::new()));
        let sink = seen.clone();
        toolbar.connect(move |action| sink.borrow_mut().push(action));
        seen
    }

    #[rstest]
    #[case(OutputMode::File)]
    #[case(OutputMode::Clipboard)]
    #[case(OutputMode::Both)]
    fn the_switcher_opens_on_the_resolved_destination(#[case] initial: OutputMode) {
        require_gtk!();
        let toolbar = selector_toolbar(initial);
        let ui = toolbar
            .state
            .output
            .as_ref()
            .expect("switcher was requested");

        assert_eq!(ui.mode.get(), initial);
        assert_eq!(
            ui.button.tooltip_text().unwrap(),
            output_mode_label(initial)
        );
    }

    #[test]
    fn clicking_the_switcher_cycles_and_reports_each_destination() {
        require_gtk!();
        let toolbar = selector_toolbar(OutputMode::File);
        let seen = record(&toolbar);
        let button = toolbar.state.output.as_ref().unwrap().button.clone();

        for _ in 0..3 {
            button.emit_clicked();
        }

        assert_eq!(
            *seen.borrow(),
            vec![
                ToolbarAction::OutputModeChanged(OutputMode::Clipboard),
                ToolbarAction::OutputModeChanged(OutputMode::Both),
                ToolbarAction::OutputModeChanged(OutputMode::File),
            ]
        );
    }

    #[test]
    fn the_switcher_reskins_itself_as_it_cycles() {
        require_gtk!();
        let toolbar = selector_toolbar(OutputMode::File);
        let ui = toolbar.state.output.as_ref().unwrap();

        let icon_count = || ui.icons.observe_children().n_items();
        assert_eq!(icon_count(), 1, "File shows a single icon");

        ui.button.emit_clicked(); // -> Clipboard
        assert_eq!(icon_count(), 1);
        assert_eq!(
            ui.button.tooltip_text().unwrap(),
            output_mode_label(OutputMode::Clipboard)
        );

        ui.button.emit_clicked(); // -> Both
        assert_eq!(
            icon_count(),
            2,
            "Both composes the file and clipboard icons"
        );
        assert_eq!(
            ui.button.tooltip_text().unwrap(),
            output_mode_label(OutputMode::Both)
        );
    }

    #[test]
    fn set_output_mode_updates_the_button_without_emitting() {
        require_gtk!();
        // The draw overlay calls this to mirror a choice made in the selector it popped;
        // re-emitting would write the value back and could loop.
        let toolbar = selector_toolbar(OutputMode::File);
        let seen = record(&toolbar);

        toolbar.set_output_mode(OutputMode::Both);

        let ui = toolbar.state.output.as_ref().unwrap();
        assert_eq!(ui.mode.get(), OutputMode::Both);
        assert_eq!(
            ui.button.tooltip_text().unwrap(),
            output_mode_label(OutputMode::Both)
        );
        assert!(seen.borrow().is_empty(), "setter must stay silent");
    }

    #[test]
    fn set_output_mode_is_a_no_op_without_a_switcher() {
        require_gtk!();
        // The editor-only presets in some callers omit it; the setter must not panic.
        let toolbar = Toolbar::new(ToolbarSpec {
            tools: EDITOR_TOOLS,
            show_save: true,
            ..Default::default()
        });
        toolbar.set_output_mode(OutputMode::Both);
        assert!(toolbar.state.output.is_none());
    }

    #[test]
    fn holding_shift_dims_the_switcher() {
        require_gtk!();
        // Shift turns Capture into Annotate, and the editor that opens carries its own
        // switcher — so this one must step aside rather than offer a second control.
        let toolbar = selector_toolbar(OutputMode::File);
        let capture = toolbar
            .state
            .capture
            .as_ref()
            .expect("capture was requested");
        let switcher = toolbar.state.output.as_ref().unwrap().button.clone();

        assert!(switcher.is_sensitive());

        capture.apply_shift(true);
        assert!(!switcher.is_sensitive(), "dimmed while Shift is held");
        assert_eq!(
            capture.button.tooltip_text().unwrap(),
            fl!("toolbar-annotate-tooltip")
        );

        capture.apply_shift(false);
        assert!(switcher.is_sensitive(), "restored on release");
    }

    #[test]
    fn the_shift_skin_survives_a_toolbar_without_a_switcher() {
        require_gtk!();
        let toolbar = Toolbar::new(ToolbarSpec {
            modes: SELECTOR_MODES,
            show_capture: true,
            ..Default::default()
        });
        let capture = toolbar.state.capture.as_ref().unwrap();
        capture.apply_shift(true);
        assert!(toolbar.state.output.is_none());
    }

    #[test]
    fn ctrl_o_cycles_the_switcher() {
        require_gtk!();
        let toolbar = selector_toolbar(OutputMode::File);
        let seen = record(&toolbar);

        assert!(toolbar.dispatch_shortcut(&ShortcutAction::OutputCycle));

        assert_eq!(
            *seen.borrow(),
            vec![ToolbarAction::OutputModeChanged(OutputMode::Clipboard)]
        );
        // Routed through the button, so the visuals follow the keyboard too.
        assert_eq!(
            toolbar.state.output.as_ref().unwrap().mode.get(),
            OutputMode::Clipboard
        );
    }

    #[test]
    fn ctrl_o_is_inert_without_a_switcher() {
        require_gtk!();
        let toolbar = Toolbar::new(ToolbarSpec {
            tools: EDITOR_TOOLS,
            show_save: true,
            ..Default::default()
        });
        assert!(!toolbar.dispatch_shortcut(&ShortcutAction::OutputCycle));
    }

    #[test]
    fn the_switcher_registers_ctrl_o_and_sits_before_capture() {
        require_gtk!();
        let toolbar = selector_toolbar(OutputMode::File);

        let shortcut = toolbar
            .state
            .shortcuts
            .iter()
            .find(|s| matches!(s.action, ShortcutAction::OutputCycle))
            .expect("Ctrl+O must be registered");
        assert_eq!(shortcut.key, gdk4::Key::o);
        assert_eq!(shortcut.modifiers, gdk4::ModifierType::CONTROL_MASK);

        // Reading order is "where it goes" then "send it there".
        let switcher = toolbar.state.output.as_ref().unwrap().button.clone();
        let capture = toolbar.state.capture.as_ref().unwrap().button.clone();
        let mut child = switcher.next_sibling();
        let mut found = false;
        while let Some(w) = child {
            if w == capture.clone().upcast::<gtk4::Widget>() {
                found = true;
                break;
            }
            child = w.next_sibling();
        }
        assert!(found, "Capture must follow the output switcher");
    }

    #[test]
    fn rgba_to_hex_drops_alpha_when_opaque() {
        let c = gdk4::RGBA::new(1.0, 0.0, 0.5, 1.0);
        assert_eq!(rgba_to_hex(&c), "#ff0080");
    }

    #[test]
    fn rgba_to_hex_includes_alpha_when_translucent() {
        let c = gdk4::RGBA::new(0.0, 1.0, 0.0, 0.5);
        // 0.5 * 255 rounds to 128 (0x80).
        assert_eq!(rgba_to_hex(&c), "#00ff0080");
    }

    #[test]
    fn stroke_style_default_is_solid() {
        assert_eq!(StrokeStyle::default(), StrokeStyle::Solid);
    }
}
