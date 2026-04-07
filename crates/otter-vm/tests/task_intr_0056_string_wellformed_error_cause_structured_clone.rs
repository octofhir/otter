//! Integration tests for Step 66 audit findings:
//!
//! - §22.1.3.13.1 String.prototype.isWellFormed: <https://tc39.es/ecma262/#sec-string.prototype.iswellformed>
//! - §22.1.3.33.1 String.prototype.toWellFormed: <https://tc39.es/ecma262/#sec-string.prototype.towellformed>
//! - §20.5.1.1 Error cause (InstallErrorCause): <https://tc39.es/ecma262/#sec-installerrorcause>
//! - §2.7 structuredClone (WHATWG HTML): <https://html.spec.whatwg.org/multipage/structured-data.html#dom-structuredclone>

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
//  String.prototype.isWellFormed
// ═══════════════════════════════════════════════════════════════════════════

#[test]
fn is_well_formed_exists() {
    assert!(run_bool("typeof ''.isWellFormed === 'function'"));
}

#[test]
fn is_well_formed_ascii() {
    assert!(run_bool("'hello world'.isWellFormed()"));
}

#[test]
fn is_well_formed_empty() {
    assert!(run_bool("''.isWellFormed()"));
}

#[test]
fn is_well_formed_unicode() {
    assert!(run_bool("'日本語'.isWellFormed()"));
}

#[test]
fn is_well_formed_emoji() {
    // Emoji with surrogate pairs (well-formed)
    assert!(run_bool("'😀'.isWellFormed()"));
}

#[test]
fn is_well_formed_supplementary_char() {
    // U+10000 (𐀀) encoded as surrogate pair \uD800\uDC00 — well-formed
    assert!(run_bool("'\\uD800\\uDC00'.isWellFormed()"));
}

// NOTE: Lone surrogate tests (isWellFormed returning false) require WTF-16
// internal string representation. Our VM uses UTF-8 strings, so lone surrogates
// are replaced with U+FFFD at compile time, making them always "well-formed"
// from the UTF-8 perspective. These tests will be enabled when we migrate to
// WTF-16 string storage.

#[test]
fn is_well_formed_lone_high_surrogate() {
    assert!(!run_bool("'\\uD800'.isWellFormed()"));
}

#[test]
fn is_well_formed_lone_low_surrogate() {
    assert!(!run_bool("'\\uDC00'.isWellFormed()"));
}

#[test]
fn is_well_formed_lone_surrogate_in_middle() {
    assert!(!run_bool("'abc\\uD800def'.isWellFormed()"));
}

#[test]
fn is_well_formed_reversed_surrogates() {
    assert!(!run_bool("'\\uDC00\\uD800'.isWellFormed()"));
}

// ═══════════════════════════════════════════════════════════════════════════
//  String.prototype.toWellFormed
// ═══════════════════════════════════════════════════════════════════════════

#[test]
fn to_well_formed_exists() {
    assert!(run_bool("typeof ''.toWellFormed === 'function'"));
}

#[test]
fn to_well_formed_ascii() {
    assert_eq!(run_string("'hello'.toWellFormed()"), "hello");
}

#[test]
fn to_well_formed_already_well_formed() {
    assert_eq!(run_string("'abc'.toWellFormed()"), "abc");
}

#[test]
fn to_well_formed_replaces_lone_high_surrogate() {
    assert!(run_bool("'\\uD800'.toWellFormed() === '\\uFFFD'"));
}

#[test]
fn to_well_formed_replaces_lone_low_surrogate() {
    assert!(run_bool("'\\uDC00'.toWellFormed() === '\\uFFFD'"));
}

#[test]
fn to_well_formed_preserves_valid_surrogate_pair() {
    // \uD800\uDC00 is a valid pair → should not be replaced
    assert!(run_bool(
        "'\\uD800\\uDC00'.toWellFormed() === '\\uD800\\uDC00'"
    ));
}

