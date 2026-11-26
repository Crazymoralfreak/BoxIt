use tauri_build::{Attributes, WindowsAttributes};

fn main() {
    // The manifest of the program: see the comments inside it.
    let windows = WindowsAttributes::new().app_manifest(include_str!("windows/app.manifest"));
    tauri_build::try_build(Attributes::new().windows_attributes(windows))
        .expect("the build script of Tauri failed");
}
