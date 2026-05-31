//! Runtime coverage for `for (let x of …)` / `for (const x of …)`
//! per-iteration binding and head Temporal Dead Zone.
//!
//! Each `let`/`const` single-identifier head gets a fresh binding cell
//! per iteration (§14.7.5.6 CreatePerIterationEnvironment), so closures
//! created in distinct iterations observe distinct values; and the head
//! binding is in the TDZ while the loop's right-hand side is evaluated
//! (§14.7.5.12 ForIn/OfHeadEvaluation).
//!
//! # See also
//! - <https://tc39.es/ecma262/#sec-for-in-and-for-of-statements-runtime-semantics-labelledevaluation>

use otter_runtime::{Runtime, SourceInput};

fn eval(source: &str) -> String {
    let mut rt = Runtime::builder().build().expect("runtime");
    rt.run_script(
        SourceInput::from_javascript(source),
        "<per-iter-scope-test>",
    )
    .expect("script")
    .completion_string()
    .to_string()
}

#[test]
fn let_head_captures_fresh_binding_per_iteration() {
    let src = "var f = [];\n\
         for (let x of [1, 2, 3]) { f[x - 1] = function () { return x; }; }\n\
         f[0]() + ',' + f[1]() + ',' + f[2]();";
    assert_eq!(eval(src), "1,2,3");
}

#[test]
fn const_head_captures_fresh_binding_per_iteration() {
    let src = "var f = [];\n\
         var i = 0;\n\
         for (const x of ['a', 'b', 'c']) { f[i++] = function () { return x; }; }\n\
         f[0]() + ',' + f[1]() + ',' + f[2]();";
    assert_eq!(eval(src), "a,b,c");
}

#[test]
fn accumulator_does_not_collapse_per_iteration_bindings() {
    // Reading the binding earlier in the body (the `s += x`) must not
    // make every closure observe the final value.
    let src = "var s = 0;\n\
         var f = [];\n\
         for (let x of [1, 2, 3]) { s += x; f[x - 1] = function () { return x; }; }\n\
         f[0]() + ',' + f[1]() + ',' + f[2]() + ',' + s;";
    assert_eq!(eval(src), "1,2,3,6");
}

#[test]
fn head_binding_is_in_tdz_during_rhs() {
    // §14.7.5.12 — the head `let x` shadows the outer `x` and is in the
    // TDZ while the right-hand side is evaluated, so `[x]` throws.
    let src = "var threw = false;\n\
         try { let x = 1; for (let x of [x]) {} } catch (e) { threw = e instanceof ReferenceError; }\n\
         threw;";
    assert_eq!(eval(src), "true");
}

#[test]
fn const_head_in_tdz_during_rhs() {
    let src = "var threw = false;\n\
         try { let y = 1; for (const y of [y]) {} } catch (e) { threw = e instanceof ReferenceError; }\n\
         threw;";
    assert_eq!(eval(src), "true");
}
