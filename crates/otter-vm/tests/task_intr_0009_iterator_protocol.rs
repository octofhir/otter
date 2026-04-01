//! Integration tests for the ECMAScript iterator protocol.
//!
//! Validates:
//! - %IteratorPrototype%[@@iterator]()  — §27.1.2
//!   <https://tc39.es/ecma262/#sec-%iteratorprototype%-@@iterator>
//! - %ArrayIteratorPrototype%.next()     — §23.1.5
//!   <https://tc39.es/ecma262/#sec-%arrayiteratorprototype%.next>
//! - %StringIteratorPrototype%.next()    — §22.1.5
//!   <https://tc39.es/ecma262/#sec-%stringiteratorprototype%.next>
//! - %MapIteratorPrototype%.next()       — §24.1.5
//!   <https://tc39.es/ecma262/#sec-%mapiteratorprototype%.next>
//! - %SetIteratorPrototype%.next()       — §24.2.5
//!   <https://tc39.es/ecma262/#sec-%setiteratorprototype%.next>
//! - CreateIterResultObject              — §7.4.14
//!   <https://tc39.es/ecma262/#sec-createiterresultobject>

use otter_vm::source::compile_test262_basic_script;
use otter_vm::value::RegisterValue;
use otter_vm::{Interpreter, RuntimeState};

fn execute_test262_basic(source: &str, source_url: &str) -> RegisterValue {
    let module = compile_test262_basic_script(source, source_url)
        .expect("test262 basic script should compile on the new VM path");

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
        .expect("test262 basic script should execute on the new VM path")
        .return_value()
}

#[test]
fn array_values_iterator_protocol() {
    let result = execute_test262_basic(
        concat!(
            "var iter = [10, 20, 30].values();\n",
            "var r1 = iter.next();\n",
            "assert.sameValue(r1.value, 10, 'first value');\n",
            "assert.sameValue(r1.done, false, 'first not done');\n",
            "var r2 = iter.next();\n",
            "assert.sameValue(r2.value, 20, 'second value');\n",
            "var r3 = iter.next();\n",
            "assert.sameValue(r3.value, 30, 'third value');\n",
            "var r4 = iter.next();\n",
            "assert.sameValue(r4.value, undefined, 'done value is undefined');\n",
            "assert.sameValue(r4.done, true, 'done flag is true');\n",
        ),
        "iterator-protocol-array-values.js",
    );
    assert_eq!(result, RegisterValue::from_i32(0));
}

#[test]
fn array_keys_iterator_protocol() {
    let result = execute_test262_basic(
        concat!(
            "var keys = [];\n",
            "for (var k of [10, 20, 30].keys()) { keys.push(k); }\n",
            "assert.sameValue(keys.length, 3, 'keys length');\n",
            "assert.sameValue(keys[0], 0, 'first key');\n",
            "assert.sameValue(keys[1], 1, 'second key');\n",
            "assert.sameValue(keys[2], 2, 'third key');\n",
        ),
        "iterator-protocol-array-keys.js",
    );
    assert_eq!(result, RegisterValue::from_i32(0));
}

#[test]
fn array_entries_iterator_protocol() {
    let result = execute_test262_basic(
        concat!(
            "var entries = [];\n",
            "for (var e of [10, 20].entries()) { entries.push(e); }\n",
            "assert.sameValue(entries.length, 2, 'entries length');\n",
            "assert.sameValue(entries[0][0], 0, 'first index');\n",
            "assert.sameValue(entries[0][1], 10, 'first value');\n",
            "assert.sameValue(entries[1][0], 1, 'second index');\n",
            "assert.sameValue(entries[1][1], 20, 'second value');\n",
        ),
        "iterator-protocol-array-entries.js",
    );
    assert_eq!(result, RegisterValue::from_i32(0));
}

#[test]
fn string_iterator_protocol() {
    let result = execute_test262_basic(
        concat!(
            "var iter = 'ab'[Symbol.iterator]();\n",
            "var r1 = iter.next();\n",
            "assert.sameValue(r1.value, 'a', 'first char');\n",
            "assert.sameValue(r1.done, false, 'first not done');\n",
            "var r2 = iter.next();\n",
            "assert.sameValue(r2.value, 'b', 'second char');\n",
            "var r3 = iter.next();\n",
            "assert.sameValue(r3.done, true, 'exhausted');\n",
        ),
        "iterator-protocol-string.js",
    );
    assert_eq!(result, RegisterValue::from_i32(0));
}

#[test]
fn map_iterator_protocol() {
    let result = execute_test262_basic(
        concat!(
            "var m = new Map([['a', 1], ['b', 2]]);\n",
            "var keys = [];\n",
            "for (var k of m.keys()) { keys.push(k); }\n",
            "assert.sameValue(keys[0], 'a', 'first map key');\n",
            "assert.sameValue(keys[1], 'b', 'second map key');\n",
            "var vals = [];\n",
            "for (var v of m.values()) { vals.push(v); }\n",
            "assert.sameValue(vals[0], 1, 'first map value');\n",
            "assert.sameValue(vals[1], 2, 'second map value');\n",
            "var ents = [];\n",
            "for (var e of m) { ents.push(e); }\n",
            "assert.sameValue(ents[0][0], 'a', 'for-of map key');\n",
            "assert.sameValue(ents[0][1], 1, 'for-of map value');\n",
        ),
        "iterator-protocol-map.js",
    );
    assert_eq!(result, RegisterValue::from_i32(0));
}

#[test]
fn set_iterator_protocol() {
    let result = execute_test262_basic(
        concat!(
            "var s = new Set([1, 2, 3]);\n",
            "var vals = [];\n",
            "for (var v of s) { vals.push(v); }\n",
            "assert.sameValue(vals.length, 3, 'set for-of length');\n",
            "assert.sameValue(vals[0], 1, 'first set value');\n",
            "assert.sameValue(vals[2], 3, 'third set value');\n",
            "var entries = [];\n",
            "for (var e of s.entries()) { entries.push(e); }\n",
            "assert.sameValue(entries[0][0], 1, 'set entry key === value');\n",
            "assert.sameValue(entries[0][1], 1, 'set entry value');\n",
        ),
        "iterator-protocol-set.js",
    );
    assert_eq!(result, RegisterValue::from_i32(0));
}

#[test]
fn iterator_prototype_symbol_iterator_returns_this() {
    let result = execute_test262_basic(
        concat!(
            "var iter = [1, 2][Symbol.iterator]();\n",
            "assert.sameValue(iter[Symbol.iterator]() === iter, true, ",
            "'@@iterator returns this');\n",
        ),
        "iterator-protocol-self.js",
    );
    assert_eq!(result, RegisterValue::from_i32(0));
}
