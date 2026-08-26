# Snypr — English (en) — source-of-truth catalog.
#
# Keys are referenced via the `fl!` macro; the i18n-embed-fl proc-macro
# validates every key against this file at compile time.

## Toolbar — tool buttons (tooltips)
toolbar-tool-select = Select
toolbar-tool-rect = Rectangle
toolbar-tool-ellipse = Ellipse
toolbar-tool-arrow = Arrow
toolbar-tool-line = Line
toolbar-tool-highlight = Highlight
toolbar-tool-freehand = Freehand
toolbar-tool-number = Number
toolbar-tool-text = Text
toolbar-tool-blur = Blur
toolbar-tool-redact = Redact
toolbar-tool-crop = Crop

## Toolbar — mode buttons (tooltips)
toolbar-mode-full = Full
toolbar-mode-screen = Screen
toolbar-mode-window = Window
toolbar-mode-region = Region

## Toolbar — action buttons / pickers
toolbar-annotate-tooltip = Annotate (Shift-click or Shift+Enter)
toolbar-capture-tooltip-shift = Capture (Enter) — Shift to annotate
toolbar-capture-tooltip-plain = Capture (Enter)
toolbar-color-tooltip = Tool color (alpha included)
toolbar-stroke-solid = Solid stroke
toolbar-stroke-dashed = Dashed stroke
toolbar-stroke-dotted = Dotted stroke
toolbar-font-size-tooltip = Font size (pt)
toolbar-undo-tooltip = Undo (Ctrl+Z)
toolbar-clear-tooltip = Clear (Ctrl+L)
toolbar-cursor-tooltip = Include cursor in capture
toolbar-delay-tooltip = Delay before capture, in seconds
toolbar-passthrough-tooltip = Toggle pointer passthrough (P)
toolbar-save-tooltip = Save (Ctrl+S or Enter)
toolbar-output-file = Destination: file (Ctrl+O to cycle)
toolbar-output-clipboard = Destination: clipboard (Ctrl+O to cycle)
toolbar-output-both = Destination: file and clipboard (Ctrl+O to cycle)
toolbar-delay-label = { $secs }s
toolbar-font-size-label = { $pt }pt
toolbar-color-dialog-title = Pick a Color

## Selector hints
selector-hint-region-empty = Drag to select a region — Enter to confirm, Esc to cancel
selector-hint-region-size = { $width } × { $height } — Enter to confirm, Esc to cancel
selector-hint-full = Full desktop — Enter to confirm, Esc to cancel
selector-hint-screen-selected = Screen selected — Enter to confirm, Esc to cancel
selector-hint-screen-pick = Click a screen — Enter to confirm, Esc to cancel
selector-hint-window-class-title = { $class }: { $title } — Enter to confirm, Esc to cancel
selector-hint-window-class = { $class } — Enter to confirm, Esc to cancel
selector-hint-window-title = { $title } — Enter to confirm, Esc to cancel
selector-hint-window-selected = Window selected — Enter to confirm, Esc to cancel
selector-hint-window-pick = Click a window — Enter to confirm, Esc to cancel

## Tray menu entries
tray-screenshot-full = Screenshot (full)
tray-annotate-region = Annotate region…
tray-draw-on-screen = Draw on screen
tray-quit = Quit

## Desktop notifications
notify-copied = Screenshot copied to clipboard
notify-saved-single = Screenshot saved
notify-saved-multi = Screenshots saved
notify-saved-multi-body = { $first } ({ $count ->
        [one] { $count } file
       *[other] { $count } files
    })

## User-visible errors
error-edit-incompatible-per-output = `--edit` is incompatible with `--per-output` (the annotation editor operates on a single image)
error-edit-requires-ui-feature = `--edit` requires the `ui` cargo feature; rebuild with it or drop the flag
error-interactive-requires-ui-feature = interactive selector requires the `ui` cargo feature; pass a concrete --region, --full, or other flag
error-draw-requires-ui-feature = snypr was built without the `ui` feature; `draw` is unavailable
error-invalid-region = invalid region: { $spec } (expected X,Y,WxH)
error-invalid-region-size = invalid region size: { $size } (expected WxH)
error-overlay-no-monitor = no monitor intersected the requested edit region; nothing to annotate
error-daemon-no-response = daemon closed connection without responding
error-daemon-message = daemon: { $message }
error-no-display = no GDK display available
error-no-monitors = no monitors reported by GDK
error-gtk-exit = GTK exited with status { $code }
error-no-active-window = no window is currently focused
error-no-focused-monitor = no monitor is currently focused
error-not-under-hyprland = HYPRLAND_INSTANCE_SIGNATURE is not set; snypr does not appear to be running under Hyprland
error-not-under-sway = SWAYSOCK is not set; snypr does not appear to be running under Sway
error-not-under-niri = NIRI_SOCKET is not set; snypr does not appear to be running under Niri
error-unsupported-compositor = no supported window manager IPC was detected (neither Hyprland, Sway, nor Niri); this feature is unavailable on this compositor
error-no-draw-overlay = no draw overlay is currently running
error-overlay-channel-closed = the draw overlay stopped accepting commands
error-editor-busy = another editor session is already in progress
error-malformed-request = malformed request: { $reason }
error-unknown-clipboard-kind = unknown clipboard kind `{ $kind }` (expected `regular`, `primary`, or `both`)
error-unknown-sink = unknown sink `{ $sink }` (expected `file` or `clipboard`)
