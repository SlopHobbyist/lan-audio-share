//! Embeds the Windows application icon into the executable.
//!
//! This is what Explorer, the taskbar and Alt-Tab read. The window itself also
//! sets an icon at runtime (see `main.rs`); both are needed, because the
//! executable resource and the window icon are separate things to Windows.

fn main() {
    println!("cargo:rerun-if-changed=assets/AppIcon.ico");

    #[cfg(windows)]
    {
        let mut resource = winresource::WindowsResource::new();
        resource.set_icon("assets/AppIcon.ico");
        // A missing resource compiler should cost the icon, not the build.
        if let Err(err) = resource.compile() {
            println!("cargo:warning=could not embed the app icon: {err}");
        }
    }
}