#[test]
fn to_well_formed_replaces_multiple_lone_surrogates() {
    assert!(run_bool(
        "'a\\uD800b\\uDC00c'.toWellFormed() === 'a\\uFFFDb\\uFFFDc'"
    ));
}

#[test]
fn to_well_formed_empty() {
    assert_eq!(run_string("''.toWellFormed()"), "");
}

// ═══════════════════════════════════════════════════════════════════════════
//  Error.prototype.cause (§20.5.1.1 InstallErrorCause)
// ═══════════════════════════════════════════════════════════════════════════

#[test]
fn error_cause_basic() {
    assert_eq!(
        run_i32("var e = new Error('msg', { cause: 42 }); e.cause"),
        42
    );
}

#[test]
fn error_cause_string() {
    assert_eq!(
        run_string("var e = new Error('msg', { cause: 'reason' }); e.cause"),
        "reason"
    );
}

#[test]
fn error_cause_error_object() {
    assert!(run_bool(
        "var inner = new Error('inner'); \
         var outer = new Error('outer', { cause: inner }); \
         outer.cause === inner"
    ));
}

#[test]
fn error_cause_undefined_when_no_options() {
    assert!(run_bool("var e = new Error('msg'); e.cause === undefined"));
}

#[test]
fn error_cause_undefined_when_options_has_no_cause() {
    assert!(run_bool(
        "var e = new Error('msg', {}); e.cause === undefined"
    ));
}

#[test]
fn type_error_cause() {
    assert_eq!(
        run_i32("var e = new TypeError('msg', { cause: 99 }); e.cause"),
        99
    );
}

#[test]
fn range_error_cause() {
    assert_eq!(
        run_i32("var e = new RangeError('msg', { cause: 7 }); e.cause"),
        7
    );
}

#[test]
fn reference_error_cause() {
    assert!(run_bool(
        "var e = new ReferenceError('msg', { cause: true }); e.cause === true"
    ));
}

#[test]
fn syntax_error_cause() {
    assert_eq!(
        run_string("var e = new SyntaxError('msg', { cause: 'bad' }); e.cause"),
        "bad"
    );
}

#[test]
fn error_cause_null_is_valid() {
    assert!(run_bool(
        "var e = new Error('msg', { cause: null }); e.cause === null"
    ));
}

#[test]
fn error_cause_false_is_valid() {
    assert!(run_bool(
        "var e = new Error('msg', { cause: false }); e.cause === false"
    ));
}

#[test]
fn error_cause_zero_is_valid() {
    assert_eq!(
        run_i32("var e = new Error('msg', { cause: 0 }); e.cause"),
        0
    );
}

// ═══════════════════════════════════════════════════════════════════════════
//  structuredClone (§2.7 WHATWG HTML)
// ═══════════════════════════════════════════════════════════════════════════

#[test]
fn structured_clone_exists() {
    assert!(run_bool("typeof structuredClone === 'function'"));
}

// --- Primitives ---

#[test]
fn structured_clone_number() {
    assert_eq!(run_i32("structuredClone(42)"), 42);
}

#[test]
fn structured_clone_string() {
    assert_eq!(run_string("structuredClone('hello')"), "hello");
}

#[test]
fn structured_clone_boolean_true() {
    assert!(run_bool("structuredClone(true) === true"));
}

#[test]
fn structured_clone_boolean_false() {
    assert!(run_bool("structuredClone(false) === false"));
}

#[test]
fn structured_clone_null() {
    assert!(run_bool("structuredClone(null) === null"));
}

#[test]
fn structured_clone_undefined() {
    assert!(run_bool("structuredClone(undefined) === undefined"));
}

#[test]
fn structured_clone_nan() {
    assert!(run_bool("Number.isNaN(structuredClone(NaN))"));
}

#[test]
fn structured_clone_float() {
    assert!(run_bool("structuredClone(3.14) === 3.14"));
}

// --- Objects ---

#[test]
fn structured_clone_plain_object() {
    assert!(run_bool(
        "var o = { a: 1, b: 'hello' }; \
         var c = structuredClone(o); \
         c.a === 1 && c.b === 'hello' && c !== o"
    ));
}

