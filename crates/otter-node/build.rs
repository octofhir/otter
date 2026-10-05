//! Product-build compilation of the Node builtin modules and realm installer.
//!
//! # Contents
//! The small Cargo entry delegates to the focused builtin producer.
//!
//! # Invariants
//! The producer depends only on frontend/bytecode crates, never the Runtime.

#[path = "build/builtins.rs"]
mod builtins;

fn main() -> Result<(), Box<dyn std::error::Error>> {
    let out = std::env::var_os("OUT_DIR").ok_or("OUT_DIR is required")?;
    let manifest =
        std::env::var_os("CARGO_MANIFEST_DIR").ok_or("CARGO_MANIFEST_DIR is required")?;
    builtins::generate(
        std::path::Path::new(&out),
        &std::path::Path::new(&manifest).join("src"),
    )
}
