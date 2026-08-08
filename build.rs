//! Compiles the Windows resource script into the executable.
//!
//! This is the one piece of the build that needs a resource compiler, which is
//! why the tray and window icons deliberately avoid it and are embedded as
//! plain bytes instead. Only the icon Explorer shows on the file requires it.
//!
//! On any non-Windows target this is a no-op, so `cargo test` and `cargo run`
//! on macOS are unaffected. Cross-compiling from macOS needs `llvm-rc` on PATH;
//! see the build instructions in README.md.

fn main() {
    println!("cargo:rerun-if-changed=assets/winsend.rc");
    println!("cargo:rerun-if-changed=assets/winsend.ico");

    // Deliberately not fatal. A missing resource compiler costs the icon on
    // the .exe and nothing else, and failing the build over it would make the
    // cross-compile depend on tooling the rest of the binary does not need.
    if let Err(error) = embed_resource::compile("assets/winsend.rc", embed_resource::NONE).manifest_optional() {
        println!("cargo:warning=resource icon not embedded: {error}");
    }
}
