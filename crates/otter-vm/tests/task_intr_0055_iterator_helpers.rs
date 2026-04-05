//! Integration tests for ES2025 Iterator Helpers (Step 64).
//!
//! Spec references:
//! - §27.1.4.1 Iterator.prototype.map:     <https://tc39.es/ecma262/#sec-iteratorprototype.map>
//! - §27.1.4.2 Iterator.prototype.filter:  <https://tc39.es/ecma262/#sec-iteratorprototype.filter>
//! - §27.1.4.3 Iterator.prototype.take:    <https://tc39.es/ecma262/#sec-iteratorprototype.take>
//! - §27.1.4.4 Iterator.prototype.drop:    <https://tc39.es/ecma262/#sec-iteratorprototype.drop>
//! - §27.1.4.5 Iterator.prototype.flatMap: <https://tc39.es/ecma262/#sec-iteratorprototype.flatmap>
//! - §27.1.4.6 Iterator.prototype.reduce:  <https://tc39.es/ecma262/#sec-iteratorprototype.reduce>
//! - §27.1.4.7 Iterator.prototype.toArray: <https://tc39.es/ecma262/#sec-iteratorprototype.toarray>
//! - §27.1.4.8 Iterator.prototype.forEach: <https://tc39.es/ecma262/#sec-iteratorprototype.foreach>
//! - §27.1.4.9 Iterator.prototype.some:    <https://tc39.es/ecma262/#sec-iteratorprototype.some>
//! - §27.1.4.10 Iterator.prototype.every:  <https://tc39.es/ecma262/#sec-iteratorprototype.every>
//! - §27.1.4.11 Iterator.prototype.find:   <https://tc39.es/ecma262/#sec-iteratorprototype.find>
//! - §27.1.2.1 Iterator.from:              <https://tc39.es/ecma262/#sec-iterator.from>

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
//  Iterator global
// ═══════════════════════════════════════════════════════════════════════════

#[test]
fn iterator_exists() {
    assert!(run_bool("typeof Iterator === 'function'"));
}

#[test]
fn iterator_from_exists() {
    assert!(run_bool("typeof Iterator.from === 'function'"));
}

// ═══════════════════════════════════════════════════════════════════════════
//  toArray
// ═══════════════════════════════════════════════════════════════════════════

#[test]
fn to_array_basic() {
    assert_eq!(
        run_i32("var arr = [1, 2, 3]; arr.values().toArray().length"),
        3
    );
}

#[test]
fn to_array_preserves_values() {
    assert_eq!(
        run_string("var r = [10, 20, 30].values().toArray(); r.join(',')"),
        "10,20,30"
    );
}

#[test]
fn to_array_empty() {
    assert_eq!(run_i32("[].values().toArray().length"), 0);
}

// ═══════════════════════════════════════════════════════════════════════════
//  map
// ═══════════════════════════════════════════════════════════════════════════

#[test]
fn map_basic() {
    assert_eq!(
        run_string("var r = [1, 2, 3].values().map(x => x * 2).toArray(); r.join(',')"),
        "2,4,6"
    );
}

#[test]
fn map_returns_iterator() {
    assert!(run_bool(
        "var it = [1].values().map(x => x); typeof it.next === 'function'"
    ));
}

#[test]
fn map_lazy_evaluation() {
    // map should not consume the entire source eagerly
    assert_eq!(
        run_i32(
            "var count = 0; \
             var it = [1, 2, 3, 4, 5].values().map(x => { count++; return x * 2; }); \
             it.next(); it.next(); \
             count"
        ),
        2
    );
}

// ═══════════════════════════════════════════════════════════════════════════
//  filter
// ═══════════════════════════════════════════════════════════════════════════

#[test]
fn filter_basic() {
    assert_eq!(
        run_string(
            "var r = [1, 2, 3, 4, 5].values().filter(x => x % 2 === 0).toArray(); r.join(',')"
        ),
        "2,4"
    );
}

#[test]
fn filter_empty_result() {
    assert_eq!(
        run_i32("[1, 3, 5].values().filter(x => x % 2 === 0).toArray().length"),
        0
    );
}

// ═══════════════════════════════════════════════════════════════════════════
//  take
// ═══════════════════════════════════════════════════════════════════════════

#[test]
fn take_basic() {
    assert_eq!(
        run_string("var r = [1, 2, 3, 4, 5].values().take(3).toArray(); r.join(',')"),
        "1,2,3"
    );
}

#[test]
fn take_more_than_available() {
    assert_eq!(run_i32("[1, 2].values().take(10).toArray().length"), 2);
}

#[test]
fn take_zero() {
    assert_eq!(run_i32("[1, 2, 3].values().take(0).toArray().length"), 0);
}

// ═══════════════════════════════════════════════════════════════════════════
//  drop
// ═══════════════════════════════════════════════════════════════════════════

#[test]
fn drop_basic() {
    assert_eq!(
        run_string("var r = [1, 2, 3, 4, 5].values().drop(2).toArray(); r.join(',')"),
        "3,4,5"
    );
}

#[test]
fn drop_more_than_available() {
    assert_eq!(run_i32("[1, 2].values().drop(10).toArray().length"), 0);
}

#[test]
fn drop_zero() {
    assert_eq!(run_i32("[1, 2, 3].values().drop(0).toArray().length"), 3);
}

