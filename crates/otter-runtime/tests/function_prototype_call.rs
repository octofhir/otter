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
fn function_prototype_apply_accepts_array_like_objects() {
    let completion = run(r#"
        function f(a, b) {
            return this.prefix + ":" + a + ":" + b;
        }
        f.apply({ prefix: "ok" }, { length: 2, 0: "a", 1: "b" });
        "#);
    assert_eq!(completion, "ok:a:b");
}

#[test]
fn function_prototype_apply_reads_array_like_accessors() {
    let completion = run(r#"
        let reads = 0;
        const args = {
            get length() {
                reads++;
                return 1;
            },
            get 0() {
                reads++;
                return "x";
            }
        };
        function f(value) {
            return value + ":" + reads;
        }
        f.apply(null, args);
        "#);
    assert_eq!(completion, "x:2");
}

#[test]
fn function_prototype_apply_rejects_primitive_argument_list() {
    let completion = run(r#"
        let threw = false;
        try {
            (function() {}).apply(null, 1);
        } catch (e) {
            threw = e instanceof TypeError;
        }
        threw;
        "#);
    assert_eq!(completion, "true");
}

#[test]
fn sloppy_script_top_level_this_is_global_for_apply_legacy_cases() {
    let completion = run(r#"
        Function("this.applyTouched = 1;").apply();
        this.applyTouched + ":" + (this === globalThis);
        "#);
    assert_eq!(completion, "1:true");
}

#[test]
fn function_prototype_apply_boxes_sloppy_primitive_this() {
    let completion = run(r#"
        var obj = 1;
        var retobj = Function("this.touched = true; return this;").apply(obj);
        typeof obj.touched + ":" + retobj.touched + ":" + (retobj instanceof Number);
        "#);
    assert_eq!(completion, "undefined:true:true");
}

#[test]
fn function_constructor_parameters_use_observable_to_string() {
    let completion = run(r#"
        var i = 0;
        var p = {
            toString: function() {
                return "a" + (++i);
            }
        };
        var obj = {};
        Function(p, "a2,a3", "this.shifted = a1;")
            .apply(obj, ["nine", "inch", "nails"]);
        obj.shifted + ":" + i;
        "#);
    assert_eq!(completion, "nine:1");
}

#[test]
fn function_constructor_length_uses_formal_parameter_list() {
    let completion = run(r#"
        var f = new Function("arg1,arg2,arg3", "arg4,arg5", null);
        var before = f.length;
        f.length = function() {};
        f.hasOwnProperty("length") + ":" + before + ":" + f.length;
        "#);
    assert_eq!(completion, "true:5:5");
}

#[test]
fn function_constructor_body_can_use_arguments_object() {
    let completion = run(r#"
        var f = new Function("return arguments[0];");
        f("A");
        "#);
    assert_eq!(completion, "A");
}

#[test]
fn function_constructor_is_callable_through_function_prototype_call() {
    let completion = run(r#"
        var mars = { name: "mars", color: "red", number: 4 };
        var f = Function.call(mars, "this.godname = \"ares\"; return this.color;");
        var about = f();
        String(about) + ":" + this.godname + ":" + mars.godname;
        "#);
    assert_eq!(completion, "undefined:ares:undefined");
}

#[test]
fn function_prototype_has_function_to_string_tag() {
    let completion = run(r#"
        Object.prototype.toString.call(Function.prototype);
        "#);
    assert_eq!(completion, "[object Function]");
}

#[test]
fn function_values_can_be_ordinary_object_prototypes() {
    let completion = run(r#"
        var proto = Function();
        function Factory() {}
        Factory.prototype = proto;
        var obj = new Factory();
        var applyThrows = false;
        try {
            obj.apply();
        } catch (e) {
            applyThrows = e instanceof TypeError;
        }
        typeof obj.apply + ":" + (Object.getPrototypeOf(obj) === proto) + ":" + applyThrows;
        "#);
    assert_eq!(completion, "function:true:true");
}

#[test]
fn function_constructor_returned_functions_preserve_object_metadata() {
    let completion = run(r#"
        var f = Function("return function named(a, b) { this.p1 = 1; }")();
        var obj = new f();
        f.name + ":" +
            f.length + ":" +
            (typeof f.prototype) + ":" +
            (f.prototype.constructor === f) + ":" +
            obj.p1 + ":" +
            (Object.getPrototypeOf(obj) === f.prototype);
        "#);
    assert_eq!(completion, "named:2:object:true:1:true");
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
fn function_constructor_metadata_is_object_visible() {
    let completion = run(r#"
        const length = Object.getOwnPropertyDescriptor(Function, "length");
        const name = Object.getOwnPropertyDescriptor(Function, "name");
        const prototype = Object.getOwnPropertyDescriptor(Function, "prototype");
        Object.getOwnPropertyNames(Function).slice(0, 3).join(",") + ":" +
            Function.hasOwnProperty("length") + ":" +
            length.value + ":" + length.writable + ":" +
            length.enumerable + ":" + length.configurable + ":" +
            name.value + ":" + name.writable + ":" +
            name.enumerable + ":" + name.configurable + ":" +
            prototype.writable + ":" + prototype.enumerable + ":" +
            prototype.configurable;
        "#);
    assert_eq!(
        completion,
        "length,name,prototype:true:1:false:false:true:Function:false:false:true:false:false:false"
    );
}

#[test]
fn function_prototype_is_callable_but_not_constructible() {
    let completion = run(r#"
        const length = Object.getOwnPropertyDescriptor(Function.prototype, "length");
        const name = Object.getOwnPropertyDescriptor(Function.prototype, "name");
        let constructThrows = false;
        try {
            new Function.prototype();
        } catch (e) {
            constructThrows = e instanceof TypeError;
        }
        typeof Function.prototype + ":" +
            String(Function.prototype()) + ":" +
            constructThrows + ":" +
            length.value + ":" + length.writable + ":" +
            length.enumerable + ":" + length.configurable + ":" +
            name.value + ":" + name.writable + ":" +
            name.enumerable + ":" + name.configurable + ":" +
            (Object.getPrototypeOf(Function.prototype) === Object.prototype) + ":" +
            (Function.prototype.constructor === Function);
        "#);
    assert_eq!(
        completion,
        "function:undefined:true:0:false:false:true::false:false:true:true:true"
    );
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
        const bound = (function() {}).bind(null);
        let boundReadThrows = false;
        let boundWriteThrows = false;
        try {
            bound.caller;
        } catch (e) {
            boundReadThrows = e instanceof TypeError;
        }
        try {
            bound.arguments = {};
        } catch (e) {
            boundWriteThrows = e instanceof TypeError;
        }
        (typeof caller.get) + ":" + (caller.get === caller.set) + ":" +
            (caller.get === args.get) + ":" + caller.enumerable + ":" +
            caller.configurable + ":" + readThrows + ":" + writeThrows + ":" +
            bound.hasOwnProperty("caller") + ":" + bound.hasOwnProperty("arguments") + ":" +
            boundReadThrows + ":" + boundWriteThrows;
        "#);
    assert_eq!(
        completion,
        "function:true:true:false:true:true:true:false:false:true:true"
    );
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
fn strict_arguments_object_remains_unmapped() {
    let completion = run(r#"
        function f(a) {
            "use strict";
            arguments[0] = 2;
            return a;
        }
        f(1);
        "#);
    assert_eq!(completion, "1");
}

#[test]
fn sloppy_simple_arguments_write_updates_parameter_binding() {
    let completion = run(r#"
        function f(a) {
            arguments[0] = 2;
            return a;
        }
        f(1);
        "#);
    assert_eq!(completion, "2");
}

#[test]
fn sloppy_simple_parameter_write_updates_arguments_index() {
    let completion = run(r#"
        function f(a) {
            a = 3;
            return arguments[0];
        }
        f(1);
        "#);
    assert_eq!(completion, "3");
}

#[test]
fn mapped_arguments_index_descriptor_reflects_parameter_value() {
    let completion = run(r#"
        function f(a) {
            a = 7;
            const desc = Object.getOwnPropertyDescriptor(arguments, "0");
            return desc.value + ":" + desc.writable + ":" +
                desc.enumerable + ":" + desc.configurable;
        }
        f(1);
        "#);
    assert_eq!(completion, "7:true:true:true");
}

#[test]
fn deleting_mapped_arguments_index_breaks_parameter_alias() {
    let completion = run(r#"
        function f(a) {
            delete arguments[0];
            a = 3;
            return String(arguments[0]);
        }
        f(1);
        "#);
    assert_eq!(completion, "undefined");
}

#[test]
fn define_property_on_mapped_arguments_index_breaks_parameter_alias() {
    let completion = run(r#"
        function f(a) {
            Object.defineProperty(arguments, "0", {
                value: 2,
                writable: false,
                enumerable: true,
                configurable: true,
            });
            a = 3;
            return arguments[0] + ":" + a;
        }
        f(1);
        "#);
    assert_eq!(completion, "2:3");
}

#[test]
fn sloppy_mapped_arguments_callee_is_configurable_data_property() {
    let completion = run(r#"
        function f() {
            const desc = Object.getOwnPropertyDescriptor(arguments, "callee");
            return (typeof desc.value) + ":" + desc.writable + ":" +
                desc.enumerable + ":" + desc.configurable + ":" +
                (delete arguments.callee) + ":" +
                arguments.hasOwnProperty("callee");
        }
        f();
        "#);
    assert_eq!(completion, "function:true:false:true:true:false");
}

#[test]
fn duplicate_sloppy_parameters_map_arguments_to_last_occurrence() {
    let completion = run(r#"
        function f(a, a) {
            arguments[1] = 5;
            return a;
        }
        f(1, 2);
        "#);
    assert_eq!(completion, "5");
}

#[test]
fn non_simple_parameters_use_unmapped_arguments_object() {
    let completion = run(r#"
        function f(a = 1) {
            arguments[0] = 2;
            return a;
        }
        f(undefined);
        "#);
    assert_eq!(completion, "1");
}

#[test]
fn function_constructor_allows_duplicate_parameters_for_sloppy_body() {
    let completion = run(r#"
        "use strict";
        Function("a", "a", "return a;")(1, 2);
        "#);
    assert_eq!(completion, "2");
}

#[test]
fn function_strictness_controls_ordinary_this_binding() {
    let completion = run(r#"
        Function("return this === globalThis;")() + ":" +
            Function("\"use strict\"; return this === undefined;")() + ":" +
            Function("return typeof this + ':' + (this instanceof Number);").call(7) + ":" +
            Function("return typeof this + ':' + (this instanceof Boolean);").call(false) + ":" +
            Function("return typeof this + ':' + (this instanceof String);").call("x") + ":" +
            Function("\"use strict\"; return typeof this + ':' + this;").call(7);
        "#);
    assert_eq!(
        completion,
        "true:true:object:true:object:true:object:true:number:7"
    );
}

#[test]
fn function_constructor_strict_body_duplicate_parameters_throw_syntax_error() {
    let completion = run(r#"
        let caught = false;
        try {
            Function("a", "a", "\"use strict\"; return a;");
        } catch (e) {
            caught = e instanceof SyntaxError;
        }
        caught;
        "#);
    assert_eq!(completion, "true");
}

#[test]
fn direct_eval_inherits_caller_strictness_for_early_errors() {
    let completion = run(r#"
        (function() {
            "use strict";
            try {
                eval("function f(a, a) { return a; }");
                return false;
            } catch (e) {
                return e instanceof SyntaxError;
            }
        })();
        "#);
    assert_eq!(completion, "true");
}

#[test]
fn strict_delete_identifier_from_dynamic_function_is_syntax_error() {
    let completion = run(r#"
        let caught = false;
        try {
            Function("\"use strict\"; delete x;");
        } catch (e) {
            caught = e instanceof SyntaxError;
        }
        caught;
        "#);
    assert_eq!(completion, "true");
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
            SourceInput::from_javascript(
                r#""use strict"; Function.prototype.call.length = "shifted";"#,
            ),
            "<test>",
        )
        .expect_err("assignment should reject");
    let message = err.to_string();
    assert!(
        message.contains("Cannot assign to read-only property 'length' of function call"),
        "{message}"
    );
}

#[test]
fn sloppy_readonly_assignment_noops_and_strict_assignment_throws() {
    let completion = run(r#"
        function target() {}
        const before = Function.prototype.call.length;
        Function.prototype.call.length = 99;
        target.length = 42;
        let strictThrow = false;
        try {
            Function('"use strict"; Function.prototype.call.length = 99;')();
        } catch (e) {
            strictThrow = e instanceof TypeError;
        }
        before + ":" + Function.prototype.call.length + ":" +
            target.length + ":" + strictThrow;
        "#);
    assert_eq!(completion, "1:1:0:true");
}

#[test]
fn sloppy_primitive_property_assignment_noops_but_strict_throws() {
    let completion = run(r#"
        let sloppyOk = false;
        let strictThrow = false;
        try {
            (1).x = 2;
            "s".x = 3;
            sloppyOk = true;
        } catch (e) {}
        try {
            Function('"use strict"; (1).x = 2;')();
        } catch (e) {
            strictThrow = e instanceof TypeError;
        }
        sloppyOk + ":" + strictThrow;
        "#);
    assert_eq!(completion, "true:true");
}

#[test]
fn bound_function_metadata_composes_from_target_name_and_length() {
    let completion = run(r#"
        function target(a, b, c) {}
        Object.defineProperty(target, "name", { value: "renamed" });
        const once = target.bind(null, 1);
        const twice = once.bind(null, 2);
        const uncurried = Function.prototype.call.bind(Object.prototype.hasOwnProperty);
        once.name + ":" + once.length + ":" +
            twice.name + ":" + twice.length + ":" +
            uncurried.name + ":" + uncurried.length;
        "#);
    assert_eq!(
        completion,
        "bound renamed:2:bound bound renamed:1:bound call:1"
    );
}

#[test]
fn bound_function_metadata_has_configurable_descriptor_shape() {
    let completion = run(r#"
        function target(a) {}
        const bound = target.bind(null);
        const length = Object.getOwnPropertyDescriptor(bound, "length");
        const name = Object.getOwnPropertyDescriptor(bound, "name");
        const deleted = delete bound.name;
        length.value + ":" + length.writable + ":" + length.enumerable + ":" +
            length.configurable + ":" + name.value + ":" + name.configurable + ":" +
            deleted + ":" + Object.prototype.hasOwnProperty.call(bound, "name") + ":" +
            Object.getOwnPropertyNames(bound).join(",");
        "#);
    assert_eq!(
        completion,
        "1:false:false:true:bound target:true:true:false:length"
    );
}

#[test]
fn bound_function_builtin_metadata_is_not_for_in_enumerable() {
    let completion = run(r#"
        function target(a) {}
        const bound = target.bind(null);
        let seen = "";
        for (const key in bound) {
            seen += key;
        }
        seen + ":" + Object.keys(bound).join(",") + ":" +
            bound.propertyIsEnumerable("name");
        "#);
    assert_eq!(completion, "::false");
}

#[test]
fn bound_function_overridden_metadata_enumerability_is_observable() {
    let completion = run(r#"
        function target(a) {}
        const bound = target.bind(null);
        Object.defineProperty(bound, "name", {
            value: "visible",
            enumerable: true,
            configurable: true,
        });
        Object.defineProperty(bound, "length", {
            value: 0,
            enumerable: true,
            configurable: true,
        });
        const keys = [];
        for (const key in bound) {
            keys.push(key);
        }
        keys.join(",") + ":" + Object.keys(bound).join(",") + ":" +
            bound.propertyIsEnumerable("name");
    "#);
    assert_eq!(completion, "length,name:length,name:true");
}

#[test]
fn bound_function_expando_properties_are_ordinary_own_properties() {
    let completion = run(r#"
        function target(a) {}
        const bound = target.bind(null);
        bound.extra = 12;
        Object.defineProperty(bound, "hidden", {
            value: 99,
            enumerable: false,
            configurable: true,
        });
        Object.defineProperty(bound, "accessor", {
            get: function() { return this.extra + 1; },
            set: function(value) { this.extra = value; },
            enumerable: true,
            configurable: true,
        });
        const read = bound.accessor;
        bound.accessor = 40;
        const keys = [];
        for (const key in bound) {
            keys.push(key);
        }
        read + ":" + bound.extra + ":" + bound.hasOwnProperty("extra") + ":" +
            bound.propertyIsEnumerable("extra") + ":" +
            bound.propertyIsEnumerable("hidden") + ":" +
            keys.join(",") + ":" + Object.getOwnPropertyNames(bound).join(",");
    "#);
    assert_eq!(
        completion,
        "13:40:true:true:false:extra,accessor:length,name,extra,hidden,accessor"
    );
}

#[test]
fn bound_function_construct_ignores_bound_this_and_preserves_object_return() {
    let completion = run(r#"
        Object.prototype.verifyThis = "verifyThis";
        const boundThis = { verifyThis: "wrong" };
        function Target() {
            return new Boolean(
                arguments.length === 0 &&
                Object.prototype.toString.call(this) === "[object Object]" &&
                this.verifyThis === "verifyThis"
            );
        }
        const Bound = Target.bind(boundThis);
        const value = new Bound();
        value.valueOf() + ":" + (value instanceof Boolean);
    "#);
    assert_eq!(completion, "true:true");
}

#[test]
fn builtin_constructor_objects_bind_through_function_prototype() {
    let completion = run(r#"
        const BoundObject = Object.bind(null);
        const BoundNumber = Number.bind(null);
        const BoundBoolean = Boolean.bind(null);
        const BoundString = String.bind(null);
        const BoundDate = Date.bind(null);
        (BoundObject(42) == 42) + ":" + BoundNumber(42) + ":" +
            BoundBoolean(1) + ":" + BoundString("ok") + ":" +
            typeof BoundDate(0, 0, 0) + ":" +
            BoundNumber.name + ":" + BoundNumber.length;
        "#);
    assert_eq!(completion, "true:42:true:ok:string:bound Number:1");
}

#[test]
fn callable_values_expose_function_prototype_chain() {
    let completion = run(r#"
        function target() {}
        const bound = target.bind(null);
        const native = Function.prototype.call;
        [
            Function.prototype.isPrototypeOf(target),
            Function.prototype.isPrototypeOf(bound),
            Function.prototype.isPrototypeOf(native),
            Function.prototype.isPrototypeOf(Number),
            Object.prototype.isPrototypeOf(bound),
            Object.prototype.isPrototypeOf(Number),
            Object.getPrototypeOf(bound) === Function.prototype,
            Object.getPrototypeOf(Number) === Function.prototype,
            Object.getPrototypeOf(Function) === Function.prototype,
            Object.getPrototypeOf(Function.prototype) === Object.prototype,
        ].join(":");
        "#);
    assert_eq!(
        completion,
        "true:true:true:true:true:true:true:true:true:true"
    );
}

#[test]
fn function_bind_observes_target_metadata_getters() {
    let completion = run(r#"
        const target = function(a, b) {};
        let nameReads = 0;
        let lengthReads = 0;
        Object.defineProperty(target, "name", {
            get: function() { nameReads += 1; return "dynamic"; },
            configurable: true
        });
        Object.defineProperty(target, "length", {
            get: function() { lengthReads += 1; return 9; },
            configurable: true
        });
        const bound = target.bind(null, 1, 2);
        let throws = false;
        Object.defineProperty(target, "name", {
            get: function() { throw new TypeError("name"); },
            configurable: true
        });
        try {
            target.bind(null);
        } catch (e) {
            throws = e instanceof TypeError;
        }
        bound.name + ":" + bound.length + ":" + nameReads + ":" +
            lengthReads + ":" + throws;
        "#);
    assert_eq!(completion, "bound dynamic:7:1:1:true");
}

#[test]
fn user_function_prototype_constructor_links_instances() {
    let completion = run(r#"
        function Target() {}
        const proto = Target.prototype;
        const instance = new Target();
        (proto.constructor === Target) + ":" +
            Object.prototype.hasOwnProperty.call(proto, "constructor") + ":" +
            proto.propertyIsEnumerable("constructor") + ":" +
            (instance.constructor === Target);
        "#);
    assert_eq!(completion, "true:true:false:true");
}

#[test]
fn bound_construct_rewrites_new_target_through_reflect_construct() {
    let completion = run(r#"
        let observed;
        function Target() {
            observed = new.target;
        }
        const Bound = Target.bind(null);
        const BoundBound = Bound.bind(null);
        const instance = Reflect.construct(BoundBound, [], Target);
        (observed === Target) + ":" +
            (Object.getPrototypeOf(instance) === Target.prototype);
        "#);
    assert_eq!(completion, "true:true");
}

#[test]
fn bound_builtin_constructor_preserves_object_like_return() {
    let completion = run(r#"
        function construct(f, args) {
            const bound = Function.prototype.bind.apply(f, [null].concat(args));
            return new bound();
        }
        Object.prototype.toString.call(construct(Date, [1957, 4, 27]));
        "#);
    assert_eq!(completion, "[object Date]");
}

#[test]
fn primitive_wrapper_internal_slots_are_not_reflected_as_properties() {
    let completion = run(r#"
        const bool = new Boolean(true);
        const num = new Number(7);
        bool.valueOf() + ":" + num.valueOf() + ":" +
            Object.getOwnPropertyNames(bool).join(",") + ":" +
            Object.getOwnPropertyNames(num).join(",") + ":" +
            Object.getOwnPropertyNames(Boolean).indexOf("__construct__") + ":" +
            Object.getOwnPropertyNames(Object).indexOf("__construct__") + ":" +
            Object.getOwnPropertyNames(Date).indexOf("__construct__");
        "#);
    assert_eq!(completion, "true:7:::-1:-1:-1");
}

#[test]
fn ordinary_function_metadata_redefinition_preserves_builtin_configurable_shape() {
    let completion = run(r#"
        function target(a, b) {}
        Object.defineProperty(target, "length", { value: undefined });
        const first = Object.getOwnPropertyDescriptor(target, "length");
        Object.defineProperty(target, "length", { value: null });
        const second = Object.getOwnPropertyDescriptor(target, "length");
        first.value + ":" + first.writable + ":" + first.enumerable + ":" +
            first.configurable + ":" + second.value + ":" + target.bind(null, 1).length;
        "#);
    assert_eq!(completion, "undefined:false:false:true:null:0");
}

#[test]
fn ordinary_function_metadata_delete_removes_virtual_own_property() {
    let completion = run(r#"
        function target(a, b) {}
        const before = target.hasOwnProperty("length");
        const deleted = delete target.length;
        const after = target.hasOwnProperty("length");
        const desc = Object.getOwnPropertyDescriptor(target, "length");
        const bound = Function.prototype.bind.call(target, null, 1);
        before + ":" + deleted + ":" + after + ":" + desc + ":" + bound.length;
        "#);
    assert_eq!(completion, "true:true:false:undefined:0");
}
