fn main() {
    // Build the test shared library for integration tests
    if std::env::var("CARGO_CFG_TEST").is_ok() || std::env::var("OTTER_FFI_BUILD_TEST_LIB").is_ok()
    {
        let out_dir = std::env::var("OUT_DIR").unwrap();
        let suffix = dylib_suffix();
        let lib_path = format!("{}/libtest_ffi.{}", out_dir, suffix);

        let status = std::process::Command::new("cc")
            .args(["-shared", "-o", &lib_path, "test_lib/test_ffi.c"])
            .status()
            .expect("Failed to run C compiler");
        assert!(status.success(), "Failed to build test shared library");

        println!("cargo:rustc-env=TEST_FFI_LIB_PATH={}", lib_path);
        println!("cargo:rerun-if-changed=test_lib/test_ffi.c");
    }
}

fn dylib_suffix() -> &'static str {
    if cfg!(target_os = "macos") {
        "dylib"
    } else if cfg!(target_os = "windows") {
        "dll"
    } else {
        "so"
    }
}
