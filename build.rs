//! Embeds the application icon and version metadata into the Windows executable.
//!
//! This is what Explorer, the taskbar and the Alt-Tab switcher read. The window's
//! own icon is set separately in `main.rs` from `assets/icon.rgba`.
//!
//! Note: `#[cfg(target_os = "windows")]` is wrong here - a build script runs on the
//! *host*, so the target OS has to come from Cargo's environment instead.

fn main() {
    println!("cargo:rerun-if-changed=assets/icon.ico");
    println!("cargo:rerun-if-changed=build.rs");

    if std::env::var("CARGO_CFG_TARGET_OS").as_deref() != Ok("windows") {
        return;
    }

    #[cfg(windows)]
    {
        let mut res = winresource::WindowsResource::new();
        res.set_icon("assets/icon.ico");
        res.set("FileDescription", "Macro Recorder");
        res.set("ProductName", "Macro Recorder");
        res.set("OriginalFilename", "MacroRecorder.exe");
        res.set("LegalCopyright", "MIT License");

        // A missing resource compiler (rc.exe / windres.exe) should cost you the
        // Explorer icon, not the whole build - the app still works fine without it.
        if let Err(e) = res.compile() {
            println!("cargo:warning=could not embed the executable icon: {e}");
        }
    }
}

