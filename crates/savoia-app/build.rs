//! Embeds the app icon into the Windows executable. GPUI loads resource 1 as
//! the window and taskbar icon, and Explorer and the installer's shortcuts
//! show it too; without it every one of them falls back to a blank icon.

fn main() {
    println!("cargo::rerun-if-changed=build.rs");
    println!("cargo::rerun-if-changed=../../assets/icons/icon.ico");

    #[cfg(windows)]
    if std::env::var("CARGO_CFG_TARGET_OS").as_deref() == Ok("windows") {
        let mut res = winresource::WindowsResource::new();
        res.set_icon("../../assets/icons/icon.ico");
        res.compile().expect("failed to embed the Windows icon");
    }
}
