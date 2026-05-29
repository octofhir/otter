//! Runtime regression coverage for the §7.3.11 `GetMethod` + §7.3.14
//! `Call` lowering of `Array.prototype.map` (method-dispatch refactor
//! Stage 4, canonical-builtin identity guard).
//!
//! # Contents
//! - Canonical `map` keeps the re-entrant Array driver.
//! - A user `Array.prototype.map` override is called instead.
//! - An own non-callable `map` shadow reports the shared non-callable
//!   `TypeError`.
//!
//! # Invariants
//! - The specialized Array callback driver runs only when the resolved
//!   method is the realm's canonical `Array.prototype.map` native.
//! - A user override resolves through the ordinary prototype walk and is
//!   invoked with `this` bound to the receiver array.
//!
//! # See also
//! - <https://tc39.es/ecma262/#sec-array.prototype.map>

use otter_runtime::{OtterError, Runtime, SourceInput};

fn run(source: &str) -> String {
    let mut rt = Runtime::builder().build().expect("runtime");
    rt.run_script(SourceInput::from_javascript(source), "<map-guard-test>")
        .expect("script")
        .completion_string()
        .to_string()
}

fn run_throwing(source: &str) -> OtterError {
    let mut rt = Runtime::builder().build().expect("runtime");
    rt.run_script(SourceInput::from_javascript(source), "<map-guard-test>")
        .expect_err("script throws")
}

#[test]
fn canonical_map_runs_the_array_driver() {
    let completion = run(r#"
        [1, 2, 3].map(x => x * 2).join(",");
        "#);
    assert_eq!(completion, "2,4,6");
}

#[test]
fn prototype_override_is_invoked_with_receiver() {
    let completion = run(r#"
        Array.prototype.map = function () { return "override:" + this.length; };
        [1, 2, 3].map(x => x);
        "#);
    assert_eq!(completion, "override:3");
}

#[test]
fn own_non_callable_map_shadow_is_not_callable() {
    let err = run_throwing(r#"
        const a = [1];
        a.map = 1;
        a.map(x => x);
        "#);
    let OtterError::Runtime { diagnostic } = err else {
        panic!("expected Runtime error, got {err:?}");
    };
    assert!(
        diagnostic.message.contains("TypeError"),
        "expected a TypeError, got {:?}",
        diagnostic.message
    );
}
