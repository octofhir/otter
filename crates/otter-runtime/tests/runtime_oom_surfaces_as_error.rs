//! Script-driven heap-cap failures surface as catchable JavaScript
//! `RangeError` instances.
//!
//! `Runtime::max_heap_bytes` is a mutator boundary: recoverable allocation
//! refusal is converted once into the realm's `RangeError` and follows normal
//! throw/catch semantics. Unrecoverable VM allocation failures may still cross
//! the embedder boundary as structured host errors.
//!
//! # See also
//!
//! - GC architecture plan §1.2 NF3, §7.5.

use otter_runtime::{Runtime, SourceInput};

#[test]
fn runtime_string_cap_is_catchable_as_range_error() {
    // Tight cap so a tiny accumulation of string concatenations
    // overshoots — keeps the test fast and deterministic.
    let mut runtime = Runtime::builder()
        .max_heap_bytes(2 * 1024 * 1024)
        .build()
        .expect("runtime");
    // Append a 1024-char chunk in a loop until the heap cap rejects
    // a string allocation. With a 2 MiB cap and per-step ~2 KiB
    // doubled cons-rope overhead, this terminates in well under a
    // second on every supported platform.
    let source = SourceInput::from_javascript(
        r#"
            let caught = false;
            try {
                let s = "";
                let chunk = "";
                for (let i = 0; i < 1024; i++) chunk += "x";
                for (let i = 0; i < 100000; i++) {
                    s += chunk;
                }
            } catch (e) {
                caught = e instanceof RangeError;
            }
            caught;
        "#,
    );
    let result = runtime
        .run_script(source, "<script>")
        .expect("script should catch string heap cap as RangeError");
    assert_eq!(result.completion_string(), "true");
}

#[test]
fn runtime_array_cap_is_catchable_as_range_error() {
    let mut runtime = Runtime::builder()
        .max_heap_bytes(2 * 1024 * 1024)
        .build()
        .expect("runtime");
    let source = SourceInput::from_javascript(
        r#"
            let caught = false;
            try {
                let a = [];
                while (true) a.push(0);
            } catch (e) {
                caught = e instanceof RangeError;
            }
            caught;
        "#,
    );
    let result = runtime
        .run_script(source, "<script>")
        .expect("script should catch heap cap as RangeError");
    assert_eq!(result.completion_string(), "true");
}

#[test]
fn runtime_max_heap_bytes_zero_disables_cap() {
    let runtime = Runtime::builder()
        .max_heap_bytes(0)
        .build()
        .expect("runtime");
    assert_eq!(runtime.max_heap_bytes(), 0);
}

#[test]
fn runtime_gc_heap_observes_configured_cap() {
    let cap = 4 * 1024 * 1024;
    let runtime = Runtime::builder()
        .max_heap_bytes(cap)
        .build()
        .expect("runtime");
    assert_eq!(runtime.max_heap_bytes(), cap);
}