// ═══════════════════════════════════════════════════════════════════════════
//  flatMap
// ═══════════════════════════════════════════════════════════════════════════

#[test]
fn flat_map_basic() {
    assert_eq!(
        run_string("var r = [1, 2, 3].values().flatMap(x => [x, x * 10]).toArray(); r.join(',')"),
        "1,10,2,20,3,30"
    );
}

#[test]
fn flat_map_empty_inner() {
    assert_eq!(
        run_i32("[1, 2, 3].values().flatMap(x => []).toArray().length"),
        0
    );
}

// ═══════════════════════════════════════════════════════════════════════════
//  forEach
// ═══════════════════════════════════════════════════════════════════════════

#[test]
fn for_each_runs_callback() {
    assert_eq!(
        run_i32("var sum = 0; [1, 2, 3].values().forEach(x => { sum += x; }); sum"),
        6
    );
}

#[test]
fn for_each_returns_undefined() {
    assert!(run_bool("[1].values().forEach(x => {}) === undefined"));
}

// ═══════════════════════════════════════════════════════════════════════════
//  some
// ═══════════════════════════════════════════════════════════════════════════

#[test]
fn some_true() {
    assert!(run_bool("[1, 2, 3].values().some(x => x > 2)"));
}

#[test]
fn some_false() {
    assert!(!run_bool("[1, 2, 3].values().some(x => x > 10)"));
}

#[test]
fn some_empty() {
    assert!(!run_bool("[].values().some(x => true)"));
}

// ═══════════════════════════════════════════════════════════════════════════
//  every
// ═══════════════════════════════════════════════════════════════════════════

#[test]
fn every_true() {
    assert!(run_bool("[1, 2, 3].values().every(x => x > 0)"));
}

#[test]
fn every_false() {
    assert!(!run_bool("[1, 2, 3].values().every(x => x > 2)"));
}

#[test]
fn every_empty() {
    assert!(run_bool("[].values().every(x => false)"));
}

// ═══════════════════════════════════════════════════════════════════════════
//  find
// ═══════════════════════════════════════════════════════════════════════════

#[test]
fn find_found() {
    assert_eq!(run_i32("[10, 20, 30].values().find(x => x > 15)"), 20);
}

#[test]
fn find_not_found() {
    assert!(run_bool(
        "[1, 2, 3].values().find(x => x > 10) === undefined"
    ));
}

// ═══════════════════════════════════════════════════════════════════════════
//  reduce
// ═══════════════════════════════════════════════════════════════════════════

#[test]
fn reduce_with_initial() {
    assert_eq!(
        run_i32("[1, 2, 3].values().reduce((acc, x) => acc + x, 0)"),
        6
    );
}

#[test]
fn reduce_without_initial() {
    assert_eq!(run_i32("[1, 2, 3].values().reduce((acc, x) => acc + x)"), 6);
}

#[test]
fn reduce_with_initial_string() {
    assert_eq!(
        run_string("[1, 2, 3].values().reduce((acc, x) => acc + x, 'sum:')"),
        "sum:123"
    );
}

// ═══════════════════════════════════════════════════════════════════════════
//  Iterator.from
// ═══════════════════════════════════════════════════════════════════════════

#[test]
fn iterator_from_array() {
    assert_eq!(
        run_i32("var it = Iterator.from([1, 2, 3]); it.next().value"),
        1
    );
}

#[test]
fn iterator_from_iterator_object() {
    assert!(run_bool(
        "var it = [1, 2].values(); Iterator.from(it) === it || typeof Iterator.from(it).next === 'function'"
    ));
}

// ═══════════════════════════════════════════════════════════════════════════
//  Chaining
// ═══════════════════════════════════════════════════════════════════════════

#[test]
fn chain_map_filter() {
    assert_eq!(
        run_string(
            "var r = [1, 2, 3, 4, 5].values() \
                .map(x => x * 2) \
                .filter(x => x > 4) \
                .toArray(); \
             r.join(',')"
        ),
        "6,8,10"
    );
}

#[test]
fn chain_drop_take() {
    assert_eq!(
        run_string("var r = [1, 2, 3, 4, 5].values().drop(1).take(3).toArray(); r.join(',')"),
        "2,3,4"
    );
}

#[test]
fn chain_map_reduce() {
    assert_eq!(
        run_i32("[1, 2, 3].values().map(x => x * x).reduce((a, b) => a + b, 0)"),
        14
    );
}

// ═══════════════════════════════════════════════════════════════════════════
//  Method existence
// ═══════════════════════════════════════════════════════════════════════════

#[test]
fn all_methods_exist_on_array_iterator() {
    assert!(run_bool("typeof [].values().map === 'function'"));
    assert!(run_bool("typeof [].values().filter === 'function'"));
    assert!(run_bool("typeof [].values().take === 'function'"));
    assert!(run_bool("typeof [].values().drop === 'function'"));
    assert!(run_bool("typeof [].values().flatMap === 'function'"));
    assert!(run_bool("typeof [].values().toArray === 'function'"));
    assert!(run_bool("typeof [].values().forEach === 'function'"));
    assert!(run_bool("typeof [].values().some === 'function'"));
    assert!(run_bool("typeof [].values().every === 'function'"));
    assert!(run_bool("typeof [].values().find === 'function'"));
    assert!(run_bool("typeof [].values().reduce === 'function'"));
}
