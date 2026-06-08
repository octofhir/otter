//! §13.15.2 PutValue / §9.1.1.1.5 SetMutableBinding — assigning to an
//! immutable caller binding from a direct `eval` body.
//!
//! # Contents
//! - A `const` / `class` caller binding rejects an eval-body write with
//!   a `TypeError` in both strict and sloppy mode (§13.3.1).
//! - A named function expression's self-name binding rejects an
//!   eval-body write with a `TypeError` in strict mode and silently
//!   drops it in sloppy mode (§10.2.11 funcEnv immutable binding).
//! - A plain `const` reassignment outside `eval` is a runtime
//!   `TypeError` (not an early error) — the RHS still evaluates.
//!
//! # Invariants
//! - The immutability of a caller binding survives the direct-eval
//!   caller-scope plumbing: `const` / self-name flags reach the eval
//!   chunk's binding table.

use otter_runtime::{Runtime, SourceInput};

fn run(source: &str) -> String {
    let mut rt = Runtime::builder().build().expect("runtime");
    rt.run_script(SourceInput::from_javascript(source), "<eval-immutable>")
        .expect("script")
        .completion_string()
        .to_string()
}

#[test]
fn strict_eval_assign_to_named_fn_expr_self_name_throws() {
    let out = run(r#"
        "use strict";
        var ref = function BindingIdentifier() {
            eval("BindingIdentifier = 1");
            return "unreached";
        };
        var name = "none";
        try { ref(); } catch (e) { name = e.constructor.name; }
        name;
    "#);
    assert_eq!(out, "TypeError");
}

#[test]
fn sloppy_eval_assign_to_named_fn_expr_self_name_is_dropped() {
    let out = run(r#"
        var ref = function NFE() {
            eval("NFE = 1");
            return typeof NFE;
        };
        ref();
    "#);
    assert_eq!(out, "function");
}

#[test]
fn eval_assign_to_const_caller_binding_throws() {
    let out = run(r#"
        function f() {
            const x = 1;
            var name = "none";
            try { eval("x = 2"); } catch (e) { name = e.constructor.name; }
            return name + "," + x;
        }
        f();
    "#);
    assert_eq!(out, "TypeError,1");
}

#[test]
fn sloppy_eval_assign_to_const_caller_binding_still_throws() {
    // const reassignment is a TypeError in every mode, unlike the
    // self-name binding which is strict-gated.
    let out = run(r#"
        function f() {
            const x = 1;
            var name = "none";
            try { eval("x = 9"); } catch (e) { name = e.constructor.name; }
            return name;
        }
        f();
    "#);
    assert_eq!(out, "TypeError");
}

#[test]
fn plain_const_reassignment_is_runtime_type_error() {
    let out = run(r#"
        function f() {
            const x = 1;
            x = 2;
        }
        var name = "none";
        try { f(); } catch (e) { name = e.constructor.name; }
        name;
    "#);
    assert_eq!(out, "TypeError");
}

#[test]
fn const_reassignment_evaluates_rhs_before_throwing() {
    // §13.15.2 — PutValue throws only after the RHS is evaluated.
    let out = run(r#"
        function f() {
            const x = 1;
            x = (globalThis.__hit = true, 2);
        }
        globalThis.__hit = false;
        try { f(); } catch (e) {}
        String(globalThis.__hit);
    "#);
    assert_eq!(out, "true");
}
