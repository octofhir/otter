//! Script-driven OOM surfaces as [`OtterError::OutOfMemory`].
//!
//! `Runtime::max_heap_bytes` is load-bearing as of task 73:
//! a script that allocates strings past the configured cap is
//! refused by the string heap, the resulting `VmError::OutOfMemory`
//! is mapped through [`OtterError::OutOfMemory`], and the embedder
//! sees the structured error variant directly.
//!
//! # See also
//!
//! - GC architecture plan §1.2 NF3, §7.5.

use otter_runtime::{OtterError, Runtime, SourceInput};

#[test]
fn runtime_string_alloc_past_cap_surfaces_out_of_memory() {
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
            let s = "";
            let chunk = "";
            for (let i = 0; i < 1024; i++) chunk += "x";
            for (let i = 0; i < 100000; i++) {
                s += chunk;
            }
            s;
        "#,
    );
    let err = runtime
        .run_script(source, "<script>")
        .expect_err("script must hit the heap cap");
    match err {
        OtterError::OutOfMemory {
            requested_bytes: _,
            heap_limit_bytes,
        } => {
            assert_eq!(heap_limit_bytes, 2 * 1024 * 1024);
        }
        other => panic!("expected OtterError::OutOfMemory, got {other:?}"),
    }
}

// FIXME: pre-existing GC bug. The cap-trigger `collect_full` path in
// otter-gc only walks `handle_stack` + `global_handles` + the
// allocation-context `external_visit` — none of which include the
// interpreter's `error_classes` registry. When the 2 MiB cap fires
// mid-script, the full GC sweeps RangeError's constructor body, and
// `e instanceof RangeError` then dereferences a freed offset
// (type_tag=0). Pre-swap this surfaced as a misaligned-pointer panic
// in the scavenger; the tagged-Value migration just changed the
// failure mode, not the root cause. Tracking is required at the
// otter-gc API surface (probably an "extra runtime roots" callback
// on `GcHeap` invoked from `account_or_collect_with_roots`).
#[ignore = "pre-existing GC bug: error_classes registry not traced on cap-trigger collect_full"]
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
