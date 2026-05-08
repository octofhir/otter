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
        message.contains("type mismatch") || message.contains("TypeMismatch"),
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
        message.contains("type mismatch") || message.contains("TypeMismatch"),
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
        message.contains("type mismatch") || message.contains("TypeMismatch"),
        "{message}"
    );
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
fn proxy_set_fallback_rejects_non_extensible_target() {
    let mut rt = Runtime::builder().build().expect("runtime");
    let err = rt
        .run_script(
            SourceInput::from_javascript(
                r#"
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
        message.contains("type mismatch") || message.contains("TypeMismatch"),
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
