//! Runtime regression coverage for derived-class `this` binding and
//! `super` property access (§10.2.2, §13.3.5, §13.3.7).
//!
//! # Contents
//! - `super(...)` to a plain-function base binds `this` to the result.
//! - A base constructor that returns an object overrides the bound `this`.
//! - Reading `this` (or `super.x`) before `super()` is a ReferenceError.
//! - `super.x` getter / `super.x = v` setter run with `this` as receiver.
//! - `super[expr]` evaluates `GetSuperBase` before `ToPropertyKey`.
//! - `class C extends null` defines and its `super.x` throws TypeError.
//!
//! # Invariants
//! - Derived constructors enter with `this` in the TDZ; only the
//!   `super(...)` result initializes it.
//! - Super-reference reads/writes resolve against the home object's
//!   prototype but target the active `this`.
//!
//! # See also
//! - <https://tc39.es/ecma262/#sec-runtime-semantics-classdefinitionevaluation>
//! - <https://tc39.es/ecma262/#sec-makesuperpropertyreference>

use otter_runtime::{Runtime, SourceInput};

fn run(source: &str) -> String {
    let mut rt = Runtime::builder().build().expect("runtime");
    rt.run_script(SourceInput::from_javascript(source), "<super-derived-test>")
        .expect("script")
        .completion_string()
        .to_string()
}

#[test]
fn super_to_plain_function_base_binds_this() {
    // The base is an ordinary function constructor (not a class).
    let out = run(r#"
        function Parent() { this.x = 41; }
        class Child extends Parent {
            constructor() { super(); this.x += 1; }
        }
        String(new Child().x);
    "#);
    assert_eq!(out, "42");
}

#[test]
fn base_constructor_object_return_overrides_this() {
    let out = run(r#"
        var custom = { tag: "custom" };
        function Parent() { return custom; }
        class Child extends Parent {
            constructor() { super(); }
        }
        String(new Child() === custom);
    "#);
    assert_eq!(out, "true");
}

#[test]
fn this_before_super_is_reference_error() {
    let out = run(r#"
        class Base {}
        class Derived extends Base {
            constructor() { this.x = 1; super(); }
        }
        var name = "none";
        try { new Derived(); } catch (e) { name = e.constructor.name; }
        name;
    "#);
    assert_eq!(out, "ReferenceError");
}

#[test]
fn calling_super_twice_is_reference_error() {
    let out = run(r#"
        class Base {}
        class Derived extends Base {
            constructor() { super(); super(); }
        }
        var name = "none";
        try { new Derived(); } catch (e) { name = e.constructor.name; }
        name;
    "#);
    assert_eq!(out, "ReferenceError");
}

#[test]
fn super_getter_runs_with_this_receiver() {
    let out = run(r#"
        var parent = { get This() { return this; } };
        var obj = { method() { return super.This; } };
        Object.setPrototypeOf(obj, parent);
        String(obj.method() === obj);
    "#);
    assert_eq!(out, "true");
}

#[test]
fn super_setter_runs_with_this_receiver() {
    let out = run(r#"
        var parent = { set x(v) { this.stored = v; } };
        var obj = { method() { super.x = 7; } };
        Object.setPrototypeOf(obj, parent);
        obj.method();
        String(obj.stored);
    "#);
    assert_eq!(out, "7");
}

#[test]
fn super_set_without_setter_writes_own_on_this() {
    let out = run(r#"
        var parent = {};
        var obj = { method() { super.x = 9; } };
        Object.setPrototypeOf(obj, parent);
        obj.method();
        String(obj.x) + "," + String("x" in parent);
    "#);
    assert_eq!(out, "9,false");
}

#[test]
fn super_computed_read_gets_super_base_before_to_property_key() {
    // The key's `toString` re-points obj's prototype; the read must
    // already have captured the original super base.
    let out = run(r#"
        var proto = { p: "ok" };
        var proto2 = { p: "bad" };
        var obj = {
            __proto__: proto,
            m() { return super[key]; }
        };
        var key = { toString() { Object.setPrototypeOf(obj, proto2); return "p"; } };
        obj.m();
    "#);
    assert_eq!(out, "ok");
}

#[test]
fn extends_null_defines_and_super_read_throws_type_error() {
    let out = run(r#"
        var name = "none";
        class C extends null {
            method() { try { super.x; } catch (e) { name = e.constructor.name; } }
        }
        C.prototype.method();
        name + "," + String(Object.getPrototypeOf(C.prototype) === null);
    "#);
    assert_eq!(out, "TypeError,true");
}
