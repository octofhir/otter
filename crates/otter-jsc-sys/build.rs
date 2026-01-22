use cc::Build;
use std::env;
use std::fs;
use std::path::PathBuf;

// bun-webkit version from oven-sh/WebKit releases
// Update this when a new version is needed
const BUN_WEBKIT_VERSION: &str = "aaf3f80b1cc701b412f8abfb7c7f413644a229ff";

fn main() {
    // Avoid "unexpected_cfgs" warnings for cfg(has_bmalloc)
    println!("cargo:rustc-check-cfg=cfg(has_bmalloc)");

    let target_os = env::var("CARGO_CFG_TARGET_OS").unwrap();
    let target_arch = env::var("CARGO_CFG_TARGET_ARCH").unwrap();

    println!("cargo:rerun-if-env-changed=BUN_WEBKIT_VERSION");
    println!("cargo:rerun-if-changed=src/heap_stats.cpp");
    println!("cargo:rerun-if-changed=src/heap_stats_stub.c");

    match target_os.as_str() {
        "macos" => configure_macos(),
        "linux" => configure_linux(&target_arch),
        "windows" => configure_windows(&target_arch),
        _ => panic!("Unsupported OS for JSC: {}", target_os),
    }
}

fn configure_macos() {
    let target_arch = env::var("CARGO_CFG_TARGET_ARCH").unwrap();

    // Check if we should use system framework (for quick dev builds, no JIT)
    // or bun-webkit (for production builds with JIT enabled)
    let use_system_jsc = env::var("OTTER_USE_SYSTEM_JSC").is_ok();

    if use_system_jsc {
        // System JavaScriptCore - fast build, but NO JIT (interpreter only)
        println!("cargo:warning=Using system JavaScriptCore (no JIT - slower runtime)");
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
        compile_heap_stats_stub();
    } else {
        // bun-webkit with JIT enabled - slower build, but fast runtime
        let arch = match target_arch.as_str() {
            "x86_64" => "amd64",
            "aarch64" => "arm64",
            _ => panic!("Unsupported architecture for bun-webkit: {}", target_arch),
        };

        let webkit_path = download_bun_webkit("macos", arch);
        let include_dir = link_bun_webkit(&webkit_path, "macos");
        if let Some(include_dir) = include_dir {
            compile_heap_stats_cpp(&include_dir, "macos");
        } else {
            compile_heap_stats_stub();
        }
    }
}

fn configure_linux(target_arch: &str) {
    // Always use statically linked bun-webkit for Linux
    let arch = match target_arch {
        "x86_64" => "amd64",
        "aarch64" => "arm64",
        _ => panic!("Unsupported architecture for bun-webkit: {}", target_arch),
    };

    let webkit_path = download_bun_webkit("linux", arch);
    let include_dir = link_bun_webkit(&webkit_path, "linux");
    if let Some(include_dir) = include_dir {
        compile_heap_stats_cpp(&include_dir, "linux");
    } else {
        compile_heap_stats_stub();
    }
}

fn configure_windows(target_arch: &str) {
    let arch = match target_arch {
        "x86_64" => "amd64",
        _ => panic!(
            "Unsupported architecture for Windows bun-webkit: {}",
            target_arch
        ),
    };

    let webkit_path = download_bun_webkit("windows", arch);
    let include_dir = link_bun_webkit(&webkit_path, "windows");
    if let Some(include_dir) = include_dir {
        compile_heap_stats_cpp(&include_dir, "windows");
    } else {
        compile_heap_stats_stub();
    }

    // Windows-specific system libraries
    println!("cargo:rustc-link-lib=winmm");
    println!("cargo:rustc-link-lib=bcrypt");
    println!("cargo:rustc-link-lib=ntdll");
    println!("cargo:rustc-link-lib=userenv");
    println!("cargo:rustc-link-lib=dbghelp");
    println!("cargo:rustc-link-lib=crypt32");
    println!("cargo:rustc-link-lib=wsock32");
    println!("cargo:rustc-link-lib=ws2_32");
    println!("cargo:rustc-link-lib=advapi32");
    println!("cargo:rustc-link-lib=ole32");
    println!("cargo:rustc-link-lib=oleaut32");
    println!("cargo:rustc-link-lib=uuid");
    println!("cargo:rustc-link-lib=shell32");

    // MSVC C++ runtime (required for JSC on Windows)
    // Use static linking to avoid runtime dependencies
    println!("cargo:rustc-link-arg=/NODEFAULTLIB:libcmt");
    println!("cargo:rustc-link-lib=msvcrt");
}

