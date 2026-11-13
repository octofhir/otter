//! Fill admission preserves live Array length descriptors after coercion.
//!
//! # Contents
//! - Direct and Proxy controls for shrink-and-lock in both bound coercions.
//! - Prefix effects, exact failure and legal writes within read-only length.
//!
//! # Invariants
//! - A bulk hit cannot extend non-writable length.
//! - A declined hit enters ordinary Set without replaying bound coercions.
//! - Earlier valid writes remain observable when a later Set throws.
//!
//! # See also
//! - `otter-vm/src/array.rs::can_fast_fill_dense_range` owns bulk admission.

use otter_runtime::{JitSelection, Runtime, SourceInput};

#[test]
fn fill_respects_live_readonly_length_and_commits_the_valid_prefix() {
    for selection in [
        JitSelection::InterpreterOnly,
        JitSelection::Template,
        JitSelection::ProductionTiered,
    ] {
        let mut runtime = Runtime::builder().jit_selection(selection).build().unwrap();
        let result = runtime
            .run_script(
                SourceInput::from_javascript(include_str!(
                    "../../otter-difftest/corpus/array_fill_length_guard.js"
                )),
                "fill-length-guard.js",
            )
            .unwrap_or_else(|error| panic!("{selection:?}: {error:?}"));
        assert_eq!(result.completion_string(), "ok:fill-length-guard");
    }
}
