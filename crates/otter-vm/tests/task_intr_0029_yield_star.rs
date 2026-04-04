//! Integration tests for `yield*` delegating yield.
//!
//! ES2024 §14.4.4 Runtime Semantics: Evaluation — YieldExpression : `yield * AssignmentExpression`
//! Spec: <https://tc39.es/ecma262/#sec-generator-function-definitions-runtime-semantics-evaluation>

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

fn run_i32(source: &str) -> i32 {
    let v = run(source);
    v.as_i32()
        .unwrap_or_else(|| panic!("expected i32, got {v:?}"))
}

// ═══════════════════════════════════════════════════════════════════════════
//  yield* with array (built-in Array iterator)
// ═══════════════════════════════════════════════════════════════════════════

/// §14.4.4 — yield* delegates to array's [Symbol.iterator]().
#[test]
fn yield_star_array_delegation() {
    assert_eq!(
        run_i32(
            "function* gen() { yield* [10, 20, 30]; }\n\
             var g = gen();\n\
             var a = g.next().value;\n\
             var b = g.next().value;\n\
             var c = g.next().value;\n\
             a + b + c"
        ),
        60
    );
}

/// yield* array — the outer generator is done after inner finishes.
#[test]
fn yield_star_array_done_after_delegation() {
    assert_eq!(
        run_i32(
            "function* gen() { yield* [1, 2]; }\n\
             var g = gen();\n\
             g.next(); g.next();\n\
             g.next().done ? 1 : 0"
        ),
        1
    );
}

// ═══════════════════════════════════════════════════════════════════════════
//  yield* with another generator
// ═══════════════════════════════════════════════════════════════════════════

/// §14.4.4 — yield* delegates to another generator function.
#[test]
fn yield_star_generator_to_generator() {
    assert_eq!(
        run_i32(
            "function* inner() { yield 1; yield 2; yield 3; }\n\
             function* outer() { yield* inner(); }\n\
             var g = outer();\n\
             var v1 = g.next().value;\n\
             var v2 = g.next().value;\n\
             var v3 = g.next().value;\n\
             v1 + v2 + v3"
        ),
        6
    );
}

/// yield* return value — the final value of inner becomes the value of the yield* expression.
/// Spec: §14.4.4 step 7.a.ii.1 — If innerResult.[[Done]] is true, return innerResult.[[Value]].
#[test]
fn yield_star_return_value() {
    assert_eq!(
        run_i32(
            "function* inner() { yield 10; return 42; }\n\
             function* outer() {\n\
               var result = yield* inner();\n\
               yield result;\n\
             }\n\
             var g = outer();\n\
             g.next();      // yields 10 from inner\n\
             g.next().value // yields 42 (the return value of inner)"
        ),
        42
    );
}

// ═══════════════════════════════════════════════════════════════════════════
//  yield* with string (built-in String iterator)
// ═══════════════════════════════════════════════════════════════════════════

/// §14.4.4 — yield* delegates to string's [Symbol.iterator]().
#[test]
fn yield_star_string_delegation() {
    assert_eq!(
        run_i32(
            "function* gen() { yield* 'abc'; }\n\
             var g = gen();\n\
             var a = g.next().value;\n\
             var b = g.next().value;\n\
             var c = g.next().value;\n\
             (a === 'a' && b === 'b' && c === 'c') ? 1 : 0"
        ),
        1
    );
}

// ═══════════════════════════════════════════════════════════════════════════
//  Interleaving yield and yield*
// ═══════════════════════════════════════════════════════════════════════════

/// yield before and after yield* — outer yields interleave with inner delegation.
#[test]
fn yield_star_interleaved_with_yield() {
    assert_eq!(
        run_i32(
            "function* gen() {\n\
               yield 1;\n\
               yield* [2, 3];\n\
               yield 4;\n\
             }\n\
             var g = gen();\n\
             var sum = 0;\n\
             var r;\n\
             while (!(r = g.next()).done) { sum += r.value; }\n\
             sum"
        ),
        10
    );
}

// ═══════════════════════════════════════════════════════════════════════════
//  Multiple yield* in sequence
// ═══════════════════════════════════════════════════════════════════════════

