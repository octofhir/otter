//! Integration tests for Step 55b — TypedArray change-by-copy methods (§23.2.3)
//!
//! - §23.2.3.30 %TypedArray%.prototype.toReversed: <https://tc39.es/ecma262/#sec-%typedarray%.prototype.toreversed>
//! - §23.2.3.31 %TypedArray%.prototype.toSorted: <https://tc39.es/ecma262/#sec-%typedarray%.prototype.tosorted>
//! - §23.2.3.37 %TypedArray%.prototype.with: <https://tc39.es/ecma262/#sec-%typedarray%.prototype.with>

use otter_vm::source::compile_eval;
use otter_vm::value::RegisterValue;
use otter_vm::{Interpreter, RuntimeState};

fn run(source: &str) -> RegisterValue {
    let module = compile_eval(source, "<test>").expect("should compile");
    let mut runtime = RuntimeState::new();
    let global = runtime.intrinsics().global_object();
    let registers = [RegisterValue::from_object_handle(global.0)];
    Interpreter::new()
        .execute_with_runtime(
            &module,
            otter_vm::module::FunctionIndex(0),
            &registers,
            &mut runtime,
        )
        .expect("should execute")
        .return_value()
}

fn run_bool(source: &str) -> bool {
    let v = run(source);
    v.as_bool()
        .unwrap_or_else(|| panic!("expected bool, got {v:?}"))
}

fn run_i32(source: &str) -> i32 {
    let v = run(source);
    v.as_i32()
        .unwrap_or_else(|| panic!("expected i32, got {v:?}"))
}

fn run_string(source: &str) -> String {
    let module = compile_eval(source, "<test>").expect("should compile");
    let mut runtime = RuntimeState::new();
    let global = runtime.intrinsics().global_object();
    let registers = [RegisterValue::from_object_handle(global.0)];
    let v = Interpreter::new()
        .execute_with_runtime(
            &module,
            otter_vm::module::FunctionIndex(0),
            &registers,
            &mut runtime,
        )
        .expect("should execute")
        .return_value();
    let handle = v.as_object_handle().expect("expected string handle");
    runtime
        .objects()
        .string_value(otter_vm::object::ObjectHandle(handle))
        .expect("string lookup")
        .expect("string value")
        .to_string()
}

// ═══════════════════════════════════════════════════════════════════════════
//  %TypedArray%.prototype.toReversed
// ═══════════════════════════════════════════════════════════════════════════

#[test]
fn to_reversed_int32() {
    assert_eq!(
        run_string(
            r#"
            var ta = new Int32Array([1, 2, 3, 4, 5]);
            var reversed = ta.toReversed();
            reversed.join(",");
            "#
        ),
        "5,4,3,2,1"
    );
}

#[test]
fn to_reversed_does_not_mutate_original() {
    assert_eq!(
        run_string(
            r#"
            var ta = new Int32Array([1, 2, 3]);
            ta.toReversed();
            ta.join(",");
            "#
        ),
        "1,2,3"
    );
}

#[test]
fn to_reversed_returns_new_instance() {
    assert!(run_bool(
        r#"
        var ta = new Int32Array([1, 2, 3]);
        var reversed = ta.toReversed();
        reversed !== ta;
        "#
    ));
}

#[test]
fn to_reversed_preserves_kind() {
    assert!(run_bool(
        r#"
        var ta = new Uint8Array([10, 20, 30]);
        var reversed = ta.toReversed();
        reversed instanceof Uint8Array;
        "#
    ));
}

#[test]
fn to_reversed_float64() {
    assert_eq!(
        run_string(
            r#"
            var ta = new Float64Array([1.5, 2.5, 3.5]);
            ta.toReversed().join(",");
            "#
        ),
        "3.5,2.5,1.5"
    );
}

#[test]
fn to_reversed_empty() {
    assert_eq!(
        run_i32(
            r#"
            var ta = new Int32Array([]);
            ta.toReversed().length;
            "#
        ),
        0
    );
}

#[test]
fn to_reversed_single_element() {
    assert_eq!(
        run_string(
            r#"
            var ta = new Int32Array([42]);
            ta.toReversed().join(",");
            "#
        ),
        "42"
    );
}

// ═══════════════════════════════════════════════════════════════════════════
//  %TypedArray%.prototype.toSorted
// ═══════════════════════════════════════════════════════════════════════════

#[test]
fn to_sorted_default_numeric() {
    assert_eq!(
        run_string(
            r#"
            var ta = new Int32Array([3, 1, 4, 1, 5, 9, 2, 6]);
            ta.toSorted().join(",");
            "#
        ),
        "1,1,2,3,4,5,6,9"
    );
}

#[test]
fn to_sorted_does_not_mutate_original() {
    assert_eq!(
        run_string(
            r#"
            var ta = new Int32Array([3, 1, 2]);
            ta.toSorted();
            ta.join(",");
            "#
        ),
        "3,1,2"
    );
}

