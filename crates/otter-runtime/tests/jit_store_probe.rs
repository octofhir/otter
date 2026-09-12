//! Guarded runtime store probes across prototype and descriptor changes.
//!
//! # Contents
//! - A hot direct-prototype-data transition that requires the runtime IC.
//! - Read-only, setter, own-slot, non-extensible and Proxy invalidations.
//! - Nested allocating getter/setter scopes, object throws and later reuse.
//!
//! # Invariants
//! - Cached writes validate their complete guard program before effects.
//! - A miss preserves strict rejection and invokes each setter/Proxy once.
//! - Both generated tiers agree with the interpreter after hot-site mutation.
//!
//! # See also
//! - `otter_vm::property_dispatch::jit_runtime` owns generated store misses.

#![cfg(target_arch = "aarch64")]

use otter_runtime::{JitSelection, Runtime, SourceInput};

const SOURCE: &str = include_str!("../../otter-difftest/corpus/store_probe_invalidation.js");

#[test]
fn runtime_store_probe_preserves_invalidations_and_exact_effects() {
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
            .run_script(SourceInput::from_javascript(SOURCE), "store-probe.js")
            .expect("store invalidation sequence");
        assert_eq!(
            result.completion_string(),
            "[17997000,true,1,7,8,9,true,1,17]",
            "{selection:?}"
        );
        if selection != JitSelection::InterpreterOnly {
            let stats = runtime.execution_stats();
            assert!(
                stats.jit_runtime_property_stubs > 1000,
                "inherited-data transitions must exercise the runtime IC: {stats:?}"
            );
        }
    }
}

#[test]
fn property_handles_survive_nested_and_abrupt_reentry() {
    let source = include_str!("../../otter-difftest/corpus/property_handle_reentry.js");
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
            .run_script(SourceInput::from_javascript(source), "property-handles.js")
            .expect("nested and abrupt property reentry");
        assert_eq!(result.completion_string(), "[16,12,7,66]", "{selection:?}");
        if selection != JitSelection::InterpreterOnly {
            assert!(runtime.execution_stats().jit_runtime_property_stubs > 1000);
        }
    }
}
