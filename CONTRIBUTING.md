# Contributing to hyprsnap

## Adding a bundled icon

Hyprsnap ships a small set of [icon-development-kit](https://gitlab.gnome.org/Teams/Design/icon-development-kit)
SVGs via a gresource so that toolbar buttons render reliably regardless of the
user's icon theme. To add one more:

1. Pick the icon you want from the [GNOME Icon Library](https://flathub.org/apps/org.gnome.design.IconLibrary)
   app — every icon shown there is CC0 1.0 and safe to vendor here under MIT.
2. Save the SVG to `data/icons/scalable/actions/<name>-symbolic.svg`.
   - Use the **freedesktop** name (e.g. `document-save-symbolic`). GTK will then
     resolve `Image::from_icon_name("document-save-symbolic")` to your file
     before falling back to the system theme.
3. Add one `<file>` line to `data/icons.gresource.xml`:

   ```xml
   <file alias="<name>-symbolic.svg" preprocess="xml-stripblanks">icons/scalable/actions/<name>-symbolic.svg</file>
   ```

4. `cargo build` — `build.rs` recompiles the gresource automatically and
   `cargo:rerun-if-changed=...` directives ensure the SVG change is picked up.
5. No Rust code changes needed: any existing `Image::from_icon_name("<name>-symbolic")`
   call now picks up the vendored asset.

The vendored icons are CC0; attribution lives in `data/icons/LICENSE.md` and
the README `Acknowledgements` section.
