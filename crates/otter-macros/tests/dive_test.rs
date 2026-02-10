//! Integration tests for the #[dive] macro (native-first)

#![allow(dead_code)]

use otter_macros::dive;
use otter_vm_core::context::NativeContext;
use otter_vm_core::error::VmError;
use otter_vm_core::value::Value;
use std::sync::Arc;

// ---------------------------------------------------------------------------
// Pattern C: Typed params via FromValue/IntoValue
// ---------------------------------------------------------------------------

#[dive(name = "abs", length = 1)]
fn math_abs(x: f64) -> f64 {
    x.abs()
}

#[dive(name = "add", length = 2)]
fn math_add(a: i32, b: i32) -> i32 {
    a + b
}

#[dive(name = "greet", length = 1)]
fn greet(name: String) -> String {
    format!("Hello, {}!", name)
}

#[dive(name = "isPositive", length = 1)]
fn is_positive(x: f64) -> bool {
    x > 0.0
}

#[dive(name = "noop", length = 0)]
fn noop() {}

// ---------------------------------------------------------------------------
// Pattern A: Full native signature
// ---------------------------------------------------------------------------

#[dive(name = "fullNative", length = 1)]
fn full_native(_this: &Value, args: &[Value], _ncx: &mut NativeContext) -> Result<Value, VmError> {
    let x = args.first().and_then(|v| v.as_number()).unwrap_or(0.0);
    Ok(Value::number(x * 2.0))
}

// ---------------------------------------------------------------------------
// Pattern B: Args + NativeContext
// ---------------------------------------------------------------------------

#[dive(name = "sumArgs", length = 0)]
fn sum_args(args: &[Value], _ncx: &mut NativeContext) -> Result<Value, VmError> {
    let sum: f64 = args.iter().filter_map(|v| v.as_number()).sum();
    Ok(Value::number(sum))
}

// ---------------------------------------------------------------------------
// Tests: generated constants
// ---------------------------------------------------------------------------

#[test]
fn test_name_constants() {
    assert_eq!(MATH_ABS_NAME, "abs");
    assert_eq!(MATH_ADD_NAME, "add");
    assert_eq!(GREET_NAME, "greet");
    assert_eq!(FULL_NATIVE_NAME, "fullNative");
    assert_eq!(SUM_ARGS_NAME, "sumArgs");
    assert_eq!(IS_POSITIVE_NAME, "isPositive");
    assert_eq!(NOOP_NAME, "noop");
}

#[test]
fn test_length_constants() {
    assert_eq!(MATH_ABS_LENGTH, 1);
    assert_eq!(MATH_ADD_LENGTH, 2);
    assert_eq!(GREET_LENGTH, 1);
    assert_eq!(FULL_NATIVE_LENGTH, 1);
    assert_eq!(SUM_ARGS_LENGTH, 0);
    assert_eq!(IS_POSITIVE_LENGTH, 1);
    assert_eq!(NOOP_LENGTH, 0);
}

// ---------------------------------------------------------------------------
// Tests: generated functions exist and return correct types
// ---------------------------------------------------------------------------

#[test]
fn test_native_fn_returns_arc() {
    // Just verify these compile and return the right type
    let _: Arc<
        dyn Fn(&Value, &[Value], &mut NativeContext<'_>) -> Result<Value, VmError> + Send + Sync,
    > = math_abs_native_fn();

    let _: Arc<
        dyn Fn(&Value, &[Value], &mut NativeContext<'_>) -> Result<Value, VmError> + Send + Sync,
    > = full_native_native_fn();

    let _: Arc<
        dyn Fn(&Value, &[Value], &mut NativeContext<'_>) -> Result<Value, VmError> + Send + Sync,
    > = sum_args_native_fn();
}

#[test]
fn test_decl_fn_returns_tuple() {
    let (name, _native_fn, length) = math_abs_decl();
    assert_eq!(name, "abs");
    assert_eq!(length, 1);

    let (name, _native_fn, length) = math_add_decl();
    assert_eq!(name, "add");
    assert_eq!(length, 2);

    let (name, _native_fn, length) = full_native_decl();
    assert_eq!(name, "fullNative");
    assert_eq!(length, 1);
}

// ---------------------------------------------------------------------------
// Tests: original functions still work
// ---------------------------------------------------------------------------

#[test]
fn test_original_functions() {
    assert_eq!(math_abs(-5.0), 5.0);
    assert_eq!(math_add(2, 3), 5);
    assert_eq!(greet("World".to_string()), "Hello, World!");
    assert!(is_positive(1.0));
    assert!(!is_positive(-1.0));
}
