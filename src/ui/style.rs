//! Shared CSS / styling for HyprSnap windows.

pub const CSS: &str = r#"
.hyprsnap-toolbar {
    background: alpha(@theme_bg_color, 0.85);
    border-radius: 12px;
    padding: 6px;
    margin: 12px;
}

.hyprsnap-toolbar separator {
    margin: 2px 4px;
    background-color: alpha(@theme_fg_color, 0.18);
}

.hyprsnap-toolbar button.suggested-action {
    background-color: @theme_selected_bg_color;
    color: @theme_selected_fg_color;
}

/* The selector overlay window must be fully transparent so the snapshot path
 * (which paints a translucent dim + selection outline) composites directly
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
