//! Integration tests for §19.2.1 eval() and §19.2.6 URI functions.
//!
//! Spec references:
//! - §19.2.1 eval(x): <https://tc39.es/ecma262/#sec-eval-x>
//! - §19.2.6.1 encodeURI: <https://tc39.es/ecma262/#sec-encodeuri-uristring>
//! - §19.2.6.2 encodeURIComponent: <https://tc39.es/ecma262/#sec-encodeuricomponent-uricomponent>
//! - §19.2.6.3 decodeURI: <https://tc39.es/ecma262/#sec-decodeuri-encodeduri>
//! - §19.2.6.4 decodeURIComponent: <https://tc39.es/ecma262/#sec-decodeuricomponent-encodeduricomponent>

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
//  §19.2.1 — eval(x) — Indirect eval
// ═══════════════════════════════════════════════════════════════════════════

/// eval(42) returns 42 unchanged (non-string argument).
/// §19.2.1 Step 1: If x is not a String, return x.
#[test]
fn eval_returns_non_string_argument_unchanged() {
    assert_eq!(run_i32("eval(42)"), 42);
}

/// eval() with no arguments returns undefined.
#[test]
fn eval_returns_undefined_with_no_args() {
    assert_eq!(run("eval()"), RegisterValue::undefined());
}

/// eval(true) returns true (boolean is not a string).
#[test]
fn eval_returns_boolean_unchanged() {
    assert!(run_bool("eval(true)"));
}

/// eval(null) returns null.
#[test]
fn eval_returns_null_unchanged() {
    assert_eq!(run("eval(null)"), RegisterValue::null());
}

/// eval('1 + 2') evaluates the expression and returns 3.
#[test]
fn eval_evaluates_string_expression() {
    assert_eq!(run_i32("eval('1 + 2')"), 3);
}

/// eval returns the completion value of the last expression statement.
#[test]
fn eval_returns_completion_value() {
    assert_eq!(run_i32("eval('1; 2; 3')"), 3);
}

/// eval can call functions defined in the global scope.
#[test]
fn eval_can_call_global_functions() {
    assert!(run_bool(
        "function add(a,b){return a+b} eval('add(3,4)') === 7"
    ));
}

/// eval with a syntax error throws SyntaxError.
#[test]
fn eval_syntax_error_throws() {
    assert!(run_bool(
        "var caught = false; try { eval('var 123'); } catch(e) { caught = true; } caught"
    ));
}

/// Nested eval calls work.
#[test]
fn eval_nested() {
    assert_eq!(run_i32("eval(\"eval('1 + 2')\")"), 3);
}

/// eval defines a function accessible in global scope.
#[test]
fn eval_function_declaration_global() {
    assert_eq!(run_i32("eval('function foo() { return 99; }'); foo()"), 99);
}

/// Indirect eval: (0, eval)('expr') works the same as eval('expr').
#[test]
fn indirect_eval_via_comma() {
    assert_eq!(run_i32("(0, eval)('10 + 20')"), 30);
}

/// eval returns undefined for statements without completion value.
#[test]
fn eval_var_declaration_returns_undefined() {
    assert_eq!(run("eval('var x = 5;')"), RegisterValue::undefined());
}

// ═══════════════════════════════════════════════════════════════════════════
//  §19.2.6.1 — encodeURI
// ═══════════════════════════════════════════════════════════════════════════

/// encodeURI preserves URI-safe characters.
#[test]
fn encode_uri_preserves_reserved() {
    assert_eq!(
        run_string("encodeURI('http://example.com/path?q=1&x=2#h')"),
        "http://example.com/path?q=1&x=2#h"
    );
}

/// encodeURI encodes spaces.
#[test]
fn encode_uri_encodes_spaces() {
    assert_eq!(run_string("encodeURI('hello world')"), "hello%20world");
}

/// encodeURI encodes multibyte characters.
#[test]
fn encode_uri_encodes_multibyte() {
    assert_eq!(run_string("encodeURI('café')"), "caf%C3%A9");
}

// ═══════════════════════════════════════════════════════════════════════════
//  §19.2.6.2 — encodeURIComponent
// ═══════════════════════════════════════════════════════════════════════════

/// encodeURIComponent encodes reserved characters.
#[test]
fn encode_uri_component_encodes_reserved() {
    assert_eq!(
        run_string("encodeURIComponent('hello=world&foo')"),
        "hello%3Dworld%26foo"
    );
}

/// encodeURIComponent preserves unreserved marks.
#[test]
fn encode_uri_component_preserves_unreserved() {
    assert_eq!(
        run_string("encodeURIComponent('hello-world_test.js')"),
        "hello-world_test.js"
    );
}

