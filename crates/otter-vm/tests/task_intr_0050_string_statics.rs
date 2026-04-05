//! Integration tests for String static methods (Step 54).
//!
//! Spec references:
//! - §22.1.2.1 String.fromCharCode: <https://tc39.es/ecma262/#sec-string.fromcharcode>
//! - §22.1.2.2 String.fromCodePoint: <https://tc39.es/ecma262/#sec-string.fromcodepoint>
//! - §22.1.2.4 String.raw: <https://tc39.es/ecma262/#sec-string.raw>

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
//  §22.1.2.1 — String.fromCharCode(...codeUnits)
// ═══════════════════════════════════════════════════════════════════════════

/// Basic ASCII characters.
#[test]
fn from_char_code_basic_ascii() {
    assert_eq!(
        run_string("String.fromCharCode(72, 101, 108, 108, 111)"),
        "Hello"
    );
}

/// No arguments returns empty string.
/// §22.1.2.1 Step 1: If no arguments, return "".
#[test]
fn from_char_code_no_args() {
    assert_eq!(run_string("String.fromCharCode()"), "");
}

/// Single character.
#[test]
fn from_char_code_single() {
    assert_eq!(run_string("String.fromCharCode(65)"), "A");
}

/// Values are converted to Uint16 (modulo 65536).
/// §7.1.8 ToUint16
#[test]
fn from_char_code_wraps_to_uint16() {
    // 65536 + 65 = 65601 → 65 (mod 65536) → 'A'
    assert_eq!(run_string("String.fromCharCode(65601)"), "A");
}

/// Negative values wrap via ToUint16.
#[test]
fn from_char_code_negative_wraps() {
    // -1 → 65535 (mod 65536) → U+FFFF
    assert_eq!(run_string("String.fromCharCode(-1)"), "\u{FFFF}");
}

/// NaN becomes 0 via ToUint16.
#[test]
fn from_char_code_nan_becomes_zero() {
    assert_eq!(run_string("String.fromCharCode(NaN)"), "\0");
}

/// String.fromCharCode.length === 1.
#[test]
fn from_char_code_length() {
    assert_eq!(run_i32("String.fromCharCode.length"), 1);
}

/// Null character.
#[test]
fn from_char_code_null_char() {
    assert_eq!(run_string("String.fromCharCode(0)"), "\0");
}

/// Surrogate pair code units.
#[test]
fn from_char_code_surrogate_pair() {
    // U+1F600 (😀) = surrogate pair D83D DE00
    assert_eq!(
        run_string("String.fromCharCode(0xD83D, 0xDE00)"),
        "\u{1F600}"
    );
}

// ═══════════════════════════════════════════════════════════════════════════
//  §22.1.2.2 — String.fromCodePoint(...codePoints)
// ═══════════════════════════════════════════════════════════════════════════

/// Basic ASCII code point.
#[test]
fn from_code_point_basic() {
    assert_eq!(run_string("String.fromCodePoint(65)"), "A");
}

/// No arguments returns empty string.
#[test]
fn from_code_point_no_args() {
    assert_eq!(run_string("String.fromCodePoint()"), "");
}

/// Multiple code points including BMP and supplementary plane.
#[test]
fn from_code_point_multiple() {
    assert_eq!(
        run_string("String.fromCodePoint(72, 101, 108, 108, 111)"),
        "Hello"
    );
}

/// Supplementary plane code point (emoji).
#[test]
fn from_code_point_supplementary() {
    assert_eq!(run_string("String.fromCodePoint(0x1F600)"), "\u{1F600}");
}

/// Code point 0 (null).
#[test]
fn from_code_point_zero() {
    assert_eq!(run_string("String.fromCodePoint(0)"), "\0");
}

/// Maximum valid code point 0x10FFFF.
#[test]
fn from_code_point_max() {
    assert_eq!(run_string("String.fromCodePoint(0x10FFFF)"), "\u{10FFFF}");
}

/// Throws RangeError for negative code point.
/// §22.1.2.2 Step 5.c
#[test]
fn from_code_point_negative_throws() {
    assert!(run_bool(
        "var ok = false; try { String.fromCodePoint(-1); } catch(e) { ok = e instanceof RangeError; } ok"
    ));
}