/// Two yield* in sequence — both get delegated correctly.
#[test]
fn yield_star_multiple_delegations() {
    assert_eq!(
        run_i32(
            "function* gen() {\n\
               yield* [1, 2];\n\
               yield* [3, 4];\n\
             }\n\
             var g = gen();\n\
             var sum = 0;\n\
             var r;\n\
             while (!(r = g.next()).done) { sum += r.value; }\n\
             sum"
        ),
        10
    );
}

// ═══════════════════════════════════════════════════════════════════════════
//  Nested yield* (generator delegates to generator that delegates)
// ═══════════════════════════════════════════════════════════════════════════

/// §14.4.4 — nested delegation: outer -> middle -> inner.
#[test]
fn yield_star_nested_delegation() {
    assert_eq!(
        run_i32(
            "function* inner() { yield 1; yield 2; }\n\
             function* middle() { yield* inner(); yield 3; }\n\
             function* outer() { yield* middle(); }\n\
             var g = outer();\n\
             var sum = 0;\n\
             var r;\n\
             while (!(r = g.next()).done) { sum += r.value; }\n\
             sum"
        ),
        6
    );
}

// ═══════════════════════════════════════════════════════════════════════════
//  .next(value) forwarding through yield*
// ═══════════════════════════════════════════════════════════════════════════

/// §14.4.4 step 7.a — sent values from .next(v) are forwarded to inner iterator.
#[test]
fn yield_star_next_value_forwarding() {
    assert_eq!(
        run_i32(
            "function* inner() {\n\
               var a = yield 'first';\n\
               var b = yield 'second';\n\
               return a + b;\n\
             }\n\
             function* outer() {\n\
               var result = yield* inner();\n\
               yield result;\n\
             }\n\
             var g = outer();\n\
             g.next();        // yields 'first' from inner\n\
             g.next(10);      // sends 10 to inner (a=10), yields 'second'\n\
             g.next(20).value // sends 20 to inner (b=20), inner returns 30, outer yields 30"
        ),
        30
    );
}

// ═══════════════════════════════════════════════════════════════════════════
//  .throw() forwarding through yield*
// ═══════════════════════════════════════════════════════════════════════════

/// §14.4.4 step 7.b — .throw() is forwarded to inner iterator.
#[test]
fn yield_star_throw_forwarding() {
    assert_eq!(
        run_i32(
            "function* inner() {\n\
               try { yield 1; } catch (e) { yield e + 100; }\n\
             }\n\
             function* outer() { yield* inner(); }\n\
             var g = outer();\n\
             g.next();            // yields 1\n\
             g.throw(42).value   // inner catches 42, yields 142"
        ),
        142
    );
}

/// §14.4.4 step 7.b — .throw() on inner without .throw method closes inner and throws TypeError.
#[test]
fn yield_star_throw_no_throw_method() {
    assert_eq!(
        run_i32(
            "var iter = {\n\
               [Symbol.iterator]() { return this; },\n\
               next() { return { value: 1, done: false }; },\n\
               return() { return { value: undefined, done: true }; }\n\
             };\n\
             function* gen() { yield* iter; }\n\
             var g = gen();\n\
             g.next();\n\
             try { g.throw(42); 0; } catch (e) { 1; }"
        ),
        1
    );
}

// ═══════════════════════════════════════════════════════════════════════════
//  .return() forwarding through yield*
// ═══════════════════════════════════════════════════════════════════════════

/// §14.4.4 step 7.c — .return() is forwarded to inner iterator.
/// When inner iterator's .return() reports done, outer generator completes.
#[test]
fn yield_star_return_forwarding() {
    assert_eq!(
        run_i32(
            "function* inner() { yield 1; yield 2; }\n\
             function* outer() { yield* inner(); }\n\
             var g = outer();\n\
             g.next();         // yields 1\n\
             var r = g.return(99);\n\
             (r.done === true && r.value === 99) ? 1 : 0"
        ),
        1
    );
}

