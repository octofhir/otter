fn main() {
    let out_dir = std::env::var("OUT_DIR").expect("OUT_DIR should be set");
    let suffix = dylib_suffix();
    let lib_path = format!("{out_dir}/libtest_ffi.{suffix}");

    let status = std::process::Command::new("cc")
        .args(["-shared", "-o", &lib_path, "test_lib/test_ffi.c"])
        .status()
        .expect("failed to run C compiler");
    assert!(status.success(), "failed to build ffi test library");

    println!("cargo:rustc-env=TEST_FFI_LIB_PATH={lib_path}");
    println!("cargo:rerun-if-changed=test_lib/test_ffi.c");
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