/// Throws RangeError for code point > 0x10FFFF.
#[test]
fn from_code_point_too_large_throws() {
    assert!(run_bool(
        "var ok = false; try { String.fromCodePoint(0x110000); } catch(e) { ok = e instanceof RangeError; } ok"
    ));
}

/// Throws RangeError for non-integer code point.
/// §22.1.2.2 Step 5.b
#[test]
fn from_code_point_non_integer_throws() {
    assert!(run_bool(
        "var ok = false; try { String.fromCodePoint(3.14); } catch(e) { ok = e instanceof RangeError; } ok"
    ));
}

/// Throws RangeError for Infinity.
#[test]
fn from_code_point_infinity_throws() {
    assert!(run_bool(
        "var ok = false; try { String.fromCodePoint(Infinity); } catch(e) { ok = e instanceof RangeError; } ok"
    ));
}

/// Throws RangeError for NaN.
#[test]
fn from_code_point_nan_throws() {
    assert!(run_bool(
        "var ok = false; try { String.fromCodePoint(NaN); } catch(e) { ok = e instanceof RangeError; } ok"
    ));
}

/// String.fromCodePoint.length === 1.
#[test]
fn from_code_point_length() {
    assert_eq!(run_i32("String.fromCodePoint.length"), 1);
}

// ═══════════════════════════════════════════════════════════════════════════
//  §22.1.2.4 — String.raw(template, ...substitutions)
// ═══════════════════════════════════════════════════════════════════════════

/// Tagged template usage — basic string passthrough.
#[test]
fn raw_tagged_template_basic() {
    assert_eq!(run_string(r#"String.raw`hello`"#), "hello");
}

/// Tagged template with substitutions.
#[test]
fn raw_tagged_template_substitutions() {
    assert_eq!(
        run_string(r#"var x = 'world'; String.raw`hello ${x}!`"#),
        "hello world!"
    );
}

/// Raw strings preserve escape sequences (backslashes are literal).
#[test]
fn raw_preserves_backslashes() {
    assert_eq!(run_string(r#"String.raw`\n\t\\`"#), r"\n\t\\");
}

/// Manual call with template object.
#[test]
fn raw_manual_call() {
    assert_eq!(
        run_string(r#"String.raw({ raw: ['a', 'b', 'c'] }, 1, 2)"#),
        "a1b2c"
    );
}

/// Fewer substitutions than template segments — missing ones are omitted.
#[test]
fn raw_fewer_substitutions() {
    assert_eq!(
        run_string(r#"String.raw({ raw: ['a', 'b', 'c'] }, 1)"#),
        "a1bc"
    );
}

/// More substitutions than needed — extras are ignored.
#[test]
fn raw_extra_substitutions_ignored() {
    assert_eq!(
        run_string(r#"String.raw({ raw: ['a', 'b'] }, 1, 2, 3)"#),
        "a1b"
    );
}

/// Empty raw array returns empty string.
#[test]
fn raw_empty_raw_array() {
    assert_eq!(run_string(r#"String.raw({ raw: [] })"#), "");
}

/// Single element in raw array, no substitutions.
#[test]
fn raw_single_element() {
    assert_eq!(run_string(r#"String.raw({ raw: ['hello'] })"#), "hello");
}

/// Throws TypeError when template is undefined.
#[test]
fn raw_undefined_template_throws() {
    assert!(run_bool(
        "var ok = false; try { String.raw(undefined); } catch(e) { ok = e instanceof TypeError; } ok"
    ));
}

/// Throws TypeError when template is null.
#[test]
fn raw_null_template_throws() {
    assert!(run_bool(
        "var ok = false; try { String.raw(null); } catch(e) { ok = e instanceof TypeError; } ok"
    ));
}

/// String.raw.length === 1.
#[test]
fn raw_length() {
    assert_eq!(run_i32("String.raw.length"), 1);
}

/// Substitutions are coerced to string.
#[test]
fn raw_substitutions_coerced() {
    assert_eq!(
        run_string(r#"String.raw({ raw: ['a', 'b', 'c'] }, 42, true)"#),
        "a42btruec"
    );
}

/// typeof String.fromCharCode/fromCodePoint/raw are all "function".
#[test]
fn static_methods_are_functions() {
    assert!(run_bool("typeof String.fromCharCode === 'function'"));
    assert!(run_bool("typeof String.fromCodePoint === 'function'"));
    assert!(run_bool("typeof String.raw === 'function'"));
}
