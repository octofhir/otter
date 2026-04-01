//! Integration tests for WeakMap and WeakSet.
//!
//! Spec references:
//! - WeakMap: <https://tc39.es/ecma262/#sec-weakmap-objects>
//! - WeakSet: <https://tc39.es/ecma262/#sec-weakset-objects>

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
fn weakmap_basic_get_set_has_delete() {
    let result = execute_test262_basic(
        concat!(
            "var key1 = {};\n",
            "var key2 = {};\n",
            "var wm = new WeakMap();\n",
            "wm.set(key1, 'hello');\n",
            "wm.set(key2, 42);\n",
            "assert.sameValue(wm.get(key1), 'hello', 'get returns stored value');\n",
            "assert.sameValue(wm.has(key1), true, 'has returns true for existing key');\n",
            "assert.sameValue(wm.has({}), false, 'has returns false for new object');\n",
            "assert.sameValue(wm.delete(key1), true, 'delete returns true');\n",
            "assert.sameValue(wm.has(key1), false, 'has returns false after delete');\n",
            "assert.sameValue(wm.get(key1), undefined, 'get returns undefined after delete');\n",
            "assert.sameValue(wm.get(key2), 42, 'other key unaffected');\n",
        ),
        "weakmap-basic.js",
    );
    assert_eq!(result, RegisterValue::from_i32(0));
}

#[test]
fn weakmap_set_returns_this() {
    let result = execute_test262_basic(
        concat!(
            "var wm = new WeakMap();\n",
            "var key = {};\n",
            "assert.sameValue(wm.set(key, 1) === wm, true, 'set returns this for chaining');\n",
        ),
        "weakmap-set-returns-this.js",
    );
    assert_eq!(result, RegisterValue::from_i32(0));
}

#[test]
fn weakset_basic_add_has_delete() {
    let result = execute_test262_basic(
        concat!(
            "var obj1 = {};\n",
            "var obj2 = {};\n",
            "var ws = new WeakSet();\n",
            "ws.add(obj1);\n",
            "ws.add(obj2);\n",
            "assert.sameValue(ws.has(obj1), true, 'has returns true for added value');\n",
            "assert.sameValue(ws.has({}), false, 'has returns false for new object');\n",
            "assert.sameValue(ws.delete(obj1), true, 'delete returns true');\n",
            "assert.sameValue(ws.has(obj1), false, 'has returns false after delete');\n",
            "assert.sameValue(ws.has(obj2), true, 'other value unaffected');\n",
        ),
        "weakset-basic.js",
    );
    assert_eq!(result, RegisterValue::from_i32(0));
}

#[test]
fn weakset_add_returns_this() {
    let result = execute_test262_basic(
        concat!(
            "var ws = new WeakSet();\n",
            "var obj = {};\n",
            "assert.sameValue(ws.add(obj) === ws, true, 'add returns this for chaining');\n",
        ),
        "weakset-add-returns-this.js",
    );
    assert_eq!(result, RegisterValue::from_i32(0));
}

#[test]
fn weakmap_non_object_key_returns_undefined_for_get() {
    let result = execute_test262_basic(
        concat!(
            "var wm = new WeakMap();\n",
            "assert.sameValue(wm.get(42), undefined, 'non-object key returns undefined');\n",
            "assert.sameValue(wm.has('str'), false, 'non-object key has returns false');\n",
            "assert.sameValue(wm.delete(null), false, 'non-object key delete returns false');\n",
        ),
        "weakmap-non-object-key.js",
    );
    assert_eq!(result, RegisterValue::from_i32(0));
}
