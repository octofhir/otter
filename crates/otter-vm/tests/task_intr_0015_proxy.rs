//! Integration tests for ES2024 Proxy (§28.2).
//!
//! Spec references:
//! - Proxy Constructor: <https://tc39.es/ecma262/#sec-proxy-constructor>
//! - Proxy.revocable: <https://tc39.es/ecma262/#sec-proxy.revocable>
//! - Proxy Internal Methods: <https://tc39.es/ecma262/#sec-proxy-object-internal-methods-and-internal-slots>

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

fn run_bool(source: &str) -> bool {
    let v = run(source);
    v.as_bool()
        .unwrap_or_else(|| panic!("expected bool, got {v:?}"))
}

fn run_f64(source: &str) -> f64 {
    let v = run(source);
    v.as_number()
        .unwrap_or_else(|| panic!("expected number, got {v:?}"))
}

// ── Constructor ──────────────────────────────────────────────────────────────

#[test]
fn proxy_constructor_creates_proxy() {
    assert_eq!(
        run_f64("let t = {x: 42}; let p = new Proxy(t, {}); p.x"),
        42.0
    );
}

#[test]
fn proxy_constructor_requires_new() {
    assert!(run_bool(
        "let threw = false; try { Proxy({}, {}); } catch(e) { threw = true; } threw"
    ));
}

#[test]
fn proxy_constructor_requires_object_target() {
    assert!(run_bool(
        "let threw = false; try { new Proxy(42, {}); } catch(e) { threw = true; } threw"
    ));
}

#[test]
fn proxy_constructor_requires_object_handler() {
    assert!(run_bool(
        "let threw = false; try { new Proxy({}, 42); } catch(e) { threw = true; } threw"
    ));
}

// ── Get trap (§10.5.8) ──────────────────────────────────────────────────────

#[test]
fn proxy_get_trap() {
    assert_eq!(
        run_f64(
            r#"
        let target = { x: 1 };
        let handler = {
            get(target, prop, receiver) {
                return prop === "x" ? 42 : target[prop];
            }
        };
        let p = new Proxy(target, handler);
        p.x
    "#
        ),
        42.0
    );
}

#[test]
fn proxy_get_no_trap_forwards_to_target() {
    assert_eq!(run_f64("let t = {x: 99}; new Proxy(t, {}).x"), 99.0);
}

#[test]
fn proxy_get_undefined_property() {
    assert!(run_bool(
        "let p = new Proxy({}, {}); p.nonexistent === undefined"
    ));
}

// ── Set trap (§10.5.9) ──────────────────────────────────────────────────────

#[test]
fn proxy_set_trap() {
    assert_eq!(
        run_f64(
            r#"
        let target = {};
        let handler = {
            set(target, prop, value, receiver) {
                target[prop] = value * 2;
                return true;
            }
        };
        let p = new Proxy(target, handler);
        p.x = 21;
        target.x
    "#
        ),
        42.0
    );
}

#[test]
fn proxy_set_no_trap_forwards_to_target() {
    assert_eq!(
        run_f64("let t = {}; let p = new Proxy(t, {}); p.x = 42; t.x"),
        42.0
    );
}

// ── Has trap (§10.5.7) ──────────────────────────────────────────────────────

#[test]
fn proxy_has_trap() {
    assert!(run_bool(
        r#"
        let handler = {
            has(target, prop) {
                return prop === "secret" ? false : prop in target;
            }
        };
        let p = new Proxy({ secret: 42, visible: 1 }, handler);
        !("secret" in p) && ("visible" in p)
    "#
    ));
}

#[test]
fn proxy_has_no_trap_forwards() {
    assert!(run_bool(
        r#"
        let p = new Proxy({ x: 1 }, {});
        ("x" in p) && !("y" in p)
    "#
    ));
}

// ── DeleteProperty trap (§10.5.10) ──────────────────────────────────────────

#[test]
fn proxy_delete_property_trap() {
    assert!(run_bool(
        r#"
        let log = [];
        let handler = {
            deleteProperty(target, prop) {
                log.push(prop);
                delete target[prop];
                return true;
            }
        };
        let target = { x: 1, y: 2 };
        let p = new Proxy(target, handler);
        delete p.x;
        log.length === 1 && log[0] === "x" && target.x === undefined
    "#
    ));
}

#[test]
fn proxy_delete_no_trap_forwards() {
    assert!(run_bool(
        "let t = {x: 1}; let p = new Proxy(t, {}); delete p.x; t.x === undefined"
    ));
}

