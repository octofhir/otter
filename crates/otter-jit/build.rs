//! Expose Cargo's exact native target triple to JIT manifests.
//!
//! The build script performs no probing and emits no generated source. The
//! ordinary compiler path reads the value only when artifact capture is
//! explicitly enabled.

fn main() {
    println!("cargo:rerun-if-changed=build.rs");
    let target = std::env::var("TARGET").expect("Cargo provides TARGET to build scripts");
    println!("cargo:rustc-env=OTTER_JIT_TARGET={target}");
}
