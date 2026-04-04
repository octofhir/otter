//! Integration tests for tagged template literals.
//!
//! ES2024 §13.3.11 Tagged Templates
//! Spec: <https://tc39.es/ecma262/#sec-tagged-templates>
//!
//! §13.2.8.3 GetTemplateObject
//! Spec: <https://tc39.es/ecma262/#sec-gettemplateobject>

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
//  §13.3.11 — Basic tag function receives correct arguments
// ═══════════════════════════════════════════════════════════════════════════

/// Tag function receives the strings array as first argument.
#[test]
fn tag_receives_strings_array() {
    assert_eq!(
        run_i32(
            "function tag(strings) { return strings.length; }\n\
             tag`hello`"
        ),
        1
    );
}

/// Tag function receives substitution values as additional arguments.
#[test]
fn tag_receives_substitutions() {
    assert_eq!(
        run_i32(
            "function tag(strings, a, b) { return a + b; }\n\
             tag`${10}${32}`"
        ),
        42
    );
}

/// Strings array has one more element than substitutions.
#[test]
fn strings_count_is_subs_plus_one() {
    assert_eq!(
        run_i32(
            "function tag(strings, a) { return strings.length; }\n\
             tag`hello ${42} world`"
        ),
        2
    );
}

// ═══════════════════════════════════════════════════════════════════════════
//  §13.2.8.3 — Template object structure
// ═══════════════════════════════════════════════════════════════════════════

/// Strings array contains the cooked string parts.
#[test]
fn tag_strings_cooked_values() {
    assert_eq!(
        run_string(
            "function tag(strings) { return strings[0]; }\n\
             tag`hello world`"
        ),
        "hello world"
    );
}

/// Strings array contains parts split around substitutions.
#[test]
fn tag_strings_split_around_subs() {
    assert_eq!(
        run_string(
            "function tag(strings) { return strings[0] + '|' + strings[1]; }\n\
             tag`hello ${42} world`"
        ),
        "hello | world"
    );
}

/// Template object has a `.raw` property.
#[test]
fn tag_has_raw_property() {
    assert!(run_bool(
        "function tag(strings) { return strings.raw !== undefined; }\n\
         tag`test`"
    ));
}

/// `.raw` array has the same length as cooked strings array.
#[test]
fn tag_raw_same_length() {
    assert!(run_bool(
        "function tag(strings) { return strings.raw.length === strings.length; }\n\
         tag`a ${1} b ${2} c`"
    ));
}

/// `.raw` contains raw (unprocessed) string values.
#[test]
fn tag_raw_values() {
    assert_eq!(
        run_string(
            "function tag(strings) { return strings.raw[0]; }\n\
             tag`hello`"
        ),
        "hello"
    );
}

// ═══════════════════════════════════════════════════════════════════════════
//  §13.3.11 — Tag function return value
// ═══════════════════════════════════════════════════════════════════════════

/// Tag function can return any value, not just strings.
#[test]
fn tag_returns_non_string() {
    assert_eq!(
        run_i32(
            "function tag(strings, val) { return val * 2; }\n\
             tag`${21}`"
        ),
        42
    );
}

/// Tag function can return arrays.
#[test]
fn tag_returns_array() {
    assert_eq!(
        run_i32(
            "function tag(strings, a, b) { return [a, b]; }\n\
             var result = tag`${3}${7}`;\n\
             result[0] + result[1]"
        ),
        10
    );
}

// ═══════════════════════════════════════════════════════════════════════════
//  §13.3.11 — Method as tag function (this binding)
// ═══════════════════════════════════════════════════════════════════════════

/// Object method used as tag function receives correct `this`.
#[test]
fn method_tag_receives_this() {
    assert_eq!(
        run_i32(
            "var obj = {\n\
               multiplier: 10,\n\
               tag: function(strings, val) { return val * this.multiplier; }\n\
             };\n\
             obj.tag`${5}`"
        ),
        50
    );
}

// ═══════════════════════════════════════════════════════════════════════════
//  §13.3.11 — String.raw built-in tag (if available)
// ═══════════════════════════════════════════════════════════════════════════

/// Custom implementation of String.raw behavior.
#[test]
fn custom_string_raw_behavior() {
    assert_eq!(
        run_string(
            "function raw(strings) {\n\
               var result = '';\n\
               for (var i = 0; i < strings.raw.length; i++) {\n\
                 result = result + strings.raw[i];\n\
                 if (i + 1 < arguments.length) {\n\
                   result = result + arguments[i + 1];\n\
                 }\n\
               }\n\
               return result;\n\
             }\n\
             raw`hello ${'world'} end`"
        ),
        "hello world end"
    );
}

// ═══════════════════════════════════════════════════════════════════════════
//  §13.3.11 — No substitutions
// ═══════════════════════════════════════════════════════════════════════════

/// Tagged template with no substitutions — only strings array, no extra args.
#[test]
fn tag_no_substitutions() {
    assert_eq!(
        run_i32(
            "function tag(strings) { return strings.length * 100 + arguments.length; }\n\
             tag`plain text`"
        ),
        101
    );
}

// ═══════════════════════════════════════════════════════════════════════════
//  §13.3.11 — Multiple substitutions
// ═══════════════════════════════════════════════════════════════════════════

/// Three substitutions produces 4 string parts and 3 extra arguments.
#[test]
fn tag_multiple_substitutions() {
    assert_eq!(
        run_i32(
            "function tag(strings, a, b, c) {\n\
               return strings.length * 1000 + a + b + c;\n\
             }\n\
             tag`${1} and ${2} and ${3}`"
        ),
        4006
    );
}

/// Empty strings between consecutive substitutions.
#[test]
fn tag_consecutive_substitutions() {
    assert_eq!(
        run_i32(
            "function tag(strings, a, b) {\n\
               return strings.length * 100 + a + b;\n\
             }\n\
             tag`${10}${20}`"
        ),
        330
    );
}

/// Empty string parts when template starts/ends with substitutions.
#[test]
fn tag_empty_string_parts() {
    assert_eq!(
        run_string(
            "function tag(strings) {\n\
               return strings[0] + '|' + strings[1] + '|' + strings[2];\n\
             }\n\
             tag`${1}middle${2}`"
        ),
        "|middle|"
    );
}

// ═══════════════════════════════════════════════════════════════════════════
//  §13.3.11 — Expressions are evaluated in order
// ═══════════════════════════════════════════════════════════════════════════

/// Substitution expressions are evaluated left-to-right before tag is called.
#[test]
fn tag_evaluates_expressions_in_order() {
    assert_eq!(
        run_i32(
            "var counter = 0;\n\
             function inc() { counter = counter + 1; return counter; }\n\
             function tag(strings, a, b, c) { return a * 100 + b * 10 + c; }\n\
             tag`${inc()}${inc()}${inc()}`"
        ),
        123
    );
}
