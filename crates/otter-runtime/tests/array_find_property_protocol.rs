//! Observable FindViaPredicate property operations through the public runtime.
//!
//! # Contents
//! - Shared differential fixture for all four Array `find*` methods.
//! - Forward/reverse order, holes, Proxy traps, abrupt completion and GC roots.
//! - Preservation of HasProperty for callback methods that skip absent indices.
//!
//! # Invariants
//! - FindViaPredicate performs Get without a preceding HasProperty operation.
//! - Getter values and thrown objects retain identity across collecting callbacks.
//! - Every supported tier policy agrees on the fixture's expected completion.
//!
//! # See also
//! - `otter-difftest/corpus/array_find_property_protocol.js` owns the shared fixture.
//! - <https://tc39.es/ecma262/#sec-findviapredicate>

use otter_runtime::{JitSelection, Runtime, SourceInput};

#[test]
fn find_methods_observe_get_without_has_property() {
    let source = include_str!("../../otter-difftest/corpus/array_find_property_protocol.js");
    for selection in [
        JitSelection::InterpreterOnly,
        JitSelection::Template,
        JitSelection::ProductionTiered,
    ] {
        let mut runtime = Runtime::builder()
            .jit_selection(selection)
            .build()
            .expect("runtime");
        let result = runtime
            .run_script(
                SourceInput::from_javascript(source),
                "<array-find-property-protocol>",
            )
            .expect("FindViaPredicate property protocol");
        assert_eq!(result.completion_string(), "ok:find-property-protocol");
    }
}
