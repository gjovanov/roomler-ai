fn main() {
    // This file is the script's only input. Without the line, cargo re-runs
    // it whenever ANY file of the crate looks newer than its last run (it
    // fingerprints the crate's newest mtime), and a release lane that reuses
    // a same-tree build would recompile roomlerd behind it (FR-6 wave 29).
    println!("cargo:rerun-if-changed=build.rs");
    if cfg!(windows) {
        // The first choice is Windows because DXGI is amazing.
        println!("cargo:rustc-cfg=dxgi");
    } else if cfg!(target_os="macos") {
        // Quartz is second because macOS is the (annoying) exception.
        println!("cargo:rustc-cfg=quartz");
    } else if cfg!(unix) {
        // On UNIX we pray that X11 (with XCB) is available.
        println!("cargo:rustc-cfg=x11");
    }
}
