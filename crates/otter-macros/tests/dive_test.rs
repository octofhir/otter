//! Integration tests for the #[dive] macro

#![allow(dead_code)]

use otter_macros::dive;

// Test basic sync dive
#[dive(swift)]
fn add(a: i32, b: i32) -> i32 {
    a + b
}

// Test custom name
#[dive(swift, name = "custom_multiply")]
fn multiply(a: i32, b: i32) -> i32 {
    a * b
}

// Test Result return type
#[dive(swift)]
fn divide(a: f64, b: f64) -> Result<f64, String> {
    if b == 0.0 {
        Err("Division by zero".to_string())
    } else {
        Ok(a / b)
    }
}

// Test no arguments
#[dive(swift)]
fn get_answer() -> i32 {
    42
}

// Test string arguments
#[dive(swift)]
fn greet(name: String) -> String {
    format!("Hello, {}!", name)
}

#[test]
fn test_dive_add() {
    let result = __otter_dive_add(&[serde_json::json!(5), serde_json::json!(3)]).unwrap();
    assert_eq!(result, serde_json::json!(8));
}

#[test]
fn test_dive_custom_name() {
    assert_eq!(add::NAME, "add");
    const { assert!(!add::IS_ASYNC) };

    assert_eq!(multiply::NAME, "custom_multiply");
    const { assert!(!multiply::IS_ASYNC) };
}

#[test]
fn test_dive_multiply() {
    let result = __otter_dive_multiply(&[serde_json::json!(4), serde_json::json!(7)]).unwrap();
    assert_eq!(result, serde_json::json!(28));
}

#[test]
fn test_dive_result_ok() {
    let result = __otter_dive_divide(&[serde_json::json!(10.0), serde_json::json!(2.0)]).unwrap();
    assert_eq!(result, serde_json::json!(5.0));
}

#[test]
fn test_dive_result_err() {
    let result = __otter_dive_divide(&[serde_json::json!(10.0), serde_json::json!(0.0)]);
    assert!(result.is_err());
    assert!(result.unwrap_err().contains("Division by zero"));
}

#[test]
fn test_dive_no_args() {
    let result = __otter_dive_get_answer(&[]).unwrap();
    assert_eq!(result, serde_json::json!(42));
}

#[test]
fn test_dive_string() {
    let result = __otter_dive_greet(&[serde_json::json!("World")]).unwrap();
    assert_eq!(result, serde_json::json!("Hello, World!"));
}

#[test]
fn test_original_function_still_works() {
    // The original function should still be callable
    assert_eq!(add(2, 3), 5);
    assert_eq!(multiply(3, 4), 12);
    assert_eq!(divide(10.0, 2.0).unwrap(), 5.0);
}
