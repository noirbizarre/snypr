//! Shared CSS / styling for HyprSnap windows.

pub const CSS: &str = r#"
.hyprsnap-toolbar {
    background: alpha(@theme_bg_color, 0.85);
    border-radius: 12px;
    padding: 6px;
    margin: 12px;
}

/* The selector overlay window must be fully transparent so the Cairo draw_func
 * (which paints a translucent dim + cleared selection cutout) composites directly
 * over the live desktop, not over GTK's default opaque window background. */
window.hyprsnap-selector,
window.hyprsnap-selector decoration,
window.hyprsnap-selector > *,
window.hyprsnap-overlay,
window.hyprsnap-overlay decoration,
window.hyprsnap-overlay > * {
    background: transparent;
    background-color: transparent;
    box-shadow: none;
}
"#;

/// Install the CSS provider on the default display.
pub fn install() {
    let provider = gtk4::CssProvider::new();
    provider.load_from_string(CSS);
    if let Some(display) = gdk4::Display::default() {
        gtk4::style_context_add_provider_for_display(
            &display,
            &provider,
            gtk4::STYLE_PROVIDER_PRIORITY_APPLICATION,
        );
    }
}
