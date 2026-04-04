//! Integration tests for WeakRef and FinalizationRegistry.
//!
//! Spec references:
//! - WeakRef:                §26.1 <https://tc39.es/ecma262/#sec-weak-ref-objects>
//! - FinalizationRegistry:   §26.2 <https://tc39.es/ecma262/#sec-finalization-registry-objects>

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

// ═══════════════════════════════════════════════════════════════════════════
//  WeakRef — §26.1
// ═══════════════════════════════════════════════════════════════════════════

#[test]
fn weakref_constructor_and_deref() {
    let result = execute_test262_basic(
        concat!(
            "var target = { x: 42 };\n",
            "var wr = new WeakRef(target);\n",
            "var d = wr.deref();\n",
            "assert.sameValue(d === target, true, 'deref returns the target');\n",
            "assert.sameValue(d.x, 42, 'target properties accessible via deref');\n",
        ),
        "weakref-basic.js",
    );
    assert_eq!(result, RegisterValue::from_i32(0));
}

#[test]
fn weakref_deref_returns_same_object() {
    let result = execute_test262_basic(
        concat!(
            "var obj = { name: 'test' };\n",
            "var wr = new WeakRef(obj);\n",
            "assert.sameValue(wr.deref() === wr.deref(), true, 'deref returns same object each time');\n",
            "assert.sameValue(wr.deref() === obj, true, 'deref returns original object');\n",
        ),
        "weakref-deref-identity.js",
    );
    assert_eq!(result, RegisterValue::from_i32(0));
}

#[test]
fn weakref_requires_new() {
    let result = execute_test262_basic(
        concat!(
            "var threw = false;\n",
            "try { WeakRef({}); } catch (e) {\n",
            "  threw = e instanceof TypeError;\n",
            "}\n",
            "assert.sameValue(threw, true, 'WeakRef without new throws TypeError');\n",
        ),
        "weakref-requires-new.js",
    );
    assert_eq!(result, RegisterValue::from_i32(0));
}

#[test]
fn weakref_target_must_be_object() {
    let result = execute_test262_basic(
        concat!(
            "var threw = false;\n",
            "try { new WeakRef(42); } catch (e) {\n",
            "  threw = e instanceof TypeError;\n",
            "}\n",
            "assert.sameValue(threw, true, 'non-object target throws TypeError');\n",
        ),
        "weakref-target-object.js",
    );
    assert_eq!(result, RegisterValue::from_i32(0));
}

#[test]
fn weakref_target_must_not_be_undefined() {
    let result = execute_test262_basic(
        concat!(
            "var threw = false;\n",
            "try { new WeakRef(); } catch (e) {\n",
            "  threw = e instanceof TypeError;\n",
            "}\n",
            "assert.sameValue(threw, true, 'no-arg constructor throws TypeError');\n",
        ),
        "weakref-target-undefined.js",
    );
    assert_eq!(result, RegisterValue::from_i32(0));
}

#[test]
fn weakref_typeof_and_instanceof() {
    let result = execute_test262_basic(
        concat!(
            "var wr = new WeakRef({});\n",
            "assert.sameValue(typeof wr, 'object', 'typeof WeakRef instance is object');\n",
            "assert.sameValue(wr instanceof WeakRef, true, 'instanceof WeakRef');\n",
        ),
        "weakref-typeof.js",
    );
    assert_eq!(result, RegisterValue::from_i32(0));
}

#[test]
fn weakref_to_string_tag() {
    let result = execute_test262_basic(
        concat!(
            "var wr = new WeakRef({});\n",
            "assert.sameValue(\n",
            "  Object.prototype.toString.call(wr),\n",
            "  '[object WeakRef]',\n",
            "  '@@toStringTag is WeakRef'\n",
            ");\n",
        ),
        "weakref-tostringtag.js",
    );
    assert_eq!(result, RegisterValue::from_i32(0));
}

#[test]
fn weakref_deref_with_function_target() {
    let result = execute_test262_basic(
        concat!(
            "function foo() { return 99; }\n",
            "var wr = new WeakRef(foo);\n",
            "assert.sameValue(wr.deref() === foo, true, 'deref returns the function');\n",
            "assert.sameValue(wr.deref()(), 99, 'derefed function is callable');\n",
        ),
        "weakref-function-target.js",
    );
    assert_eq!(result, RegisterValue::from_i32(0));
}

