// Embeds the FROG icon and name in the Windows executable, so Explorer and the taskbar show
// them. Other platforms get their icon at run time, from assets/frog-64.rgba.
fn main() {
    println!("cargo:rerun-if-changed=assets/frog.ico");
    if std::env::var("CARGO_CFG_TARGET_OS").as_deref() == Ok("windows") {
        let mut res = winres::WindowsResource::new();
        res.set_icon("assets/frog.ico");
        // Cross-compiling from Linux, the resource compiler is the mingw one.
        if !cfg!(windows) {
            res.set_windres_path("x86_64-w64-mingw32-windres");
        }
        res.set("FileDescription", "FROG");
        res.set("ProductName", "FROG");
        res.compile().expect("could not embed the Windows icon");
    }
}
