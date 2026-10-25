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

#[test]
fn prepare_stack_trace_hands_the_hook_structured_call_sites() {
    let mut runtime = Runtime::builder().build().expect("runtime");
    let result = runtime
        .run_script(
            SourceInput::from_javascript(
                r#"
                function sites() {
                    const target = {};
                    const previous = Error.prepareStackTrace;
                    Error.prepareStackTrace = (_, callSites) => callSites;
                    Error.captureStackTrace(target);
                    const stack = target.stack;
                    Error.prepareStackTrace = previous;
                    return stack;
                }
                function caller() {
                    return sites();
                }
                const callSites = caller();
                const top = callSites[0];

                let inner;
                Error.prepareStackTrace = function (error, callSites) {
                    inner = new Error("inside").stack;
                    return { frames: callSites.length, self: this === Error };
                };
                const shaped = new Error("hooked").stack;
                delete Error.prepareStackTrace;
                const plain = new Error("plain").stack;
                [
                    Array.isArray(callSites),
                    top.getFunctionName(),
                    callSites[1].getFunctionName(),
                    top.getFileName(),
                    typeof top.getLineNumber(),
                    top.getColumnNumber() > 0,
                    top.isEval(),
                    String(top).startsWith("sites ("),
                    shaped.frames > 0,
                    shaped.self,
                    inner.startsWith("Error: inside"),
                    plain.startsWith("Error: plain")
                ].join("|");
                "#,
            ),
            "prepare-stack-trace.js",
        )
        .expect("prepare stack trace fixture");

    assert_eq!(
        result.completion_string(),
        "true|sites|caller|prepare-stack-trace.js|number|true|false|true|true|true|true|true"
    );
}
