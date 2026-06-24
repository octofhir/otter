//! Runtime coverage for V8 native optimization hints in `eval`.
//!
//! # Contents
//! - Direct eval of `%PrepareFunctionForOptimization` and
//!   `%OptimizeFunctionOnNextCall`.
//!
//! # Invariants
//! - V8 optimization hints are accepted as no-ops for compatibility harnesses.
//! - Ordinary code around the hints still executes normally.
//!
//! # See also
//! - `otter-vm::eval_ops`

use otter_runtime::{Runtime, SourceInput};

#[test]
fn v8_native_eval_optimization_hints_are_noops() {
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
            "<v8-native-eval-hints>",
        )
        .expect("script")
        .completion_string()
        .to_string();

    assert_eq!(result, "42");
}
