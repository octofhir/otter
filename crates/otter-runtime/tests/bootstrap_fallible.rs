//! Typed isolate bootstrap and descriptor refusal at the public boundary.
//!
//! # Contents
//! - A real heap cap rejects construction with the original allocation facts.
//! - Installing a new global on a non-extensible global object fails explicitly.
//!
//! # Invariants
//! - Both failures enter through production runtime APIs and remain errors.

use otter_runtime::{OtterError, Runtime, SourceInput};

#[test]
fn bootstrap_heap_cap_preserves_real_request_and_limit() {
    let error = match Runtime::builder().max_heap_bytes(1).build() {
        Ok(_) => panic!("one-byte cap cannot admit an isolate bootstrap"),
        Err(error) => error,
    };
    assert!(
        matches!(error, OtterError::OutOfMemory { requested_bytes, heap_limit_bytes: 1 } if requested_bytes > 1),
        "{error:?}"
    );
    assert_eq!(error.exit_code(), 5);
    let wire: serde_json::Value = serde_json::from_str(&error.to_json().unwrap()).unwrap();
    assert_eq!(wire["error"]["kind"], "out_of_memory");
    assert_eq!(wire["error"]["heap_limit_bytes"], 1);
    assert!(wire["error"]["requested_bytes"].as_u64().unwrap() > 1);
}

#[test]
fn public_global_setter_propagates_descriptor_refusal() {
    let mut runtime = Runtime::builder().build().unwrap();
    runtime
        .run_script(
            SourceInput::from_javascript("Object.preventExtensions(globalThis);"),
            "bootstrap-refusal.js",
        )
        .unwrap();
    assert!(
        runtime
            .set_global("refusedFreshGlobal", otter_vm::Value::number_i32(42))
            .is_err()
    );
    assert_eq!(
        runtime
            .run_script(
                SourceInput::from_javascript("Object.hasOwn(globalThis, 'refusedFreshGlobal');"),
                "bootstrap-refusal.js"
            )
            .unwrap()
            .completion_string(),
        "false"
    );
}
