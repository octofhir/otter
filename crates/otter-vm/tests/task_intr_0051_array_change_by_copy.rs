//! Integration tests for Array change-by-copy methods (Step 55).
//!
//! Spec references:
//! - §23.1.3.30 toReversed(): <https://tc39.es/ecma262/#sec-array.prototype.toreversed>
//! - §23.1.3.31 toSorted():   <https://tc39.es/ecma262/#sec-array.prototype.tosorted>
//! - §23.1.3.32 toSpliced():  <https://tc39.es/ecma262/#sec-array.prototype.tospliced>
//! - §23.1.3.37 with():       <https://tc39.es/ecma262/#sec-array.prototype.with>

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

// ═══════════════════════════════════════════════════════════════════════════
//  §23.1.3.30 — Array.prototype.toReversed()
// ═══════════════════════════════════════════════════════════════════════════

/// Basic reverse returns new array.
#[test]
fn to_reversed_basic() {
    assert_eq!(run_string("[1, 2, 3].toReversed().join(',')"), "3,2,1");
}

/// Original array is NOT mutated.
#[test]
fn to_reversed_does_not_mutate() {
    assert_eq!(
        run_string("var a = [1, 2, 3]; a.toReversed(); a.join(',')"),
        "1,2,3"
    );
}

/// Empty array.
#[test]
fn to_reversed_empty() {
    assert_eq!(run_i32("[].toReversed().length"), 0);
}

/// Single element.
#[test]
fn to_reversed_single() {
    assert_eq!(run_string("[42].toReversed().join(',')"), "42");
}

/// toReversed.length === 0.
#[test]
fn to_reversed_length_prop() {
    assert_eq!(run_i32("Array.prototype.toReversed.length"), 0);
}

// ═══════════════════════════════════════════════════════════════════════════
//  §23.1.3.31 — Array.prototype.toSorted(compareFn?)
// ═══════════════════════════════════════════════════════════════════════════

/// Default sort (string comparison).
#[test]
fn to_sorted_default() {
    assert_eq!(run_string("[3, 1, 2].toSorted().join(',')"), "1,2,3");
}

/// Sort with custom compareFn.
#[test]
fn to_sorted_comparefn() {
    assert_eq!(
        run_string("[3, 1, 2].toSorted((a, b) => b - a).join(',')"),
        "3,2,1"
    );
}

/// Original array is NOT mutated.
#[test]
fn to_sorted_does_not_mutate() {
    assert_eq!(
        run_string("var a = [3, 1, 2]; a.toSorted(); a.join(',')"),
        "3,1,2"
    );
}

/// Empty array.
#[test]
fn to_sorted_empty() {
    assert_eq!(run_i32("[].toSorted().length"), 0);
}

/// toSorted.length === 1.
#[test]
fn to_sorted_length_prop() {
    assert_eq!(run_i32("Array.prototype.toSorted.length"), 1);
}

/// Strings sort lexicographically by default.
#[test]
fn to_sorted_strings() {
    assert_eq!(
        run_string("['banana', 'apple', 'cherry'].toSorted().join(',')"),
        "apple,banana,cherry"
    );
}

// ═══════════════════════════════════════════════════════════════════════════
//  §23.1.3.32 — Array.prototype.toSpliced(start, deleteCount, ...items)
// ═══════════════════════════════════════════════════════════════════════════

/// Remove elements.
#[test]
fn to_spliced_remove() {
    assert_eq!(run_string("[1, 2, 3, 4].toSpliced(1, 2).join(',')"), "1,4");
}

/// Insert elements.
#[test]
fn to_spliced_insert() {
    assert_eq!(
        run_string("[1, 4].toSpliced(1, 0, 2, 3).join(',')"),
        "1,2,3,4"
    );
}

/// Replace elements.
#[test]
fn to_spliced_replace() {
    assert_eq!(
        run_string("var r = [1, 2, 3].toSpliced(1, 1, 99); r.join(',')"),
        "1,99,3"
    );
}

/// Original array is NOT mutated.
#[test]
fn to_spliced_does_not_mutate() {
    assert_eq!(
        run_string("var a = [1, 2, 3]; a.toSpliced(0, 1); a.join(',')"),
        "1,2,3"
    );
}

/// Negative start index.
#[test]
fn to_spliced_negative_start() {
    assert_eq!(
        run_string("[1, 2, 3, 4].toSpliced(-2, 1).join(',')"),
        "1,2,4"
    );
}

/// No deleteCount — remove from start to end.
#[test]
fn to_spliced_no_delete_count() {
    assert_eq!(run_string("[1, 2, 3, 4].toSpliced(2).join(',')"), "1,2");
}

/// Empty array splice.
#[test]
fn to_spliced_empty() {
    assert_eq!(run_string("[].toSpliced(0, 0, 1, 2).join(',')"), "1,2");
}

/// toSpliced.length === 2.
#[test]
fn to_spliced_length_prop() {
    assert_eq!(run_i32("Array.prototype.toSpliced.length"), 2);
}

// ═══════════════════════════════════════════════════════════════════════════
//  §23.1.3.37 — Array.prototype.with(index, value)
// ═══════════════════════════════════════════════════════════════════════════

/// Basic replacement.
#[test]
fn with_basic() {
    assert_eq!(run_string("[1, 2, 3].with(1, 99).join(',')"), "1,99,3");
}

/// Negative index.
#[test]
fn with_negative_index() {
    assert_eq!(run_string("[1, 2, 3].with(-1, 99).join(',')"), "1,2,99");
}

/// Original array is NOT mutated.
#[test]
fn with_does_not_mutate() {
    assert_eq!(
        run_string("var a = [1, 2, 3]; a.with(0, 99); a.join(',')"),
        "1,2,3"
    );
}

/// Out-of-bounds throws RangeError.
#[test]
fn with_out_of_bounds_throws() {
    assert!(run_bool(
        "var ok = false; try { [1, 2].with(5, 99); } catch(e) { ok = true; } ok"
    ));
}

/// Negative out-of-bounds throws RangeError.
#[test]
fn with_negative_out_of_bounds_throws() {
    assert!(run_bool(
        "var ok = false; try { [1, 2].with(-3, 99); } catch(e) { ok = true; } ok"
    ));
}

/// with.length === 2.
#[test]
fn with_length_prop() {
    assert_eq!(run_i32("Array.prototype.with.length"), 2);
}

/// Returns a new array (not the same reference).
#[test]
fn with_returns_new_array() {
    assert!(run_bool("var a = [1, 2]; var b = a.with(0, 9); a !== b"));
}

/// All four methods are functions.
#[test]
fn change_by_copy_methods_exist() {
    assert!(run_bool("typeof [].toReversed === 'function'"));
    assert!(run_bool("typeof [].toSorted === 'function'"));
    assert!(run_bool("typeof [].toSpliced === 'function'"));
    assert!(run_bool("typeof [].with === 'function'"));
}
