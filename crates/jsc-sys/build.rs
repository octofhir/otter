use std::env;

fn main() {
    let target_os = env::var("CARGO_CFG_TARGET_OS").unwrap();

    match target_os.as_str() {
        "macos" => configure_macos(),
        "linux" => configure_linux(),
        _ => panic!("Unsupported OS for JSC: {}", target_os),
    }
}

fn configure_macos() {
    println!("cargo:rustc-link-lib=framework=JavaScriptCore");

    if let Ok(sdk_path) = std::process::Command::new("xcrun")
        .args(["--show-sdk-path"])
        .output()
    {
        let sdk_path = String::from_utf8_lossy(&sdk_path.stdout);
        let sdk_path = sdk_path.trim();
        println!(
            "cargo:rustc-link-search=framework={}/System/Library/Frameworks",
            sdk_path
        );
    }
}

fn configure_linux() {
    let jsc_pkg = if pkg_config::probe_library("javascriptcoregtk-4.1").is_ok() {
        "javascriptcoregtk-4.1"
    } else if pkg_config::probe_library("javascriptcoregtk-4.0").is_ok() {
        "javascriptcoregtk-4.0"
    } else {
        panic!(
            "JavaScriptCore not found on Linux. Install with:\n\
             Ubuntu/Debian: sudo apt-get install libjavascriptcoregtk-4.1-dev\n\
             Fedora: sudo dnf install webkit2gtk4.1-devel\n\
             Or build from WebKit sources."
        );
    };

    println!("cargo:rerun-if-env-changed=PKG_CONFIG_PATH");
    println!("cargo:rustc-cfg=jsc_gtk");

    let lib = pkg_config::Config::new()
        .atleast_version("2.30")
        .probe(jsc_pkg)
        .expect("Failed to configure JSC via pkg-config");

    for path in lib.include_paths {
        println!("cargo:include={}", path.display());
    }
}
