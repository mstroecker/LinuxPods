//! Compiles the artwork into a GResource that `ui::register_resources` includes
//! in the binary, so an installed copy no longer reads it from the checkout it
//! was built in.

fn main() {
    glib_build_tools::compile_resources(
        &["assets"],
        "assets/resources.gresource.xml",
        "linuxpods.gresource",
    );
}