fn download_bun_webkit(os: &str, arch: &str) -> PathBuf {
    let version = env::var("BUN_WEBKIT_VERSION").unwrap_or_else(|_| BUN_WEBKIT_VERSION.to_string());

    // Cache directory
    let cache_dir = get_cache_dir();
    let webkit_dir = cache_dir.join(&version).join(format!("{}-{}", os, arch));

    // Check if already downloaded
    let marker = webkit_dir.join(".downloaded");
    if marker.exists() {
        println!(
            "cargo:warning=Using cached bun-webkit from {}",
            webkit_dir.display()
        );
        return webkit_dir;
    }

    // Download URL from oven-sh/WebKit
    let artifact_name = format!("bun-webkit-{}-{}.tar.gz", os, arch);
    let url = format!(
        "https://github.com/oven-sh/WebKit/releases/download/autobuild-{}/{}",
        version, artifact_name
    );

    println!("cargo:warning=Downloading bun-webkit from {}", url);

    // Create cache directory
    fs::create_dir_all(&webkit_dir).expect("Failed to create cache directory");

    // Download using ureq - stream directly to decoder to avoid memory limits
    let response = ureq::get(&url)
        .call()
        .unwrap_or_else(|e| panic!("Failed to download bun-webkit: {}. URL: {}", e, url));

    // Stream directly to tar decoder without loading entire file into memory
    let reader = response.into_body().into_reader();
    let tar_gz = flate2::read::GzDecoder::new(reader);
    let mut archive = tar::Archive::new(tar_gz);

    archive
        .unpack(&webkit_dir)
        .expect("Failed to extract bun-webkit archive");

    // Create marker file
    fs::write(&marker, "").expect("Failed to create marker file");

    println!(
        "cargo:warning=bun-webkit extracted to {}",
        webkit_dir.display()
    );

    webkit_dir
}

fn link_bun_webkit(webkit_path: &PathBuf, os: &str) -> Option<PathBuf> {
    // Find the lib directory - bun-webkit extracts to a subdirectory
    let lib_dir = find_lib_dir(webkit_path);

    println!("cargo:rustc-link-search=native={}", lib_dir.display());

    // Print available libraries for debugging
    if let Ok(entries) = fs::read_dir(&lib_dir) {
        println!("cargo:warning=Libraries found in {}:", lib_dir.display());
        for entry in entries.flatten() {
            println!("cargo:warning=  - {}", entry.path().display());
        }
    }

    // Link order matters! WTF -> JSC -> bmalloc -> ICU
    // Libraries that depend on others must come first (Unix ld resolves left-to-right)
    println!("cargo:rustc-link-lib=static=WTF");
    println!("cargo:rustc-link-lib=static=JavaScriptCore");

    // bmalloc may be bundled into WTF on some Windows builds
    if lib_exists(&lib_dir, "bmalloc") {
        println!("cargo:rustc-link-lib=static=bmalloc");
        println!("cargo:rustc-cfg=has_bmalloc");
    } else {
        println!("cargo:warning=bmalloc library not found, assuming bundled in WTF");
    }

    // ICU libraries (statically linked in bun-webkit)
    // Windows bun-webkit uses "sicu*" names instead of "icu*"
    // Order: icui18n depends on icuuc, icuuc depends on icudata
    if lib_exists(&lib_dir, "icudata") {
        println!("cargo:rustc-link-lib=static=icui18n");
        println!("cargo:rustc-link-lib=static=icuuc");
        println!("cargo:rustc-link-lib=static=icudata");
    } else if lib_exists(&lib_dir, "sicudt") {
        println!("cargo:rustc-link-lib=static=sicuin");
        println!("cargo:rustc-link-lib=static=sicuuc");
        println!("cargo:rustc-link-lib=static=sicudt");
        // sicuio/sicutu are present but not always required; avoid overlinking
    } else {
        println!(
            "cargo:warning=ICU libraries not found in {}",
            lib_dir.display()
        );
    }

    // C++ runtime and system libraries (required by JSC)
    if os == "linux" {
        println!("cargo:rustc-link-lib=stdc++");
        println!("cargo:rustc-link-lib=atomic");
        println!("cargo:rustc-link-lib=dl");
        println!("cargo:rustc-link-lib=pthread");
        println!("cargo:rustc-link-lib=m");
    } else if os == "macos" {
        // macOS system frameworks and libraries required by bun-webkit
        println!("cargo:rustc-link-lib=c++");
        println!("cargo:rustc-link-lib=framework=CoreFoundation");
        println!("cargo:rustc-link-lib=framework=Foundation");
        println!("cargo:rustc-link-lib=framework=Security");
        println!("cargo:rustc-link-lib=framework=SystemConfiguration");

        // ICU libraries - use system ICU on macOS
        // macOS 10.14+ has ICU in /usr/lib
        println!("cargo:rustc-link-lib=icucore");
    }

    // Set include path for headers
    // 1. Try direct include
    let include_dir = webkit_path.join("include");
    if include_dir.exists() {
        println!("cargo:include={}", include_dir.display());
        return Some(include_dir);
    }

    // 2. Try bun-webkit subdirectory (common in tarballs)
    let subdir_include = webkit_path.join("bun-webkit").join("include");
    if subdir_include.exists() {
        println!("cargo:include={}", subdir_include.display());
        return Some(subdir_include);
    }

    println!(
        "cargo:warning=Include directory not found in {}",
        webkit_path.display()
    );
    None
}

