//! Runtime regression coverage for `Function.prototype.call`.
//!
//! # Contents
//! - JS-visible builtin metadata checks.
//! - `call` dispatch with an explicit receiver.
//! - Uncurried builtin methods through `Function.prototype.call.bind`.
//!
//! # Invariants
//! - `Function.prototype.call` is a VM-owned intrinsic, not a host
//!   native function that re-enters the interpreter through
//!   `NativeCtx`.
//! - Native builtin functions expose spec-shaped `name` / `length`
//!   own properties.
//!
//! # See also
//! - <https://tc39.es/ecma262/#sec-function.prototype.call>

use otter_runtime::{Runtime, SourceInput};

fn run(source: &str) -> String {
    let mut rt = Runtime::builder().build().expect("runtime");
    rt.run_script(SourceInput::from_javascript(source), "<test>")
        .expect("script")
        .completion_string()
        .to_string()
}

#[test]
fn function_prototype_call_invokes_callable_with_receiver() {
    let completion = run(r#"
        function f(a, b) {
            return this.prefix + ":" + a + ":" + b;
        }
        f.call({ prefix: "ok" }, "a", "b");
        "#);
    assert_eq!(completion, "ok:a:b");
}

#[test]
fn function_prototype_call_bind_uncurries_builtin_methods() {
    let completion = run(r#"
        const hasOwn = Function.prototype.call.bind(Object.prototype.hasOwnProperty);
        const join = Function.prototype.call.bind(Array.prototype.join);
        hasOwn({ x: 1 }, "x") + ":" + join(["a", "b"], "-");
        "#);
    assert_eq!(completion, "true:a-b");
}

#[test]
fn native_builtin_function_metadata_is_object_visible() {
    let completion = run(r#"
        Function.prototype.call.hasOwnProperty("length") + ":" +
            Function.prototype.call.propertyIsEnumerable("length") + ":" +
            typeof Function.prototype.call.hasOwnProperty + ":" +
            Function.prototype.call.length + ":" +
            Function.prototype.call.name;
        "#);
    assert_eq!(completion, "true:false:function:1:call");
}

#[test]
fn native_builtin_function_metadata_supports_computed_property_access() {
    let completion = run(r#"
        const key = "name";
        const before = Function.prototype.call[key];
        const deleted = delete Function.prototype.call[key];
        before + ":" + deleted + ":" + Function.prototype.call.hasOwnProperty(key);
        "#);
    assert_eq!(completion, "call:true:false");
}

#[test]
fn array_is_array_is_a_capturable_builtin_function() {
    let completion = run(r#"
        const isArray = Array.isArray;
        isArray([]) + ":" + isArray(Function.prototype.call) + ":" +
            typeof isArray + ":" + isArray.length + ":" + isArray.name;
        "#);
    assert_eq!(completion, "true:false:function:1:isArray");
}

#[test]
fn function_prototype_restricted_accessors_throw_type_error() {
    let completion = run(r#"
        const caller = Object.getOwnPropertyDescriptor(Function.prototype, "caller");
        const args = Object.getOwnPropertyDescriptor(Function.prototype, "arguments");
        let readThrows = false;
        let writeThrows = false;
        try {
            Function.prototype.caller;
        } catch (e) {
            readThrows = e instanceof TypeError;
        }
        try {
            Function.prototype.arguments = function() {};
        } catch (e) {
            writeThrows = e instanceof TypeError;
        }
        (typeof caller.get) + ":" + (caller.get === caller.set) + ":" +
            (caller.get === args.get) + ":" + caller.enumerable + ":" +
            caller.configurable + ":" + readThrows + ":" + writeThrows;
        "#);
    assert_eq!(completion, "function:true:true:false:true:true:true");
}

#[test]
fn strict_arguments_object_has_unmapped_descriptor_shape() {
    let completion = run(r#"
        (function(a, b) {
            "use strict";
            const desc = Object.getOwnPropertyDescriptor(arguments, "callee");
            const names = Object.keys(arguments).join(",");
            let calleeThrows = false;
            try {
                arguments.callee;
            } catch (e) {
                calleeThrows = e instanceof TypeError;
            }
            arguments[0] = "changed";
            return Array.isArray(arguments) + ":" + arguments.length + ":" +
                arguments[0] + ":" + a + ":" + names + ":" +
                (typeof desc.get) + ":" + (desc.get === desc.set) + ":" +
                desc.enumerable + ":" + desc.configurable + ":" + calleeThrows;
        })("a", "b");
        "#);
    assert_eq!(
        completion,
        "false:2:changed:a:0,1:function:true:false:false:true"
    );
}

#[test]
fn throw_type_error_intrinsic_has_frozen_builtin_metadata() {
    let completion = run(r#"
        const thrower = Object.getOwnPropertyDescriptor(
            (function() { "use strict"; return arguments; })(),
            "callee"
        ).get;
        const length = Object.getOwnPropertyDescriptor(thrower, "length");
        const name = Object.getOwnPropertyDescriptor(thrower, "name");
        Object.getOwnPropertyNames(thrower).join(",") + ":" +
            name.value + ":" + name.configurable + ":" +
            length.value + ":" + length.configurable + ":" +
            (Object.getPrototypeOf(thrower) === Function.prototype);
        "#);
    assert_eq!(completion, "length,name::false:0:false:true");
}

#[test]
fn object_get_own_property_names_accepts_primitive_values() {
    let completion = run(r#"
        Object.getOwnPropertyNames(true).length + ":" +
            Object.getOwnPropertyNames(1).length + ":" +
            Object.getOwnPropertyNames(Symbol()).length + ":" +
            Object.getOwnPropertyNames("").join(",");
        "#);
    assert_eq!(completion, "0:0:0:length");
}

#[test]
fn native_builtin_function_metadata_is_not_for_in_enumerable() {
    let completion = run(r#"
        let seen = "";
        for (const key in Function.prototype.call) {
            seen += key;
        }
        seen;
        "#);
    assert_eq!(completion, "");
}

#[test]
fn native_builtin_readonly_assignment_has_contextual_type_error() {
    let mut rt = Runtime::builder().build().expect("runtime");
    let err = rt
        .run_script(
            SourceInput::from_javascript(r#"Function.prototype.call.length = "shifted";"#),
            "<test>",
        )
        .expect_err("assignment should reject");
    let message = err.to_string();
    assert!(
        message.contains("Cannot assign to read-only property 'length' of function call"),
        "{message}"
    );
}
