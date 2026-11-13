//! Live Array join reads across reentrant string conversions.
//!
//! # Contents
//! - Shared differential corpus under all ordinary execution policies.
//!
//! # Invariants
//! - A ToString effect is visible to the next indexed Get.
//! - Source length remains captured while element values remain live.
//! - UTF-16 units, abrupt values and property-operation order are exact.
//!
//! # See also
//! - `otter-difftest/corpus/array_join_live_coercion.js` owns the JS contract.
//! - `otter-vm/src/array_prototype/join_rooting_tests.rs` checks root cleanup.

use otter_runtime::{JitSelection, Runtime, SourceInput};

#[test]
fn join_keeps_live_values_coercion_order_and_utf16_in_every_policy() {
    for selection in [
        JitSelection::InterpreterOnly,
        JitSelection::Template,
        JitSelection::ProductionTiered,
    ] {
        let mut runtime = Runtime::builder().jit_selection(selection).build().unwrap();
        let result = runtime
            .run_script(
                SourceInput::from_javascript(include_str!(
                    "../../otter-difftest/corpus/array_join_live_coercion.js"
                )),
                "join-live-coercion.js",
            )
            .unwrap_or_else(|error| panic!("{selection:?}: {error:?}"));
        assert_eq!(result.completion_string(), "ok:join-live-coercion");
    }
}
