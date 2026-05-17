<p align="center">
  <img src="docs/logo-text.png" alt="HyprSnap" />
</p>

A GTK4-based screenshot, annotation, and live-drawing tool for [Hyprland](https://hyprland.org/).

HyprSnap pulls together what currently requires three separate tools on a Wayland desktop:

- **Capture** — like [`grim`](https://sr.ht/~emersion/grim/) / [HyprCapture](https://github.com/gfhdhytghd/HyprCapture), but native and integrated.
- **Annotate** — like [Satty](https://github.com/Satty-org/Satty): arrow, rectangle, ellipse, highlight, blur, text, freehand, numbered marker, redact, crop.
- **Draw live on the screen** — like [Draw-On-Gnome](https://github.com/daveprowse/Draw-On-Gnome) but on wlroots / Hyprland, ideal for streaming and Google Meet presentations.

Screen capture talks the `zwlr_screencopy_manager_v1` Wayland protocol directly; UI is GTK4 with `gtk4-layer-shell`, and the annotation canvas uses GSK render nodes for GPU-accelerated drawing.

## Status

The four subcommands are wired end-to-end. All ten annotation tools (Rect, Ellipse, Arrow,
Highlight, Freehand, Number, Text, Blur, Redact, Crop) render through GSK render nodes on
screen and flatten to PNG through Cairo on save.

| Subcommand    | Status                                                                            |
| ------------- | --------------------------------------------------------------------------------- |
| `screenshot`  | Capture pipeline, all selection modes, file/clipboard sinks, `--per-output`, `--edit` opens the in-place annotation overlay before sinks |
| `draw`        | Live overlay with pointer passthrough toggle, exclusive keyboard, shared tools; Ctrl+S saves via the zone selector |
| `daemon`      | IPC server: `Ping`, `Screenshot`, `DrawToggle`; tray (StatusNotifierItem) when enabled in config |

## Build

```sh
mise run build           # cargo build
mise run test            # cargo nextest run
mise run lint            # cargo clippy --all-targets --all-features -- -Dclippy::all
mise run fmt             # cargo fmt --all
mise run cover           # cargo llvm-cov nextest
```

System dependencies (Debian / Ubuntu):

```sh
sudo apt install libgtk-4-dev libgtk4-layer-shell-dev libwayland-dev pkg-config
```

## Usage

```sh
# Full-screen capture, stitched across all outputs, saved as PNG.
hyprsnap screenshot --full --to file=/tmp/shot.png

# Default sinks come from ~/.config/hyprsnap/config.toml; without --to,
# the screenshot is written to $XDG_PICTURES_DIR/Screenshots/.
hyprsnap screenshot --full

# One file per output (uses {output} in the filename template).
hyprsnap screenshot --full --per-output

# Specific output by name, copied to the clipboard.
hyprsnap screenshot --output DP-1 --to clipboard

# Focused window, queried over Hyprland IPC.
hyprsnap screenshot --focused

# Explicit region (logical pixels): X,Y,WxH.
hyprsnap screenshot --region 100,200,800x600 --to file

# Interactive region selector → in-place annotation overlay → sinks.
hyprsnap screenshot --edit --to clipboard --to file

# Live draw-on-screen overlay (R/O/A/H/F/N/X tools, Ctrl+Z undo, P passthrough, Esc quit).
# Press Ctrl+S to save: pops the zone selector to pick what to capture (region/output/
# window/full); strokes are baked in and the overlay stays open for more drawing.
hyprsnap draw --to file --to clipboard --cursor

# Run the daemon (IPC server; add `--systray` for a StatusNotifierItem icon).
hyprsnap daemon

# Take a screenshot via the running daemon instead of spawning a fresh process.
hyprsnap screenshot --full --via-daemon
```

### Interactive selector

The selector (used by `screenshot --edit` and the default `screenshot`) shows a floating
toolbar on the primary monitor with four modes — **Full**, **Screen**, **Window**,
**Region** — plus a cursor toggle and a **Capture** button. Hold `Shift` while
clicking Capture (the button's icon swaps live) to *also* open the in-place editor on
the captured image. Keyboard shortcuts: `1/2/3/4` switch modes, drag with the mouse in
Region mode, click on a monitor in Screen mode, then press `Enter` (Capture) or
`Shift+Enter` (Capture + Annotate) to commit. `Esc` cancels.

### Editor & overlay keybinds

| Key      | Action                       |
| -------- | ---------------------------- |
| `R`      | Rectangle tool               |
| `O`      | Ellipse tool                 |
| `A`      | Arrow tool                   |
| `L`      | Line tool (no arrowhead)     |
| `H`      | Highlight tool               |
| `F`      | Freehand tool                |
| `N`      | Numbered marker              |
| `T`      | Text (popover entry)         |
| `B`      | Blur (editor only)           |
| `X`      | Redact (solid black)         |
| `C`      | Crop (editor only)           |
| `Ctrl+Z` | Undo last layer              |
| `Ctrl+S` / `Enter` | Save (editor and draw overlay) |
| `P`      | Toggle pointer passthrough (overlay only) |
| `Ctrl+L` | Clear all layers (overlay only)           |
| `Esc`    | Quit                         |

A color picker (with alpha) sits next to the tool buttons. Each tool remembers its own
color across switches within a session; it's disabled for tools whose appearance is
hardcoded (Blur, Crop, Redact).

In the **draw overlay**, `Ctrl+S` (or `Enter`, or the toolbar Save button) pops the
screenshot zone selector so you choose what part of the screen to capture (region,
monitor, window, or full desktop). Because the strokes are already painted on the
layer-shell surfaces, the captured PNG naturally contains "desktop + strokes" — no
post-processing. The overlay stays alive with strokes intact after saving, so you can
keep drawing or save another zone. Sinks come from `--to` (repeatable; defaults to
`[output].sinks` from the config), and `--cursor` seeds the selector's cursor toggle.

Next to the color picker, a stroke-style picker offers Solid / Dashed / Dotted dash
patterns for the outline-rendering tools (Rectangle, Ellipse, Arrow, Line, Freehand).
Like the color picker, each tool remembers its own style across switches. The Arrow
tool's shaft honours the style while its arrowhead stays solid so it remains a
recognisable pointer.

The picker opens GTK's native `GtkColorDialog`. Placement, sizing, and the resize
behaviour when toggling the *Custom Color* editor are delegated to the compositor —
`GtkColorDialog` does not expose its window to applications. On Hyprland (0.55+, Lua
config), add a window rule if you want it to always float and centre:

```lua
hl.window_rule({
  match  = { title = "^Pick a Color$" },
  float  = true,
  center = true,
})
```

### Hyprland keybindings

A ready-to-paste sample lives in [`docs/hyprland.conf.example`](docs/hyprland.conf.example):

```hyprlang
# ~/.config/hypr/hyprland.conf
bind = SUPER,        Print, exec, hyprsnap screenshot --edit --to clipboard --to file
bind = SUPER SHIFT,  Print, exec, hyprsnap screenshot --full --to file
bind = SUPER CTRL,   Print, exec, hyprsnap screenshot --focused --to clipboard
bind = SUPER ALT,    Print, exec, hyprsnap draw

# Toggle pointer passthrough on a running draw overlay. Useful because
# passthrough mode detaches the surface from the keyboard, so the overlay's
# own `P` shortcut can't turn passthrough back off — a global keybind can.
bind = SUPER ALT,    P,     exec, hyprsnap draw --via-daemon --toggle-passthrough

# Autostart the daemon (enables `--via-daemon`; add `--systray` for a tray icon).
exec-once = hyprsnap daemon --systray
```

### Troubleshooting

**Pressing the keybind does nothing?**

1. Inspect the actually-registered bind:
   ```sh
   hyprctl binds | grep -i print
   ```
   The dispatcher must be `exec` (not `exec_cmd` — that's the load-time
   directive). If your config layer (Nix module, etc.) emits `exec_cmd` inside
   a `bind`, Hyprland silently drops it.
2. Confirm the binary resolves on Hyprland's `PATH`:
   ```sh
   hyprctl dispatch exec "which hyprsnap"
   tail -n5 ~/.local/share/hyprland/hyprland.log
   ```
3. Check Hyprland's log for stderr from the spawned hyprsnap process — that's
   where errors land when launched from a keybind. The optional `notify`
   feature (enabled by default) also surfaces fatal errors as a desktop
   notification.
4. Add `-vv` to your bind (e.g. `hyprsnap -vv screenshot`) to upgrade the
   `hyprsnap` log level to trace without needing `RUST_LOG`.

## Configuration

`~/.config/hyprsnap/config.toml` (every field is optional):

```toml
[output]
directory          = "/home/me/Pictures/Screenshots"
filename_template  = "hyprsnap_{date}_{time}_{output}.png"
default_sinks      = ["file", "clipboard"]
use_utc            = false
# PNG compression preset: "fast" (largest, fastest), "balanced" (default), or "best"
# (smallest, ~10x slower than fast). Balanced typically halves file size vs fast.
compression        = "balanced"

[capture]
cursor = false

[keybinds.editor]
save = "<Ctrl>s"
copy = "<Ctrl>c"
quit = "Escape"

[keybinds.overlay]
toggle_passthrough = "p"
snapshot           = "s"
quit               = "Escape"
```

Template tokens: `{ts}`, `{date}`, `{time}`, `{output}`, `{selection}`.

## Architecture

See the design plan at `.opencode/plans/1778929144226-shiny-panda.md` for full details. The crate is a single binary with internal modules:

```
src/
├── cli/         # clap subcommands
├── capture/     # wlr-screencopy backend (smithay-client-toolkit)
├── annotate/    # document model, tool trait, GSK-free helpers
├── output/      # file + clipboard sinks
├── ui/          # GTK4 windows + AnnotationCanvas (gated behind `ui` feature)
├── hypr.rs      # Hyprland IPC
├── ipc.rs       # daemon protocol
├── daemon.rs    # Unix-socket IPC server
└── config.rs    # TOML configuration
```

## Acknowledgements

Toolbar icons are sourced from the GNOME [icon-development-kit](https://gitlab.gnome.org/Teams/Design/icon-development-kit)
and bundled under [CC0 1.0](https://creativecommons.org/publicdomain/zero/1.0/).
See `data/icons/LICENSE.md` for details.

## License

MIT. See [LICENSE](LICENSE).
