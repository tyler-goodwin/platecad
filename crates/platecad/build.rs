//! Embeds the app icon and version details into `plate.exe` on Windows.

fn main() {
    println!("cargo:rerun-if-changed=../../assets/plate.ico");
    if std::env::var("CARGO_CFG_TARGET_OS").as_deref() != Ok("windows") {
        return;
    }
    let mut resource = winresource::WindowsResource::new();
    // Resource id 1: Explorer shows the lowest-numbered icon, and the viewer
    // loads this one for its window (cad_view_threed::ICON_RESOURCE_ID).
    resource
        .set_icon_with_id("../../assets/plate.ico", "1")
        .set("ProductName", "Plate CAD")
        .set("FileDescription", "Plate CAD: code-first CAD for 3D printing")
        .set("LegalCopyright", "Copyright (c) 2026 Tyler Goodwin");
    resource.compile().expect("embedding the Windows icon resource");
}
