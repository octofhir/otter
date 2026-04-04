//! Integration tests for async generator functions.
//!
//! ES2024 §27.6 AsyncGenerator Objects
//! Spec: <https://tc39.es/ecma262/#sec-asyncgenerator-objects>
//!
//! ES2024 §27.4 AsyncGeneratorFunction Objects
//! Spec: <https://tc39.es/ecma262/#sec-asyncgeneratorfunction-objects>

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
//  Basic async generator declaration and invocation
// ═══════════════════════════════════════════════════════════════════════════

#[test]
fn async_generator_declaration_returns_object() {
    assert_eq!(
        run_i32(
            "async function* gen() { yield 1; }\n\
             var g = gen();\n\
             typeof g === 'object' ? 0 : 1"
        ),
        0
    );
}

#[test]
fn async_generator_expression_returns_object() {
    assert_eq!(
        run_i32(
            "var gen = async function*() { yield 42; };\n\
             var g = gen();\n\
             typeof g === 'object' ? 0 : 1"
        ),
        0
    );
}

// ═══════════════════════════════════════════════════════════════════════════
//  .next() returns a Promise
// ═══════════════════════════════════════════════════════════════════════════

#[test]
fn async_generator_next_returns_promise() {
    assert_eq!(
        run_i32(
            "async function* gen() { yield 1; }\n\
             var g = gen();\n\
             var p = g.next();\n\
             p instanceof Promise ? 0 : 1"
        ),
        0
    );
}

// ═══════════════════════════════════════════════════════════════════════════
//  Yield values through async generators consumed by async function
// ═══════════════════════════════════════════════════════════════════════════

#[test]
fn async_generator_yield_single_value() {
    assert_eq!(
        run_i32(
            "var result = -1;\n\
             async function* gen() { yield 42; }\n\
             (async function() {\n\
               var g = gen();\n\
               var r = await g.next();\n\
               result = r.value;\n\
             })();\n\
             result"
        ),
        42
    );
}

#[test]
fn async_generator_yield_multiple_values() {
    assert_eq!(
        run_i32(
            "var sum = 0;\n\
             async function* gen() { yield 10; yield 20; yield 30; }\n\
             (async function() {\n\
               var g = gen();\n\
               var r1 = await g.next(); sum += r1.value;\n\
               var r2 = await g.next(); sum += r2.value;\n\
               var r3 = await g.next(); sum += r3.value;\n\
             })();\n\
             sum"
        ),
        60
    );
}

#[test]
fn async_generator_done_flag() {
    assert_eq!(
        run_i32(
            "var done_val = -1;\n\
             async function* gen() { yield 1; }\n\
             (async function() {\n\
               var g = gen();\n\
               await g.next();\n\
               var r = await g.next();\n\
               done_val = r.done ? 1 : 0;\n\
             })();\n\
             done_val"
        ),
        1
    );
}

// ═══════════════════════════════════════════════════════════════════════════
//  .return() on async generator
// ═══════════════════════════════════════════════════════════════════════════

#[test]
fn async_generator_return() {
    assert_eq!(
        run_i32(
            "var rv = -1;\n\
             async function* gen() { yield 1; yield 2; }\n\
             (async function() {\n\
               var g = gen();\n\
               await g.next();\n\
               var r = await g.return(99);\n\
               rv = r.done ? r.value : -2;\n\
             })();\n\
             rv"
        ),
        99
    );
}

// ═══════════════════════════════════════════════════════════════════════════
//  .throw() on async generator
// ═══════════════════════════════════════════════════════════════════════════

#[test]
fn async_generator_throw_rejects() {
    assert_eq!(
        run_i32(
            "var caught = -1;\n\
             async function* gen() { yield 1; yield 2; }\n\
             (async function() {\n\
               var g = gen();\n\
               await g.next();\n\
               try { await g.throw(42); } catch(e) { caught = e; }\n\
             })();\n\
             caught"
        ),
        42
    );
}

