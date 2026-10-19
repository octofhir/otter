//! Linker flags for the CLI binaries: the Node-API symbols dynamically loaded
//! addons bind to, and a load-time-friendly fixup format.
//!
//! # Contents
//! - Platform linker flags for the `otter` and `otterx` binaries.
//!
//! # Invariants
//! - Addons resolve only the `napi_*` / `node_api_*` families from the
//!   executable. On macOS exactly those are exported; every other global stays
//!   local, so the loader never builds or searches a trie of the whole binary.
//! - macOS binaries carry chained fixups, which the loader applies lazily per
//!   page instead of walking every rebase at launch.
//! - None of this enables addon loading or bypasses the runtime's `read` and
//!   `ffi` capabilities.
//!
//! # See also
//! - `otter_node::napi`

fn main() {
    let target_os = std::env::var("CARGO_CFG_TARGET_OS").unwrap_or_default();
    match target_os.as_str() {
        "macos" => {
            for arg in [
                "-Wl,-export_dynamic",
                "-Wl,-exported_symbol,_napi_*",
                "-Wl,-exported_symbol,_node_api_*",
                "-Wl,-fixup_chains",
            ] {
                println!("cargo:rustc-link-arg-bins={arg}");
            }
        }
        "linux" => println!("cargo:rustc-link-arg-bins=-Wl,--export-dynamic"),
        _ => {}
    }
}
