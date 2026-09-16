//! Moving-GC coverage for intrinsic Error messages.
//!
//! # Contents
//! - Error construction from a freshly allocated string message.
//! - A full collection between construction and the repeated message read.
//!
//! # Invariants
//! - The pending message is a mutable root while the error shell allocates.
//! - The installed own `message` slot never retains a forwarding-cell offset.
//! - Interpreter, Template, and production-tiered execution agree.

use otter_runtime::{JitSelection, Runtime, SourceInput};

#[test]
fn error_message_survives_shell_allocation_and_full_collection_on_every_tier() {
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
globalThis.step10Error = new Error("step10-moving-message");
step10Error.message;
"#,
                ),
                "step10-error-message-before-gc.js",
            )
            .expect("construct Error with message");
        assert_eq!(
            before.completion_string(),
            "step10-moving-message",
            "{selection:?}"
        );

        runtime.force_gc().expect("full moving collection");

        let after = runtime
            .run_script(
                SourceInput::from_javascript("step10Error.message;"),
                "step10-error-message-after-gc.js",
            )
            .expect("read Error message after collection");
        assert_eq!(
            after.completion_string(),
            "step10-moving-message",
            "{selection:?}"
        );
    }
}
