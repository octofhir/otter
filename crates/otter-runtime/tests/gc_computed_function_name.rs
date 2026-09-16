//! Moving-GC coverage for computed-property function names.
//!
//! # Contents
//! - An anonymous function named by object-literal `SetFunctionName`.
//! - A full collection between the initial and repeated `.name` reads.
//!
//! # Invariants
//! - Virtual function-metadata lookup cannot leave an incoming descriptor with
//!   a pre-move string handle.
//! - The closure-owned property bag retains and traces the inferred name on
//!   interpreter, Template, and production-tiered execution.

use otter_runtime::{JitSelection, Runtime, SourceInput};

#[test]
fn computed_function_name_survives_the_next_full_collection_on_every_tier() {
    for selection in [
        JitSelection::InterpreterOnly,
        JitSelection::Template,
        JitSelection::ProductionTiered,
    ] {
        let mut runtime = Runtime::builder()
            .jit_selection(selection)
            .build()
            .expect("runtime");
        let before = runtime
            .run_script(
                SourceInput::from_javascript(
                    r#"
globalThis.step10ComputedFunction = {
  ["computed" + "Name"]: function () {}
}.computedName;
step10ComputedFunction.name;
"#,
                ),
                "step10-computed-name-before-gc.js",
            )
            .expect("create computed-name function");
        assert_eq!(before.completion_string(), "computedName", "{selection:?}");

        runtime.force_gc().expect("full moving collection");

        let after = runtime
            .run_script(
                SourceInput::from_javascript("step10ComputedFunction.name;"),
                "step10-computed-name-after-gc.js",
            )
            .expect("read computed name after collection");
        assert_eq!(after.completion_string(), "computedName", "{selection:?}");
    }
}
