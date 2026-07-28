//! Regression coverage for capture analysis and block-scoped shadowing.
//!
//! # Contents
//! - A `let` inside a block of a closure does not stop that closure from
//!   capturing the equally-named binding of its enclosing function.
//! - The same for `const`, for a `for…of` head, and through two levels of
//!   nesting.
//! - Bindings that really do cover a whole nested function — its parameters and
//!   its `var`s — still shadow, so the outer binding is left alone.
//!
//! # Invariants
//! - The capture pre-pass promotes an enclosing binding to an upvalue cell
//!   whenever a nested function can still reach it. A block-scoped declaration
//!   shadows inside its block and nowhere else, so it must not be mistaken for
//!   a binding of the whole nested function: the references outside the block
//!   would then have no cell to read, and a live binding would raise
//!   `ReferenceError`.
//!
//! # See also
//! - `crates/otter-compiler/src/capture.rs`

use otter_runtime::{Runtime, SourceInput};

fn run(source: &str) -> String {
    let mut runtime = Runtime::builder().build().expect("runtime");
    runtime
        .run_script(SourceInput::from_javascript(source), "<block-let-capture>")
        .expect("script")
        .completion_string()
        .to_string()
}

#[test]
fn a_block_let_does_not_shadow_the_whole_closure() {
    let completion = run(r"
        function outer() {
            var value = 'outer';
            return (() => {
                { let value = 'inner'; void value; }
                return value;
            })();
        }
        outer();
        ");
    assert_eq!(completion, "outer");
}

#[test]
fn a_block_const_behaves_the_same() {
    let completion = run(r"
        function outer() {
            let value = 'outer';
            return (function () {
                { const value = 'inner'; void value; }
                return value;
            })();
        }
        outer();
        ");
    assert_eq!(completion, "outer");
}

#[test]
fn a_for_of_head_binding_does_not_shadow_the_whole_closure() {
    let completion = run(r"
        function outer() {
            var value = () => 'called';
            return (items => {
                for (let value of items) { void value; }
                return value();
            })(['a', 'b']);
        }
        outer();
        ");
    assert_eq!(completion, "called");
}

#[test]
fn the_capture_survives_two_levels_of_nesting() {
    let completion = run(r"
        function outer() {
            var value = 'outer';
            return (() => (() => {
                { let value = 'inner'; void value; }
                return value;
            })())();
        }
        outer();
        ");
    assert_eq!(completion, "outer");
}

#[test]
fn a_reference_before_the_block_reads_the_captured_binding_too() {
    let completion = run(r"
        function outer() {
            var value = 'outer';
            return (() => {
                const first = value;
                { let value = 'inner'; void value; }
                return first + '/' + value;
            })();
        }
        outer();
        ");
    assert_eq!(completion, "outer/outer");
}

#[test]
fn a_parameter_still_shadows_the_whole_closure() {
    let completion = run(r"
        function outer() {
            var value = 'outer';
            return (value => {
                { let inner = 1; void inner; }
                return value;
            })('parameter');
        }
        outer();
        ");
    assert_eq!(completion, "parameter");
}

#[test]
fn a_var_in_a_block_still_shadows_the_whole_closure() {
    let completion = run(r"
        function outer() {
            var value = 'outer';
            return (function () {
                { var value = 'inner'; }
                return value;
            })();
        }
        outer();
        ");
    assert_eq!(completion, "inner");
}

#[test]
fn the_block_binding_itself_still_wins_inside_the_block() {
    let completion = run(r"
        function outer() {
            var value = 'outer';
            return (() => {
                let seen;
                { let value = 'inner'; seen = value; }
                return seen + '/' + value;
            })();
        }
        outer();
        ");
    assert_eq!(completion, "inner/outer");
}