fn compile_heap_stats_cpp(include_dir: &PathBuf, os: &str) {
    let mut build = Build::new();
    build
        .cpp(true)
        .file("src/heap_stats.cpp")
        .include(include_dir)
        .flag_if_supported("-std=c++20");

    if os == "macos" {
        build.flag_if_supported("-stdlib=libc++");
    }

    build.compile("otter_jsc_heap_stats");
}

fn compile_heap_stats_stub() {
    Build::new()
        .file("src/heap_stats_stub.c")
        .compile("otter_jsc_heap_stats");
}

fn find_lib_dir(webkit_path: &PathBuf) -> PathBuf {
    // bun-webkit may extract to a subdirectory
    let direct_lib = webkit_path.join("lib");
    if direct_lib.exists() {
        return direct_lib;
    }

    // Look for extracted subdirectory
    if let Ok(entries) = fs::read_dir(webkit_path) {
        for entry in entries.flatten() {
            let path = entry.path();
            if path.is_dir() {
                let lib_in_subdir = path.join("lib");
                if lib_in_subdir.exists() {
                    return lib_in_subdir;
                }
            }
        }
    }

    // Fallback to webkit_path itself
    webkit_path.clone()
}

fn lib_exists(lib_dir: &PathBuf, lib_name: &str) -> bool {
    if let Ok(entries) = fs::read_dir(lib_dir) {
        for entry in entries.flatten() {
            let path = entry.path();
            if let Some(name) = path.file_name().and_then(|n| n.to_str()) {
                // Check for both Windows (bmalloc.lib) and Unix (libbmalloc.a) naming
                let lib_prefix = format!("lib{}", lib_name);
                let is_match = (name.starts_with(lib_name) || name.starts_with(&lib_prefix))
                    && (name.ends_with(".lib") || name.ends_with(".a"));
                if is_match {
                    return true;
                }
            }
        }
    }
    false
}

fn get_cache_dir() -> PathBuf {
    // Try CARGO_HOME first, then fallback to home directory
    if let Ok(cargo_home) = env::var("CARGO_HOME") {
        return PathBuf::from(cargo_home).join("cache").join("bun-webkit");
    }

    if let Ok(home) = env::var("HOME") {
        return PathBuf::from(home)
            .join(".cargo")
            .join("cache")
            .join("bun-webkit");
    }

    if let Ok(userprofile) = env::var("USERPROFILE") {
        return PathBuf::from(userprofile)
            .join(".cargo")
            .join("cache")
            .join("bun-webkit");
    }

    // Fallback to OUT_DIR
    PathBuf::from(env::var("OUT_DIR").unwrap()).join("bun-webkit-cache")
}
