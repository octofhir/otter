//! Integration tests for missing builtins & globals (Step 48).
//!
//! §19.1 globalThis
//! Spec: <https://tc39.es/ecma262/#sec-globalthis>
//!
//! §20.5.7 AggregateError
//! Spec: <https://tc39.es/ecma262/#sec-aggregate-error-objects>

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

fn run_bool(source: &str) -> bool {
    let v = run(source);
    v.as_bool()
        .unwrap_or_else(|| panic!("expected bool, got {v:?}"))
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
//  §19.1 — globalThis
// ═══════════════════════════════════════════════════════════════════════════

/// `globalThis` exists and is an object.
#[test]
fn global_this_exists() {
    assert!(run_bool("typeof globalThis === 'object'"));
}

/// `globalThis` is a self-reference to the global object.
#[test]
fn global_this_is_self_reference() {
    assert!(run_bool("globalThis === globalThis"));
}

/// Properties set on the global are accessible via globalThis.
#[test]
fn global_this_accesses_globals() {
    assert!(run_bool(
        "var testGlobal = 42;\n\
         globalThis.testGlobal === 42"
    ));
}

/// `globalThis.globalThis === globalThis` (reflexive).
#[test]
fn global_this_reflexive() {
    assert!(run_bool("globalThis.globalThis === globalThis"));
}

/// Built-in constructors are accessible via globalThis.
#[test]
fn global_this_has_builtins() {
    assert!(run_bool("typeof globalThis.Array === 'function'"));
}

// ═══════════════════════════════════════════════════════════════════════════
//  §20.5.7 — AggregateError
// ═══════════════════════════════════════════════════════════════════════════

/// AggregateError exists as a global constructor.
#[test]
fn aggregate_error_exists() {
    assert!(run_bool("typeof AggregateError === 'function'"));
}

/// AggregateError is instanceof Error.
#[test]
fn aggregate_error_is_error() {
    assert!(run_bool("new AggregateError([]) instanceof Error"));
}

/// AggregateError stores the errors argument.
#[test]
fn aggregate_error_stores_errors() {
    assert_eq!(
        run_i32(
            "var e = new AggregateError([1, 2, 3], 'test');\n\
             e.errors.length"
        ),
        3
    );
}

/// AggregateError stores the message.
#[test]
fn aggregate_error_stores_message() {
    assert_eq!(
        run_string(
            "var e = new AggregateError([], 'something went wrong');\n\
             e.message"
        ),
        "something went wrong"
    );
}

/// AggregateError with no message — message defaults to empty.
#[test]
fn aggregate_error_no_message() {
    assert_eq!(
        run_string(
            "var e = new AggregateError([]);\n\
             e.message"
        ),
        ""
    );
}

/// AggregateError.prototype.name is "AggregateError".
#[test]
fn aggregate_error_name() {
    assert_eq!(
        run_string(
            "var e = new AggregateError([]);\n\
             e.name"
        ),
        "AggregateError"
    );
}

/// AggregateError errors can be accessed individually.
#[test]
fn aggregate_error_individual_errors() {
    assert_eq!(
        run_i32(
            "var errs = [new Error('a'), new Error('b')];\n\
             var e = new AggregateError(errs, 'multi');\n\
             e.errors.length"
        ),
        2
    );
}

/// AggregateError can be caught and inspected.
#[test]
fn aggregate_error_catch() {
    assert_eq!(
        run_i32(
            "var result = 0;\n\
             try { throw new AggregateError([1, 2], 'test'); }\n\
             catch (e) { result = e.errors[0] + e.errors[1]; }\n\
             result"
        ),
        3
    );
}

/// AggregateError toString produces "AggregateError: message".
#[test]
fn aggregate_error_to_string() {
    assert_eq!(
        run_string(
            "var e = new AggregateError([], 'oops');\n\
             e.toString()"
        ),
        "AggregateError: oops"
    );
}
