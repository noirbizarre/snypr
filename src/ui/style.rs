//! Shared CSS / styling for Snypr windows.

pub const CSS: &str = r#"
.snypr-toolbar {
    background: alpha(@theme_bg_color, 0.85);
    border-radius: 12px;
    padding: 6px;
    margin: 12px;
}

.snypr-toolbar separator {
    margin: 2px 4px;
    background-color: alpha(@theme_fg_color, 0.18);
}

.snypr-toolbar button.suggested-action {
    background-color: @theme_selected_bg_color;
    color: @theme_selected_fg_color;
}

/* The selector overlay window must be fully transparent so the snapshot path
 * (which paints a translucent dim + selection outline) composites directly
 * over the live desktop, not over GTK's default opaque window background. */
window.snypr-selector,
window.snypr-selector decoration,
window.snypr-selector > *,
window.snypr-overlay,
window.snypr-overlay decoration,
window.snypr-overlay > * {
    background: transparent;
    background-color: transparent;
    box-shadow: none;
}

/* Pre-capture countdown window: full-screen translucent dim with a huge
 * white seconds-remaining numeral. Used by non-interactive CLI paths
 * (`--full --delay 3s`, daemon screenshot, tray) where there is no
 * selector to host the countdown. Colors are injected per-invocation by
 * `crate::ui::countdown::install_countdown_css` from `[ui.selector]` config
 * (`countdown_bg`, `countdown_fg`); only the structural rules live here. */
window.snypr-countdown label.snypr-countdown-number {
    font-weight: bold;
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
