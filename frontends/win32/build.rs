//! Embeds the manifest: Common Controls 6, per-monitor DPI v2, asInvoker (as rusty-backup does).

fn main() {
    println!("cargo:rerun-if-changed=legcon-win32.manifest");
    let target = std::env::var("TARGET").unwrap_or_default();
    if target.contains("windows-msvc") {
        let manifest = std::path::Path::new(&std::env::var("CARGO_MANIFEST_DIR").unwrap())
            .join("legcon-win32.manifest");
        println!("cargo:rustc-link-arg-bins=/MANIFEST:EMBED");
        println!(
            "cargo:rustc-link-arg-bins=/MANIFESTINPUT:{}",
            manifest.display()
        );
    }
}
