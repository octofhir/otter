//! Integration tests for FFI library loading and function calls.

use std::collections::HashMap;

use otter_ffi::library::FfiLibrary;
use otter_ffi::types::{FFIType, FfiSignature};

fn test_lib_path() -> String {
    std::env::var("TEST_FFI_LIB_PATH").unwrap_or_else(|_| {
        // Fallback: build the test library on the fly
        let out_dir = std::env::var("OUT_DIR")
            .unwrap_or_else(|_| std::env::temp_dir().to_string_lossy().to_string());
        let suffix = if cfg!(target_os = "macos") {
            "dylib"
        } else if cfg!(target_os = "windows") {
            "dll"
        } else {
            "so"
        };
        let lib_path = format!("{}/libtest_ffi.{}", out_dir, suffix);

        // Build if not exists
        if !std::path::Path::new(&lib_path).exists() {
            let manifest_dir = env!("CARGO_MANIFEST_DIR");
            let status = std::process::Command::new("cc")
                .args([
                    "-shared",
                    "-o",
                    &lib_path,
                    &format!("{}/test_lib/test_ffi.c", manifest_dir),
                ])
                .status()
                .expect("Failed to run C compiler");
            assert!(status.success(), "Failed to build test shared library");
        }
        lib_path
    })
}

#[test]
fn test_load_and_call_add() {
    let mut sigs = HashMap::new();
    sigs.insert(
        "add".to_string(),
        FfiSignature {
            args: vec![FFIType::I32, FFIType::I32],
            returns: FFIType::I32,
        },
    );

    let lib = FfiLibrary::open(&test_lib_path(), &sigs).expect("Failed to open test lib");
    let result = lib.call_raw("add", &[3, 4]).expect("Failed to call add");
    assert_eq!(result as i32, 7);
}

#[test]
fn test_call_negate() {
    let mut sigs = HashMap::new();
    sigs.insert(
        "negate".to_string(),
        FfiSignature {
            args: vec![FFIType::I32],
            returns: FFIType::I32,
        },
    );

    let lib = FfiLibrary::open(&test_lib_path(), &sigs).expect("Failed to open test lib");
    let result = lib.call_raw("negate", &[42]).expect("Failed to call negate");
    assert_eq!(result as i32, -42);
}

#[test]
fn test_call_multiply() {
    let mut sigs = HashMap::new();
    sigs.insert(
        "multiply".to_string(),
        FfiSignature {
            args: vec![FFIType::F64, FFIType::F64],
            returns: FFIType::F64,
        },
    );

    let lib = FfiLibrary::open(&test_lib_path(), &sigs).expect("Failed to open test lib");

    let a = 3.5_f64.to_bits();
    let b = 2.0_f64.to_bits();
    let result = lib.call_raw("multiply", &[a, b]).expect("Failed to call multiply");
    let result_f64 = f64::from_bits(result);
    assert!((result_f64 - 7.0).abs() < f64::EPSILON);
}

#[test]
fn test_call_hello_cstring() {
    let mut sigs = HashMap::new();
    sigs.insert(
        "hello".to_string(),
        FfiSignature {
            args: vec![],
            returns: FFIType::CString,
        },
    );

    let lib = FfiLibrary::open(&test_lib_path(), &sigs).expect("Failed to open test lib");
    let result = lib.call_raw("hello", &[]).expect("Failed to call hello");

    // Result is a pointer to a C string
    assert_ne!(result, 0, "hello() should return a non-null pointer");

    // Read the C string
    let s = unsafe { otter_ffi::pointer::read_cstring(result as usize, 0) }
        .expect("Failed to read cstring");
    assert_eq!(s, "Hello from C!");
}

#[test]
fn test_call_void() {
    let mut sigs = HashMap::new();
    sigs.insert(
        "do_nothing".to_string(),
        FfiSignature {
            args: vec![],
            returns: FFIType::Void,
        },
    );

    let lib = FfiLibrary::open(&test_lib_path(), &sigs).expect("Failed to open test lib");
    let result = lib.call_raw("do_nothing", &[]).expect("Failed to call do_nothing");
    assert_eq!(result, 0);
}

#[test]
fn test_call_square() {
    let mut sigs = HashMap::new();
    sigs.insert(
        "square".to_string(),
        FfiSignature {
            args: vec![FFIType::U32],
            returns: FFIType::U32,
        },
    );

    let lib = FfiLibrary::open(&test_lib_path(), &sigs).expect("Failed to open test lib");
    let result = lib.call_raw("square", &[5]).expect("Failed to call square");
    assert_eq!(result as u32, 25);
}

#[test]
fn test_call_add_float() {
    let mut sigs = HashMap::new();
    sigs.insert(
        "add_float".to_string(),
        FfiSignature {
            args: vec![FFIType::F32, FFIType::F32],
            returns: FFIType::F32,
        },
    );

    let lib = FfiLibrary::open(&test_lib_path(), &sigs).expect("Failed to open test lib");

    let a = 1.5_f32.to_bits() as u64;
    let b = 2.5_f32.to_bits() as u64;
    let result = lib.call_raw("add_float", &[a, b]).expect("Failed to call add_float");
    let result_f32 = f32::from_bits(result as u32);
    assert!((result_f32 - 4.0).abs() < f32::EPSILON);
}

#[test]
fn test_symbol_not_found() {
    let mut sigs = HashMap::new();
    sigs.insert(
        "nonexistent_function_xyz".to_string(),
        FfiSignature {
            args: vec![],
            returns: FFIType::Void,
        },
    );

    let result = FfiLibrary::open(&test_lib_path(), &sigs);
    assert!(result.is_err());
}

#[test]
fn test_arg_count_mismatch() {
    let mut sigs = HashMap::new();
    sigs.insert(
        "add".to_string(),
        FfiSignature {
            args: vec![FFIType::I32, FFIType::I32],
            returns: FFIType::I32,
        },
    );

    let lib = FfiLibrary::open(&test_lib_path(), &sigs).expect("Failed to open test lib");

    // Too few args
    let result = lib.call_raw("add", &[1]);
    assert!(result.is_err());

    // Too many args
    let result = lib.call_raw("add", &[1, 2, 3]);
    assert!(result.is_err());
}