#[test]
fn to_sorted_with_comparefn() {
    assert_eq!(
        run_string(
            r#"
            var ta = new Int32Array([3, 1, 4, 1, 5]);
            var sorted = ta.toSorted(function(a, b) { return b - a; });
            sorted.join(",");
            "#
        ),
        "5,4,3,1,1"
    );
}

#[test]
fn to_sorted_preserves_kind() {
    assert!(run_bool(
        r#"
        var ta = new Uint16Array([5, 3, 1]);
        var sorted = ta.toSorted();
        sorted instanceof Uint16Array;
        "#
    ));
}

#[test]
fn to_sorted_returns_new_instance() {
    assert!(run_bool(
        r#"
        var ta = new Int32Array([1, 2, 3]);
        ta.toSorted() !== ta;
        "#
    ));
}

#[test]
fn to_sorted_empty() {
    assert_eq!(
        run_i32(
            r#"
            var ta = new Int32Array([]);
            ta.toSorted().length;
            "#
        ),
        0
    );
}

#[test]
fn to_sorted_negative_values() {
    assert_eq!(
        run_string(
            r#"
            var ta = new Int32Array([5, -3, 0, -1, 2]);
            ta.toSorted().join(",");
            "#
        ),
        "-3,-1,0,2,5"
    );
}

#[test]
fn to_sorted_float32() {
    assert!(run_bool(
        r#"
        var ta = new Float32Array([3.14, 1.41, 2.72]);
        var s = ta.toSorted();
        s[0] < s[1] && s[1] < s[2];
        "#
    ));
}

// ═══════════════════════════════════════════════════════════════════════════
//  %TypedArray%.prototype.with
// ═══════════════════════════════════════════════════════════════════════════

#[test]
fn with_replaces_element() {
    assert_eq!(
        run_string(
            r#"
            var ta = new Int32Array([1, 2, 3, 4, 5]);
            ta.with(2, 99).join(",");
            "#
        ),
        "1,2,99,4,5"
    );
}

#[test]
fn with_does_not_mutate_original() {
    assert_eq!(
        run_string(
            r#"
            var ta = new Int32Array([1, 2, 3]);
            ta.with(0, 99);
            ta.join(",");
            "#
        ),
        "1,2,3"
    );
}

#[test]
fn with_negative_index() {
    assert_eq!(
        run_string(
            r#"
            var ta = new Int32Array([1, 2, 3, 4, 5]);
            ta.with(-1, 99).join(",");
            "#
        ),
        "1,2,3,4,99"
    );
}

#[test]
fn with_negative_index_from_start() {
    assert_eq!(
        run_string(
            r#"
            var ta = new Int32Array([1, 2, 3]);
            ta.with(-3, 99).join(",");
            "#
        ),
        "99,2,3"
    );
}

#[test]
fn with_out_of_range_throws() {
    assert!(run_bool(
        r#"
        var ta = new Int32Array([1, 2, 3]);
        try { ta.with(5, 99); false; } catch(e) { e instanceof RangeError; }
        "#
    ));
}

#[test]
fn with_negative_out_of_range_throws() {
    assert!(run_bool(
        r#"
        var ta = new Int32Array([1, 2, 3]);
        try { ta.with(-4, 99); false; } catch(e) { e instanceof RangeError; }
        "#
    ));
}

#[test]
fn with_preserves_kind() {
    assert!(run_bool(
        r#"
        var ta = new Uint8Array([10, 20, 30]);
        ta.with(1, 99) instanceof Uint8Array;
        "#
    ));
}

#[test]
fn with_returns_new_instance() {
    assert!(run_bool(
        r#"
        var ta = new Int32Array([1, 2, 3]);
        ta.with(0, 1) !== ta;
        "#
    ));
}

#[test]
fn with_uint8_clamps_not_applicable() {
    // with() uses standard numeric conversion, not clamped
    // Uint8ClampedArray.with uses ToNumber then write_typed_element
    assert_eq!(
        run_i32(
            r#"
            var ta = new Uint8Array([1, 2, 3]);
            ta.with(0, 300)[0];
            "#
        ),
        44 // 300 & 0xFF = 44
    );
}

// ═══════════════════════════════════════════════════════════════════════════
//  Cross-kind tests
// ═══════════════════════════════════════════════════════════════════════════

#[test]
fn to_reversed_int8() {
    assert_eq!(
        run_string(
            r#"
            var ta = new Int8Array([10, -20, 30, -40]);
            ta.toReversed().join(",");
            "#
        ),
        "-40,30,-20,10"
    );
}

#[test]
fn to_sorted_uint32() {
    assert_eq!(
        run_string(
            r#"
            var ta = new Uint32Array([100, 10, 1000, 1]);
            ta.toSorted().join(",");
            "#
        ),
        "1,10,100,1000"
    );
}

#[test]
fn with_int16() {
    assert_eq!(
        run_string(
            r#"
            var ta = new Int16Array([100, 200, 300]);
            ta.with(1, -500).join(",");
            "#
        ),
        "100,-500,300"
    );
}
