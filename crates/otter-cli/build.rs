//! Export executable symbols needed by dynamically loaded Node-API addons.
//!
//! # Contents
//! - Platform linker flags for the `otter` binary.
//!
//! # Invariants
//! - This only changes dynamic symbol visibility; it does not enable addon
//!   loading or bypass the runtime's `read` and `ffi` capabilities.
//!
//! # See also
//! - `otter_node::napi`

fn main() {
    let target_os = std::env::var("CARGO_CFG_TARGET_OS").unwrap_or_default();
    match target_os.as_str() {
        "macos" => println!("cargo:rustc-link-arg-bins=-Wl,-export_dynamic"),
        "linux" => println!("cargo:rustc-link-arg-bins=-Wl,--export-dynamic"),
        _ => {}
    }
}
