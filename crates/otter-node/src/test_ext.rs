//! Test extension module using the new architecture.
//!
//! This module provides the node:test extension with test runner functionality.
//!
//! ## Architecture
//!
//! - `test.rs` - Rust test runner implementation
//! - `test_ext.rs` - Extension creation with ops
//! - `test.js` - JavaScript test runner wrapper (describe, it, assert)
//!
//! Note: This module uses shared state (TestRunner) which doesn't fit the #[dive]
//! pattern, so we use traditional op_sync with closures.

use otter_runtime::Extension;
use otter_runtime::extension::{OpDecl, op_sync};
use serde_json::json;

use crate::test::{TestSummary, create_test_runner};

/// Create the test extension.
///
/// This extension provides test runner functionality including describe, it, test,
/// and assertions compatible with Node.js test runner.
pub fn extension() -> Extension {
    let runner = create_test_runner();

    let mut ops: Vec<OpDecl> = Vec::new();

    // Internal ops (prefixed with __) - used by JS wrapper
    // __startSuite(name) - start a test suite
    let runner_describe = runner.clone();
    ops.push(op_sync("__startSuite", move |_ctx, args| {
        let name = args.first().and_then(|v| v.as_str()).unwrap_or("anonymous");
        runner_describe.lock().unwrap().start_suite(name);
        Ok(json!(null))
    }));

    // __endSuite() - end current test suite
    let runner_end = runner.clone();
    ops.push(op_sync("__endSuite", move |_ctx, _args| {
        runner_end.lock().unwrap().end_suite();
        Ok(json!(null))
    }));

    // __recordResult(name, passed, duration, error?) - record a test result
    let runner_record = runner.clone();
    ops.push(op_sync("__recordResult", move |_ctx, args| {
        let name = args.first().and_then(|v| v.as_str()).unwrap_or("");
        let passed = args.get(1).and_then(|v| v.as_bool()).unwrap_or(false);
        let duration = args.get(2).and_then(|v| v.as_u64()).unwrap_or(0);
        let error = args.get(3).and_then(|v| v.as_str()).map(|s| s.to_string());

        runner_record
            .lock()
            .unwrap()
            .record_test(name, passed, duration, error);
        Ok(json!(null))
    }));

    // __skip(name) - skip a test
    let runner_skip = runner.clone();
    ops.push(op_sync("__skipTest", move |_ctx, args| {
        let name = args.first().and_then(|v| v.as_str()).unwrap_or("");
        runner_skip.lock().unwrap().skip_test(name);
        Ok(json!(null))
    }));

    // __getSummary() - get test results summary
    let runner_summary = runner.clone();
    ops.push(op_sync("__getSummary", move |_ctx, _args| {
        let runner = runner_summary.lock().unwrap();
        let summary = TestSummary::from(&*runner);
        Ok(serde_json::to_value(summary).unwrap_or(json!(null)))
    }));

    // __resetTests() - reset test runner for a new run
    let runner_reset = runner.clone();
    ops.push(op_sync("__resetTests", move |_ctx, _args| {
        runner_reset.lock().unwrap().reset();
        Ok(json!(null))
    }));

    // assertEqual(actual, expected) - assert two values are equal
    ops.push(op_sync("assertEqual", |_ctx, args| {
        let actual = args.first();
        let expected = args.get(1);

        if actual == expected {
            Ok(json!(true))
        } else {
            Err(otter_runtime::error::JscError::internal(format!(
                "Assertion failed: {:?} !== {:?}",
                actual, expected
            )))
        }
    }));

    // assertNotEqual(actual, expected) - assert two values are not equal
    ops.push(op_sync("assertNotEqual", |_ctx, args| {
        let actual = args.first();
        let expected = args.get(1);

        if actual != expected {
            Ok(json!(true))
        } else {
            Err(otter_runtime::error::JscError::internal(format!(
                "Assertion failed: {:?} === {:?} (expected not equal)",
                actual, expected
            )))
        }
    }));

    // assertTrue(value) - assert value is truthy
    ops.push(op_sync("assertTrue", |_ctx, args| {
        let value = args.first();
        let is_truthy = match value {
            Some(v) => {
                !v.is_null()
                    && v.as_bool() != Some(false)
                    && v.as_i64() != Some(0)
                    && v.as_str() != Some("")
            }
            None => false,
        };

        if is_truthy {
            Ok(json!(true))
        } else {
            Err(otter_runtime::error::JscError::internal(
                "Assertion failed: expected truthy value",
            ))
        }
    }));

    // assertFalse(value) - assert value is falsy
    ops.push(op_sync("assertFalse", |_ctx, args| {
        let value = args.first();
        let is_falsy = match value {
            Some(v) => {
                v.is_null()
                    || v.as_bool() == Some(false)
                    || v.as_i64() == Some(0)
                    || v.as_str() == Some("")
            }
            None => true,
        };

        if is_falsy {
            Ok(json!(true))
        } else {
            Err(otter_runtime::error::JscError::internal(
                "Assertion failed: expected falsy value",
            ))
        }
    }));

    // assertOk(value) - assert value exists and is truthy
    ops.push(op_sync("assertOk", |_ctx, args| {
        let value = args.first();

        match value {
            Some(v) if !v.is_null() => Ok(json!(true)),
            _ => Err(otter_runtime::error::JscError::internal(
                "Assertion failed: expected ok value",
            )),
        }
    }));

    // assertDeepEqual(actual, expected) - deep equality check via JSON
    ops.push(op_sync("assertDeepEqual", |_ctx, args| {
        let actual = args.first();
        let expected = args.get(1);

        // Compare JSON representations
        let actual_str = actual.map(|v| v.to_string()).unwrap_or_default();
        let expected_str = expected.map(|v| v.to_string()).unwrap_or_default();

        if actual_str == expected_str {
            Ok(json!(true))
        } else {
            Err(otter_runtime::error::JscError::internal(format!(
                "Deep equal assertion failed:\n  actual: {}\n  expected: {}",
                actual_str, expected_str
            )))
        }
    }));

    Extension::new("test")
        .with_ops(ops)
        .with_js(include_str!("test.js"))
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn test_extension_creation() {
        let ext = extension();
        assert_eq!(ext.name(), "test");
        assert!(ext.js_code().is_some());
    }
}