/// §14.4.4 step 7.c — .return() when inner has no .return() method just returns.
#[test]
fn yield_star_return_no_return_method() {
    assert_eq!(
        run_i32(
            "var iter = {\n\
               [Symbol.iterator]() { return this; },\n\
               next() { return { value: 1, done: false }; }\n\
             };\n\
             function* gen() { yield* iter; }\n\
             var g = gen();\n\
             g.next();\n\
             var r = g.return(42);\n\
             r.done && r.value === 42 ? 1 : 0"
        ),
        1
    );
}

// ═══════════════════════════════════════════════════════════════════════════
//  yield* with empty iterable
// ═══════════════════════════════════════════════════════════════════════════

/// yield* with empty array — immediately done, expression evaluates to undefined.
#[test]
fn yield_star_empty_iterable() {
    assert_eq!(
        run_i32(
            "function* gen() {\n\
               var result = yield* [];\n\
               yield result === undefined ? 1 : 0;\n\
             }\n\
             var g = gen();\n\
             g.next().value"
        ),
        1
    );
}

// ═══════════════════════════════════════════════════════════════════════════
//  yield* with custom iterable (Symbol.iterator protocol)
// ═══════════════════════════════════════════════════════════════════════════

/// yield* with a custom iterable object that implements [Symbol.iterator].
#[test]
fn yield_star_custom_iterable() {
    assert_eq!(
        run_i32(
            "var myIterable = {\n\
               [Symbol.iterator]() {\n\
                 var i = 0;\n\
                 return {\n\
                   next() {\n\
                     i++;\n\
                     if (i <= 3) return { value: i * 10, done: false };\n\
                     return { value: undefined, done: true };\n\
                   }\n\
                 };\n\
               }\n\
             };\n\
             function* gen() { yield* myIterable; }\n\
             var g = gen();\n\
             var sum = 0;\n\
             var r;\n\
             while (!(r = g.next()).done) { sum += r.value; }\n\
             sum"
        ),
        60
    );
}

// ═══════════════════════════════════════════════════════════════════════════
//  yield* with Map and Set iterables
// ═══════════════════════════════════════════════════════════════════════════

/// yield* delegating to Map.prototype.values().
#[test]
fn yield_star_map_values() {
    assert_eq!(
        run_i32(
            "var m = new Map();\n\
             m.set('a', 1);\n\
             m.set('b', 2);\n\
             m.set('c', 3);\n\
             function* gen() { yield* m.values(); }\n\
             var g = gen();\n\
             var sum = 0;\n\
             var r;\n\
             while (!(r = g.next()).done) { sum += r.value; }\n\
             sum"
        ),
        6
    );
}

/// yield* delegating to a Set.
#[test]
fn yield_star_set() {
    assert_eq!(
        run_i32(
            "var s = new Set([10, 20, 30]);\n\
             function* gen() { yield* s; }\n\
             var g = gen();\n\
             var sum = 0;\n\
             var r;\n\
             while (!(r = g.next()).done) { sum += r.value; }\n\
             sum"
        ),
        60
    );
}

// ═══════════════════════════════════════════════════════════════════════════
//  for-of consuming yield* generator
// ═══════════════════════════════════════════════════════════════════════════

/// for-of over a generator that uses yield* internally.
#[test]
fn yield_star_consumed_by_for_of() {
    assert_eq!(
        run_i32(
            "function* gen() {\n\
               yield* [1, 2];\n\
               yield* [3, 4];\n\
             }\n\
             var sum = 0;\n\
             for (var x of gen()) { sum += x; }\n\
             sum"
        ),
        10
    );
}

// ═══════════════════════════════════════════════════════════════════════════
//  yield* return value from inner generator (via return statement)
// ═══════════════════════════════════════════════════════════════════════════

/// The return value of the inner generator becomes the yield* expression value.
#[test]
fn yield_star_captures_inner_return() {
    assert_eq!(
        run_i32(
            "function* inner() {\n\
               yield 1;\n\
               return 99;\n\
             }\n\
             function* outer() {\n\
               var x = yield* inner();\n\
               return x;\n\
             }\n\
             var g = outer();\n\
             g.next();          // yields 1 from inner\n\
             g.next().value     // inner returns 99, outer returns 99"
        ),
        99
    );
}