// ── Apply trap (§10.5.12) ───────────────────────────────────────────────────

#[test]
fn proxy_apply_no_trap_forwards() {
    assert_eq!(
        run_f64(
            r#"
        function add(a, b) { return a + b; }
        let p = new Proxy(add, {});
        p(10, 20)
    "#
        ),
        30.0
    );
}

#[test]
fn proxy_apply_trap() {
    assert_eq!(
        run_f64(
            r#"
        let handler = {
            apply(target, thisArg, argsList) {
                return target(argsList[0], argsList[1]) * 2;
            }
        };
        function sum(a, b) { return a + b; }
        let p = new Proxy(sum, handler);
        p(3, 4)
    "#
        ),
        14.0
    );
}

// ── Construct trap (§10.5.13) ───────────────────────────────────────────────

#[test]
fn proxy_construct_no_trap_forwards() {
    assert_eq!(
        run_f64(
            r#"
        function Point(x, y) { this.x = x; this.y = y; }
        let P = new Proxy(Point, {});
        let pt = new P(10, 20);
        pt.x + pt.y
    "#
        ),
        30.0
    );
}

#[test]
fn proxy_construct_trap() {
    assert_eq!(
        run_f64(
            r#"
        function Point(x, y) { this.x = x; this.y = y; }
        let handler = {
            construct(target, args, newTarget) {
                let obj = new target(args[0], args[1]);
                obj.z = args[0] + args[1];
                return obj;
            }
        };
        let P = new Proxy(Point, handler);
        let pt = new P(3, 4);
        pt.z
    "#
        ),
        7.0
    );
}

// ── Proxy.revocable (§28.2.2) ───────────────────────────────────────────────

#[test]
fn proxy_revocable_basic() {
    assert!(run_bool(
        r#"
        let { proxy, revoke } = Proxy.revocable({ x: 42 }, {});
        let before = proxy.x;
        revoke();
        let threw = false;
        try { proxy.x; } catch(e) { threw = true; }
        before === 42 && threw
    "#
    ));
}

#[test]
fn proxy_revoke_is_idempotent() {
    assert!(run_bool(
        r#"
        let { proxy, revoke } = Proxy.revocable({}, {});
        revoke();
        revoke();
        true
    "#
    ));
}

// ── Revoked proxy throws ────────────────────────────────────────────────────

#[test]
fn proxy_revoked_set_throws() {
    assert!(run_bool(
        r#"
        let { proxy, revoke } = Proxy.revocable({}, {});
        revoke();
        let threw = false;
        try { proxy.x = 1; } catch(e) { threw = true; }
        threw
    "#
    ));
}

#[test]
fn proxy_revoked_has_throws() {
    assert!(run_bool(
        r#"
        let { proxy, revoke } = Proxy.revocable({}, {});
        revoke();
        let threw = false;
        try { "x" in proxy; } catch(e) { threw = true; }
        threw
    "#
    ));
}

#[test]
fn proxy_revoked_delete_throws() {
    assert!(run_bool(
        r#"
        let { proxy, revoke } = Proxy.revocable({}, {});
        revoke();
        let threw = false;
        try { delete proxy.x; } catch(e) { threw = true; }
        threw
    "#
    ));
}

// ── Typeof proxy ────────────────────────────────────────────────────────────

