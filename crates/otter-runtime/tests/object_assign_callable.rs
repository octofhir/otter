//! Regression coverage for `Object.assign` writes to callable targets.
//!
//! # Contents
//! - String-keyed properties copied to ordinary functions.
//! - Symbol-keyed properties copied to native builtin functions.
//!
//! # Invariants
//! - Callable values participate in ordinary `[[Set]]` through their expando
//!   property storage instead of being rejected as unsupported exotics.
//!
//! # See also
//! - <https://tc39.es/ecma262/#sec-object.assign>

use otter_runtime::{Runtime, SourceInput};

fn run(source: &str) -> String {
    let mut runtime = Runtime::builder().build().expect("runtime");
    runtime
        .run_script(
            SourceInput::from_javascript(source),
            "<object-assign-callable>",
        )
        .expect("script")
        .completion_string()
        .to_string()
}

#[test]
fn object_assign_sets_string_properties_on_function() {
    assert_eq!(
        run(r#"
            function target() {}
            Object.assign(target, { test: 42 });
            target.test + ":" + typeof target;
        "#),
        "42:function"
    );
}

#[test]
fn object_assign_sets_symbol_properties_on_native_function() {
    assert_eq!(
        run(r#"
            const key = Symbol("rollup");
            Object.assign(Math.max, { [key]: "ok" });
            Math.max[key];
        "#),
        "ok"
    );
}