// ═══════════════════════════════════════════════════════════════════════════
//  yield + await interleaving
// ═══════════════════════════════════════════════════════════════════════════

#[test]
fn async_generator_yield_and_await() {
    assert_eq!(
        run_i32(
            "var val = -1;\n\
             async function* gen() {\n\
               var x = await Promise.resolve(10);\n\
               yield x + 5;\n\
             }\n\
             (async function() {\n\
               var g = gen();\n\
               var r = await g.next();\n\
               val = r.value;\n\
             })();\n\
             val"
        ),
        15
    );
}

#[test]
fn async_generator_multiple_await_yield() {
    assert_eq!(
        run_i32(
            "var sum = 0;\n\
             async function* gen() {\n\
               var a = await Promise.resolve(1);\n\
               yield a;\n\
               var b = await Promise.resolve(2);\n\
               yield b;\n\
             }\n\
             (async function() {\n\
               var g = gen();\n\
               var r1 = await g.next(); sum += r1.value;\n\
               var r2 = await g.next(); sum += r2.value;\n\
             })();\n\
             sum"
        ),
        3
    );
}

// ═══════════════════════════════════════════════════════════════════════════
//  for-await-of consuming async generators
// ═══════════════════════════════════════════════════════════════════════════

#[test]
fn async_generator_for_await_of() {
    assert_eq!(
        run_i32(
            "var sum = 0;\n\
             async function* gen() { yield 1; yield 2; yield 3; }\n\
             (async function() {\n\
               for await (var v of gen()) { sum += v; }\n\
             })();\n\
             sum"
        ),
        6
    );
}

// ═══════════════════════════════════════════════════════════════════════════
//  Prototype chain
// ═══════════════════════════════════════════════════════════════════════════

#[test]
fn async_generator_to_string_tag() {
    assert_eq!(
        run_i32(
            "async function* gen() {}\n\
             var g = gen();\n\
             var tag = Object.prototype.toString.call(g);\n\
             tag === '[object AsyncGenerator]' ? 0 : 1"
        ),
        0
    );
}

// ═══════════════════════════════════════════════════════════════════════════
//  Class methods as async generators
// ═══════════════════════════════════════════════════════════════════════════

#[test]
fn async_generator_class_method() {
    assert_eq!(
        run_i32(
            "var val = -1;\n\
             class Foo {\n\
               async *items() { yield 100; yield 200; }\n\
             }\n\
             (async function() {\n\
               var f = new Foo();\n\
               var g = f.items();\n\
               var r = await g.next();\n\
               val = r.value;\n\
             })();\n\
             val"
        ),
        100
    );
}

// ═══════════════════════════════════════════════════════════════════════════
//  Async generator with return value
// ═══════════════════════════════════════════════════════════════════════════

#[test]
fn async_generator_return_value() {
    assert_eq!(
        run_i32(
            "var rv = -1;\n\
             async function* gen() { yield 1; return 99; }\n\
             (async function() {\n\
               var g = gen();\n\
               await g.next();\n\
               var r = await g.next();\n\
               rv = r.done ? r.value : -2;\n\
             })();\n\
             rv"
        ),
        99
    );
}

// ═══════════════════════════════════════════════════════════════════════════
//  Error handling inside async generator body
// ═══════════════════════════════════════════════════════════════════════════

#[test]
fn async_generator_try_catch_yield() {
    // Verify that .throw() is caught inside the generator body
    // and the catch block can yield a value back to the caller.
    assert_eq!(
        run_i32(
            "var result = -1;\n\
             async function* gen() {\n\
               try { yield 1; } catch(e) { yield e + 100; }\n\
             }\n\
             (async function() {\n\
               var g = gen();\n\
               await g.next();\n\
               var r = await g.throw(42);\n\
               result = r.value;\n\
             })();\n\
             result"
        ),
        142
    );
}
