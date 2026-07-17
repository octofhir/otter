//! Stack diagnostics use the explicit runtime-turn activation owner.
//!
//! # Invariants
//!
//! - Inline native Error construction walks the caller's live activation stack.
//! - `Error.captureStackTrace` uses the same explicit stack without an ambient
//!   interpreter pointer.

use otter_runtime::{Runtime, SourceInput};

#[test]
fn error_diagnostics_walk_the_current_runtime_turn() {
    let mut runtime = Runtime::builder().build().expect("runtime");
    let result = runtime
        .run_script(
            SourceInput::from_javascript(
                r#"
                Error.stackTraceLimit = 20;

                function errorLeaf() {
                    return new Error("turn-owned").stack;
                }
                function errorOuter() {
                    return errorLeaf();
                }

                function captureLeaf() {
                    const target = {};
                    Error.captureStackTrace(target);
                    return target.stack;
                }
                function captureOuter() {
                    return captureLeaf();
                }

                const errorStack = errorOuter();
                const captureStack = captureOuter();
                [
                    errorStack.includes("Error: turn-owned"),
                    errorStack.includes("errorLeaf"),
                    errorStack.includes("errorOuter"),
                    captureStack.includes("captureLeaf"),
                    captureStack.includes("captureOuter")
                ].join("|");
                "#,
            ),
            "<runtime-turn-stack-diagnostics>",
        )
        .expect("stack diagnostics fixture");

    assert_eq!(result.completion_string(), "true|true|true|true|true");
}
