fn main() {
    glib_build_tools::compile_resources(
        &["data"],
        "data/icons.gresource.xml",
        "hyprsnap.gresource",
    );
}
