//! Runtime regression coverage for strict script global `this`.
//!
//! # Contents
//! - Strict global Script `this` remains `globalThis`.
//! - Strict function bare-call `this` remains `undefined`.
//!
//! # Invariants
//! - Strict mode changes function `this` substitution, not the global
//!   Script this binding.
//!
//! # See also
//! - <https://tc39.es/ecma262/#sec-global-environment-records-getthisbinding>

use otter_runtime::{Runtime, SourceInput};

fn run(source: &str) -> String {
    let mut rt = Runtime::builder().build().expect("runtime");
    rt.run_script(SourceInput::from_javascript(source), "<strict-script-this>")
        .expect("script")
        .completion_string()
        .to_string()
}

#[test]
fn strict_script_top_level_this_is_global_this() {
    let completion = run(r#""use strict"; this === globalThis;"#);
    assert_eq!(completion, "true");
}

#[test]
fn strict_function_bare_call_this_is_undefined() {
    let completion = run(r#""use strict";
        (function() { return this; })() === undefined;
        "#);
    assert_eq!(completion, "true");
}
