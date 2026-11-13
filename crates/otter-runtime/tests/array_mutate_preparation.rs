//! Array mutation preserves inputs through reentrant preparation.
//!
//! # Contents
//! - Shared fill/reverse protocol corpus in all ordinary execution policies.
//!
//! # Invariants
//! - Length and bound coercions precede ordered mutation exactly once.
//! - Receivers, fill payloads and pending end operands remain current.
//! - Empty ranges still return the actual receiver after coercion.
//!
//! # See also
//! - `otter-difftest/corpus/array_mutate_preparation.js` owns shared cases.
//! - `otter-vm/src/array_prototype/mutate_tests.rs` proves actual moving GC.

use otter_runtime::{JitSelection, Runtime, SourceInput};

#[test]
fn preparation_and_live_mutation_keep_order_values_and_utf16() {
    for selection in [
        JitSelection::InterpreterOnly,
        JitSelection::Template,
        JitSelection::ProductionTiered,
    ] {
        let mut runtime = Runtime::builder().jit_selection(selection).build().unwrap();
        let result = runtime
            .run_script(
                SourceInput::from_javascript(include_str!(
                    "../../otter-difftest/corpus/array_mutate_preparation.js"
                )),
                "mutate-preparation.js",
            )
            .unwrap_or_else(|error| panic!("{selection:?}: {error:?}"));
        assert_eq!(result.completion_string(), "ok:mutate-preparation");
    }
}
