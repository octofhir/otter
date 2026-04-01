//! Integration tests for ES2024 RegExp (§22.2).
//!
//! Spec references:
//! - RegExp Constructor: <https://tc39.es/ecma262/#sec-regexp-constructor>
//! - RegExp.prototype.exec: <https://tc39.es/ecma262/#sec-regexp.prototype.exec>
//! - RegExp.prototype.test: <https://tc39.es/ecma262/#sec-regexp.prototype.test>
//! - String.prototype.match: <https://tc39.es/ecma262/#sec-string.prototype.match>
//! - String.prototype.replace: <https://tc39.es/ecma262/#sec-string.prototype.replace>
//! - String.prototype.search: <https://tc39.es/ecma262/#sec-string.prototype.search>
//! - String.prototype.split: <https://tc39.es/ecma262/#sec-string.prototype.split>

use otter_vm::source::compile_test262_basic_script;
use otter_vm::value::RegisterValue;
use otter_vm::{Interpreter, RuntimeState};

fn run(source: &str, url: &str) -> RegisterValue {
    let module = compile_test262_basic_script(source, url).expect("should compile");
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

#[test]
fn regexp_literal_test() {
    let r = run(
        concat!(
            "var re = /hello/;\n",
            "assert.sameValue(re.test('say hello world'), true, 'matches');\n",
            "assert.sameValue(re.test('goodbye'), false, 'no match');\n",
        ),
        "regexp-literal-test.js",
    );
    assert_eq!(r, RegisterValue::from_i32(0));
}

#[test]
fn regexp_constructor_test() {
    let r = run(
        concat!(
            "var re = new RegExp('world');\n",
            "assert.sameValue(re.test('hello world'), true, 'matches');\n",
            "assert.sameValue(re.test('hello'), false, 'no match');\n",
        ),
        "regexp-constructor-test.js",
    );
    assert_eq!(r, RegisterValue::from_i32(0));
}

#[test]
fn regexp_exec_basic() {
    let r = run(
        concat!(
            "var re = /fo+/;\n",
            "var m = re.exec('foobar');\n",
            "assert.sameValue(m[0], 'foo', 'matched text');\n",
            "assert.sameValue(m.index, 0, 'match index');\n",
            "assert.sameValue(m.input, 'foobar', 'input');\n",
        ),
        "regexp-exec-basic.js",
    );
    assert_eq!(r, RegisterValue::from_i32(0));
}

#[test]
fn regexp_exec_no_match() {
    let r = run(
        concat!(
            "var re = /xyz/;\n",
            "var m = re.exec('foobar');\n",
            "assert.sameValue(m, null, 'no match returns null');\n",
        ),
        "regexp-exec-no-match.js",
    );
    assert_eq!(r, RegisterValue::from_i32(0));
}

#[test]
fn regexp_global_flag_match_all() {
    let r = run(
        concat!(
            "var re = /a/g;\n",
            "var m = 'banana'.match(re);\n",
            "assert.sameValue(m.length, 3, 'three matches');\n",
            "assert.sameValue(m[0], 'a', 'first');\n",
            "assert.sameValue(m[1], 'a', 'second');\n",
            "assert.sameValue(m[2], 'a', 'third');\n",
        ),
        "regexp-global-match.js",
    );
    assert_eq!(r, RegisterValue::from_i32(0));
}

#[test]
fn regexp_case_insensitive() {
    let r = run(
        concat!(
            "var re = /hello/i;\n",
            "assert.sameValue(re.test('HELLO WORLD'), true, 'case insensitive');\n",
            "assert.sameValue(re.test('WORLD'), false, 'no match');\n",
        ),
        "regexp-case-insensitive.js",
    );
    assert_eq!(r, RegisterValue::from_i32(0));
}

#[test]
fn regexp_capture_groups() {
    let r = run(
        concat!(
            "var re = /(\\d+)-(\\d+)/;\n",
            "var m = re.exec('abc 123-456 def');\n",
            "assert.sameValue(m[0], '123-456', 'full match');\n",
            "assert.sameValue(m[1], '123', 'group 1');\n",
            "assert.sameValue(m[2], '456', 'group 2');\n",
            "assert.sameValue(m.index, 4, 'index');\n",
        ),
        "regexp-capture.js",
    );
    assert_eq!(r, RegisterValue::from_i32(0));
}

#[test]
fn string_search() {
    let r = run(
        concat!(
            "assert.sameValue('foobar'.search(/bar/), 3, 'found at 3');\n",
            "assert.sameValue('foobar'.search(/baz/), -1, 'not found');\n",
        ),
        "string-search.js",
    );
    assert_eq!(r, RegisterValue::from_i32(0));
}

#[test]
fn string_match_non_global() {
    let r = run(
        concat!(
            "var m = 'hello world'.match(/w(\\w+)/);\n",
            "assert.sameValue(m[0], 'world', 'full match');\n",
            "assert.sameValue(m[1], 'orld', 'capture');\n",
        ),
        "string-match-non-global.js",
    );
    assert_eq!(r, RegisterValue::from_i32(0));
}

#[test]
fn string_replace_regexp() {
    let r = run(
        concat!(
            "var result = 'hello world'.replace(/world/, 'there');\n",
            "assert.sameValue(result, 'hello there', 'replace');\n",
        ),
        "string-replace-regexp.js",
    );
    assert_eq!(r, RegisterValue::from_i32(0));
}

#[test]
fn string_replace_global() {
    let r = run(
        concat!(
            "var result = 'aabbcc'.replace(/b/g, 'X');\n",
            "assert.sameValue(result, 'aaXXcc', 'global replace');\n",
        ),
        "string-replace-global.js",
    );
    assert_eq!(r, RegisterValue::from_i32(0));
}

#[test]
fn regexp_source_and_flags() {
    let r = run(
        concat!(
            "var re = /hello/gi;\n",
            "assert.sameValue(re.source, 'hello', 'source');\n",
            "assert.sameValue(re.flags, 'gi', 'flags');\n",
            "assert.sameValue(re.global, true, 'global');\n",
            "assert.sameValue(re.ignoreCase, true, 'ignoreCase');\n",
            "assert.sameValue(re.multiline, false, 'multiline');\n",
        ),
        "regexp-source-flags.js",
    );
    assert_eq!(r, RegisterValue::from_i32(0));
}

#[test]
fn regexp_to_string() {
    let r = run(
        concat!(
            "var re = /foo/gi;\n",
            "assert.sameValue(re.toString(), '/foo/gi', 'toString');\n",
        ),
        "regexp-to-string.js",
    );
    assert_eq!(r, RegisterValue::from_i32(0));
}

#[test]
fn regexp_last_index_sticky() {
    let r = run(
        concat!(
            "var re = /a/g;\n",
            "re.lastIndex = 2;\n",
            "var m = re.exec('baa');\n",
            "assert.sameValue(m[0], 'a', 'match after lastIndex');\n",
        ),
        "regexp-lastindex.js",
    );
    assert_eq!(r, RegisterValue::from_i32(0));
}

#[test]
fn string_split_regexp() {
    let r = run(
        concat!(
            "var parts = 'a1b2c3'.split(/\\d/);\n",
            "assert.sameValue(parts.length, 4, 'split count');\n",
            "assert.sameValue(parts[0], 'a', 'part 0');\n",
            "assert.sameValue(parts[1], 'b', 'part 1');\n",
            "assert.sameValue(parts[2], 'c', 'part 2');\n",
            "assert.sameValue(parts[3], '', 'part 3 empty');\n",
        ),
        "string-split-regexp.js",
    );
    assert_eq!(r, RegisterValue::from_i32(0));
}

#[test]
fn regexp_replace_function() {
    let r = run(
        concat!(
            "var result = 'hello world'.replace(/\\w+/g, function(m) { return m.toUpperCase(); });\n",
            "assert.sameValue(result, 'HELLO WORLD', 'replace with function');\n",
        ),
        "regexp-replace-fn.js",
    );
    assert_eq!(r, RegisterValue::from_i32(0));
}
