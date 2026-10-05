//! Record the actual compilation target for runner provenance.

fn main() {
    println!(
        "cargo:rustc-env=OTTER_TEST262_BUILD_TARGET={}",
        std::env::var("TARGET").expect("Cargo target")
    );
}