/// encodeURIComponent encodes #.
#[test]
fn encode_uri_component_encodes_hash() {
    assert_eq!(run_string("encodeURIComponent('#section')"), "%23section");
}

// ═══════════════════════════════════════════════════════════════════════════
//  §19.2.6.3 — decodeURI
// ═══════════════════════════════════════════════════════════════════════════

/// decodeURI decodes %20 to space.
#[test]
fn decode_uri_decodes_spaces() {
    assert_eq!(run_string("decodeURI('hello%20world')"), "hello world");
}

/// decodeURI preserves reserved character escapes (like %23 for #).
#[test]
fn decode_uri_preserves_reserved_escapes() {
    assert_eq!(run_string("decodeURI('hello%23world')"), "hello%23world");
}

/// decodeURI handles multibyte UTF-8.
#[test]
fn decode_uri_multibyte() {
    assert_eq!(run_string("decodeURI('caf%C3%A9')"), "café");
}

// ═══════════════════════════════════════════════════════════════════════════
//  §19.2.6.4 — decodeURIComponent
// ═══════════════════════════════════════════════════════════════════════════

/// decodeURIComponent decodes all percent-encoded sequences.
#[test]
fn decode_uri_component_decodes_all() {
    assert_eq!(
        run_string("decodeURIComponent('hello%3Dworld%26foo')"),
        "hello=world&foo"
    );
}

/// decodeURIComponent decodes # (unlike decodeURI).
#[test]
fn decode_uri_component_decodes_hash() {
    assert_eq!(
        run_string("decodeURIComponent('hello%23world')"),
        "hello#world"
    );
}

/// decodeURIComponent throws URIError on malformed input.
#[test]
fn decode_uri_component_malformed_throws() {
    assert!(run_bool(
        "var caught = false; try { decodeURIComponent('%G0'); } catch(e) { caught = true; } caught"
    ));
}

// ═══════════════════════════════════════════════════════════════════════════
//  Roundtrip tests
// ═══════════════════════════════════════════════════════════════════════════

/// encodeURIComponent/decodeURIComponent roundtrip.
#[test]
fn encode_decode_uri_component_roundtrip() {
    assert!(run_bool(
        "decodeURIComponent(encodeURIComponent('hello=world&foo')) === 'hello=world&foo'"
    ));
}

/// encodeURI/decodeURI roundtrip with multibyte.
#[test]
fn encode_decode_uri_roundtrip_multibyte() {
    assert!(run_bool("decodeURI(encodeURI('café')) === 'café'"));
}

/// typeof eval is 'function'.
#[test]
fn eval_typeof() {
    assert!(run_bool("typeof eval === 'function'"));
}

/// eval.length is 1.
#[test]
fn eval_length_is_1() {
    assert_eq!(run_i32("eval.length"), 1);
}

/// encodeURI.length is 1.
#[test]
fn encode_uri_length_is_1() {
    assert_eq!(run_i32("encodeURI.length"), 1);
}

// ═══════════════════════════════════════════════════════════════════════════
//  §20.2.1.1 — Function() constructor
// ═══════════════════════════════════════════════════════════════════════════

/// Function() with no args returns a function.
#[test]
fn function_constructor_no_args() {
    assert!(run_bool("typeof new Function() === 'function'"));
}

/// Function('return 42')() returns 42.
#[test]
fn function_constructor_body_only() {
    assert_eq!(run_i32("new Function('return 42')()"), 42);
}

/// Function('a', 'b', 'return a + b')(3, 4) returns 7.
#[test]
fn function_constructor_with_params() {
    assert_eq!(run_i32("new Function('a', 'b', 'return a + b')(3, 4)"), 7);
}

/// Function('a,b', 'return a * b')(5, 6) — comma-separated params in one string.
#[test]
fn function_constructor_comma_params() {
    assert_eq!(run_i32("new Function('a,b', 'return a * b')(5, 6)"), 30);
}

/// Function() called without new also works.
#[test]
fn function_constructor_without_new() {
    assert_eq!(run_i32("Function('return 99')()"), 99);
}

/// Function constructor syntax error throws SyntaxError.
#[test]
fn function_constructor_syntax_error() {
    assert!(run_bool(
        "var ok = false; try { new Function('return {{{'); } catch(e) { ok = true; } ok"
    ));
}

/// Function.length is 1.
#[test]
fn function_constructor_length() {
    assert_eq!(run_i32("Function.length"), 1);
}
