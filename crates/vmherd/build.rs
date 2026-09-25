//! Windows: embed the application icon and version info into vmherd.exe.

fn main() {
    println!("cargo:rerun-if-changed=../../assets/icon/vmherd.ico");
    #[cfg(windows)]
    if std::env::var("CARGO_CFG_TARGET_OS").as_deref() == Ok("windows") {
        let mut res = winresource::WindowsResource::new();
        res.set_icon("../../assets/icon/vmherd.ico");
        res.set("ProductName", "VMherd");
        res.set("FileDescription", "VMherd: many Proxmox VM consoles, one keyboard");
        res.set("CompanyName", "The Integrators BV");
        res.set("LegalCopyright", "© 2026 The Integrators BV (NL), Wietse Wind");
        if let Err(e) = res.compile() {
            println!("cargo:warning=could not embed the Windows icon: {e}");
        }
    }
}
