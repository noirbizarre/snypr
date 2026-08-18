// The icon gresource is only consumed by `ui::install_icon_resources`, so compiling it —
// and pulling in glib-sys, which needs system glib — is gated on the `ui` feature. Without
// this a `--no-default-features` build still required libglib2.0-dev to compile.
fn main() {
    #[cfg(feature = "ui")]
    glib_build_tools::compile_resources(&["data"], "data/icons.gresource.xml", "snypr.gresource");
}
