//! Runtime regression coverage for ordinary object descriptor enforcement.
//!
//! # Contents
//! - Computed property assignment through the `[[Set]]` data/reject path.
//!
//! # Invariants
//! - User-visible ordinary object writes do not bypass non-extensible
//!   descriptor checks.
//! - Descriptor-preserving data assignment remains centralized in
//!   `otter-vm::object`.

use otter_runtime::{Runtime, SourceInput};

fn run(source: &str) -> String {
    let mut rt = Runtime::builder().build().expect("runtime");
    rt.run_script(SourceInput::from_javascript(source), "<test>")
        .expect("script")
        .completion_string()
        .to_string()
}

#[test]
fn computed_assignment_preserves_data_descriptor_attrs() {
    let completion = run(r#"
        const o = {};
        Object.defineProperty(o, "x", {
            value: 1,
            writable: true,
            enumerable: false,
            configurable: false,
        });
        const key = "x";
        o[key] = 2;
        const desc = Object.getOwnPropertyDescriptor(o, "x");
        o.x + ":" + desc.enumerable + ":" + desc.configurable;
        "#);
    assert_eq!(completion, "2:false:false");
}

#[test]
fn computed_assignment_rejects_new_key_on_non_extensible_object() {
    let mut rt = Runtime::builder().build().expect("runtime");
    let err = rt
        .run_script(
            SourceInput::from_javascript(
                r#"
                "use strict";
                const o = {};
                Object.preventExtensions(o);
                const key = "x";
                o[key] = 1;
                Object.hasOwn(o, key);
                "#,
            ),
            "<test>",
        )
        .expect_err("computed assignment should reject");
    let message = err.to_string();
    assert!(
        message.contains("Cannot assign to property 'x'") || message.contains("TypeError"),
        "{message}"
    );
}

#[test]
fn computed_assignment_invokes_setter() {
    let completion = run(r#"
        let seen = false;
        const o = {};
        Object.defineProperty(o, "x", {
            set: function(value) {
                seen = this === o && value === 9;
            },
            configurable: true,
        });
        const key = "x";
        o[key] = 9;
        seen;
        "#);
    assert_eq!(completion, "true");
}

#[test]
fn deleting_missing_symbol_key_succeeds() {
    let completion = run(r#"
        const o = {};
        const sym = Symbol("missing");
        delete o[sym];
        "#);
    assert_eq!(completion, "true");
}

#[test]
fn symbol_assignment_rejects_new_key_on_non_extensible_object() {
    let mut rt = Runtime::builder().build().expect("runtime");
    let err = rt
        .run_script(
            SourceInput::from_javascript(
                r#"
                "use strict";
                const o = {};
                const sym = Symbol("x");
                Object.preventExtensions(o);
                o[sym] = 1;
                "#,
            ),
            "<test>",
        )
        .expect_err("symbol assignment should reject");
    let message = err.to_string();
    assert!(
        message.contains("Cannot assign to read-only property")
            || message.contains("Cannot assign to symbol property")
            || message.contains("TypeError"),
        "{message}"
    );
}

#[test]
fn symbol_define_property_descriptor_is_enforced() {
    let mut rt = Runtime::builder().build().expect("runtime");
    let err = rt
        .run_script(
            SourceInput::from_javascript(
                r#"
                "use strict";
                const o = {};
                const sym = Symbol("x");
                Object.defineProperty(o, sym, {
                    value: 1,
                    writable: false,
                    enumerable: true,
                    configurable: false,
                });
                o[sym] = 2;
                "#,
            ),
            "<test>",
        )
        .expect_err("non-writable symbol descriptor should reject");
    let message = err.to_string();
    assert!(
        message.contains("Cannot assign to read-only property") || message.contains("TypeError"),
        "{message}"
    );
}

#[test]
fn extracted_object_descriptor_helpers_preserve_native_surface() {
    let completion = run(r#"
        const getDesc = Object.getOwnPropertyDescriptor;
        const getDescs = Object.getOwnPropertyDescriptors;
        const create = Object.create;
        const entries = Object.entries;
        const props = {
            answer: { value: 42, enumerable: true, configurable: true }
        };
        const proto = { inherited: true };
        const obj = create(proto, props);
        const one = getDesc(obj, "answer");
        const all = getDescs(obj).answer;
        const listed = entries(obj)[0];
        (Object.getPrototypeOf(obj) === proto) + ":" +
            one.value + ":" + all.configurable + ":" +
            listed[0] + ":" + listed[1];
    "#);
    assert_eq!(completion, "true:42:true:answer:42");
}

#[test]
fn extracted_object_property_names_preserve_proxy_surface() {
    let completion = run(r#"
        const getOwnPropertyNames = Object.getOwnPropertyNames;
        const target = {};
        Object.defineProperty(target, "answer", {
            value: 42,
            enumerable: false,
            configurable: true,
        });
        const proxy = new Proxy(target, {});
        getOwnPropertyNames(proxy).join(",");
    "#);
    assert_eq!(completion, "answer");
}

#[test]
fn computed_symbol_assignment_invokes_setter() {
    let completion = run(r#"
        let seen = false;
        const o = {};
        const sym = Symbol("x");
        Object.defineProperty(o, sym, {
            set: function(value) {
                seen = this === o && value === 5;
            },
            configurable: true,
        });
        o[sym] = 5;
        seen;
        "#);
    assert_eq!(completion, "true");
}

#[test]
fn computed_assignment_reads_string_getter() {
    let completion = run(r#"
        const o = {};
        Object.defineProperty(o, "x", {
            get: function() {
                return this === o ? 11 : 0;
            },
            configurable: true,
        });
        const key = "x";
        o[key];
        "#);
    assert_eq!(completion, "11");
}

#[test]
fn computed_assignment_reads_symbol_getter() {
    let completion = run(r#"
        const o = {};
        const sym = Symbol("x");
        Object.defineProperty(o, sym, {
            get: function() {
                return this === o ? 13 : 0;
            },
            configurable: true,
        });
        o[sym];
        "#);
    assert_eq!(completion, "13");
}

#[test]
fn proxy_get_fallback_invokes_getter_with_proxy_receiver() {
    let completion = run(r#"
        let proxy;
        const target = {};
        Object.defineProperty(target, "x", {
            get: function() {
                return this === proxy ? 17 : 0;
            },
            configurable: true,
        });
        proxy = new Proxy(target, {});
        const key = "x";
        proxy[key];
    "#);
    assert_eq!(completion, "17");
}

#[test]
fn proxy_set_fallback_honors_array_extensibility() {
    let completion = run(r#"
        const target = [1, 2, 3];
        const proxy = new Proxy(new Proxy(target, {}), { set: null });
        proxy.length = 0;
        const emptied = target.length === 0;
        Object.preventExtensions(target);
        emptied + ":" + Reflect.set(proxy, "foo", 2);
        "#);
    assert_eq!(completion, "true:false");
}

#[test]
fn proxy_set_fallback_honors_regexp_readonly_accessors() {
    let completion = run(r#"
        const target = /(?:)/g;
        const proxy = new Proxy(new Proxy(target, {}), {});
        const globalResult = Reflect.set(proxy, "global", true);
        proxy.lastIndex = 7;
        globalResult + ":" + target.lastIndex;
        "#);
    assert_eq!(completion, "false:7");
}

#[test]
fn array_has_uses_actual_prototype_and_array_prototype_length() {
    let completion = run(r#"
        const target = Object.create(Array.prototype);
        const proxy = new Proxy(target, {});
        const proto = [14];
        const holder = Object.create(proto);
        const handler = { has(target, prop) { return false; } };
        const parent = new Proxy(holder, handler);
        const array = [];
        Object.setPrototypeOf(array, parent);
        ("length" in proxy) + ":" + (1 in array);
        "#);
    assert_eq!(completion, "true:false");
}

#[test]
fn class_static_define_property_setter_uses_class_receiver() {
    let completion = run(r#"
        let seen = false;
        class C {}
        Object.defineProperty(C, "x", {
            set: function(value) {
                seen = this === C && value === 4;
            },
            configurable: true,
        });
        C.x = 4;
        seen;
        "#);
    assert_eq!(completion, "true");
}

#[test]
fn class_static_descriptor_is_visible() {
    let completion = run(r#"
        class C {}
        Object.defineProperty(C, "x", {
            value: 6,
            writable: true,
            enumerable: false,
            configurable: false,
        });
        const desc = Object.getOwnPropertyDescriptor(C, "x");
        C.x + ":" + desc.enumerable + ":" + desc.configurable;
        "#);
    assert_eq!(completion, "6:false:false");
}

#[test]
fn computed_class_static_getter_and_setter_use_class_receiver() {
    let completion = run(r#"
        let seen = false;
        class C {}
        Object.defineProperty(C, "x", {
            get: function() {
                return this === C ? 3 : 0;
            },
            set: function(value) {
                seen = this === C && value === 8;
            },
            configurable: true,
        });
        const key = "x";
        const got = C[key];
        C[key] = 8;
        got + ":" + seen;
        "#);
    assert_eq!(completion, "3:true");
}

#[test]
fn native_function_non_configurable_metadata_rejects_redefinition() {
    let mut rt = Runtime::builder().build().expect("runtime");
    let err = rt
        .run_script(
            SourceInput::from_javascript(
                r#"
                const thrower = Object.getOwnPropertyDescriptor(
                    (function() { "use strict"; return arguments; })(),
                    "callee"
                ).get;
                Object.defineProperty(thrower, "name", { value: "changed" });
                "#,
            ),
            "<test>",
        )
        .expect_err("non-configurable native metadata should reject");
    let message = err.to_string();
    assert!(
        message.contains("Cannot define property") || message.contains("TypeMismatch"),
        "{message}"
    );
}

#[test]
fn user_function_define_property_uses_descriptor_bag() {
    let completion = run(r#"
        function f() {}
        Object.defineProperty(f, "x", {
            get: function() {
                return this === f ? 23 : 0;
            },
            enumerable: true,
            configurable: true,
        });
        const desc = Object.getOwnPropertyDescriptor(f, "x");
        f.x + ":" + Object.hasOwn(f, "x") + ":" + desc.enumerable;
        "#);
    assert_eq!(completion, "23:true:true");
}

#[test]
fn user_function_define_symbol_property_uses_descriptor_bag() {
    let completion = run(r#"
        function f() {}
        const sym = Symbol("x");
        Object.defineProperty(f, sym, {
            value: 29,
            writable: true,
            enumerable: false,
            configurable: true,
        });
        const desc = Object.getOwnPropertyDescriptor(f, sym);
        f[sym] + ":" + Object.hasOwn(f, sym) + ":" + desc.enumerable;
    "#);
    assert_eq!(completion, "29:true:false");
}

#[test]
fn inferred_object_method_function_name_does_not_shadow_outer_binding() {
    let completion = run(r#"
        const ownKeys = ["a"];
        const handler = {
            ownKeys: function() {
                return ownKeys;
            }
        };
        const sameBinding = handler.ownKeys() === ownKeys;
        sameBinding + ":" + handler.ownKeys.name;
        "#);
    assert_eq!(completion, "true:ownKeys");
}

#[test]
fn object_get_own_property_descriptors_skips_proxy_undefined_descriptor() {
    let completion = run(r#"
        const proxy = new Proxy({ a: 1 }, {
            ownKeys: function() {
                return ["a"];
            },
            getOwnPropertyDescriptor: function() {
                return undefined;
            }
        });
        Object.keys(Object.getOwnPropertyDescriptors(proxy)).length;
        "#);
    assert_eq!(completion, "0");
}

#[test]
fn bound_function_has_and_for_in_walk_function_prototype() {
    let completion = run(r#"
        function f() {}
        Object.defineProperty(Function.prototype, "prop", {
            value: 1001,
            writable: true,
            enumerable: true,
            configurable: true,
        });
        const bound = f.bind({});
        const keys = [];
        for (const key in bound) {
            keys.push(key);
        }
        const present = "prop" in bound;
        const own = bound.hasOwnProperty("prop");
        delete Function.prototype.prop;
        present + ":" + own + ":" + keys.join(",");
        "#);
    assert_eq!(completion, "true:false:prop");
}

#[test]
fn native_function_computed_get_walks_object_prototype_accessors() {
    let completion = run(r#"
        Object.defineProperty(Object.prototype, "1", {
            get() { return 6.99; },
            configurable: true,
        });
        try {
            Array[1];
        } finally {
            delete Object.prototype[1];
        }
        "#);
    assert_eq!(completion, "6.99");
}

#[test]
fn array_filter_observes_prototype_indices_added_mid_iteration() {
    let completion = run(r#"
        const obj = { length: 2 };
        Object.defineProperty(obj, "0", {
            get() {
                Object.defineProperty(Object.prototype, "1", {
                    get() { return 6.99; },
                    configurable: true,
                });
                return 0;
            },
            configurable: true,
        });
        try {
            const filtered = Array.prototype.filter.call(obj, () => true);
            filtered.length + "|" + filtered[0] + "|" + filtered[1] + "|" + Array[1];
        } finally {
            delete Object.prototype[1];
        }
        "#);
    assert_eq!(completion, "2|0|6.99|6.99");
}

#[test]
fn extracted_object_define_property_routes_user_function_bag() {
    let completion = run(r#"
        function f() {}
        const define = Object.defineProperty;
        const hasOwn = Object.hasOwn;
        define(f, "x", {
            value: 31,
            writable: true,
            enumerable: true,
            configurable: true,
        });
        f.x + ":" + hasOwn(f, "x");
        "#);
    assert_eq!(completion, "31:true");
}

#[test]
fn non_configurable_symbol_delete_returns_false() {
    let completion = run(r#"
        const o = {};
        const sym = Symbol("x");
        Object.defineProperty(o, sym, {
            value: 1,
            configurable: false,
        });
        delete o[sym];
        "#);
    assert_eq!(completion, "false");
}

#[test]
fn object_keys_use_integer_index_then_string_order() {
    let completion = run(r#"
        const o = {};
        o.b = 1;
        o[10] = 1;
        o[2] = 1;
        o.a = 1;
        o[1] = 1;
        o["01"] = 1;
        Object.keys(o).join(",");
        "#);
    assert_eq!(completion, "1,2,10,b,a,01");
}

#[test]
fn reflect_own_keys_appends_symbols_after_ordered_strings() {
    let completion = run(r#"
        const sym = Symbol("s");
        const o = {};
        o.b = 1;
        o[2] = 1;
        o[sym] = 1;
        o.a = 1;
        o[1] = 1;
        const keys = Reflect.ownKeys(o);
        keys[0] + ":" + keys[1] + ":" + keys[2] + ":" + keys[3] + ":" + (keys[4] === sym);
        "#);
    assert_eq!(completion, "1:2:b:a:true");
}

#[test]
fn object_freeze_set_freezes_properties_not_entries() {
    let completion = run(r#"
        const set = Object.freeze(new Set([1]));
        set.add(2);
        Object.isFrozen(set) + ":" + [...set].join(",");
        "#);
    assert_eq!(completion, "true:1,2");
}

#[test]
fn proxy_set_fallback_rejects_non_extensible_target() {
    let mut rt = Runtime::builder().build().expect("runtime");
    let err = rt
        .run_script(
            SourceInput::from_javascript(
                r#"
                "use strict";
                const target = {};
                Object.preventExtensions(target);
                const proxy = new Proxy(target, {});
                proxy.x = 1;
                "#,
            ),
            "<test>",
        )
        .expect_err("proxy fallback set should reject");
    let message = err.to_string();
    assert!(
        message.contains("Cannot assign to property") || message.contains("TypeError"),
        "{message}"
    );
}

#[test]
fn proxy_set_fallback_invokes_setter_with_proxy_receiver() {
    let completion = run(r#"
        let proxy;
        let receiverWasProxy = false;
        const target = {};
        Object.defineProperty(target, "x", {
            set: function(value) {
                receiverWasProxy = this === proxy && value === 7;
            },
            configurable: true,
        });
        proxy = new Proxy(target, {});
        proxy.x = 7;
        receiverWasProxy;
        "#);
    assert_eq!(completion, "true");
}

#[test]
fn proxy_prototype_get_trap_uses_original_receiver() {
    let completion = run(r#"
        const proto = new Proxy({}, {
            get: function(target, key, receiver) {
                return key === "x" && receiver.marker === "recv" ? "hit" : "miss";
            }
        });
        const o = { marker: "recv" };
        Object.setPrototypeOf(o, proto);
        o.x;
        "#);
    assert_eq!(completion, "hit");
}

#[test]
fn proxy_prototype_get_fallback_reads_target() {
    let completion = run(r#"
        const target = { x: 41 };
        const proto = new Proxy(target, {});
        const o = {};
        Object.setPrototypeOf(o, proto);
        o.x + 1;
        "#);
    assert_eq!(completion, "42");
}

#[test]
fn proxy_prototype_has_trap_participates_in_in_operator() {
    let completion = run(r#"
        let seen = "";
        const proto = new Proxy({}, {
            has: function(target, key) {
                seen = key;
                return key === "x";
            }
        });
        const o = {};
        Object.setPrototypeOf(o, proto);
        ("x" in o) + ":" + seen;
        "#);
    assert_eq!(completion, "true:x");
}

#[test]
fn proxy_prototype_method_call_uses_get_trap_receiver() {
    let completion = run(r#"
        const proto = new Proxy({}, {
            get: function(target, key, receiver) {
                if (key !== "m") {
                    return undefined;
                }
                return function() {
                    return this === receiver && this.marker === 5 ? "ok" : "bad";
                };
            }
        });
        const o = { marker: 5 };
        Object.setPrototypeOf(o, proto);
        o.m();
        "#);
    assert_eq!(completion, "ok");
}

#[test]
fn object_literal_getter_installs_accessor_descriptor() {
    let completion = run(r#"
        const o = {
            marker: 9,
            get x() {
                return this.marker + 1;
            }
        };
        const desc = Object.getOwnPropertyDescriptor(o, "x");
        o.x + ":" + (typeof desc.get) + ":" + desc.enumerable + ":" + desc.configurable;
        "#);
    assert_eq!(completion, "10:function:true:true");
}

#[test]
fn proxy_get_fallback_through_nested_function_proxy() {
    let completion = run(r#"
        const target = new Proxy(function(a) {}, {});
        const proxy = new Proxy(target, { get: undefined });
        Object.create(proxy).length;
        "#);
    assert_eq!(completion, "1");
}

#[test]
fn proxy_get_fallback_through_nested_array_proxy() {
    let completion = run(r#"
        const target = new Proxy([3, 4, 5], {});
        const proxy = new Proxy(target, { get: null });
        proxy.length + ":" + proxy[1];
        "#);
    assert_eq!(completion, "3:4");
}

#[test]
fn proxy_get_rejects_incompatible_frozen_data_property() {
    let mut rt = Runtime::builder().build().expect("runtime");
    let err = rt
        .run_script(
            SourceInput::from_javascript(
                r#"
                const target = {};
                Object.defineProperty(target, "x", {
                    value: 1,
                    writable: false,
                    configurable: false,
                });
                const proxy = new Proxy(target, { get: function() { return 2; } });
                proxy.x;
                "#,
            ),
            "<test>",
        )
        .expect_err("proxy get invariant should reject");
    let message = err.to_string();
    assert!(
        message.contains("Proxy get trap") || message.contains("TypeError"),
        "{message}"
    );
}

#[test]
fn proxy_get_rejects_value_for_accessor_without_getter() {
    let mut rt = Runtime::builder().build().expect("runtime");
    let err = rt
        .run_script(
            SourceInput::from_javascript(
                r#"
                const target = {};
                Object.defineProperty(target, "x", {
                    get: undefined,
                    configurable: false,
                });
                const proxy = new Proxy(target, { get: function() { return 2; } });
                proxy.x;
                "#,
            ),
            "<test>",
        )
        .expect_err("proxy accessor invariant should reject");
    let message = err.to_string();
    assert!(
        message.contains("Proxy get trap") || message.contains("TypeError"),
        "{message}"
    );
}

#[test]
fn proxy_get_own_property_descriptor_falls_through_string_proxy() {
    let completion = run(r#"
        const stringTarget = new Proxy(new String("str"), {});
        const stringProxy = new Proxy(stringTarget, {});
        const ch = Object.getOwnPropertyDescriptor(stringProxy, "0");
        const len = Object.getOwnPropertyDescriptor(stringProxy, "length");
        ch.value + ":" + ch.writable + ":" + ch.enumerable + ":" + ch.configurable + ":" +
            len.value + ":" + len.writable + ":" + len.enumerable + ":" + len.configurable;
        "#);
    assert_eq!(completion, "s:false:true:false:3:false:false:false");
}

#[test]
fn proxy_get_own_property_descriptor_falls_through_function_proxy() {
    let completion = run(r#"
        const functionTarget = new Proxy(function() {}, {});
        const functionProxy = new Proxy(functionTarget, {});
        const desc = Object.getOwnPropertyDescriptor(functionProxy, "prototype");
        (typeof desc.value) + ":" + desc.writable + ":" + desc.enumerable + ":" + desc.configurable;
        "#);
    assert_eq!(completion, "object:true:false:false");
}

#[test]
fn proxy_array_descriptor_helpers_see_enumerable_index() {
    let completion = run(r#"
        const arrayTarget = new Proxy([42], {});
        const arrayProxy = new Proxy(arrayTarget, { getOwnPropertyDescriptor: undefined });
        const desc = Object.getOwnPropertyDescriptor(arrayProxy, "0");
        const keys = Object.keys(arrayProxy);
        const enumerable = Object.prototype.propertyIsEnumerable.call(arrayProxy, "0");
        const oldValue = arrayProxy["0"];
        arrayProxy["0"] = "changed";
        const writable = arrayProxy["0"] === "changed";
        arrayProxy["0"] = oldValue;
        const removed = delete arrayProxy["0"];
        desc.enumerable + ":" + keys.length + ":" + keys[0] + ":" + enumerable + ":" +
            writable + ":" + removed + ":" + Object.prototype.hasOwnProperty.call(arrayProxy, "0");
        "#);
    assert_eq!(completion, "true:1:0:true:true:true:false");
}

#[test]
fn proxy_trap_throw_preserves_original_payload() {
    let completion = run(r#"
        const marker = { tag: "marker" };
        const getProxy = new Proxy({}, {
            get: function() { throw marker; }
        });
        const descProxy = new Proxy({}, {
            getOwnPropertyDescriptor: function() { throw marker; }
        });
        const protoProxy = new Proxy({}, {
            getPrototypeOf: function() { throw marker; }
        });
        function caughtSame(fn) {
            try {
                fn();
            } catch (e) {
                return e === marker;
            }
            return false;
        }
        caughtSame(function() { getProxy.x; }) + ":" +
            caughtSame(function() { Object.getOwnPropertyDescriptor(descProxy, "x"); }) + ":" +
            caughtSame(function() { Object.getPrototypeOf(protoProxy); });
        "#);
    assert_eq!(completion, "true:true:true");
}