// ═══════════════════════════════════════════════════════════════════════════
//  FinalizationRegistry — §26.2
// ═══════════════════════════════════════════════════════════════════════════

#[test]
fn finalization_registry_constructor_requires_new() {
    let result = execute_test262_basic(
        concat!(
            "var threw = false;\n",
            "try { FinalizationRegistry(function() {}); } catch (e) {\n",
            "  threw = e instanceof TypeError;\n",
            "}\n",
            "assert.sameValue(threw, true, 'FinalizationRegistry without new throws TypeError');\n",
        ),
        "fr-requires-new.js",
    );
    assert_eq!(result, RegisterValue::from_i32(0));
}

#[test]
fn finalization_registry_callback_must_be_callable() {
    let result = execute_test262_basic(
        concat!(
            "var threw = false;\n",
            "try { new FinalizationRegistry(42); } catch (e) {\n",
            "  threw = e instanceof TypeError;\n",
            "}\n",
            "assert.sameValue(threw, true, 'non-callable callback throws TypeError');\n",
        ),
        "fr-callback-callable.js",
    );
    assert_eq!(result, RegisterValue::from_i32(0));
}

#[test]
fn finalization_registry_register_basic() {
    let result = execute_test262_basic(
        concat!(
            "var fr = new FinalizationRegistry(function(held) {});\n",
            "var target = {};\n",
            "var result = fr.register(target, 'held-value');\n",
            "assert.sameValue(result, undefined, 'register returns undefined');\n",
        ),
        "fr-register-basic.js",
    );
    assert_eq!(result, RegisterValue::from_i32(0));
}

#[test]
fn finalization_registry_register_target_must_be_object() {
    let result = execute_test262_basic(
        concat!(
            "var fr = new FinalizationRegistry(function(held) {});\n",
            "var threw = false;\n",
            "try { fr.register(42, 'held'); } catch (e) {\n",
            "  threw = e instanceof TypeError;\n",
            "}\n",
            "assert.sameValue(threw, true, 'non-object target throws TypeError');\n",
        ),
        "fr-register-target-object.js",
    );
    assert_eq!(result, RegisterValue::from_i32(0));
}

#[test]
fn finalization_registry_register_same_value_throws() {
    let result = execute_test262_basic(
        concat!(
            "var fr = new FinalizationRegistry(function(held) {});\n",
            "var obj = {};\n",
            "var threw = false;\n",
            "try { fr.register(obj, obj); } catch (e) {\n",
            "  threw = e instanceof TypeError;\n",
            "}\n",
            "assert.sameValue(threw, true, 'target === heldValue throws TypeError');\n",
        ),
        "fr-register-same-value.js",
    );
    assert_eq!(result, RegisterValue::from_i32(0));
}

#[test]
fn finalization_registry_unregister_returns_boolean() {
    let result = execute_test262_basic(
        concat!(
            "var fr = new FinalizationRegistry(function(held) {});\n",
            "var target = {};\n",
            "var token = {};\n",
            "fr.register(target, 'held', token);\n",
            "assert.sameValue(fr.unregister(token), true, 'unregister returns true when cells removed');\n",
            "assert.sameValue(fr.unregister(token), false, 'unregister returns false when no cells to remove');\n",
        ),
        "fr-unregister-boolean.js",
    );
    assert_eq!(result, RegisterValue::from_i32(0));
}

#[test]
fn finalization_registry_unregister_token_must_be_object() {
    let result = execute_test262_basic(
        concat!(
            "var fr = new FinalizationRegistry(function(held) {});\n",
            "var threw = false;\n",
            "try { fr.unregister(42); } catch (e) {\n",
            "  threw = e instanceof TypeError;\n",
            "}\n",
            "assert.sameValue(threw, true, 'non-object token throws TypeError');\n",
        ),
        "fr-unregister-token-object.js",
    );
    assert_eq!(result, RegisterValue::from_i32(0));
}