#[test]
fn proxy_typeof_is_object() {
    assert!(run_bool(r#"typeof new Proxy({}, {}) === "object""#));
}

// ── Computed property access ────────────────────────────────────────────────

#[test]
fn proxy_get_computed() {
    assert_eq!(
        run_f64(
            r#"
        let handler = {
            get(target, prop) {
                return prop === "x" ? 100 : target[prop];
            }
        };
        let p = new Proxy({ x: 1 }, handler);
        let key = "x";
        p[key]
    "#
        ),
        100.0
    );
}

#[test]
fn proxy_set_computed() {
    assert_eq!(
        run_f64(
            r#"
        let target = {};
        let handler = {
            set(target, prop, value) {
                target[prop] = value + 1;
                return true;
            }
        };
        let p = new Proxy(target, handler);
        let key = "x";
        p[key] = 41;
        target.x
    "#
        ),
        42.0
    );
}

// ── Multiple traps on same proxy ────────────────────────────────────────────

#[test]
fn proxy_multiple_traps() {
    assert_eq!(
        run_f64(
            r#"
        let log = [];
        let handler = {
            get(target, prop) {
                log.push("get:" + prop);
                return target[prop];
            },
            set(target, prop, value) {
                log.push("set:" + prop);
                target[prop] = value;
                return true;
            },
            has(target, prop) {
                log.push("has:" + prop);
                return prop in target;
            },
            deleteProperty(target, prop) {
                log.push("delete:" + prop);
                delete target[prop];
                return true;
            }
        };
        let p = new Proxy({ x: 1 }, handler);
        p.x;
        p.y = 2;
        "x" in p;
        delete p.x;
        log.length
    "#
        ),
        4.0
    );
}

// ── getOwnPropertyDescriptor trap (§10.5.5) ────────────────────────────────

#[test]
fn proxy_get_own_property_descriptor_trap() {
    assert!(run_bool(
        r#"
        let handler = {
            getOwnPropertyDescriptor(target, prop) {
                if (prop === "x") {
                    return { value: 42, writable: true, enumerable: true, configurable: true };
                }
                return Object.getOwnPropertyDescriptor(target, prop);
            }
        };
        let p = new Proxy({ x: 1, y: 2 }, handler);
        let desc = Object.getOwnPropertyDescriptor(p, "x");
        desc.value === 42 && desc.writable === true
    "#
    ));
}

#[test]
fn proxy_get_own_property_descriptor_no_trap() {
    assert!(run_bool(
        r#"
        let p = new Proxy({ x: 99 }, {});
        let desc = Object.getOwnPropertyDescriptor(p, "x");
        desc.value === 99
    "#
    ));
}

#[test]
fn proxy_get_own_property_descriptor_returns_undefined() {
    assert!(run_bool(
        r#"
        let handler = {
            getOwnPropertyDescriptor(target, prop) {
                return undefined;
            }
        };
        let p = new Proxy({ x: 1 }, handler);
        Object.getOwnPropertyDescriptor(p, "x") === undefined
    "#
    ));
}

// ── defineProperty trap (§10.5.6) ──────────────────────────────────────────

#[test]
fn proxy_define_property_trap() {
    assert!(run_bool(
        r#"
        let log = [];
        let handler = {
            defineProperty(target, prop, descriptor) {
                log.push(prop);
                return Reflect.defineProperty(target, prop, descriptor);
            }
        };
        let target = {};
        let p = new Proxy(target, handler);
        Object.defineProperty(p, "x", { value: 42, writable: true, enumerable: true, configurable: true });
        log.length === 1 && log[0] === "x" && target.x === 42
    "#
    ));
}

#[test]
fn proxy_define_property_no_trap() {
    assert!(run_bool(
        r#"
        let target = {};
        let p = new Proxy(target, {});
        Object.defineProperty(p, "x", { value: 42, writable: true, enumerable: true, configurable: true });
        target.x === 42
    "#
    ));
}

// ── ownKeys trap (§10.5.11) ────────────────────────────────────────────────

#[test]
fn proxy_own_keys_trap() {
    assert!(run_bool(
        r#"
        let handler = {
            ownKeys(target) {
                return ["a", "b"];
            }
        };
        let p = new Proxy({ a: 1, b: 2, c: 3 }, handler);
        let keys = Reflect.ownKeys(p);
        keys.length === 2 && keys[0] === "a" && keys[1] === "b"
    "#
    ));
}

#[test]
fn proxy_own_keys_no_trap() {
    assert!(run_bool(
        r#"
        let p = new Proxy({ x: 1, y: 2 }, {});
        let keys = Reflect.ownKeys(p);
        keys.length === 2
    "#
    ));
}

#[test]
fn proxy_own_keys_object_keys() {
    assert!(run_bool(
        r#"
        let handler = {
            ownKeys(target) {
                return ["visible"];
            },
            getOwnPropertyDescriptor(target, prop) {
                return { value: target[prop], writable: true, enumerable: true, configurable: true };
            }
        };
        let p = new Proxy({ visible: 1, hidden: 2 }, handler);
        let keys = Object.keys(p);
        keys.length === 1 && keys[0] === "visible"
    "#
    ));
}

#[test]
fn proxy_own_keys_get_own_property_names() {
    assert!(run_bool(
        r#"
        let handler = {
            ownKeys(target) {
                return ["a", "b"];
            }
        };
        let p = new Proxy({}, handler);
        let names = Object.getOwnPropertyNames(p);
        names.length === 2 && names[0] === "a" && names[1] === "b"
    "#
    ));
}

// ── getPrototypeOf trap (§10.5.1) ──────────────────────────────────────────

#[test]
fn proxy_get_prototype_of_trap() {
    assert!(run_bool(
        r#"
        let customProto = { marker: true };
        let handler = {
            getPrototypeOf(target) {
                return customProto;
            }
        };
        let p = new Proxy({}, handler);
        Object.getPrototypeOf(p) === customProto
    "#
    ));
}

#[test]
fn proxy_get_prototype_of_no_trap() {
    assert!(run_bool(
        r#"
        let proto = {};
        let target = Object.create(proto);
        let p = new Proxy(target, {});
        Object.getPrototypeOf(p) === proto
    "#
    ));
}

#[test]
fn proxy_get_prototype_of_null() {
    assert!(run_bool(
        r#"
        let handler = {
            getPrototypeOf(target) {
                return null;
            }
        };
        let p = new Proxy({}, handler);
        Object.getPrototypeOf(p) === null
    "#
    ));
}

// ── setPrototypeOf trap (§10.5.2) ──────────────────────────────────────────

#[test]
fn proxy_set_prototype_of_trap() {
    assert!(run_bool(
        r#"
        let log = [];
        let handler = {
            setPrototypeOf(target, proto) {
                log.push("setProto");
                return Reflect.setPrototypeOf(target, proto);
            }
        };
        let target = {};
        let newProto = { marker: true };
        let p = new Proxy(target, handler);
        Object.setPrototypeOf(p, newProto);
        log.length === 1 && Object.getPrototypeOf(target) === newProto
    "#
    ));
}

#[test]
fn proxy_set_prototype_of_no_trap() {
    assert!(run_bool(
        r#"
        let target = {};
        let newProto = { marker: true };
        let p = new Proxy(target, {});
        Object.setPrototypeOf(p, newProto);
        Object.getPrototypeOf(target) === newProto
    "#
    ));
}

// ── isExtensible trap (§10.5.3) ────────────────────────────────────────────

#[test]
fn proxy_is_extensible_trap() {
    assert!(run_bool(
        r#"
        let handler = {
            isExtensible(target) {
                return Reflect.isExtensible(target);
            }
        };
        let p = new Proxy({}, handler);
        Object.isExtensible(p) === true
    "#
    ));
}

#[test]
fn proxy_is_extensible_no_trap() {
    assert!(run_bool(
        r#"
        let p = new Proxy({}, {});
        Object.isExtensible(p) === true
    "#
    ));
}

#[test]
fn proxy_is_extensible_invariant_violation() {
    // Trap must agree with target — returning false when target is extensible throws.
    assert!(run_bool(
        r#"
        let handler = {
            isExtensible(target) {
                return false;
            }
        };
        let p = new Proxy({}, handler);
        let threw = false;
        try { Object.isExtensible(p); } catch(e) { threw = true; }
        threw
    "#
    ));
}

// ── preventExtensions trap (§10.5.4) ───────────────────────────────────────

#[test]
fn proxy_prevent_extensions_trap() {
    assert!(run_bool(
        r#"
        let handler = {
            preventExtensions(target) {
                Object.preventExtensions(target);
                return true;
            }
        };
        let target = {};
        let p = new Proxy(target, handler);
        Object.preventExtensions(p);
        Object.isExtensible(target) === false
    "#
    ));
}

#[test]
fn proxy_prevent_extensions_no_trap() {
    assert!(run_bool(
        r#"
        let target = {};
        let p = new Proxy(target, {});
        Object.preventExtensions(p);
        Object.isExtensible(target) === false
    "#
    ));
}

#[test]
fn proxy_prevent_extensions_invariant_violation() {
    // Trap returns true but target is still extensible → TypeError.
    assert!(run_bool(
        r#"
        let handler = {
            preventExtensions(target) {
                return true; // doesn't actually prevent extensions on target
            }
        };
        let p = new Proxy({}, handler);
        let threw = false;
        try { Object.preventExtensions(p); } catch(e) { threw = true; }
        threw
    "#
    ));
}

// ── Reflect.get / Reflect.set on proxy ─────────────────────────────────────

#[test]
fn proxy_reflect_get() {
    assert_eq!(
        run_f64(
            r#"
        let handler = {
            get(target, prop) { return 100; }
        };
        let p = new Proxy({ x: 1 }, handler);
        Reflect.get(p, "x")
    "#
        ),
        100.0
    );
}

#[test]
fn proxy_reflect_set() {
    assert!(run_bool(
        r#"
        let target = {};
        let handler = {
            set(target, prop, value) {
                target[prop] = value * 3;
                return true;
            }
        };
        let p = new Proxy(target, handler);
        Reflect.set(p, "x", 10);
        target.x === 30
    "#
    ));
}

// ── Reflect.has on proxy ───────────────────────────────────────────────────

#[test]
fn proxy_reflect_has() {
    assert!(run_bool(
        r#"
        let handler = {
            has(target, prop) {
                return prop === "magic";
            }
        };
        let p = new Proxy({}, handler);
        Reflect.has(p, "magic") === true && Reflect.has(p, "other") === false
    "#
    ));
}

// ── Reflect.deleteProperty on proxy ────────────────────────────────────────

#[test]
fn proxy_reflect_delete_property() {
    assert!(run_bool(
        r#"
        let target = { x: 1, y: 2 };
        let handler = {
            deleteProperty(target, prop) {
                if (prop === "x") {
                    delete target[prop];
                    return true;
                }
                return false;
            }
        };
        let p = new Proxy(target, handler);
        Reflect.deleteProperty(p, "x") === true &&
            Reflect.deleteProperty(p, "y") === false &&
            target.x === undefined && target.y === 2
    "#
    ));
}

// ── Reflect.defineProperty on proxy ────────────────────────────────────────

#[test]
fn proxy_reflect_define_property() {
    assert!(run_bool(
        r#"
        let log = [];
        let handler = {
            defineProperty(target, prop, desc) {
                log.push(prop);
                return Reflect.defineProperty(target, prop, desc);
            }
        };
        let target = {};
        let p = new Proxy(target, handler);
        Reflect.defineProperty(p, "x", { value: 42, writable: true, enumerable: true, configurable: true });
        log.length === 1 && target.x === 42
    "#
    ));
}

// ── Revoked proxy throws on all new traps ──────────────────────────────────

#[test]
fn proxy_revoked_own_keys_throws() {
    assert!(run_bool(
        r#"
        let { proxy, revoke } = Proxy.revocable({}, {});
        revoke();
        let threw = false;
        try { Reflect.ownKeys(proxy); } catch(e) { threw = true; }
        threw
    "#
    ));
}

#[test]
fn proxy_revoked_get_own_property_descriptor_throws() {
    assert!(run_bool(
        r#"
        let { proxy, revoke } = Proxy.revocable({}, {});
        revoke();
        let threw = false;
        try { Object.getOwnPropertyDescriptor(proxy, "x"); } catch(e) { threw = true; }
        threw
    "#
    ));
}

#[test]
fn proxy_revoked_define_property_throws() {
    assert!(run_bool(
        r#"
        let { proxy, revoke } = Proxy.revocable({}, {});
        revoke();
        let threw = false;
        try { Object.defineProperty(proxy, "x", { value: 1 }); } catch(e) { threw = true; }
        threw
    "#
    ));
}

#[test]
fn proxy_revoked_get_prototype_of_throws() {
    assert!(run_bool(
        r#"
        let { proxy, revoke } = Proxy.revocable({}, {});
        revoke();
        let threw = false;
        try { Object.getPrototypeOf(proxy); } catch(e) { threw = true; }
        threw
    "#
    ));
}

#[test]
fn proxy_revoked_set_prototype_of_throws() {
    assert!(run_bool(
        r#"
        let { proxy, revoke } = Proxy.revocable({}, {});
        revoke();
        let threw = false;
        try { Object.setPrototypeOf(proxy, {}); } catch(e) { threw = true; }
        threw
    "#
    ));
}

#[test]
fn proxy_revoked_is_extensible_throws() {
    assert!(run_bool(
        r#"
        let { proxy, revoke } = Proxy.revocable({}, {});
        revoke();
        let threw = false;
        try { Object.isExtensible(proxy); } catch(e) { threw = true; }
        threw
    "#
    ));
}

#[test]
fn proxy_revoked_prevent_extensions_throws() {
    assert!(run_bool(
        r#"
        let { proxy, revoke } = Proxy.revocable({}, {});
        revoke();
        let threw = false;
        try { Object.preventExtensions(proxy); } catch(e) { threw = true; }
        threw
    "#
    ));
}
