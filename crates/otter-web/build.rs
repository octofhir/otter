//! Host compilation of the Web extension's static classic-script bundle.
//!
//! # Contents
//! The small Cargo entry delegates to the focused bootstrap producer.
//!
//! # Invariants
//! The producer depends only on frontend/bytecode crates, never the Runtime.

#[path = "build/bootstrap.rs"]
mod bootstrap;

fn main() -> Result<(), Box<dyn std::error::Error>> {
    bootstrap::generate(&std::path::PathBuf::from(
        std::env::var_os("OUT_DIR").ok_or("OUT_DIR is required")?,
    ))
}