#[test]
fn finalization_registry_to_string_tag() {
    let result = execute_test262_basic(
        concat!(
            "var fr = new FinalizationRegistry(function() {});\n",
            "assert.sameValue(\n",
            "  Object.prototype.toString.call(fr),\n",
            "  '[object FinalizationRegistry]',\n",
            "  '@@toStringTag is FinalizationRegistry'\n",
            ");\n",
        ),
        "fr-tostringtag.js",
    );
    assert_eq!(result, RegisterValue::from_i32(0));
}

#[test]
fn finalization_registry_register_with_unregister_token() {
    let result = execute_test262_basic(
        concat!(
            "var fr = new FinalizationRegistry(function(held) {});\n",
            "var t1 = {};\n",
            "var t2 = {};\n",
            "var token = {};\n",
            "fr.register(t1, 'a', token);\n",
            "fr.register(t2, 'b', token);\n",
            // Unregister removes both cells registered with the same token
            "assert.sameValue(fr.unregister(token), true, 'unregister removes both registrations');\n",
            "assert.sameValue(fr.unregister(token), false, 'second unregister finds nothing');\n",
        ),
        "fr-unregister-multiple.js",
    );
    assert_eq!(result, RegisterValue::from_i32(0));
}

#[test]
fn finalization_registry_register_different_held_values() {
    let result = execute_test262_basic(
        concat!(
            "var fr = new FinalizationRegistry(function(held) {});\n",
            "var t1 = {};\n",
            "var t2 = {};\n",
            // held values can be any type
            "fr.register(t1, 42);\n",
            "fr.register(t2, 'string-held');\n",
            // no error means success
            "fr.register({}, undefined);\n",
            "fr.register({}, null);\n",
            "fr.register({}, true);\n",
        ),
        "fr-held-value-types.js",
    );
    assert_eq!(result, RegisterValue::from_i32(0));
}

#[test]
fn weakref_multiple_refs_to_same_target() {
    let result = execute_test262_basic(
        concat!(
            "var target = { value: 'shared' };\n",
            "var wr1 = new WeakRef(target);\n",
            "var wr2 = new WeakRef(target);\n",
            "assert.sameValue(wr1.deref() === wr2.deref(), true, 'both refs point to same target');\n",
            "assert.sameValue(wr1.deref().value, 'shared', 'property accessible via wr1');\n",
            "assert.sameValue(wr2.deref().value, 'shared', 'property accessible via wr2');\n",
        ),
        "weakref-multiple-refs.js",
    );
    assert_eq!(result, RegisterValue::from_i32(0));
}

#[test]
fn weakref_constructor_prototype_chain() {
    let result = execute_test262_basic(
        concat!(
            "assert.sameValue(typeof WeakRef, 'function', 'WeakRef is a function');\n",
            "assert.sameValue(typeof WeakRef.prototype, 'object', 'WeakRef.prototype exists');\n",
            "assert.sameValue(typeof WeakRef.prototype.deref, 'function', 'deref is a function');\n",
            "var wr = new WeakRef({});\n",
            "assert.sameValue(\n",
            "  Object.getPrototypeOf(wr) === WeakRef.prototype,\n",
            "  true,\n",
            "  'instance prototype is WeakRef.prototype'\n",
            ");\n",
        ),
        "weakref-prototype-chain.js",
    );
    assert_eq!(result, RegisterValue::from_i32(0));
}

#[test]
fn finalization_registry_constructor_prototype_chain() {
    let result = execute_test262_basic(
        concat!(
            "assert.sameValue(typeof FinalizationRegistry, 'function', 'FR is a function');\n",
            "assert.sameValue(typeof FinalizationRegistry.prototype, 'object', 'FR.prototype exists');\n",
            "assert.sameValue(typeof FinalizationRegistry.prototype.register, 'function', 'register is a function');\n",
            "assert.sameValue(typeof FinalizationRegistry.prototype.unregister, 'function', 'unregister is a function');\n",
            "var fr = new FinalizationRegistry(function() {});\n",
            "assert.sameValue(\n",
            "  Object.getPrototypeOf(fr) === FinalizationRegistry.prototype,\n",
            "  true,\n",
            "  'instance prototype is FinalizationRegistry.prototype'\n",
            ");\n",
        ),
        "fr-prototype-chain.js",
    );
    assert_eq!(result, RegisterValue::from_i32(0));
}
