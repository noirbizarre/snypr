//! Shared CSS / styling for HyprSnap windows.

pub const CSS: &str = r#"
.hyprsnap-toolbar {
    background: alpha(@theme_bg_color, 0.85);
    border-radius: 12px;
    padding: 6px;
    margin: 12px;
}

.hyprsnap-selector-dim {
    background-color: rgba(0, 0, 0, 0.45);
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
