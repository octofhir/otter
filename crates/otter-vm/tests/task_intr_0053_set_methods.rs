//! Integration tests for ES2025 Set methods (Step 58).
//!
//! Spec references:
//! - §24.2.3.5  intersection:         <https://tc39.es/ecma262/#sec-set.prototype.intersection>
//! - §24.2.3.12 union:                <https://tc39.es/ecma262/#sec-set.prototype.union>
//! - §24.2.3.1  difference:           <https://tc39.es/ecma262/#sec-set.prototype.difference>
//! - §24.2.3.10 symmetricDifference:  <https://tc39.es/ecma262/#sec-set.prototype.symmetricdifference>
//! - §24.2.3.7  isSubsetOf:           <https://tc39.es/ecma262/#sec-set.prototype.issubsetof>
//! - §24.2.3.8  isSupersetOf:         <https://tc39.es/ecma262/#sec-set.prototype.issupersetof>
//! - §24.2.3.6  isDisjointFrom:       <https://tc39.es/ecma262/#sec-set.prototype.isdisjointfrom>

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

// ═══════════════════════════════════════════════════════════════════════════
//  intersection
// ═══════════════════════════════════════════════════════════════════════════

#[test]
fn intersection_basic() {
    assert_eq!(
        run_i32("new Set([1, 2, 3]).intersection(new Set([2, 3, 4])).size"),
        2
    );
}

#[test]
fn intersection_has_common() {
    assert!(run_bool(
        "new Set([1, 2, 3]).intersection(new Set([2, 3, 4])).has(2)"
    ));
}

#[test]
fn intersection_empty() {
    assert_eq!(
        run_i32("new Set([1, 2]).intersection(new Set([3, 4])).size"),
        0
    );
}

#[test]
fn intersection_returns_set() {
    assert!(run_bool(
        "new Set([1]).intersection(new Set([1])) instanceof Set"
    ));
}

// ═══════════════════════════════════════════════════════════════════════════
//  union
// ═══════════════════════════════════════════════════════════════════════════

#[test]
fn union_basic() {
    assert_eq!(run_i32("new Set([1, 2]).union(new Set([2, 3])).size"), 3);
}

#[test]
fn union_contains_all() {
    assert!(run_bool(
        "var u = new Set([1, 2]).union(new Set([3, 4])); u.has(1) && u.has(4)"
    ));
}

#[test]
fn union_empty_sets() {
    assert_eq!(run_i32("new Set().union(new Set()).size"), 0);
}

// ═══════════════════════════════════════════════════════════════════════════
//  difference
// ═══════════════════════════════════════════════════════════════════════════

#[test]
fn difference_basic() {
    assert_eq!(
        run_i32("new Set([1, 2, 3]).difference(new Set([2, 3])).size"),
        1
    );
}

#[test]
fn difference_keeps_exclusive() {
    assert!(run_bool(
        "new Set([1, 2, 3]).difference(new Set([2, 3])).has(1)"
    ));
}

#[test]
fn difference_no_common() {
    assert_eq!(
        run_i32("new Set([1, 2]).difference(new Set([3, 4])).size"),
        2
    );
}

// ═══════════════════════════════════════════════════════════════════════════
//  symmetricDifference
// ═══════════════════════════════════════════════════════════════════════════

#[test]
fn symmetric_difference_basic() {
    assert_eq!(
        run_i32("new Set([1, 2, 3]).symmetricDifference(new Set([2, 3, 4])).size"),
        2
    );
}

#[test]
fn symmetric_difference_has_exclusive() {
    assert!(run_bool(
        "var s = new Set([1, 2, 3]).symmetricDifference(new Set([2, 3, 4])); s.has(1) && s.has(4)"
    ));
}

#[test]
fn symmetric_difference_identical() {
    assert_eq!(
        run_i32("new Set([1, 2]).symmetricDifference(new Set([1, 2])).size"),
        0
    );
}

// ═══════════════════════════════════════════════════════════════════════════
//  isSubsetOf
// ═══════════════════════════════════════════════════════════════════════════

#[test]
fn is_subset_of_true() {
    assert!(run_bool("new Set([1, 2]).isSubsetOf(new Set([1, 2, 3]))"));
}

#[test]
fn is_subset_of_false() {
    assert!(!run_bool(
        "new Set([1, 2, 4]).isSubsetOf(new Set([1, 2, 3]))"
    ));
}

#[test]
fn is_subset_of_empty() {
    assert!(run_bool("new Set().isSubsetOf(new Set([1, 2]))"));
}

#[test]
fn is_subset_of_equal() {
    assert!(run_bool("new Set([1, 2]).isSubsetOf(new Set([1, 2]))"));
}

// ═══════════════════════════════════════════════════════════════════════════
//  isSupersetOf
// ═══════════════════════════════════════════════════════════════════════════

#[test]
fn is_superset_of_true() {
    assert!(run_bool("new Set([1, 2, 3]).isSupersetOf(new Set([1, 2]))"));
}

#[test]
fn is_superset_of_false() {
    assert!(!run_bool(
        "new Set([1, 2]).isSupersetOf(new Set([1, 2, 3]))"
    ));
}

// ═══════════════════════════════════════════════════════════════════════════
//  isDisjointFrom
// ═══════════════════════════════════════════════════════════════════════════

#[test]
fn is_disjoint_from_true() {
    assert!(run_bool("new Set([1, 2]).isDisjointFrom(new Set([3, 4]))"));
}

#[test]
fn is_disjoint_from_false() {
    assert!(!run_bool("new Set([1, 2]).isDisjointFrom(new Set([2, 3]))"));
}

#[test]
fn is_disjoint_from_empty() {
    assert!(run_bool("new Set([1, 2]).isDisjointFrom(new Set())"));
}

// ═══════════════════════════════════════════════════════════════════════════
//  General
// ═══════════════════════════════════════════════════════════════════════════

#[test]
fn all_set_methods_exist() {
    assert!(run_bool("typeof Set.prototype.intersection === 'function'"));
    assert!(run_bool("typeof Set.prototype.union === 'function'"));
    assert!(run_bool("typeof Set.prototype.difference === 'function'"));
    assert!(run_bool(
        "typeof Set.prototype.symmetricDifference === 'function'"
    ));
    assert!(run_bool("typeof Set.prototype.isSubsetOf === 'function'"));
    assert!(run_bool("typeof Set.prototype.isSupersetOf === 'function'"));
    assert!(run_bool(
        "typeof Set.prototype.isDisjointFrom === 'function'"
    ));
}

#[test]
fn set_method_lengths() {
    assert_eq!(run_i32("Set.prototype.intersection.length"), 1);
    assert_eq!(run_i32("Set.prototype.union.length"), 1);
    assert_eq!(run_i32("Set.prototype.difference.length"), 1);
    assert_eq!(run_i32("Set.prototype.symmetricDifference.length"), 1);
    assert_eq!(run_i32("Set.prototype.isSubsetOf.length"), 1);
    assert_eq!(run_i32("Set.prototype.isSupersetOf.length"), 1);
    assert_eq!(run_i32("Set.prototype.isDisjointFrom.length"), 1);
}
