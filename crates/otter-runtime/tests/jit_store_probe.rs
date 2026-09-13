//! Guarded runtime store probes across prototype and descriptor changes.
//!
//! # Contents
//! - Native direct-prototype-data transition fill and subsequent reuse.
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

#[test]
fn inherited_writable_data_fills_and_reuses_native_store_way() {
    use otter_runtime::{JitDebugEvent, JitDebugRequest};

    for selection in [JitSelection::Template, JitSelection::ProductionTiered] {
        let mut runtime = Runtime::builder()
            .jit_selection(selection)
            .jit_debug(JitDebugRequest::events())
            .build()
            .expect("runtime");
        let warm = runtime
            .run_script(
                SourceInput::from_javascript(
                    r#"
            const writableProto = { nativeWritable: 0 };
            function nativeWritableStore(receiver, value) {
                'use strict';
                receiver.nativeWritable = value;
                return value;
            }
            for (let i = 0; i < 6000; i++) {
                nativeWritableStore(Object.create(writableProto), i);
            }
        "#,
                ),
                "native-writable-warm.js",
            )
            .expect("warm store");
        assert!(
            warm.jit_debug_report()
                .unwrap()
                .events()
                .iter()
                .any(|event| {
                    matches!(event, JitDebugEvent::PropertyStoreRuntime {
                property_name, native_way: true, failed: false, ..
            } if property_name == "nativeWritable")
                }),
            "{selection:?}: native way must be offered"
        );
        let result = runtime
            .run_script(
                SourceInput::from_javascript(
                    r#"
            const fresh = Object.create(writableProto);
            const payload = { marker: 91 };
            nativeWritableStore(fresh, payload);
            fresh.nativeWritable === payload && Object.hasOwn(fresh, 'nativeWritable')
                && writableProto.nativeWritable === 0
        "#,
                ),
                "native-writable-reuse.js",
            )
            .expect("native reuse");
        assert_eq!(result.completion_string(), "true", "{selection:?}");
        let stores: u64 = result
            .jit_debug_report()
            .unwrap()
            .events()
            .iter()
            .filter_map(|event| match event {
                JitDebugEvent::PropertyStoreRuntime {
                    property_name,
                    count,
                    ..
                } if property_name == "nativeWritable" => Some(*count),
                _ => None,
            })
            .sum();
        assert_eq!(
            stores, 0,
            "{selection:?}: cached transition must stay generated"
        );
        let mutated = runtime
            .run_script(
                SourceInput::from_javascript(
                    r#"
            let deepTraps = 0;
            Object.setPrototypeOf(writableProto, new Proxy({}, {
                set() { deepTraps++; throw new Error('lookup went past writable own data'); }
            }));
            const deep = Object.create(writableProto);
            nativeWritableStore(deep, 17);
            const peerProto = { nativeWritable: 2 };
            const peer = Object.create(peerProto);
            nativeWritableStore(peer, 19);
            const blocked = Object.preventExtensions(Object.create(writableProto));
            let rejected = false;
            try { nativeWritableStore(blocked, 23); }
            catch (error) { rejected = error instanceof TypeError; }
            Object.freeze(peerProto);
            let frozen = false;
            const frozenTarget = Object.create(peerProto);
            try { nativeWritableStore(frozenTarget, 29); }
            catch (error) { frozen = error instanceof TypeError; }
            JSON.stringify([deep.nativeWritable, deepTraps, peer.nativeWritable,
                peerProto.nativeWritable, rejected, Object.hasOwn(blocked, 'nativeWritable'),
                frozen, Object.hasOwn(frozenTarget, 'nativeWritable')]);
        "#,
                ),
                "native-writable-mutations.js",
            )
            .expect("live prototype and receiver guards");
        assert_eq!(
            mutated.completion_string(),
            "[17,0,19,2,true,false,true,false]",
            "{selection:?}"
        );
    }
}

#[test]
fn proxy_receiver_define_preserves_relocated_descriptors() {
    let source = include_str!("../../otter-difftest/corpus/property_proxy_receiver_gc.js");
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
            .run_script(SourceInput::from_javascript(source), "proxy-receiver-gc.js")
            .expect("proxy receiver descriptor operations");
        assert_eq!(
            result.completion_string(),
            "[64,32,32,2016]",
            "{selection:?}"
        );
    }
}
