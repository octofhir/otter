//! Runtime coverage for native-syntax optimization hints in `eval`.
//!
//! # Contents
//! - Direct eval of `%PrepareFunctionForOptimization` and
//!   `%OptimizeFunctionOnNextCall`.
//!
//! # Invariants
//! - Optimization hints are accepted as no-ops for compatibility harnesses.
//! - Ordinary code around the hints still executes normally.
//!
//! # See also
//! - `otter-vm::eval_ops`

use otter_runtime::{Runtime, SourceInput};

#[test]
fn native_syntax_optimization_hints_are_noops() {
    let mut rt = Runtime::builder().build().expect("runtime");
    let result = rt
        .run_script(
            SourceInput::from_javascript(
                r#"
                function f() { return 41; }
                eval('%PrepareFunctionForOptimization(f)');
                eval('%OptimizeFunctionOnNextCall(f)');
                f() + 1;
                "#,
            ),
            "<native-syntax-eval-hints>",
        )
        .expect("script")
        .completion_string()
        .to_string();

    assert_eq!(result, "42");
}
