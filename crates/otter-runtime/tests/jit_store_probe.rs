//! Guarded runtime store probes across prototype and descriptor changes.
//!
//! # Contents
//! - A hot direct-prototype-data transition that requires the runtime IC.
//! - Read-only, setter, own-slot, non-extensible and Proxy invalidations.
//! - Nested allocating getter/setter scopes, object throws and later reuse.
//! - Bounded runtime path counters and isolation of later capture batches.
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

#[test]
fn store_diagnostics_count_paths_without_replaying_effects() {
    use otter_runtime::{JitDebugEvent, JitDebugRequest};

    let source = include_str!("../../otter-difftest/corpus/property_handle_reentry.js");
    let mut runtime = Runtime::builder()
        .jit_selection(JitSelection::Template)
        .jit_debug(JitDebugRequest::events())
        .build()
        .expect("runtime");
    let result = runtime
        .run_script(SourceInput::from_javascript(source), "property-counts.js")
        .expect("captured property reentry");
    assert_eq!(result.completion_string(), "[16,12,7,66]");
    let report = result.jit_debug_report().expect("captured report");
    assert!(!report.truncated());
    let mut cached = 0;
    let mut failed = 0;
    let mut counters = 0;
    for event in report.events() {
        if let JitDebugEvent::PropertyStoreRuntime {
            property_name,
            path,
            failed: did_fail,
            native_way,
            count,
            ..
        } = event
        {
            counters += 1;
            if property_name == "x" {
                if *did_fail {
                    failed += count;
                }
                if matches!(path, otter_vm::jit_debug::JitPropertyStorePath::Cached) && !native_way
                {
                    cached += count;
                }
            }
        }
    }
    assert!(
        cached > 1000,
        "hot unsupported native ways must aggregate: {:?}",
        report
            .events()
            .iter()
            .filter(|event| matches!(event, JitDebugEvent::PropertyStoreRuntime { .. }))
            .collect::<Vec<_>>()
    );
    assert_eq!(failed, 3, "only the three throwing setters enter a store");
    assert!(
        counters < 100,
        "hot entries must not consume individual events"
    );

    let result = runtime
        .run_script(
            SourceInput::from_javascript(
                "copyProperty({x: {marker: 7}}, Object.create(prototype)).marker",
            ),
            "property-counts-next.js",
        )
        .expect("later script");
    assert_eq!(result.completion_string(), "7");
    let next = result.jit_debug_report().expect("later report");
    let count: u64 = next
        .events()
        .iter()
        .filter_map(|event| match event {
            JitDebugEvent::PropertyStoreRuntime { count, .. } => Some(*count),
            _ => None,
        })
        .sum();
    assert_eq!(count, 1, "later capture must not inherit old counters");
}
