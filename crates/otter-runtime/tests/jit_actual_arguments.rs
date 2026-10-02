//! Actual-argument ownership through generated calls and cold completion.
//!
//! # Contents
//! - Legacy function inspection of a callee that has no arguments binding.
//! - Extra moving values retained through nested calls and numeric deopt.
//!
//! # Invariants
//! All tiers preserve the caller's complete actual list. Generated execution
//! must occur before the probe; matching output alone does not prove linkage.
//!
//! # See also
//! - `otter_vm::native_abi::Frame` for the unconditional actual window.

use otter_runtime::{JitSelection, Runtime, SourceInput};

const SETUP: &str = r#"
let token = { stamp: "identity" };
function inspect(fn) {
    const list = fn.arguments;
    const allocated = { payload: "nested-allocation" };
    return [list.length, list[0], list[1].payload, list[2] === token, allocated.payload];
}
function target(value) {
    const incremented = value + 1;
    if (value < 0 || incremented > 2147483647) return inspect(target);
    return incremented;
}
function caller(fn, value, extra, identity) {
    if (arguments.length !== 4) throw "caller arity";
    return fn(value, extra, identity);
}
for (let i = 0; i < 5000; i++) {
    target(i);
    caller(target, i, token, token);
}
"#;

const PROBE: &str = r#"
JSON.stringify([
    caller(target, -1, { payload: "cold-call" }, token),
    caller(target, 2147483647, { payload: "deopt" }, token),
    caller(target, 41, token, token),
    target.arguments === null
]);
"#;

#[test]
fn extra_actuals_remain_observable_without_an_arguments_binding() {
    let expected = r#"[[3,-1,"cold-call",true,"nested-allocation"],[3,2147483647,"deopt",true,"nested-allocation"],42,true]"#;
    for selection in [
        JitSelection::InterpreterOnly,
        JitSelection::Template,
        JitSelection::ProductionTiered,
    ] {
        let mut runtime = Runtime::builder()
            .jit_selection(selection)
            .build()
            .expect("actual-argument runtime");
        runtime
            .run_script(SourceInput::from_javascript(SETUP), "actual-setup.js")
            .unwrap_or_else(|error| panic!("{selection:?} setup: {error:?}"));
        let before = runtime.execution_stats();
        let result = runtime
            .run_script(SourceInput::from_javascript(PROBE), "actual-probe.js")
            .unwrap_or_else(|error| panic!("{selection:?} probe: {error:?}"));
        assert_eq!(result.completion_string(), expected, "{selection:?}");
        if selection != JitSelection::InterpreterOnly {
            let after = runtime.execution_stats();
            assert!(
                after.jit_generated_calls > before.jit_generated_calls,
                "{selection:?}: probe must enter a generated callee; before={before:?}; after={after:?}"
            );
        }
    }
}