#[test]
fn structured_clone_nested_object() {
    assert!(run_bool(
        "var o = { x: { y: { z: 42 } } }; \
         var c = structuredClone(o); \
         c.x.y.z === 42 && c.x !== o.x && c.x.y !== o.x.y"
    ));
}

// --- Arrays ---

#[test]
fn structured_clone_array() {
    assert!(run_bool(
        "var a = [1, 2, 3]; \
         var c = structuredClone(a); \
         c.length === 3 && c[0] === 1 && c[1] === 2 && c[2] === 3 && c !== a"
    ));
}

#[test]
fn structured_clone_nested_array() {
    assert!(run_bool(
        "var a = [[1, 2], [3, 4]]; \
         var c = structuredClone(a); \
         c[0][0] === 1 && c[1][1] === 4 && c[0] !== a[0]"
    ));
}

#[test]
fn structured_clone_array_with_objects() {
    assert!(run_bool(
        "var a = [{ x: 1 }, { x: 2 }]; \
         var c = structuredClone(a); \
         c[0].x === 1 && c[1].x === 2 && c[0] !== a[0]"
    ));
}

// --- RegExp ---

#[test]
fn structured_clone_regexp() {
    assert!(run_bool(
        "var r = /abc/gi; \
         var c = structuredClone(r); \
         c.source === 'abc' && c.flags === 'gi' && c !== r"
    ));
}

// --- Map ---

#[test]
fn structured_clone_map() {
    assert!(run_bool(
        "var m = new Map([['a', 1], ['b', 2]]); \
         var c = structuredClone(m); \
         c.get('a') === 1 && c.get('b') === 2 && c !== m && c.size === 2"
    ));
}

// --- Set ---

#[test]
fn structured_clone_set() {
    assert!(run_bool(
        "var s = new Set([1, 2, 3]); \
         var c = structuredClone(s); \
         c.has(1) && c.has(2) && c.has(3) && c !== s && c.size === 3"
    ));
}

// --- ArrayBuffer ---

#[test]
fn structured_clone_arraybuffer() {
    assert!(run_bool(
        "var ab = new ArrayBuffer(8); \
         var view = new Uint8Array(ab); \
         view[0] = 42; view[7] = 99; \
         var c = structuredClone(ab); \
         var cv = new Uint8Array(c); \
         cv[0] === 42 && cv[7] === 99 && c !== ab"
    ));
}

// --- TypedArray ---

#[test]
fn structured_clone_typed_array() {
    assert!(run_bool(
        "var ta = new Uint8Array([10, 20, 30]); \
         var c = structuredClone(ta); \
         c[0] === 10 && c[1] === 20 && c[2] === 30 && c !== ta && c.buffer !== ta.buffer"
    ));
}

// --- Non-serializable types throw DataCloneError ---

#[test]
fn structured_clone_function_throws() {
    assert!(run_bool(
        "var threw = false; \
         try { structuredClone(function() {}); } \
         catch (e) { threw = true; } \
         threw"
    ));
}

#[test]
fn structured_clone_symbol_throws() {
    assert!(run_bool(
        "var threw = false; \
         try { structuredClone(Symbol('x')); } \
         catch (e) { threw = true; } \
         threw"
    ));
}

// --- Deep clone independence ---

#[test]
fn structured_clone_independent_mutation() {
    assert!(run_bool(
        "var o = { a: 1 }; \
         var c = structuredClone(o); \
         c.a = 999; \
         o.a === 1"
    ));
}

#[test]
fn structured_clone_array_independent_mutation() {
    assert!(run_bool(
        "var a = [1, 2, 3]; \
         var c = structuredClone(a); \
         c[0] = 999; \
         a[0] === 1"
    ));
}

#[test]
fn structured_clone_map_independent_mutation() {
    assert!(run_bool(
        "var m = new Map([['key', 'val']]); \
         var c = structuredClone(m); \
         c.set('key', 'changed'); \
         m.get('key') === 'val'"
    ));
}
