# HyprSnap

A GTK4-based screenshot, annotation, and live-drawing tool for [Hyprland](https://hyprland.org/).

HyprSnap pulls together what currently requires three separate tools on a Wayland desktop:

- **Capture** — like [`grim`](https://sr.ht/~emersion/grim/) / [HyprCapture](https://github.com/gfhdhytghd/HyprCapture), but native and integrated.
- **Annotate** — like [Satty](https://github.com/Satty-org/Satty): arrow, rectangle, highlight, blur, text, freehand, numbered marker, redact, crop.
- **Draw live on the screen** — like [Draw-On-Gnome](https://github.com/daveprowse/Draw-On-Gnome) but on wlroots / Hyprland, ideal for streaming and Google Meet presentations.

Screen capture talks the `zwlr_screencopy_manager_v1` Wayland protocol directly; UI is GTK4 with `gtk4-layer-shell`, and the annotation canvas uses GSK render nodes for GPU-accelerated drawing.

## Status

This is an early-stage project. The first vertical slice (capture → encode → file / clipboard) is implemented; full UI flows are landing incrementally per the plan in `.opencode/plans/`.

| Subcommand    | Status                                                              |
| ------------- | ------------------------------------------------------------------- |
| `screenshot`  | Capture pipeline + file/clipboard sinks implemented                 |
| `annotate`    | Editor window opens and loads images; tool drawing in progress      |
| `capture`     | Wired end-to-end; selector overlay pending                          |
| `draw`        | Stub — live overlay pending                                         |
| `daemon`      | IPC server running; handlers in progress                            |

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

# Specific output by name, copied to the clipboard.
hyprsnap screenshot --output DP-1 --to clipboard

# Explicit region (logical pixels).
hyprsnap screenshot --region 100,200,800x600 --to file

# Open the annotation editor on an existing image.
hyprsnap annotate ~/Pictures/shot.png
```

### Hyprland keybindings

```hyprlang
# ~/.config/hypr/hyprland.conf
bind = SUPER,        Print, exec, hyprsnap screenshot --interactive --to clipboard --to file
bind = SUPER SHIFT,  Print, exec, hyprsnap screenshot --full --to file
bind = SUPER ALT,    Print, exec, hyprsnap draw
```

## Configuration

`~/.config/hyprsnap/config.toml` (every field is optional):

```toml
[output]
directory          = "/home/me/Pictures/Screenshots"
filename_template  = "hyprsnap_{date}_{time}_{output}.png"
default_sinks      = ["file", "clipboard"]
use_utc            = false

[capture]
cursor = false

[tray]
enabled = false

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

## License

MIT. See [LICENSE](LICENSE).
